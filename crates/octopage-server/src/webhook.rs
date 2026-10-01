use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;

use crate::Server;
use crate::error::{ApiError, ApiResult};
use crate::meta::Repository;

#[derive(Deserialize)]
struct Account {
    login: String,
}

#[derive(Deserialize)]
struct InstallationRef {
    id: i64,
    account: Option<Account>,
}

#[derive(Deserialize)]
struct RepoRef {
    id: i64,
    full_name: String,
    #[serde(default)]
    private: bool,
}

#[derive(Deserialize)]
struct Sender {
    id: i64,
}

#[derive(Deserialize)]
struct InstallationEvent {
    action: String,
    installation: InstallationRef,
    #[serde(default)]
    repositories: Vec<RepoRef>,
    sender: Option<Sender>,
}

#[derive(Deserialize)]
struct RepositoriesEvent {
    action: String,
    installation: InstallationRef,
    #[serde(default)]
    repositories_added: Vec<RepoRef>,
    #[serde(default)]
    repositories_removed: Vec<RepoRef>,
}

fn repos(installation: i64, list: Vec<RepoRef>) -> Vec<Repository> {
    list.into_iter()
        .map(|r| Repository {
            id: r.id,
            installation,
            full_name: r.full_name,
            private: r.private,
        })
        .collect()
}

pub async fn receive(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let secret = server.config.webhook_secret.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unconfigured",
            "this service has no webhook secret",
        )
    })?;
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    if !octopage::verify_signature(secret.as_bytes(), &body, &header("x-hub-signature-256")) {
        return Err(ApiError::unauthorized(
            "the delivery's signature does not match",
        ));
    }
    server.metrics.webhooks.fetch_add(1, Ordering::Relaxed);
    let bad = |e: serde_json::Error| ApiError::bad_request(format!("the delivery: {e}"));
    match header("x-github-event").as_str() {
        "ping" => Ok(StatusCode::OK),
        "push" => {
            let event = octopage::PushEvent::parse(&body)?;
            for served in server.served_on(&event.repository) {
                let branch = served.record.lock().unwrap().branch.clone();
                if event.git_ref.starts_with(octopage_pagestore::GEN_PREFIX) {
                    // The repository's databases moved: follow, and record where.
                    served.db.refresh().await?;
                    let record = served.record.lock().unwrap().clone();
                    server.note_location(&record, &served.db).await?;
                } else if event.git_ref == branch && !event.after.is_zero() {
                    served.db.store().notify_head(event.after);
                }
            }
            Ok(StatusCode::OK)
        }
        "installation" => {
            let event: InstallationEvent = serde_json::from_slice(&body).map_err(bad)?;
            let id = event.installation.id;
            match event.action.as_str() {
                "created" | "new_permissions_accepted" => {
                    let account = event
                        .installation
                        .account
                        .map_or_else(String::new, |a| a.login);
                    server.meta.upsert_installation(id, &account).await?;
                    server
                        .meta
                        .add_repositories(id, repos(id, event.repositories))
                        .await?;
                    if let Some(sender) = event.sender
                        && let Some(user) = server.meta.user_by_github_id(sender.id).await?
                    {
                        server.meta.add_membership(user.id, id).await?;
                    }
                }
                "deleted" => {
                    let repositories = server.meta.repositories_of_installation(id).await?;
                    server.close_where(|r| r.installation == id);
                    server.meta.delete_installation(id).await?;
                    for repository in repositories {
                        server.forget_repository(&repository);
                    }
                }
                "suspend" => {
                    server.close_where(|r| r.installation == id);
                    server.meta.suspend_installation(id, true).await?;
                }
                "unsuspend" => server.meta.suspend_installation(id, false).await?,
                _ => {}
            }
            Ok(StatusCode::OK)
        }
        "installation_repositories" => {
            let event: RepositoriesEvent = serde_json::from_slice(&body).map_err(bad)?;
            let id = event.installation.id;
            if event.action == "added" || !event.repositories_added.is_empty() {
                server
                    .meta
                    .add_repositories(id, repos(id, event.repositories_added))
                    .await?;
            }
            if !event.repositories_removed.is_empty() {
                let removed: Vec<String> = event
                    .repositories_removed
                    .iter()
                    .map(|r| r.full_name.to_ascii_lowercase())
                    .collect();
                server.close_where(|r| removed.contains(&r.repository.to_ascii_lowercase()));
                server
                    .meta
                    .remove_repositories(
                        id,
                        event.repositories_removed.iter().map(|r| r.id).collect(),
                    )
                    .await?;
            }
            Ok(StatusCode::OK)
        }
        _ => Ok(StatusCode::ACCEPTED),
    }
}
