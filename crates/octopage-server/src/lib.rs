use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use octopage::{Database, Encryption, Kdf, KeyProvider, Nonces, Unlock};

pub mod api;
pub mod error;
pub mod github;
pub mod governor;
pub mod kms;
pub mod meta;
pub mod metrics;
pub mod served;
pub mod values;
pub mod webhook;

use error::{ApiError, ApiResult};
use github::GitHubApp;
use governor::{Budget, Governed, Governor};
use meta::{DatabaseRecord, KeyMode, Meta, User};
use metrics::Metrics;
use served::{Db, Served, Transactions};

/// How the service runs.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Where repositories' page caches live, one directory each.
    pub data_dir: PathBuf,
    /// The service's public URL, for sign-in redirects: `https://api.octopage.dev`.
    pub public_url: String,
    /// The GitHub App's webhook secret; without it, deliveries are refused.
    pub webhook_secret: Option<String>,
    /// Each installation's pace of requests to GitHub.
    pub budget: Budget,
    /// How long an unused database stays open.
    pub idle: Duration,
    /// How long an interactive transaction may sit unused before it rolls back.
    pub transaction_idle: Duration,
    /// How long a sign-in session lasts.
    pub session: Duration,
    /// Read the head at the start of every transaction (a round trip each). Without it, the
    /// service learns of other clients' commits from webhooks and the 30-second poll.
    pub refresh_on_begin: bool,
    /// The dashboard's built files, served at `/` (none: no dashboard).
    pub dashboard_dir: Option<PathBuf>,
    /// The GitHub App's name in URLs (`https://github.com/apps/<slug>`), for the link that
    /// installs it.
    pub app_slug: Option<String>,
    /// Where the maintenance workflow builds the `octopage` CLI from (an `https://` git URL),
    /// and the branch.
    pub cli_source: Option<String>,
    pub cli_ref: String,
    /// Serve `/metrics` with the API. Off when the metrics have a listener of their own
    /// ([`api::metrics_router`]): its per-installation counts are not for the public.
    pub public_metrics: bool,
}

impl ServerConfig {
    pub fn new(data_dir: PathBuf, public_url: &str) -> Self {
        ServerConfig {
            data_dir,
            public_url: public_url.trim_end_matches('/').to_string(),
            webhook_secret: None,
            budget: Budget::default(),
            idle: Duration::from_secs(600),
            transaction_idle: Duration::from_secs(30),
            session: Duration::from_secs(12 * 3600),
            refresh_on_begin: true,
            dashboard_dir: None,
            app_slug: None,
            cli_source: None,
            cli_ref: "main".into(),
            public_metrics: true,
        }
    }
}

/// The service.
pub struct Server {
    pub config: ServerConfig,
    pub meta: Meta,
    pub app: Arc<GitHubApp>,
    /// Wraps the keys of databases in KMS mode; without it, only passphrase mode is offered.
    pub kms: Option<Arc<dyn KeyProvider>>,
    pub metrics: Metrics,
    pub transactions: Transactions,
    served: Mutex<HashMap<String, Arc<Served>>>,
    /// One open at a time per database id.
    opening: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    governors: Mutex<HashMap<i64, Arc<Governor>>>,
    /// Sign-ins under way: state → (when, where to go afterwards).
    pub(crate) sign_ins: Mutex<HashMap<String, (i64, Option<String>)>>,
    /// Usage not yet written to the metadata store: installation → (requests, commits).
    usage: Mutex<HashMap<i64, (i64, i64)>>,
    /// Requests to GitHub already written to the store, per installation.
    github_counted: Mutex<HashMap<i64, u64>>,
    /// Each database's last size report, and when it was made.
    stats: Mutex<HashMap<String, (std::time::Instant, serde_json::Value)>>,
}

/// How long a database's size report is reused: making one walks a month of history.
const STATS_TTL: Duration = Duration::from_secs(300);

/// Characters allowed in the CLI source and branch the maintenance workflow is written with.
fn plain_url(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~:/@".contains(c))
}

impl Server {
    pub fn new(
        config: ServerConfig,
        meta: Meta,
        app: Arc<GitHubApp>,
        kms: Option<Arc<dyn KeyProvider>>,
    ) -> Arc<Self> {
        Arc::new(Server {
            config,
            meta,
            app,
            kms,
            metrics: Metrics::default(),
            transactions: Transactions::default(),
            served: Mutex::new(HashMap::new()),
            opening: Mutex::new(HashMap::new()),
            governors: Mutex::new(HashMap::new()),
            sign_ins: Mutex::new(HashMap::new()),
            usage: Mutex::new(HashMap::new()),
            github_counted: Mutex::new(HashMap::new()),
            stats: Mutex::new(HashMap::new()),
        })
    }

    /// `installation`'s governor.
    pub fn governor(&self, installation: i64) -> Arc<Governor> {
        self.governors
            .lock()
            .unwrap()
            .entry(installation)
            .or_insert_with(|| Governor::new(self.config.budget))
            .clone()
    }

    /// The page cache of `repository`: a directory of its own.
    pub fn cache_dir(&self, repository: &str) -> PathBuf {
        let name: String = repository
            .to_ascii_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.config.data_dir.join("cache").join(name)
    }

    fn config_for(&self, record: &DatabaseRecord, unlock: Option<Unlock>) -> octopage::Config {
        let mut config = octopage::Config::default();
        config.store.branch = record.branch.clone();
        config.store.cache_dir = Some(self.cache_dir(&record.repository));
        config.store.unlock = unlock;
        config.sqlite.refresh_on_begin = self.config.refresh_on_begin;
        config
    }

    fn transport(&self, record: &DatabaseRecord) -> ApiResult<served::Remote> {
        let inner = self
            .app
            .transport(record.installation, &record.repository)
            .map_err(ApiError::internal)?;
        Ok(Governed::new(inner, self.governor(record.installation)))
    }

    fn unlock_for(&self, mode: KeyMode) -> Option<Unlock> {
        match mode {
            KeyMode::Kms => self.kms.clone().map(Unlock::Provider),
            _ => None,
        }
    }

    /// The database `record` names, opened if it is not open yet. A database in passphrase mode
    /// must have been unlocked since the service started (or since it was last closed).
    pub async fn open(&self, record: &DatabaseRecord) -> ApiResult<Arc<Served>> {
        if let Some(served) = self.served.lock().unwrap().get(&record.id).cloned() {
            return Ok(served);
        }
        if record.keys == KeyMode::Passphrase {
            return Err(ApiError::new(
                axum::http::StatusCode::LOCKED,
                "locked",
                "this database is encrypted with a passphrase: unlock it first \
                 (POST /v1/databases/{id}/unlock)",
            ));
        }
        self.open_with(record, self.unlock_for(record.keys)).await
    }

    /// Open `record` with `unlock` (a passphrase, for passphrase mode).
    pub async fn open_with(
        &self,
        record: &DatabaseRecord,
        unlock: Option<Unlock>,
    ) -> ApiResult<Arc<Served>> {
        let gate = self
            .opening
            .lock()
            .unwrap()
            .entry(record.id.clone())
            .or_default()
            .clone();
        let _one_at_a_time = gate.lock().await;
        if let Some(served) = self.served.lock().unwrap().get(&record.id).cloned() {
            return Ok(served);
        }
        let db: Db =
            Database::open(self.transport(record)?, self.config_for(record, unlock)).await?;
        self.note_location(record, &db).await?;
        let served = Served::new(record.clone(), db, self.cache_dir(&record.repository));
        self.served
            .lock()
            .unwrap()
            .insert(record.id.clone(), served.clone());
        Ok(served)
    }

    /// Open the database on `repository`'s `branch` for `user`, creating it if asked; register
    /// it. Returns its record, whether it was created, and a new encrypted database's recovery
    /// key (which exists only in this answer).
    pub async fn add_database(
        &self,
        user: &User,
        repository: &str,
        branch: &str,
        create: bool,
        keys: KeyMode,
        passphrase: Option<String>,
    ) -> ApiResult<(DatabaseRecord, bool, Option<String>)> {
        let repo = self.meta.repository(repository).await?.ok_or_else(|| {
            ApiError::not_found(format!(
                "the OctoPage App is not installed on {repository} (or GitHub has not told the \
                 service yet)"
            ))
        })?;
        if !self.meta.is_member(user.id, repo.installation).await? {
            return Err(ApiError::forbidden(format!(
                "{} cannot use the installation that reaches {repository}",
                user.login
            )));
        }
        let branch = if branch.starts_with("refs/") {
            branch.to_string()
        } else {
            format!("refs/heads/{branch}")
        };
        if let Some(existing) = self.meta.database_on(&repo.full_name, &branch).await? {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "exists",
                format!("that branch is database {} already", existing.id),
            ));
        }
        let mut record = DatabaseRecord {
            id: format!("db_{}", meta::random_hex(8)),
            repository: repo.full_name.clone(),
            installation: repo.installation,
            branch,
            keys,
            created: meta::now(),
        };
        let mut mode = keys;
        let config = |unlock| self.config_for(&record, unlock);
        let opened = Database::open(self.transport(&record)?, config(None)).await;
        let no_database = matches!(
            &opened,
            Err(octopage::Error::Store(
                octopage_pagestore::Error::NoDatabase(_)
            ))
        ) || matches!(
            &opened,
            Err(octopage::Error::Store(
                octopage_pagestore::Error::Format(reason)
            )) if reason == "the root tree has no pages/ tree"
        );
        let (db, created, recovery): (Db, bool, Option<String>) = match opened {
            Ok(db) => {
                mode = KeyMode::None;
                (db, false, None)
            }
            Err(octopage::Error::Store(octopage_pagestore::Error::Locked)) => {
                // An encrypted database: with the service's key, or the user's passphrase.
                let unlock = match (&passphrase, &self.kms) {
                    (Some(p), _) => {
                        mode = KeyMode::Passphrase;
                        Unlock::Passphrase(p.clone())
                    }
                    (None, Some(kms)) => {
                        mode = KeyMode::Kms;
                        Unlock::Provider(kms.clone())
                    }
                    (None, None) => {
                        return Err(ApiError::new(
                            axum::http::StatusCode::LOCKED,
                            "locked",
                            "the database is encrypted: give its passphrase",
                        ));
                    }
                };
                let db = Database::open(self.transport(&record)?, config(Some(unlock))).await?;
                (db, false, None)
            }
            Err(_) if create && no_database => {
                let transport = self.transport(&record)?;
                match keys {
                    KeyMode::None => (Database::create(transport, config(None)).await?, true, None),
                    KeyMode::Kms => {
                        let kms = self.kms.clone().ok_or_else(|| {
                            ApiError::bad_request(
                                "this service has no key service: use encryption \"passphrase\"",
                            )
                        })?;
                        let encryption = Encryption {
                            key: Unlock::Provider(kms.clone()),
                            nonces: Nonces::default(),
                            kdf: Kdf::default(),
                        };
                        let (db, recovery) = Database::create_encrypted(
                            transport,
                            config(Some(Unlock::Provider(kms))),
                            encryption,
                        )
                        .await?;
                        (db, true, Some(recovery.to_string()))
                    }
                    KeyMode::Passphrase => {
                        let passphrase = passphrase
                            .filter(|p| p.chars().count() >= 12)
                            .ok_or_else(|| {
                                ApiError::bad_request("give a passphrase of at least 12 characters")
                            })?;
                        let (db, recovery) = Database::create_encrypted(
                            transport,
                            config(Some(Unlock::Passphrase(passphrase.clone()))),
                            Encryption::passphrase(passphrase),
                        )
                        .await?;
                        (db, true, Some(recovery.to_string()))
                    }
                }
            }
            Err(e) => return Err(e.into()),
        };
        record.keys = mode;
        self.meta.add_database(record.clone()).await?;
        let served = Served::new(record.clone(), db, self.cache_dir(&record.repository));
        self.served
            .lock()
            .unwrap()
            .insert(record.id.clone(), served);
        Ok((record, created, recovery))
    }

    /// How `served` grows, and its repository against the budget in its settings. Reused for
    /// a few minutes. The growth is this database's alone; the repository's size is everything
    /// in it, as GitHub last measured it.
    pub async fn stats(&self, served: &Served) -> ApiResult<serde_json::Value> {
        use octopage_ops::Host as _;
        let record = served.record.lock().unwrap().clone();
        if let Some((at, report)) = self.stats.lock().unwrap().get(&record.id)
            && at.elapsed() < STATS_TTL
        {
            return Ok(report.clone());
        }
        let settings = served.db.settings().await?;
        let growth = octopage_ops::size::growth(served.db.store(), meta::now()).await?;
        let size = match self.app.installation_token(record.installation).await {
            Ok(token) => {
                let host = octopage_ops::GitHub::with_api(&self.app.config().api, &token, None);
                host.repository_size(&record.repository)
                    .await
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error, repository = %record.repository, "no repository size");
                        None
                    })
            }
            Err(error) => {
                tracing::warn!(%error, "no installation token for the repository size");
                None
            }
        };
        let projection =
            octopage_ops::size::project(std::slice::from_ref(&growth), size, &settings);
        let report = serde_json::json!({
            "pages": growth.pages,
            "page_size": served.db.store().page_size(),
            "live_bytes": growth.live_bytes,
            "live_limit": settings.live_limit,
            "commits_30d": growth.commits,
            "bytes_per_day": growth.bytes_per_day,
            "repository_bytes": projection.repository_bytes,
            "repository_budget": projection.budget,
            "days_left": projection.days_left,
            "warn": projection.warns(&settings),
            "measured": meta::now(),
        });
        self.stats.lock().unwrap().insert(
            record.id.clone(),
            (std::time::Instant::now(), report.clone()),
        );
        Ok(report)
    }

    /// Commit the maintenance workflow beside `served`'s pages: the CLI built from
    /// `cli_source` at `cli_ref` (by default the service's). The installation needs permission
    /// to change workflows.
    pub async fn install_maintenance(
        &self,
        served: &Served,
        cli_source: Option<&str>,
        cli_ref: Option<&str>,
    ) -> ApiResult<octopage::ObjectId> {
        let source = cli_source
            .or(self.config.cli_source.as_deref())
            .ok_or_else(|| {
                ApiError::bad_request("say where the workflow builds the CLI from: cli_source")
            })?;
        let reference = cli_ref.unwrap_or(&self.config.cli_ref);
        if !source.starts_with("https://") || !plain_url(source) || !plain_url(reference) {
            return Err(ApiError::bad_request(
                "cli_source is an https:// git URL and cli_ref a branch name",
            ));
        }
        let workflow = octopage_ops::workflows::maintenance_workflow(source, reference);
        let commit = served
            .db
            .put_file(
                octopage_ops::workflows::MAINTENANCE_PATH,
                Some(bytes::Bytes::from(workflow)),
            )
            .await?;
        Ok(commit)
    }

    /// The database with `id`, if `user` may use it.
    pub async fn database_for(&self, user: &User, id: &str) -> ApiResult<DatabaseRecord> {
        let record = self.meta.database(id).await?;
        match record {
            Some(r) if self.meta.is_member(user.id, r.installation).await? => Ok(r),
            // The same answer whether it does not exist or belongs to someone else.
            _ => Err(ApiError::not_found(format!("no database {id}"))),
        }
    }

    /// If the database moved to a new generation, record where.
    pub async fn note_location(&self, record: &DatabaseRecord, db: &Db) -> ApiResult<()> {
        if let Some(location) = db.location()
            && !location.eq_ignore_ascii_case(&record.repository)
        {
            tracing::info!(id = %record.id, from = %record.repository, to = %location, "a database moved");
            self.meta.move_database(&record.id, &location).await?;
            if let Some(served) = self.served.lock().unwrap().get(&record.id) {
                served.record.lock().unwrap().repository = location;
            }
        }
        Ok(())
    }

    /// The open databases on `repository` (case-insensitive).
    pub fn served_on(&self, repository: &str) -> Vec<Arc<Served>> {
        self.served
            .lock()
            .unwrap()
            .values()
            .filter(|s| {
                s.record
                    .lock()
                    .unwrap()
                    .repository
                    .eq_ignore_ascii_case(repository)
            })
            .cloned()
            .collect()
    }

    /// Close the open databases `condition` picks.
    pub fn close_where(&self, condition: impl Fn(&DatabaseRecord) -> bool) -> usize {
        let mut served = self.served.lock().unwrap();
        let before = served.len();
        served.retain(|_, s| !condition(&s.record.lock().unwrap()));
        before - served.len()
    }

    /// Close the databases on `repository` and delete its page cache: the service keeps no
    /// copy of a repository's pages once it serves no database there.
    pub fn forget_repository(&self, repository: &str) {
        self.close_where(|r| r.repository.eq_ignore_ascii_case(repository));
        let cache = self.cache_dir(repository);
        if let Err(error) = std::fs::remove_dir_all(&cache)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, cache = %cache.display(), "could not delete a page cache");
        }
    }

    /// Close databases unused for longer than the idle time (not while a transaction is open).
    pub fn close_idle(&self) -> usize {
        let cutoff = meta::now() - self.config.idle.as_secs() as i64;
        let mut served = self.served.lock().unwrap();
        let before = served.len();
        served.retain(|id, s| {
            s.last_used.load(Ordering::Relaxed) > cutoff || self.transactions.on(id)
        });
        before - served.len()
    }

    pub fn served_count(&self) -> usize {
        self.served.lock().unwrap().len()
    }

    /// Count a request (and a commit) against `installation`'s usage.
    pub fn note_usage(&self, installation: i64, commits: i64) {
        let mut usage = self.usage.lock().unwrap();
        let entry = usage.entry(installation).or_default();
        entry.0 += 1;
        entry.1 += commits;
        if commits > 0 {
            self.metrics
                .commits
                .fetch_add(commits as u64, Ordering::Relaxed);
        }
    }

    /// Write the usage counted since the last flush to the metadata store.
    pub async fn flush_usage(&self) -> ApiResult<()> {
        let pending: Vec<(i64, (i64, i64))> = self.usage.lock().unwrap().drain().collect();
        let governors: Vec<(i64, u64)> = self
            .governors
            .lock()
            .unwrap()
            .iter()
            .map(|(i, g)| (*i, g.requests.load(Ordering::Relaxed)))
            .collect();
        let mut installations: HashMap<i64, (i64, i64, i64)> = HashMap::new();
        for (installation, (requests, commits)) in pending {
            let e = installations.entry(installation).or_default();
            e.0 += requests;
            e.2 += commits;
        }
        {
            let mut counted = self.github_counted.lock().unwrap();
            for (installation, total) in governors {
                let before = counted.insert(installation, total).unwrap_or(0);
                installations.entry(installation).or_default().1 += (total - before) as i64;
            }
        }
        for (installation, (requests, github, commits)) in installations {
            if requests + github + commits > 0 {
                self.meta
                    .add_usage(installation, requests, github, commits)
                    .await?;
            }
        }
        Ok(())
    }

    /// Per installation: (id, requests to GitHub, requests that waited).
    pub fn installation_counters(&self) -> Vec<(i64, u64, u64)> {
        let mut out: Vec<(i64, u64, u64)> = self
            .governors
            .lock()
            .unwrap()
            .iter()
            .map(|(i, g)| {
                (
                    *i,
                    g.requests.load(Ordering::Relaxed),
                    g.waited.load(Ordering::Relaxed),
                )
            })
            .collect();
        out.sort();
        out
    }

    /// The Prometheus text for `/metrics`.
    pub fn render_metrics(&self) -> String {
        self.metrics.render(
            self.served_count(),
            self.transactions.len(),
            &self.installation_counters(),
            self.app.minted(),
        )
    }

    /// Background work: roll back idle transactions, close idle databases, delete expired
    /// sessions, write usage.
    pub fn spawn_housekeeping(self: &Arc<Self>) {
        let server = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            let mut rounds = 0u64;
            loop {
                tick.tick().await;
                let Some(server) = server.upgrade() else {
                    break;
                };
                let expired = server.transactions.expire();
                if expired > 0 {
                    tracing::info!(expired, "rolled back idle transactions");
                }
                rounds += 1;
                if rounds.is_multiple_of(12) {
                    server.close_idle();
                    if let Err(error) = server.meta.delete_expired_keys().await {
                        tracing::warn!(%error, "could not delete expired sessions");
                    }
                    if let Err(e) = server.flush_usage().await {
                        tracing::warn!(error = %e.message, "could not write usage");
                    }
                }
            }
        });
    }
}
