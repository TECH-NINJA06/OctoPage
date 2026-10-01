use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use octopage_git::{Credentials, HttpConfig, RefUpdate, SmartHttp, StaticToken, Transport};
use octopage_pagestore::{Config, PageStore};
use octopage_sqlite::{Database, soak};
use tokio::runtime::Handle;

/// Concurrent SQL writers on a temporary branch, then verify nothing was lost or doubled.
#[derive(Parser)]
struct Cli {
    /// GitHub repository, OWNER/NAME.
    #[arg(long, conflicts_with = "url", required_unless_present = "url")]
    repo: Option<String>,
    /// Any smart-HTTP git remote instead of github.com.
    #[arg(long)]
    url: Option<String>,
    /// Access token (fine-grained, Contents: read and write on the repository).
    #[arg(long, env = "OCTOPAGE_GITHUB_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long, default_value_t = 3)]
    writers: usize,
    #[arg(long, default_value_t = 50)]
    per_writer: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Keep the temporary branch afterwards.
    #[arg(long)]
    keep: bool,
}

fn transport(cli: &Cli) -> Result<SmartHttp, String> {
    let url = match (&cli.repo, &cli.url) {
        (Some(repo), _) => format!("https://github.com/{repo}.git"),
        (None, Some(url)) => url.clone(),
        (None, None) => unreachable!("clap requires one of them"),
    };
    let creds = cli.token.as_ref().map(|token| {
        // Personal access tokens sign in as the account; Actions and App tokens as x-access-token.
        let owner = cli.repo.as_deref().and_then(|r| r.split('/').next());
        let user = match owner {
            Some(owner) if !token.starts_with("ghs_") => owner.to_string(),
            _ => "x-access-token".to_string(),
        };
        Credentials::git(user, Arc::new(StaticToken::new(token.clone())))
    });
    SmartHttp::new(&url, creds, HttpConfig::default()).map_err(|e| e.to_string())
}

async fn run(cli: Cli) -> Result<(), String> {
    let e = |e: octopage_pagestore::Error| e.to_string();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let branch = format!("refs/heads/octopage-sql-soak/{stamp}");
    let config = Config {
        branch: branch.clone(),
        head_poll: None,
        ..Config::default()
    };
    println!(
        "sql soak: {} writers x {} transactions on {branch}",
        cli.writers, cli.per_writer
    );
    let store = PageStore::create(transport(&cli)?, config.clone(), 4096)
        .await
        .map_err(e)?;
    let setup = Database::new(store.clone(), Handle::current());
    let writers = cli.writers;
    tokio::task::spawn_blocking(move || {
        let conn = setup.connect().map_err(|e| e.to_string())?;
        soak::setup(&conn, writers).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let commits_before = store.log(usize::MAX).await.map_err(e)?.len();

    let mut clients = Vec::new();
    for _ in 0..cli.writers {
        let store = PageStore::open(transport(&cli)?, config.clone())
            .await
            .map_err(e)?;
        clients.push(Database::new(store, Handle::current()));
    }
    let (per_writer, seed) = (cli.per_writer, cli.seed);
    let report =
        tokio::task::spawn_blocking(move || soak::run(&clients, per_writer, seed, Some(25)))
            .await
            .map_err(|e| e.to_string())??;
    println!("{report:#?}");

    let checker = Database::new(
        PageStore::open(transport(&cli)?, config.clone())
            .await
            .map_err(e)?,
        Handle::current(),
    );
    soak::verify_all(
        checker.clone(),
        cli.writers,
        cli.per_writer,
        commits_before,
        &report,
    )
    .await?;
    println!(
        "PASS: integrity_check ok, nothing lost or applied twice, the catalog matches, one commit per transaction, old commits consistent"
    );
    if !cli.keep {
        let head = checker.store().refresh().await.map_err(e)?;
        checker
            .store()
            .transport()
            .push(&[RefUpdate::delete(&branch, head)], &[])
            .await
            .map_err(|e| e.to_string())?;
        println!("deleted {branch}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let started = std::time::Instant::now();
    let result = run(Cli::parse()).await;
    let elapsed: Duration = started.elapsed();
    match result {
        Ok(()) => {
            println!("done in {elapsed:.1?}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
