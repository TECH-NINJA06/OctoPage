use std::path::PathBuf;
use std::sync::Arc;

use octopage::KeyProvider;
use octopage_server::github::{AppConfig, GitHubApp};
use octopage_server::governor::Budget;
use octopage_server::kms::{AwsCredentials, AwsKms, LocalKms};
use octopage_server::meta::Meta;
use octopage_server::{Server, ServerConfig, api};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn required(name: &str) -> Result<String, String> {
    var(name).ok_or_else(|| format!("{name} is not set"))
}

fn kms() -> Result<Option<Arc<dyn KeyProvider>>, String> {
    let Some(spec) = var("OCTOPAGE_KMS") else {
        return Ok(None);
    };
    if let Some(hex) = spec.strip_prefix("local:") {
        return Ok(Some(LocalKms::from_hex("master", hex)?));
    }
    if let Some(rest) = spec.strip_prefix("aws:") {
        let (region, key) = rest
            .split_once(':')
            .ok_or("OCTOPAGE_KMS is aws:<region>:<key id>")?;
        let credentials = AwsCredentials {
            access_key: required("AWS_ACCESS_KEY_ID")?,
            secret_key: required("AWS_SECRET_ACCESS_KEY")?,
            session_token: var("AWS_SESSION_TOKEN"),
        };
        return Ok(Some(AwsKms::new(region, key, credentials)));
    }
    Err("OCTOPAGE_KMS is local:<hex> or aws:<region>:<key id>".into())
}

async fn run() -> Result<(), String> {
    let data = PathBuf::from(var("OCTOPAGE_DATA").unwrap_or_else(|| "octopage-data".into()));
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let public_url = var("OCTOPAGE_PUBLIC_URL").unwrap_or_else(|| "http://localhost:8080".into());
    let mut config = ServerConfig::new(data.clone(), &public_url);
    config.webhook_secret = var("GITHUB_WEBHOOK_SECRET");
    config.refresh_on_begin = match var("OCTOPAGE_REFRESH_ON_BEGIN").as_deref() {
        Some("0") => false,
        Some(_) => true,
        None => config.webhook_secret.is_none(),
    };
    config.dashboard_dir = var("OCTOPAGE_DASHBOARD").map(PathBuf::from);
    config.app_slug = var("GITHUB_APP_SLUG");
    config.cli_source = var("OCTOPAGE_CLI_SOURCE");
    let metrics_listen = var("OCTOPAGE_METRICS_LISTEN");
    config.public_metrics = metrics_listen.is_none();
    if let Some(reference) = var("OCTOPAGE_CLI_REF") {
        config.cli_ref = reference;
    }
    if let Some(per_hour) = var("OCTOPAGE_BUDGET_PER_HOUR") {
        let per_hour: f64 = per_hour
            .parse()
            .map_err(|_| "OCTOPAGE_BUDGET_PER_HOUR is a number")?;
        config.budget = Budget {
            per_second: per_hour / 3600.0,
            ..Budget::default()
        };
    }

    let private_key = match (
        var("GITHUB_APP_PRIVATE_KEY"),
        var("GITHUB_APP_PRIVATE_KEY_FILE"),
    ) {
        (Some(pem), _) => pem.replace("\\n", "\n"),
        (None, Some(path)) => std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?,
        (None, None) => return Err("GITHUB_APP_PRIVATE_KEY (or _FILE) is not set".into()),
    };
    let app_id: u64 = required("GITHUB_APP_ID")?
        .parse()
        .map_err(|_| "GITHUB_APP_ID is a number")?;
    let mut app = AppConfig::github(app_id, private_key);
    app.client_id = var("GITHUB_CLIENT_ID");
    app.client_secret = var("GITHUB_CLIENT_SECRET");
    if let Some(api) = var("GITHUB_API_URL") {
        app.api = api;
    }
    if let Some(web) = var("GITHUB_WEB_URL") {
        app.web = web;
    }
    if let Some(git) = var("GITHUB_GIT_URL") {
        app.git = git;
    }
    let app = GitHubApp::new(app)?;
    let meta = Meta::open(&data.join("meta.sqlite")).map_err(|e| e.to_string())?;
    let server = Server::new(config, meta, app, kms()?);
    server.spawn_housekeeping();

    if let Some(address) = metrics_listen {
        let listener = tokio::net::TcpListener::bind(&address)
            .await
            .map_err(|e| format!("{address}: {e}"))?;
        tracing::info!(%address, "serving metrics");
        let metrics = api::metrics_router(server.clone());
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, metrics).await {
                tracing::error!(%error, "the metrics listener stopped");
            }
        });
    }

    let listen = var("OCTOPAGE_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("{listen}: {e}"))?;
    tracing::info!(%listen, %public_url, "serving");
    axum::serve(listener, api::router(server.clone()))
        .with_graceful_shutdown(stopped())
        .await
        .map_err(|e| e.to_string())?;
    server.flush_usage().await.map_err(|e| e.message)?;
    Ok(())
}

/// Ctrl-C, or (on Unix) the SIGTERM that container hosts stop with.
async fn stopped() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("a SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("stopping");
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}
