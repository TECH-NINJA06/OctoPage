use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use octopage::{Config, Database, Encryption, InMemory, Transport, Unlock};
use octopage_pagestore::Error as StoreError;
use tokio::runtime::Runtime;

mod ops;
mod shell;

/// A SQL shell for a database kept in a GitHub repository.
#[derive(Parser)]
#[command(name = "octopage", version)]
struct Cli {
    /// GitHub repository, OWNER/NAME.
    #[arg(long, global = true, conflicts_with_all = ["url", "memory"])]
    repo: Option<String>,
    /// Any smart-HTTP git remote instead of github.com.
    #[arg(long, global = true, conflicts_with = "memory")]
    url: Option<String>,
    /// A scratch database in memory, gone when the shell exits.
    #[arg(long, global = true)]
    memory: bool,
    /// The ref that holds the database.
    #[arg(long, global = true, default_value = "refs/heads/main")]
    branch: String,
    /// Create the database if the branch does not have one yet.
    #[arg(long)]
    create: bool,
    /// Create it encrypted: pages, schema and changelogs are unreadable in the repository. The
    /// passphrase comes from OCTOPAGE_PASSPHRASE, or a prompt.
    #[arg(long)]
    encrypt: bool,
    /// Open an encrypted database with its recovery key instead of its passphrase.
    #[arg(
        long,
        global = true,
        env = "OCTOPAGE_RECOVERY_KEY",
        hide_env_values = true
    )]
    recovery_key: Option<String>,
    /// Compress the pages of a new unencrypted database.
    #[arg(long)]
    compress: bool,
    /// The database is meant to be public (an open dataset): don't warn about a public repository.
    #[arg(long)]
    public: bool,
    /// Refuse to open or create an encrypted database in a public repository.
    #[arg(long)]
    require_private: bool,
    /// Sign every commit with this SSH private key (its passphrase, if any, from
    /// OCTOPAGE_SSH_KEY_PASSPHRASE), so GitHub shows the history as verified.
    #[arg(long, value_name = "KEY")]
    sign_with: Option<PathBuf>,
    /// Access token (fine-grained, Contents: read and write on the repository).
    #[arg(
        long,
        global = true,
        env = "OCTOPAGE_GITHUB_TOKEN",
        hide_env_values = true
    )]
    token: Option<String>,
    /// User name to sign in with at --url.
    #[arg(long, global = true, default_value = "x-access-token")]
    user: String,
    /// Keep fetched pages in this directory between runs (off by default).
    #[arg(long, global = true)]
    cache: Option<PathBuf>,
    /// Run this SQL and exit.
    #[arg(short = 'c', long = "command")]
    command: Option<String>,
    /// Maintenance instead of the shell.
    #[command(subcommand)]
    op: Option<ops::Op>,
}

fn config(cli: &Cli) -> Config {
    let mut config = Config::default();
    config.store.branch = cli.branch.clone();
    config.store.cache_dir = cli.cache.clone();
    config.store.head_poll = None; // each transaction reads the head itself
    config.store.compression = cli.compress;
    config.public = cli.public;
    config.require_private = cli.require_private;
    config.store.unlock = match (&cli.recovery_key, std::env::var("OCTOPAGE_PASSPHRASE")) {
        (Some(key), _) => Some(Unlock::RecoveryKey(key.clone())),
        (None, Ok(passphrase)) => Some(Unlock::Passphrase(passphrase)),
        (None, Err(_)) => None,
    };
    config
}

/// The passphrase: from OCTOPAGE_PASSPHRASE, or asked for (twice for a new database).
fn passphrase(new: bool) -> Result<String, String> {
    if let Ok(passphrase) = std::env::var("OCTOPAGE_PASSPHRASE") {
        return Ok(passphrase);
    }
    if !std::io::stdin().is_terminal() {
        return Err("the database is encrypted: set OCTOPAGE_PASSPHRASE".into());
    }
    let first = rpassword::prompt_password("Passphrase: ").map_err(|e| e.to_string())?;
    if new {
        let again =
            rpassword::prompt_password("The same passphrase again: ").map_err(|e| e.to_string())?;
        if again != first {
            return Err("the passphrases differ".into());
        }
        if first.chars().count() < 12 {
            return Err("use a passphrase of at least 12 characters".into());
        }
    }
    Ok(first)
}

async fn create<T: Transport + 'static>(
    cli: &Cli,
    transport: T,
    config: Config,
) -> Result<Database<T>, String> {
    if !cli.encrypt {
        return Database::create(transport, config)
            .await
            .map_err(|e| e.to_string());
    }
    let encryption = Encryption::passphrase(passphrase(true)?);
    let (db, recovery) = Database::create_encrypted(transport, config, encryption)
        .await
        .map_err(|e| e.to_string())?;
    eprintln!(
        "\nThis database is encrypted. Its recovery key opens it if the passphrase is lost.\n\
         Store it somewhere safe now; it is not shown again:\n\n    {recovery}\n"
    );
    Ok(db)
}

/// Open the database (creating it if asked, or if `fresh`), then run the shell on it.
fn start<T: Transport + 'static>(
    runtime: &Runtime,
    cli: &Cli,
    transport: impl Fn() -> octopage::Result<T>,
    fresh: bool,
) -> Result<ExitCode, String> {
    let mut config = config(cli);
    if let Some(key) = &cli.sign_with {
        let passphrase = std::env::var("OCTOPAGE_SSH_KEY_PASSPHRASE").ok();
        let signer = octopage::SshSigner::from_file(key, passphrase.as_deref())
            .map_err(|e| e.to_string())?;
        eprintln!("signing commits with {}", signer.public_key());
        config.store.signer = Some(Arc::new(signer));
    }
    let transport = || transport().map_err(|e| e.to_string());
    let db = runtime.block_on(async {
        if fresh {
            return create(cli, transport()?, config).await;
        }
        match Database::open(transport()?, config.clone()).await {
            Err(octopage::Error::Store(StoreError::NoDatabase(branch))) if cli.create => {
                eprintln!("creating a new database on {branch}");
                create(cli, transport()?, config).await
            }
            Err(octopage::Error::Store(StoreError::Locked)) => {
                config.store.unlock = Some(Unlock::Passphrase(passphrase(false)?));
                Database::open(transport()?, config)
                    .await
                    .map_err(|e| e.to_string())
            }
            other => other.map_err(|e| e.to_string()),
        }
    })?;
    for warning in db.warnings() {
        eprintln!("warning: {warning}");
    }
    let mut shell = shell::Shell::new(runtime, db).map_err(|e| e.to_string())?;
    if let Some(sql) = &cli.command {
        return Ok(shell.script(sql));
    }
    if !std::io::stdin().is_terminal() {
        let mut sql = String::new();
        std::io::stdin()
            .read_to_string(&mut sql)
            .map_err(|e| e.to_string())?;
        return Ok(shell.script(&sql));
    }
    shell.interactive().map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Run a maintenance subcommand.
fn maintenance(runtime: &Runtime, cli: &Cli, op: ops::Op) -> Result<ExitCode, String> {
    if let ops::Op::Workflows {
        action: ops::WorkflowAction::Print {
            cli_source,
            cli_ref,
        },
    } = &op
    {
        // Needs no repository.
        print!(
            "{}",
            octopage_ops::workflows::maintenance_workflow(cli_source, cli_ref)
        );
        return Ok(ExitCode::SUCCESS);
    }
    let mut config = config(cli);
    config.store.head_poll = None;
    let token = cli.token.clone();
    match (&cli.repo, &cli.url) {
        (Some(repo), _) => {
            // A rollover pushes to a new repository, which the job's own token (GITHUB_TOKEN in
            // Actions) cannot reach: git goes through the admin token when there is one.
            let git_token = op.admin_token().or(token.as_deref()).map(str::to_string);
            let transport = || octopage::github(repo, git_token.as_deref());
            let cx = ops::Context {
                transport: &transport,
                config,
                interactive: ops::interactive(),
            };
            let host = |admin: Option<&str>| {
                octopage_ops::GitHub::new(token.as_deref().unwrap_or_default(), admin)
            };
            runtime.block_on(ops::run(op, &cx, host))
        }
        (None, Some(url)) => {
            let transport = || octopage::remote(url, token.as_deref(), &cli.user);
            let cx = ops::Context {
                transport: &transport,
                config,
                interactive: ops::interactive(),
            };
            runtime.block_on(ops::run(op, &cx, |_| ops::NoHost))
        }
        (None, None) => {
            Err("maintenance works on a repository: say --repo OWNER/NAME or --url URL".into())
        }
    }
}

fn main() -> ExitCode {
    let mut cli = Cli::parse();
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(op) = cli.op.take() {
        return match maintenance(&runtime, &cli, op) {
            Ok(code) => code,
            Err(message) => {
                eprintln!("error: {message}");
                ExitCode::FAILURE
            }
        };
    }
    let token = cli.token.clone();
    let result = match (&cli.repo, &cli.url) {
        _ if cli.encrypt && !cli.create && !cli.memory => {
            Err("--encrypt applies to a new database: add --create".into())
        }
        _ if cli.memory => start(&runtime, &cli, || Ok(Arc::new(InMemory::new())), true),
        (Some(repo), _) => start(
            &runtime,
            &cli,
            || octopage::github(repo, token.as_deref()),
            false,
        ),
        (None, Some(url)) => start(
            &runtime,
            &cli,
            || octopage::remote(url, token.as_deref(), &cli.user),
            false,
        ),
        (None, None) => {
            Err("say where the database is: --repo OWNER/NAME, --url URL or --memory".into())
        }
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
