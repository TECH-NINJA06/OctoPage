use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use octopage_git::{Credentials, HttpConfig, RefUpdate, SmartHttp, StaticToken, Transport};
use octopage_kv::{Kv, soak};
use octopage_pagestore::{Config, PageStore};

/// A key-value store kept in a GitHub repository (the OctoPage Phase 2 harness).
#[derive(Parser)]
#[command(name = "octopage-kv", version)]
struct Cli {
    /// GitHub repository, OWNER/NAME.
    #[arg(long, conflicts_with = "url", required_unless_present = "url")]
    repo: Option<String>,
    /// Any smart-HTTP git remote instead of github.com.
    #[arg(long)]
    url: Option<String>,
    /// The ref that holds the database.
    #[arg(long, default_value = "refs/heads/main")]
    branch: String,
    /// Object cache directory (default: the user cache directory); `none` disables it.
    #[arg(long)]
    cache: Option<String>,
    /// Access token (fine-grained, Contents: read and write on the repository).
    #[arg(long, env = "OCTOPAGE_GITHUB_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new database on the branch.
    Init {
        #[arg(long, default_value_t = 64)]
        buckets: usize,
        #[arg(long, default_value_t = 4096)]
        page_size: usize,
    },
    /// Print a key's value.
    Get { key: String },
    /// Set a key.
    Put { key: String, value: String },
    /// Delete a key.
    Del { key: String },
    /// Add to an integer value (a missing key counts as 0).
    Add { key: String, n: i64 },
    /// List keys starting with a prefix.
    Scan {
        #[arg(default_value = "")]
        prefix: String,
    },
    /// Show recent commits and the statements each ran.
    Log {
        #[arg(short = 'n', default_value_t = 10)]
        n: usize,
    },
    /// Show the database's head, epoch and layout.
    Info,
    /// The Phase 2 gate: concurrent writers on a fresh temporary branch, then verify that no
    /// write was lost or applied twice.
    Soak {
        #[arg(long, default_value_t = 3)]
        writers: usize,
        #[arg(long, default_value_t = 334)]
        per_writer: usize,
        /// Keep the temporary branch afterwards.
        #[arg(long)]
        keep: bool,
    },
}

fn remote_url(cli: &Cli) -> String {
    match (&cli.repo, &cli.url) {
        (Some(repo), _) => format!("https://github.com/{repo}.git"),
        (None, Some(url)) => url.clone(),
        (None, None) => unreachable!("clap requires one of them"),
    }
}

fn transport(cli: &Cli) -> Result<SmartHttp, String> {
    let creds = cli.token.as_ref().map(|token| {
        // Personal access tokens sign in as the account; Actions and App tokens as x-access-token.
        let owner = cli.repo.as_deref().and_then(|r| r.split('/').next());
        let user = match owner {
            Some(owner) if !token.starts_with("ghs_") => owner.to_string(),
            _ => "x-access-token".to_string(),
        };
        Credentials::git(user, Arc::new(StaticToken::new(token.clone())))
    });
    SmartHttp::new(&remote_url(cli), creds, HttpConfig::default()).map_err(|e| e.to_string())
}

fn cache_dir(cli: &Cli) -> Option<PathBuf> {
    match cli.cache.as_deref() {
        Some("none") => None,
        Some(dir) => Some(PathBuf::from(dir)),
        None => {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(|p| PathBuf::from(p).join("octopage").join("cache"))
                .or_else(|| {
                    std::env::var_os("XDG_CACHE_HOME").map(|p| PathBuf::from(p).join("octopage"))
                })
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|p| PathBuf::from(p).join(".cache").join("octopage"))
                })?;
            let name: String = remote_url(cli)
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            Some(base.join(name))
        }
    }
}

fn config(cli: &Cli, branch: &str) -> Config {
    Config {
        branch: branch.to_string(),
        cache_dir: cache_dir(cli),
        head_poll: None,
        ..Config::default()
    }
}

/// `YYYY-MM-DD hh:mm:ss UTC` from unix seconds.
fn utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

async fn open(cli: &Cli) -> Result<Kv<SmartHttp>, String> {
    let store = PageStore::open(transport(cli)?, config(cli, &cli.branch))
        .await
        .map_err(|e| e.to_string())?;
    Kv::open(store).await.map_err(|e| e.to_string())
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    let e = |e: octopage_kv::Error| e.to_string();
    match &cli.command {
        Command::Init { buckets, page_size } => {
            let store = PageStore::create(transport(&cli)?, config(&cli, &cli.branch), *page_size)
                .await
                .map_err(|e| e.to_string())?;
            let kv = Kv::create(store, *buckets).await.map_err(e)?;
            println!(
                "created a database on {} with {} buckets (head {})",
                cli.branch,
                kv.buckets(),
                kv.store().head()
            );
        }
        Command::Get { key } => match open(&cli).await?.get(key.as_bytes()).await.map_err(e)? {
            Some(value) => println!("{}", String::from_utf8_lossy(&value)),
            None => {
                eprintln!("(not found)");
                return Ok(ExitCode::FAILURE);
            }
        },
        Command::Put { key, value } => {
            let stats = open(&cli)
                .await?
                .put(key.as_bytes(), value.as_bytes())
                .await
                .map_err(e)?;
            println!(
                "committed {} (attempts {}, rebases {}, re-executions {})",
                stats.head, stats.attempts, stats.rebases, stats.reexecutions
            );
        }
        Command::Del { key } => {
            let found = open(&cli).await?.delete(key.as_bytes()).await.map_err(e)?;
            println!("{}", if found { "deleted" } else { "(not found)" });
        }
        Command::Add { key, n } => println!(
            "{}",
            open(&cli).await?.add(key.as_bytes(), *n).await.map_err(e)?
        ),
        Command::Scan { prefix } => {
            for (k, v) in open(&cli).await?.scan(prefix.as_bytes()).await.map_err(e)? {
                println!(
                    "{}\t{}",
                    String::from_utf8_lossy(&k),
                    String::from_utf8_lossy(&v)
                );
            }
        }
        Command::Log { n } => {
            let kv = open(&cli).await?;
            for entry in kv.store().log(*n).await.map_err(|e| e.to_string())? {
                println!(
                    "{}  {}  {}",
                    &entry.commit.to_hex()[..12],
                    utc(entry.time),
                    entry.message
                );
                for line in String::from_utf8_lossy(&entry.changelog).lines() {
                    println!("    {line}");
                }
            }
        }
        Command::Info => {
            let kv = open(&cli).await?;
            let snap = kv.store().latest().await.map_err(|e| e.to_string())?;
            let sb = snap.superblock();
            println!("remote     {}", remote_url(&cli));
            println!("branch     {}", cli.branch);
            println!("head       {}", snap.id());
            println!("epoch      {}", snap.epoch());
            println!("committed  {}", utc(snap.time()));
            println!("page size  {} bytes", sb.page_size);
            println!(
                "pages      {}",
                snap.page_ids().await.map_err(|e| e.to_string())?.len()
            );
            println!("next page  {}", sb.next_free);
            println!("buckets    {}", kv.buckets());
        }
        Command::Soak {
            writers,
            per_writer,
            keep,
        } => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let branch = format!("refs/heads/octopage-soak/{stamp}");
            println!("soak: {writers} writers x {per_writer} transactions on {branch}");
            let mut cfg = config(&cli, &branch);
            cfg.head_poll = Some(Duration::from_secs(1));
            let store = PageStore::create(transport(&cli)?, cfg.clone(), 4096)
                .await
                .map_err(|e| e.to_string())?;
            Kv::create(store, 64).await.map_err(e)?;
            let mut clients = Vec::new();
            for _ in 0..*writers {
                let store = PageStore::open(transport(&cli)?, cfg.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                clients.push(Kv::open(store).await.map_err(e)?);
            }
            let report = soak::run(clients, *per_writer, Some(50)).await.map_err(e)?;
            println!(
                "{} transactions in {:.1?}: {} attempts, {} rebases, {} re-executions, {} retried ({} after 8 lost races), {} late landings",
                report.transactions,
                report.elapsed,
                report.attempts,
                report.rebases,
                report.reexecutions,
                report.retries,
                report.serialization_failures,
                report.late_landings
            );
            let checker = Kv::open(
                PageStore::open(transport(&cli)?, cfg.clone())
                    .await
                    .map_err(|e| e.to_string())?,
            )
            .await
            .map_err(e)?;
            soak::verify(&checker, *writers, *per_writer, 2)
                .await
                .map_err(e)?;
            println!(
                "PASS: every write is present exactly once; history has one commit per transaction"
            );
            if !keep {
                let head = checker.store().refresh().await.map_err(|e| e.to_string())?;
                checker
                    .store()
                    .transport()
                    .push(&[RefUpdate::delete(&branch, head)], &[])
                    .await
                    .map_err(|e| e.to_string())?;
                println!("deleted {branch}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::utc;

    #[test]
    fn utc_formatting() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_790_568_493), "2026-09-28 04:08:13 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
    }
}
