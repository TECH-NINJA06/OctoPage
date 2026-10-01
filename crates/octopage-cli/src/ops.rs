use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Subcommand;
use octopage::{Database, ObjectId, Retention, Settings, Transport, Unlock};
use octopage_ops::host::Host;
use octopage_ops::{
    MaintainOptions, MigrateOptions, RolloverOptions, RolloverPolicy, reconcile, size, workflows,
};
use octopage_pagestore::{Error as StoreError, PageStore};

#[derive(Subcommand)]
pub enum Op {
    /// The maintenance job (what the scheduled workflow runs): remove abandoned staging refs,
    /// scan every page, project the repository's size, delete a previous generation after its
    /// grace period, and roll over when needed.
    Maintain {
        /// auto (when the size calls for it), always, or never.
        #[arg(long, default_value = "auto")]
        rollover: RolloverPolicy,
        /// Also append the report (Markdown) to this file, such as $GITHUB_STEP_SUMMARY.
        #[arg(long)]
        summary: Option<PathBuf>,
        /// A token that may create and delete repositories, for rollovers.
        #[arg(long, env = "OCTOPAGE_ADMIN_TOKEN", hide_env_values = true)]
        admin_token: Option<String>,
    },
    /// Move every database in the repository to a new one (OWNER/NAME-g<n+1>), keeping only the
    /// history their retention keeps. Clients follow on their own.
    Rollover {
        /// The new repository, OWNER/NAME.
        #[arg(long)]
        to: Option<String>,
        #[arg(long, env = "OCTOPAGE_ADMIN_TOKEN", hide_env_values = true)]
        admin_token: Option<String>,
    },
    /// Move the database to another page size, in a new repository. Clients must open it again.
    Migrate {
        /// 4096, 8192 or 16384.
        #[arg(long)]
        page_size: usize,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, env = "OCTOPAGE_ADMIN_TOKEN", hide_env_values = true)]
        admin_token: Option<String>,
    },
    /// Check every page of the database (and SQLite's own integrity check, with the key).
    Fsck {
        /// A commit (id prefix) or time instead of the head.
        #[arg(long)]
        at: Option<String>,
    },
    /// How large the database is, how fast it grows, and when the repository reaches its budget.
    Stats,
    /// After the branch's history was rewritten by hand: show what it dropped, then --restore it
    /// (run it again on the head) or --accept the loss.
    Reconcile {
        #[arg(long, conflicts_with = "restore")]
        accept: bool,
        #[arg(long)]
        restore: bool,
    },
    /// Show the database's maintenance settings, or change them (a commit).
    Settings {
        /// keep_all, "keep_days N" or "keep_count N".
        #[arg(long)]
        retention: Option<Retention>,
        /// The most live data, in MB (the database refuses to grow past it).
        #[arg(long)]
        live_limit_mb: Option<u64>,
        /// The repository size to stay under, in MB.
        #[arg(long)]
        budget_mb: Option<u64>,
        /// The longest a reader may hold a snapshot, in days.
        #[arg(long)]
        snapshot_days: Option<u32>,
        /// How early to warn about the budget, in days.
        #[arg(long)]
        warn_days: Option<u32>,
        /// How long a repository the database moved out of is kept, in days.
        #[arg(long)]
        grace_days: Option<u32>,
    },
    /// The GitHub Actions workflow that maintains the repository.
    Workflows {
        #[command(subcommand)]
        action: WorkflowAction,
    },
}

impl Op {
    /// The admin token, for the subcommands that take one.
    pub fn admin_token(&self) -> Option<&str> {
        match self {
            Op::Maintain { admin_token, .. }
            | Op::Rollover { admin_token, .. }
            | Op::Migrate { admin_token, .. } => admin_token.as_deref(),
            _ => None,
        }
    }
}

#[derive(Subcommand)]
pub enum WorkflowAction {
    /// Print the workflow.
    Print {
        /// The git URL `cargo install` builds the octopage CLI from.
        #[arg(long)]
        cli_source: String,
        #[arg(long, default_value = "main")]
        cli_ref: String,
    },
    /// Commit the workflow to the repository's default branch (or --into): the token needs
    /// permission to change workflows.
    Install {
        #[arg(long)]
        cli_source: String,
        #[arg(long, default_value = "main")]
        cli_ref: String,
        /// The branch to commit it to, without refs/heads/.
        #[arg(long)]
        into: Option<String>,
    },
}

/// A host without an administrative API (a plain git server): nothing can be created, deleted
/// or measured, and issues are printed instead.
pub struct NoHost;

impl Host for NoHost {
    async fn create_repository(&self, _: &str, _: bool) -> octopage::Result<()> {
        Err(octopage::Error::Invalid(
            "creating repositories needs GitHub's API: use --repo".into(),
        ))
    }

    async fn delete_repository(&self, _: &str) -> octopage::Result<()> {
        Err(octopage::Error::Invalid(
            "deleting repositories needs GitHub's API: use --repo".into(),
        ))
    }

    async fn exists(&self, _: &str) -> octopage::Result<bool> {
        Ok(false)
    }

    async fn repository_size(&self, _: &str) -> octopage::Result<Option<u64>> {
        Ok(None)
    }

    async fn default_branch(&self, _: &str) -> octopage::Result<Option<String>> {
        Ok(None)
    }

    async fn set_default_branch(&self, _: &str, _: &str) -> octopage::Result<()> {
        Ok(())
    }

    async fn open_issue(&self, location: &str, title: &str, _: &str) -> octopage::Result<bool> {
        eprintln!("{location}: {title}");
        Ok(false)
    }
}

/// What every subcommand needs.
pub struct Context<'a, T> {
    pub transport: &'a dyn Fn() -> octopage::Result<T>,
    pub config: octopage::Config,
    pub interactive: bool,
}

fn fail(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl<T: Transport + 'static> Context<'_, T> {
    fn transport(&self) -> Result<Arc<T>, String> {
        Ok(Arc::new((self.transport)().map_err(fail)?))
    }

    fn branch(&self) -> &str {
        &self.config.store.branch
    }

    /// The database, unlocked if it is encrypted (asking for the passphrase if need be).
    async fn open(&self) -> Result<Database<T>, String> {
        match Database::open((self.transport)().map_err(fail)?, self.config.clone()).await {
            Err(octopage::Error::Store(StoreError::Locked)) if self.interactive => {
                let passphrase = rpassword::prompt_password("Passphrase: ").map_err(fail)?;
                let mut config = self.config.clone();
                config.store.unlock = Some(Unlock::Passphrase(passphrase));
                Database::open((self.transport)().map_err(fail)?, config)
                    .await
                    .map_err(fail)
            }
            Err(octopage::Error::Store(StoreError::Locked)) => {
                Err("the database is encrypted: set OCTOPAGE_PASSPHRASE (or --recovery-key)".into())
            }
            other => other.map_err(fail),
        }
    }

    /// The page store, unlocked if the key is at hand, otherwise only its structure.
    async fn store(&self) -> Result<PageStore<T>, String> {
        let mut config = self.config.store.clone();
        config.head_poll = None;
        match PageStore::open_shared(self.transport()?, config.clone()).await {
            Err(StoreError::Locked | StoreError::WrongKey) => {
                PageStore::open_locked(self.transport()?, config)
                    .await
                    .map_err(fail)
            }
            other => other.map_err(fail),
        }
    }
}

fn megabytes(bytes: f64) -> String {
    format!("{:.1} MB", bytes / (1u64 << 20) as f64)
}

pub async fn run<T: Transport + 'static, H: Host>(
    op: Op,
    cx: &Context<'_, T>,
    host: impl Fn(Option<&str>) -> H,
) -> Result<ExitCode, String> {
    match op {
        Op::Maintain {
            rollover,
            summary,
            admin_token,
        } => {
            let options = MaintainOptions {
                unlock: cx.config.store.unlock.clone(),
                rollover,
                ..MaintainOptions::default()
            };
            let host = host(admin_token.as_deref());
            let report = octopage_ops::maintain(cx.transport()?, &host, &options)
                .await
                .map_err(fail)?;
            let markdown = report.to_markdown();
            print!("{markdown}");
            if let Some(path) = summary {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .and_then(|mut f| f.write_all(markdown.as_bytes()))
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
            Ok(if report.is_healthy() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Op::Rollover { to, admin_token } => {
            let host = host(admin_token.as_deref());
            let options = RolloverOptions {
                location: to,
                ..RolloverOptions::default()
            };
            let report = octopage_ops::rollover(cx.transport()?, &host, &options)
                .await
                .map_err(fail)?;
            println!(
                "moved {} to {} (generation {}): {} objects, {}",
                report.from,
                report.to,
                report.generation,
                report.objects,
                megabytes(report.bytes as f64)
            );
            for b in &report.branches {
                println!(
                    "  {}: kept {} of {} commits{}",
                    b.branch.trim_start_matches("refs/heads/"),
                    b.kept,
                    b.commits,
                    if b.database { "" } else { " (copied as it is)" }
                );
            }
            for tag in &report.skipped {
                println!("  not copied: {tag}");
            }
            println!(
                "Point clients at {}; the old repository is kept for its grace period.",
                report.to
            );
            Ok(ExitCode::SUCCESS)
        }
        Op::Migrate {
            page_size,
            to,
            admin_token,
        } => {
            let db = cx.open().await?;
            let host = host(admin_token.as_deref());
            let options = MigrateOptions {
                page_size,
                location: to,
                ..MigrateOptions::default()
            };
            let report = octopage_ops::migrate(&db, &host, &options)
                .await
                .map_err(fail)?;
            println!(
                "migrated {} to {} with {}-byte pages: {} tables, {} rows; {} commits made \
                 meanwhile were run again there",
                report.from,
                report.to,
                report.page_size,
                report.tables,
                report.rows,
                report.replayed
            );
            println!("Clients must open the database again ({}).", report.to);
            Ok(ExitCode::SUCCESS)
        }
        Op::Fsck { at } => fsck(cx, at).await,
        Op::Stats => {
            let store = cx.store().await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let snapshot = store.latest().await.map_err(fail)?;
            let settings = match snapshot.settings().await.map_err(fail)? {
                Some(bytes) => Settings::parse(&bytes)?,
                None => Settings::default(),
            };
            let growth = size::growth(&store, now).await.map_err(fail)?;
            let location = store.location().unwrap_or_default();
            let repository = host(None).repository_size(&location).await.unwrap_or(None);
            let projection = size::project(std::slice::from_ref(&growth), repository, &settings);
            println!("database     {} ({})", cx.branch(), location);
            println!(
                "live         {} pages of {} bytes, {} (limit {})",
                growth.pages,
                snapshot.page_size(),
                megabytes(growth.live_bytes as f64),
                megabytes(settings.live_limit as f64)
            );
            println!(
                "growth       {} commits in {:.0} days, {} a day",
                growth.commits,
                growth.days,
                megabytes(growth.bytes_per_day)
            );
            println!(
                "repository   {} of a {} budget",
                repository.map_or_else(|| "size unknown".into(), |b| megabytes(b as f64)),
                megabytes(settings.repository_budget as f64)
            );
            match projection.days_left {
                Some(days) => println!("budget       reached in about {days:.0} days"),
                None => println!("budget       not growing"),
            }
            println!("retention    {}", settings.retention);
            Ok(ExitCode::SUCCESS)
        }
        Op::Reconcile { accept, restore } => {
            let transport = cx.transport()?;
            let Some(d) = reconcile::divergence(&*transport, cx.branch())
                .await
                .map_err(fail)?
            else {
                println!(
                    "{} descends from the head the maintenance job last checked: nothing to \
                     reconcile",
                    cx.branch()
                );
                return Ok(ExitCode::SUCCESS);
            };
            let store = cx.store().await?;
            let describe = async |id: ObjectId| match store.snapshot(id).await {
                Ok(s) => format!("{}  {}", &id.to_hex()[..12], s.message()),
                Err(_) => id.to_hex()[..12].to_string(),
            };
            println!(
                "{} was moved from {} to {}, which does not descend from it.",
                cx.branch(),
                &d.checkpoint.to_hex()[..12],
                &d.head.to_hex()[..12]
            );
            println!("Dropped ({}):", d.lost.len());
            for id in &d.lost {
                println!("  {}", describe(*id).await);
            }
            println!("Added since ({}):", d.added.len());
            for id in &d.added {
                println!("  {}", describe(*id).await);
            }
            if accept {
                reconcile::accept(&*transport, &d).await.map_err(fail)?;
                println!("Accepted: the dropped commits are let go.");
            } else if restore {
                let db = cx.open().await?;
                let restored = tokio::task::spawn_blocking(move || reconcile::restore(&db, &d))
                    .await
                    .map_err(fail)?
                    .map_err(fail)?;
                for (old, new) in restored {
                    match new {
                        Some(new) => println!(
                            "  restored {} as {}",
                            &old.to_hex()[..12],
                            &new.to_hex()[..12]
                        ),
                        None => println!("  {} had nothing to run", &old.to_hex()[..12]),
                    }
                }
            } else {
                println!("Run again with --restore to apply them to the head, or --accept.");
                return Ok(ExitCode::FAILURE);
            }
            Ok(ExitCode::SUCCESS)
        }
        Op::Settings {
            retention,
            live_limit_mb,
            budget_mb,
            snapshot_days,
            warn_days,
            grace_days,
        } => {
            let changing = retention.is_some()
                || live_limit_mb.is_some()
                || budget_mb.is_some()
                || snapshot_days.is_some()
                || warn_days.is_some()
                || grace_days.is_some();
            if !changing {
                let store = cx.store().await?;
                let snapshot = store.latest().await.map_err(fail)?;
                let settings = match snapshot.settings().await.map_err(fail)? {
                    Some(bytes) => Settings::parse(&bytes)?,
                    None => Settings::default(),
                };
                println!("{}", String::from_utf8_lossy(&settings.to_bytes()));
                return Ok(ExitCode::SUCCESS);
            }
            let db = cx.open().await?;
            let mut settings = db.settings().await.map_err(fail)?;
            if let Some(r) = retention {
                settings.retention = r;
            }
            if let Some(mb) = live_limit_mb {
                settings.live_limit = mb << 20;
            }
            if let Some(mb) = budget_mb {
                settings.repository_budget = mb << 20;
            }
            if let Some(d) = snapshot_days {
                settings.snapshot_days = d;
            }
            if let Some(d) = warn_days {
                settings.warn_days = d;
            }
            if let Some(d) = grace_days {
                settings.grace_days = d;
            }
            let commit = db.set_settings(&settings).await.map_err(fail)?;
            println!("committed {}", &commit.to_hex()[..12]);
            println!("{}", String::from_utf8_lossy(&settings.to_bytes()));
            Ok(ExitCode::SUCCESS)
        }
        Op::Workflows { action } => match action {
            WorkflowAction::Print {
                cli_source,
                cli_ref,
            } => {
                print!("{}", workflows::maintenance_workflow(&cli_source, &cli_ref));
                Ok(ExitCode::SUCCESS)
            }
            WorkflowAction::Install {
                cli_source,
                cli_ref,
                into,
            } => {
                let transport = cx.transport()?;
                let location = transport.location().unwrap_or_default();
                let branch = match into {
                    Some(b) => b,
                    None => host(None)
                        .default_branch(&location)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "main".into()),
                };
                let yaml = workflows::maintenance_workflow(&cli_source, &cli_ref);
                let commit = workflows::install(
                    transport,
                    &format!("refs/heads/{branch}"),
                    workflows::MAINTENANCE_PATH,
                    yaml.as_bytes(),
                    cx.config.store.unlock.clone(),
                )
                .await
                .map_err(fail)?;
                println!(
                    "committed {} to {branch}: {}",
                    &commit.to_hex()[..12],
                    workflows::MAINTENANCE_PATH
                );
                println!(
                    "For rollovers add the secret OCTOPAGE_ADMIN_TOKEN, and for full scans of an \
                     encrypted database OCTOPAGE_PASSPHRASE (Settings > Secrets and variables > \
                     Actions)."
                );
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}

async fn fsck<T: Transport + 'static>(
    cx: &Context<'_, T>,
    at: Option<String>,
) -> Result<ExitCode, String> {
    let store = cx.store().await?;
    let commit = match &at {
        None => store.refresh().await.map_err(fail)?,
        Some(target) => {
            let db = Database::from_store(store.clone(), cx.config.clone()).map_err(fail)?;
            db.resolve(target).await.map_err(fail)?
        }
    };
    let scan = store
        .snapshot(commit)
        .await
        .map_err(fail)?
        .scan()
        .await
        .map_err(fail)?;
    println!(
        "{} at {}: {} pages, {}{}",
        cx.branch(),
        &commit.to_hex()[..12],
        scan.pages,
        megabytes(scan.bytes as f64),
        if scan.decoded {
            ""
        } else {
            " (encrypted, no key: framing checked only)"
        }
    );
    let mut problems = scan.problems.clone();
    if !store.is_locked() {
        let db = Database::from_store(store, cx.config.clone())
            .map_err(fail)?
            .at(commit);
        let lines = tokio::task::spawn_blocking(move || -> octopage::Result<Vec<String>> {
            let rows = db.connect()?.query("PRAGMA integrity_check", &[])?;
            Ok(rows
                .rows
                .iter()
                .filter_map(|r| r.first())
                .map(|v| v.to_string())
                .filter(|l| l != "ok")
                .collect())
        })
        .await
        .map_err(fail)?
        .map_err(fail)?;
        problems.extend(lines.into_iter().map(|l| format!("SQLite: {l}")));
    }
    if problems.is_empty() {
        println!("ok");
        return Ok(ExitCode::SUCCESS);
    }
    for p in &problems {
        println!("  {p}");
    }
    println!("{} problems", problems.len());
    Ok(ExitCode::FAILURE)
}

/// Whether the shell can ask questions.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal()
}
