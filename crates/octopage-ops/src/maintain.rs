use std::sync::Arc;
use std::time::Duration;

use octopage::{Database, Error, ObjectId, Result, Scan, Settings, Transport, Unlock};
use octopage_git::RefUpdate;
use octopage_pagestore::{GEN_PREFIX, Moved, PageStore};

use crate::host::Host;
use crate::hygiene::{STAGE_MAX_AGE, clean_staging};
use crate::records::{
    self, GenerationRecord, MAINTENANCE_REF, checkpoint_ref, read_ref, take_lease,
};
use crate::rollover::{RolloverOptions, RolloverReport, rollover};
use crate::size::{Growth, Projection, growth, project};
use crate::walk::{Commits, database_root, root_tree};

/// When to roll over to a new generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RolloverPolicy {
    /// When the repository will reach its budget within the warning period, or has used 80%
    /// of it, and some database's retention would drop history.
    #[default]
    Auto,
    Always,
    Never,
}

impl std::str::FromStr for RolloverPolicy {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(RolloverPolicy::Auto),
            "always" => Ok(RolloverPolicy::Always),
            "never" => Ok(RolloverPolicy::Never),
            _ => Err(format!(
                "{s:?}: the rollover policy is auto, always or never"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MaintainOptions {
    /// The time the job measures from (seconds since the Unix epoch); by default, now.
    pub now: Option<i64>,
    /// How to open encrypted databases, for full scans. Without it they get structural scans.
    pub unlock: Option<Unlock>,
    pub rollover: RolloverPolicy,
    pub rollover_options: RolloverOptions,
    /// How long the maintenance lease lasts; a job still running after it can be overlapped.
    pub lease: Duration,
}

impl Default for MaintainOptions {
    fn default() -> Self {
        MaintainOptions {
            now: None,
            unlock: None,
            rollover: RolloverPolicy::Auto,
            rollover_options: RolloverOptions::default(),
            lease: Duration::from_secs(3 * 3600),
        }
    }
}

/// One database, as the job found it.
#[derive(Clone, Debug)]
pub struct DatabaseReport {
    pub branch: String,
    pub head: ObjectId,
    pub encrypted: bool,
    pub scan: Option<Scan>,
    /// SQLite's `integrity_check` answers other than `ok`; `None` when it could not run
    /// (an encrypted database without its key).
    pub sql_check: Option<Vec<String>>,
    pub growth: Option<Growth>,
    pub settings: Settings,
    /// The head does not descend from the last checked head: the history was rewritten.
    pub rewritten_from: Option<ObjectId>,
}

/// What the job did.
#[derive(Clone, Debug, Default)]
pub struct MaintainReport {
    pub location: String,
    /// Why the job did nothing.
    pub skipped: Option<String>,
    pub staging_removed: Vec<String>,
    pub databases: Vec<DatabaseReport>,
    pub projection: Option<Projection>,
    /// Titles of the issues opened.
    pub issues: Vec<String>,
    pub previous_deleted: Option<String>,
    pub rollover: Option<RolloverReport>,
    /// Things that went wrong, one line each: the job fails when there are any.
    pub problems: Vec<String>,
    /// Things worth knowing that are not failures.
    pub notes: Vec<String>,
}

impl MaintainReport {
    pub fn is_healthy(&self) -> bool {
        self.problems.is_empty()
    }

    /// A summary for people (Markdown: GitHub shows it on the workflow run's page).
    pub fn to_markdown(&self) -> String {
        let mut out = format!("## OctoPage maintenance: {}\n\n", self.location);
        if let Some(why) = &self.skipped {
            out += &format!("Nothing to do: {why}\n");
            return out;
        }
        if self.problems.is_empty() {
            out += "**Healthy.**\n\n";
        } else {
            out += "**Problems:**\n\n";
            for p in &self.problems {
                out += &format!("- {p}\n");
            }
            out += "\n";
        }
        out += "| Database | Pages | Live | Scan | Growth a day | Retention |\n";
        out += "|---|---:|---:|---|---:|---|\n";
        for db in &self.databases {
            let scan = match &db.scan {
                Some(s) if s.is_clean() && s.decoded => format!("{} pages ok", s.pages),
                Some(s) if s.is_clean() => format!("{} pages ok (framing only)", s.pages),
                Some(s) => format!("{} problems", s.problems.len()),
                None => "not run".into(),
            };
            let (pages, live, per_day) = match &db.growth {
                Some(g) => (
                    g.pages.to_string(),
                    megabytes(g.live_bytes as f64),
                    megabytes(g.bytes_per_day),
                ),
                None => ("?".into(), "?".into(), "?".into()),
            };
            out += &format!(
                "| {} | {pages} | {live} | {scan} | {per_day} | {} |\n",
                db.branch.trim_start_matches("refs/heads/"),
                db.settings.retention
            );
        }
        if let Some(p) = &self.projection {
            out += &format!(
                "\nRepository: {} of a {} budget, growing {} a day; {}.\n",
                p.repository_bytes
                    .map_or_else(|| "unknown size".into(), |b| megabytes(b as f64)),
                megabytes(p.budget as f64),
                megabytes(p.bytes_per_day),
                match p.days_left {
                    Some(d) => format!("about {d:.0} days left"),
                    None => "not growing".into(),
                }
            );
        }
        if !self.staging_removed.is_empty() {
            out += &format!(
                "\nRemoved {} abandoned staging refs.\n",
                self.staging_removed.len()
            );
        }
        if let Some(r) = &self.rollover {
            out += &format!(
                "\nRolled over to **{}** (generation {}): {} objects, {} copied.\n",
                r.to,
                r.generation,
                r.objects,
                megabytes(r.bytes as f64)
            );
        }
        if let Some(previous) = &self.previous_deleted {
            out += &format!("\nDeleted the previous generation, {previous}.\n");
        }
        for issue in &self.issues {
            out += &format!("\nOpened an issue: {issue}\n");
        }
        for note in &self.notes {
            out += &format!("\nNote: {note}\n");
        }
        out
    }
}

fn megabytes(bytes: f64) -> String {
    format!("{:.1} MB", bytes / (1u64 << 20) as f64)
}

/// Run the maintenance job on the repository `transport` points at.
pub async fn maintain<T: Transport + 'static, H: Host>(
    transport: Arc<T>,
    host: &H,
    options: &MaintainOptions,
) -> Result<MaintainReport> {
    let location = transport
        .location()
        .unwrap_or_else(|| "the repository".into());
    let now = options.now.unwrap_or_else(records::now);
    let mut report = MaintainReport {
        location: location.clone(),
        ..MaintainReport::default()
    };
    let pointers = transport.list_refs(&[GEN_PREFIX]).await?;
    if let Some(pointer) = pointers.last() {
        let moved = Moved::from_pointer(records::object(&*transport, pointer.id).await?.data())?;
        report.skipped = Some(format!(
            "the databases moved to {} (generation {}); maintain that repository",
            moved.location, moved.generation
        ));
        return Ok(report);
    }
    let holder = format!("octopage-maintenance-{:016x}", fastrand::u64(..));
    let Some(lease) = take_lease(
        &*transport,
        MAINTENANCE_REF,
        &holder,
        options.lease,
        Duration::ZERO,
    )
    .await?
    else {
        report.skipped = Some("another maintenance job is running".into());
        return Ok(report);
    };
    let outcome = run(transport.clone(), host, options, now, &mut report).await;
    lease.release(&*transport).await;
    outcome?;
    Ok(report)
}

async fn run<T: Transport + 'static, H: Host>(
    transport: Arc<T>,
    host: &H,
    options: &MaintainOptions,
    now: i64,
    report: &mut MaintainReport,
) -> Result<()> {
    report.staging_removed = clean_staging(&*transport, now, STAGE_MAX_AGE).await?;

    let mut commits = Commits::default();
    for head in transport.list_refs(&["refs/heads/"]).await? {
        let (_, commit) = commits.load(&*transport, head.id).await?;
        let tree = root_tree(&*transport, commit.tree).await?;
        if database_root(&tree).is_none() {
            continue;
        }
        let db = check_database(&transport, &head.name, head.id, options, now, &mut commits)
            .await
            .unwrap_or_else(|e| {
                report.problems.push(format!("{}: {e}", head.name));
                None
            });
        let Some(db) = db else { continue };
        let mut damage: Vec<String> = Vec::new();
        if let Some(scan) = &db.scan {
            damage.extend(scan.problems.iter().take(20).cloned());
            if scan.problems.len() > 20 {
                damage.push(format!(
                    "and {} more page problems",
                    scan.problems.len() - 20
                ));
            }
        }
        damage.extend(
            db.sql_check
                .iter()
                .flatten()
                .map(|line| format!("SQLite integrity_check: {line}")),
        );
        if !damage.is_empty() {
            report
                .problems
                .extend(damage.iter().map(|d| format!("{}: {d}", db.branch)));
            let title = format!(
                "OctoPage: the integrity scan of {} found problems",
                db.branch.trim_start_matches("refs/heads/")
            );
            let body = format!(
                "The maintenance job scanned `{}` at `{}` and found:\n\n{}\n\n\
                 Run `octopage fsck --repo {} --branch {}` to check again. To recover, read the \
                 database `AS OF` the last good commit, and run later changelogs again.",
                db.branch,
                db.head,
                damage
                    .iter()
                    .map(|d| format!("- {d}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                report.location,
                db.branch
            );
            if host.open_issue(&report.location, &title, &body).await? {
                report.issues.push(title);
            }
        }
        if let Some(old) = db.rewritten_from {
            let title = format!(
                "OctoPage: the history of {} was rewritten",
                db.branch.trim_start_matches("refs/heads/")
            );
            let body = format!(
                "The head of `{}` is `{}`, which does not descend from `{old}`, the head the \
                 maintenance job checked last. Someone moved the branch by hand (a force-push, \
                 say). Commits after the point where the histories part are not in the \
                 database any more, but the checkpoint ref `{}` still holds them.\n\n\
                 Run `octopage reconcile --repo {} --branch {}` to see them, then \
                 `--restore` to run them again on the current head, or `--accept` to let \
                 them go.",
                db.branch,
                db.head,
                checkpoint_ref(&db.branch),
                report.location,
                db.branch
            );
            report.problems.push(format!(
                "{}: the history was rewritten (the head does not descend from {old})",
                db.branch
            ));
            if host.open_issue(&report.location, &title, &body).await? {
                report.issues.push(title);
            }
        }
        report.databases.push(db);
    }

    // The repository's size against the tightest budget of its databases.
    let settings = report
        .databases
        .iter()
        .map(|d| d.settings.clone())
        .min_by_key(|s| s.repository_budget)
        .unwrap_or_default();
    let growths: Vec<Growth> = report
        .databases
        .iter()
        .filter_map(|d| d.growth.clone())
        .collect();
    let size = match host.repository_size(&report.location).await {
        Ok(size) => size,
        Err(e) => {
            report
                .notes
                .push(format!("could not read the repository's size: {e}"));
            None
        }
    };
    let projection = project(&growths, size, &settings);
    if projection.warns(&settings) {
        let days = projection.days_left.unwrap_or_default();
        let title = format!(
            "OctoPage: {} will reach its size budget in about {days:.0} days",
            report.location
        );
        let body = format!(
            "At {:.1} MB a day, this repository reaches its {:.0} MB budget in about {days:.0} \
             days. The maintenance job rolls the databases over to a new generation (a new \
             repository with only the history their retention keeps) when it has an admin \
             token (`OCTOPAGE_ADMIN_TOKEN`); otherwise run `octopage rollover`. Tighter \
             retention (`octopage settings --retention`) slows the growth.",
            projection.bytes_per_day / (1u64 << 20) as f64,
            projection.budget as f64 / (1u64 << 20) as f64
        );
        if host.open_issue(&report.location, &title, &body).await? {
            report.issues.push(title);
        }
    }

    delete_previous(&transport, host, &settings, now, report).await?;

    let drops_history = report
        .databases
        .iter()
        .any(|d| d.settings.retention != octopage::Retention::KeepAll);
    let crowded = projection
        .repository_bytes
        .is_some_and(|b| b as f64 >= 0.8 * projection.budget as f64);
    let roll = match options.rollover {
        RolloverPolicy::Always => true,
        RolloverPolicy::Never => false,
        RolloverPolicy::Auto => drops_history && (projection.warns(&settings) || crowded),
    };
    report.projection = Some(projection);
    if roll {
        if report.databases.iter().any(|d| d.rewritten_from.is_some()) {
            report
                .notes
                .push("not rolling over: a history was rewritten; reconcile it first".into());
        } else {
            match rollover(transport.clone(), host, &options.rollover_options).await {
                Ok(done) => report.rollover = Some(done),
                Err(e) => report.problems.push(format!("the rollover failed: {e}")),
            }
        }
    }
    Ok(())
}

/// Scan one database, check its history against the checkpoint, and measure its growth.
async fn check_database<T: Transport + 'static>(
    transport: &Arc<T>,
    branch: &str,
    head: ObjectId,
    options: &MaintainOptions,
    now: i64,
    commits: &mut Commits,
) -> Result<Option<DatabaseReport>> {
    let config = octopage_pagestore::Config {
        branch: branch.to_string(),
        head_poll: None,
        lease_after_losses: None,
        unlock: options.unlock.clone(),
        ..octopage_pagestore::Config::default()
    };
    let store = match PageStore::open_shared(transport.clone(), config.clone()).await {
        Ok(store) => store,
        // An encrypted database the job cannot unlock: work on its structure.
        Err(octopage_pagestore::Error::Locked | octopage_pagestore::Error::WrongKey) => {
            PageStore::open_locked(transport.clone(), config).await?
        }
        Err(e) => return Err(e.into()),
    };
    let snapshot = store.snapshot(head).await?;
    if snapshot.moved().await?.is_some() {
        return Ok(None);
    }
    let settings = match snapshot.settings().await? {
        Some(bytes) => Settings::parse(&bytes).map_err(Error::Invalid)?,
        None => Settings::default(),
    };
    let scan = snapshot.scan().await?;
    let sql_check = if store.is_locked() {
        None
    } else {
        Some(sql_integrity(store.clone(), head).await?)
    };
    let rewritten_from = checkpoint(&**transport, branch, head, commits).await?;
    let growth = growth(&store, now).await?;
    Ok(Some(DatabaseReport {
        branch: branch.to_string(),
        head,
        encrypted: store.is_encrypted(),
        scan: Some(scan),
        sql_check,
        growth: Some(growth),
        settings,
        rewritten_from,
    }))
}

/// SQLite's own check of the database at `commit`: the lines it reports other than `ok`.
async fn sql_integrity<T: Transport + 'static>(
    store: PageStore<T>,
    commit: ObjectId,
) -> Result<Vec<String>> {
    let db = Database::from_store(store, octopage::Config::default())?.at(commit);
    tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let rows = db.connect()?.query("PRAGMA integrity_check", &[])?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| row.first())
            .map(|v| v.to_string())
            .filter(|line| line != "ok")
            .collect())
    })
    .await
    .map_err(|e| Error::Invalid(format!("the integrity check stopped: {e}")))?
}

/// Move the checkpoint to `head` if it descends from it. Returns the checkpoint if it does
/// not: the history was rewritten.
async fn checkpoint<T: Transport>(
    t: &T,
    branch: &str,
    head: ObjectId,
    commits: &mut Commits,
) -> Result<Option<ObjectId>> {
    let name = checkpoint_ref(branch);
    let update = match read_ref(t, &name).await? {
        None => RefUpdate::create(&name, head),
        Some(old) if old == head => return Ok(None),
        Some(old) => {
            let chain = commits.chain(t, head, Some(old)).await?;
            let descends = chain
                .last()
                .and_then(|last| commits.get(*last))
                .is_some_and(|(_, c)| c.parents.first() == Some(&old));
            if !descends {
                return Ok(Some(old));
            }
            RefUpdate::update(&name, old, head)
        }
    };
    match t.push(&[update], &[]).await {
        Ok(()) | Err(octopage_git::Error::Conflict { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete the repository the databases moved out of, once its grace period is over and it
/// says it moved here.
async fn delete_previous<T: Transport + 'static, H: Host>(
    transport: &Arc<T>,
    host: &H,
    settings: &Settings,
    now: i64,
    report: &mut MaintainReport,
) -> Result<()> {
    let Some((commit, mut record)) = GenerationRecord::read(&**transport).await? else {
        return Ok(());
    };
    let (Some(previous), Some(sealed)) = (record.previous.clone(), record.sealed) else {
        return Ok(());
    };
    if record.previous_deleted || now < sealed + settings.grace_days as i64 * 86_400 {
        return Ok(());
    }
    // Only a repository whose pointer names this one.
    let old = match (**transport).relocate(&previous) {
        Ok(old) => old,
        Err(e) => {
            report.notes.push(format!(
                "cannot reach the previous generation {previous}: {e}"
            ));
            return Ok(());
        }
    };
    let pointer = match old.list_refs(&[GEN_PREFIX]).await {
        Ok(refs) => refs.into_iter().last(),
        Err(_) => None, // deleted already, or unreachable
    };
    let points_here = match pointer {
        Some(p) => Moved::from_pointer(records::object(&old, p.id).await?.data())
            .is_ok_and(|m| Some(m.location) == transport.location()),
        None => false,
    };
    if !points_here {
        if !host.exists(&previous).await.unwrap_or(true) {
            record.previous_deleted = true; // gone already
            record.write(&**transport, Some(commit)).await?;
        } else {
            report.notes.push(format!(
                "not deleting {previous}: it does not point to this repository"
            ));
        }
        return Ok(());
    }
    match host.delete_repository(&previous).await {
        Ok(()) => {
            record.previous_deleted = true;
            record.write(&**transport, Some(commit)).await?;
            report.previous_deleted = Some(previous);
        }
        Err(e) => report.notes.push(format!(
            "the previous generation {previous} is past its grace period but could not be \
             deleted: {e}"
        )),
    }
    Ok(())
}
