use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequestParts, MatchedPath, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, delete, get, post};
use octopage::{Statement, Unlock};
use serde::Deserialize;
use serde_json::{Value as Json_, json};

use crate::Server;
use crate::error::{ApiError, ApiResult};
use crate::meta::{KeyKind, KeyMode, KeyRecord, User};
use crate::served::{self, Served, Tx};
use crate::values;
use octopage_ops::workflows::MAINTENANCE_PATH;
use tower_http::services::{ServeDir, ServeFile};

/// Everything the service answers.
pub fn router(server: Arc<Server>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz));
    if server.config.public_metrics {
        router = router.route("/metrics", get(metrics));
    }
    let router = router
        .route("/webhooks/github", post(crate::webhook::receive))
        .route("/auth/github/login", get(login))
        .route("/auth/github/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/v1/me", get(me).delete(forget_me))
        .route("/v1/repositories", get(repositories))
        .route("/v1/usage", get(usage))
        .route("/v1/keys", get(list_keys).post(create_key))
        .route("/v1/keys/{key}", delete(revoke_key))
        .route("/v1/databases", get(list_databases).post(add_database))
        .route(
            "/v1/databases/{id}",
            get(database_info).delete(remove_database),
        )
        .route("/v1/databases/{id}/unlock", post(unlock))
        .route("/v1/databases/{id}/stats", get(stats))
        .route(
            "/v1/databases/{id}/settings",
            get(get_settings).put(put_settings),
        )
        .route("/v1/databases/{id}/maintenance", post(maintenance))
        .route("/v1/databases/{id}/query", post(statement))
        .route("/v1/databases/{id}/execute", post(statement))
        .route("/v1/databases/{id}/batch", post(batch))
        .route("/v1/databases/{id}/transactions", post(begin))
        .route(
            "/v1/databases/{id}/transactions/{tx}/execute",
            post(tx_execute),
        )
        .route(
            "/v1/databases/{id}/transactions/{tx}/commit",
            post(tx_commit),
        )
        .route(
            "/v1/databases/{id}/transactions/{tx}/rollback",
            post(tx_rollback),
        )
        .route("/v1/databases/{id}/log", get(log))
        .route(
            "/v1/databases/{id}/branches",
            get(branches).post(create_branch),
        )
        .route("/v1/databases/{id}/branches/{name}", delete(drop_branch))
        .route("/v1/databases/{id}/merge", post(merge))
        .route("/v1/{*rest}", any(no_route));
    // Everything else is the dashboard, if there is one: its files, and its page for any path
    // it routes itself.
    let router = match &server.config.dashboard_dir {
        Some(dir) => {
            let files = ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html")));
            router.fallback_service(
                Router::new()
                    .fallback_service(files)
                    .layer(middleware::from_fn(dashboard_headers)),
            )
        }
        None => router,
    };
    router
        .layer(middleware::from_fn_with_state(server.clone(), count))
        .layer(DefaultBodyLimit::max(16 << 20))
        .with_state(server)
}

/// `/metrics` alone, for a listener of its own on a private address.
pub fn metrics_router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .with_state(server)
}

async fn no_route() -> ApiError {
    ApiError::not_found("no such route (see the API reference)")
}

/// What the dashboard's pages may do: load only their own scripts, talk only to the service.
/// Styles may be inline (the SQL editor adds its own).
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data: https://avatars.githubusercontent.com; \
    connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";

/// The dashboard's headers: its hashed assets are cached for good, its page never.
async fn dashboard_headers(request: Request, next: Next) -> Response {
    let hashed = request.uri().path().starts_with("/assets/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if hashed {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

/// Count every request by route and status.
async fn count(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "other".to_string(), |m| m.as_str().to_string());
    let response = next.run(request).await;
    server.metrics.request(&route, response.status().as_u16());
    response
}

// ------------------------------------------------------------------------------ authentication

/// The caller: a user, by an API key or a sign-in session.
pub struct Auth {
    pub user: User,
    pub key: KeyRecord,
}

/// The scheme, host and port of `url`: what browsers send as `Origin`.
fn origin_of(url: &str) -> &str {
    match url.find("://") {
        Some(at) => match url[at + 3..].find('/') {
            Some(end) => &url[..at + 3 + end],
            None => url,
        },
        None => url,
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|t| t.trim().to_string())
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
}

const SESSION_COOKIE: &str = "octopage_session";

impl FromRequestParts<Arc<Server>> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> ApiResult<Self> {
        let (secret, by_cookie) = match bearer(&parts.headers) {
            Some(secret) => (secret, false),
            None => {
                let secret = cookie(&parts.headers, SESSION_COOKIE).ok_or_else(|| {
                    ApiError::unauthorized("send an API key: Authorization: Bearer opk_…")
                })?;
                (secret, true)
            }
        };
        // A browser sends the cookie wherever the request comes from, so a change made with it
        // must come from the dashboard's own pages (against cross-site request forgery).
        if by_cookie && !matches!(parts.method, Method::GET | Method::HEAD) {
            let origin = parts
                .headers
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok());
            if origin != Some(origin_of(&server.config.public_url)) {
                return Err(ApiError::forbidden(
                    "a change made with a sign-in session must come from the dashboard",
                ));
            }
        }
        let (user, key) = server
            .meta
            .authenticate(&secret)
            .await?
            .ok_or_else(|| ApiError::unauthorized("that key is not valid (or has expired)"))?;
        Ok(Auth { user, key })
    }
}

// ------------------------------------------------------------------------------ service

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(server): State<Arc<Server>>) -> ApiResult<&'static str> {
    server.meta.ping().await?;
    Ok("ready")
}

async fn metrics(State(server): State<Arc<Server>>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        server.render_metrics(),
    )
}

// ------------------------------------------------------------------------------ signing in

#[derive(Deserialize)]
struct LoginQuery {
    redirect: Option<String>,
}

async fn login(
    State(server): State<Arc<Server>>,
    Query(query): Query<LoginQuery>,
) -> ApiResult<Redirect> {
    let state = crate::meta::random_hex(16);
    let callback = format!("{}/auth/github/callback", server.config.public_url);
    let url = server
        .app
        .authorize_url(&state, &callback)
        .ok_or_else(|| ApiError::not_found("signing in with GitHub is not set up"))?;
    // Only back to a page of this service: anything else would make it an open redirect.
    let redirect = query
        .redirect
        .filter(|to| to.starts_with('/') && !to.starts_with("//") && !to.contains('\\'));
    let mut sign_ins = server.sign_ins.lock().unwrap();
    let now = crate::meta::now();
    sign_ins.retain(|_, (at, _)| now - *at < 600);
    sign_ins.insert(state, (now, redirect));
    Ok(Redirect::to(&url))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
}

async fn callback(
    State(server): State<Arc<Server>>,
    Query(query): Query<CallbackQuery>,
) -> ApiResult<Response> {
    let started = server.sign_ins.lock().unwrap().remove(&query.state);
    let Some((at, redirect)) = started else {
        return Err(ApiError::bad_request(
            "that sign-in was not started here, or took too long",
        ));
    };
    if crate::meta::now() - at > 600 {
        return Err(ApiError::bad_request("that sign-in took too long"));
    }
    let signed_in = server
        .app
        .sign_in(&query.code)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, "github", e))?;
    let user = server
        .meta
        .upsert_user(signed_in.github_id, &signed_in.login)
        .await?;
    let mut ids = Vec::new();
    for (installation, account) in &signed_in.installations {
        server
            .meta
            .upsert_installation(*installation, account)
            .await?;
        match server.app.installation_repositories(*installation).await {
            Ok(repos) => server.meta.add_repositories(*installation, repos).await?,
            Err(error) => tracing::warn!(%error, installation, "could not list repositories"),
        }
        ids.push(*installation);
    }
    server.meta.set_memberships(user.id, ids).await?;
    let lifetime = server.config.session.as_secs() as i64;
    let (session, _) = server
        .meta
        .create_key(user.id, "sign-in", KeyKind::Session, Some(lifetime))
        .await?;
    let secure = if server.config.public_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{SESSION_COOKIE}={session}; Path=/; HttpOnly; SameSite=Lax; Max-Age={lifetime}{secure}"
    );
    let response = match redirect {
        Some(to) => ([(header::SET_COOKIE, cookie)], Redirect::to(&to)).into_response(),
        None => (
            [(header::SET_COOKIE, cookie)],
            Json(json!({ "session": session, "user": { "login": user.login } })),
        )
            .into_response(),
    };
    Ok(response)
}

/// Sign out: end the session, and clear its cookie.
async fn logout(State(server): State<Arc<Server>>, auth: Auth) -> ApiResult<Response> {
    if auth.key.kind == KeyKind::Session {
        server.meta.revoke_key(auth.user.id, &auth.key.id).await?;
    }
    let cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

async fn me(State(server): State<Arc<Server>>, auth: Auth) -> ApiResult<Json<Json_>> {
    let installations = server.meta.installations_of(auth.user.id).await?;
    let install_url = server
        .config
        .app_slug
        .as_ref()
        .map(|slug| format!("{}/apps/{slug}/installations/new", server.app.config().web));
    Ok(Json(json!({
        "login": auth.user.login,
        "github_id": auth.user.github_id,
        "key": auth.key.id,
        "session": auth.key.kind == KeyKind::Session,
        "installations": installations.iter().map(|i| json!({
            "id": i.id, "account": i.account, "suspended": i.suspended,
        })).collect::<Vec<_>>(),
        "install_url": install_url,
        "kms": server.kms.is_some(),
    })))
}

/// Delete the user's account: their keys, sessions and memberships. Their repositories, and
/// the databases in them, are untouched.
async fn forget_me(State(server): State<Arc<Server>>, auth: Auth) -> ApiResult<Response> {
    server.meta.delete_user(auth.user.id).await?;
    tracing::info!(login = %auth.user.login, "a user deleted their account");
    let cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

/// The repositories the user's installations of the App reach: where databases can go.
async fn repositories(State(server): State<Arc<Server>>, auth: Auth) -> ApiResult<Json<Json_>> {
    let repositories = server.meta.repositories_of(auth.user.id).await?;
    Ok(Json(json!({
        "repositories": repositories.iter().map(|r| json!({
            "repository": r.full_name, "installation": r.installation, "private": r.private,
        })).collect::<Vec<_>>()
    })))
}

/// Requests, requests to GitHub, and commits, per installation and day (days since the Unix
/// epoch). Written every minute or so.
async fn usage(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Json_>> {
    let days = query
        .get("days")
        .and_then(|d| d.parse().ok())
        .unwrap_or(30i64)
        .clamp(1, 366);
    let mut installations = Vec::new();
    for installation in server.meta.installations_of(auth.user.id).await? {
        let usage = server.meta.usage(installation.id, days).await?;
        installations.push(json!({
            "installation": installation.id,
            "account": installation.account,
            "days": usage.iter().map(|u| json!({
                "day": u.day, "requests": u.requests,
                "github_requests": u.github_requests, "commits": u.commits,
            })).collect::<Vec<_>>(),
        }));
    }
    Ok(Json(json!({ "installations": installations })))
}

// ------------------------------------------------------------------------------ keys

fn key_json(key: &KeyRecord) -> Json_ {
    json!({
        "id": key.id,
        "name": key.name,
        "prefix": key.prefix,
        "created": key.created,
        "last_used": key.last_used,
    })
}

async fn list_keys(State(server): State<Arc<Server>>, auth: Auth) -> ApiResult<Json<Json_>> {
    let keys = server.meta.keys_of(auth.user.id).await?;
    Ok(Json(
        json!({ "keys": keys.iter().map(key_json).collect::<Vec<_>>() }),
    ))
}

#[derive(Deserialize)]
struct NewKey {
    name: String,
}

async fn create_key(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Json(body): Json<NewKey>,
) -> ApiResult<(StatusCode, Json<Json_>)> {
    let (secret, key) = server
        .meta
        .create_key(auth.user.id, &body.name, KeyKind::Api, None)
        .await?;
    let mut answer = key_json(&key);
    answer["key"] = json!(secret);
    Ok((StatusCode::CREATED, Json(answer)))
}

async fn revoke_key(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(key): Path<String>,
) -> ApiResult<StatusCode> {
    if server.meta.revoke_key(auth.user.id, &key).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("no key {key}")))
    }
}

// ------------------------------------------------------------------------------ databases

fn database_json(record: &crate::meta::DatabaseRecord) -> Json_ {
    json!({
        "id": record.id,
        "repository": record.repository,
        "branch": record.branch,
        "encryption": record.keys.as_str(),
        "created": record.created,
    })
}

async fn list_databases(State(server): State<Arc<Server>>, auth: Auth) -> ApiResult<Json<Json_>> {
    let databases = server.meta.databases_of(auth.user.id).await?;
    Ok(Json(json!({
        "databases": databases.iter().map(database_json).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
struct NewDatabase {
    repository: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    encryption: Option<String>,
    #[serde(default)]
    passphrase: Option<String>,
}

async fn add_database(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Json(body): Json<NewDatabase>,
) -> ApiResult<(StatusCode, Json<Json_>)> {
    let keys = match body.encryption.as_deref() {
        None if body.passphrase.is_some() => KeyMode::Passphrase,
        None if server.kms.is_some() => KeyMode::Kms,
        None => KeyMode::None,
        Some(name) => KeyMode::parse(name).ok_or_else(|| {
            ApiError::bad_request("encryption is \"kms\", \"passphrase\" or \"none\"")
        })?,
    };
    let (record, created, recovery) = server
        .add_database(
            &auth.user,
            &body.repository,
            body.branch.as_deref().unwrap_or("main"),
            body.create,
            keys,
            body.passphrase,
        )
        .await?;
    let mut answer = database_json(&record);
    answer["new"] = json!(created);
    if let Some(key) = recovery {
        answer["recovery_key"] = json!(key);
    }
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(answer)))
}

async fn database_info(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<Json_>> {
    let record = server.database_for(&auth.user, &id).await?;
    let served = server.open(&record).await?;
    let head = served.db.refresh().await?;
    let settings = served.db.settings().await?;
    server.note_location(&record, &served.db).await?;
    let mut answer = database_json(&served.record.lock().unwrap().clone());
    answer["head"] = json!(head.to_hex());
    answer["page_size"] = json!(served.db.store().page_size());
    answer["retention"] = json!(settings.retention.to_string());
    Ok(Json(answer))
}

async fn remove_database(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let record = server.database_for(&auth.user, &id).await?;
    server.close_where(|r| r.id == record.id);
    server.meta.remove_database(&record.id).await?;
    if !server.meta.has_databases_on(&record.repository).await? {
        server.forget_repository(&record.repository);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UnlockBody {
    passphrase: String,
}

async fn unlock(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
    Json(body): Json<UnlockBody>,
) -> ApiResult<StatusCode> {
    let record = server.database_for(&auth.user, &id).await?;
    if record.keys != KeyMode::Passphrase {
        return Err(ApiError::bad_request(
            "this database is not in passphrase mode",
        ));
    }
    server
        .open_with(&record, Some(Unlock::Passphrase(body.passphrase)))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stats(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<Json_>> {
    let served = served(&server, &auth, &id).await?;
    Ok(Json(server.stats(&served).await?))
}

fn settings_json(settings: &octopage::Settings) -> Json_ {
    json!({
        "retention": settings.retention.to_string(),
        "snapshot_days": settings.snapshot_days,
        "live_limit": settings.live_limit,
        "repository_budget": settings.repository_budget,
        "warn_days": settings.warn_days,
        "grace_days": settings.grace_days,
    })
}

async fn get_settings(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<Json_>> {
    let served = served(&server, &auth, &id).await?;
    Ok(Json(settings_json(&served.db.settings().await?)))
}

/// The settings to change; the rest stay as they are.
#[derive(Deserialize)]
struct SettingsBody {
    retention: Option<String>,
    snapshot_days: Option<u32>,
    live_limit: Option<u64>,
    repository_budget: Option<u64>,
    warn_days: Option<u32>,
    grace_days: Option<u32>,
}

/// Change the settings (a commit beside the pages). A new live limit applies to connections
/// opened afterwards.
async fn put_settings(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
    Json(body): Json<SettingsBody>,
) -> ApiResult<Json<Json_>> {
    let served = served(&server, &auth, &id).await?;
    let mut settings = served.db.settings().await?;
    if let Some(retention) = body.retention {
        settings.retention = retention
            .parse()
            .map_err(|e: String| ApiError::bad_request(format!("retention: {e}")))?;
    }
    settings.snapshot_days = body.snapshot_days.unwrap_or(settings.snapshot_days);
    settings.live_limit = body.live_limit.unwrap_or(settings.live_limit);
    settings.repository_budget = body.repository_budget.unwrap_or(settings.repository_budget);
    settings.warn_days = body.warn_days.unwrap_or(settings.warn_days);
    settings.grace_days = body.grace_days.unwrap_or(settings.grace_days);
    let commit = served.db.set_settings(&settings).await?;
    server.stats.lock().unwrap().remove(&id);
    Ok(Json(json!({
        "settings": settings_json(&settings),
        "commit": commit.to_hex(),
    })))
}

#[derive(Deserialize, Default)]
struct MaintenanceBody {
    cli_source: Option<String>,
    cli_ref: Option<String>,
}

/// Install the maintenance workflow in the database's repository.
async fn maintenance(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
    body: Option<Json<MaintenanceBody>>,
) -> ApiResult<(StatusCode, Json<Json_>)> {
    let served = served(&server, &auth, &id).await?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let commit = server
        .install_maintenance(&served, body.cli_source.as_deref(), body.cli_ref.as_deref())
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "path": MAINTENANCE_PATH, "commit": commit.to_hex() })),
    ))
}

// ------------------------------------------------------------------------------ statements

#[derive(Deserialize)]
struct StatementBody {
    sql: String,
    #[serde(default)]
    params: Option<Json_>,
}

/// Rows, rows changed, and the commit a statement made (if it made one).
fn outcome_json(outcome: &octopage::Outcome, commit: Option<octopage::ObjectId>) -> Json_ {
    let mut answer = values::rows(&outcome.rows);
    answer["changed"] = json!(outcome.changed);
    answer["commit"] = json!(commit.map(|c| c.to_hex()));
    answer
}

/// One statement, as `/execute`, `/query` and a transaction's `execute` take: several belong
/// in `/batch`.
fn one_statement(sql: &str) -> ApiResult<()> {
    if octopage::statements(sql).len() > 1 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "multiple_statements",
            "one statement per call: send several to /batch, which runs them as one transaction",
        ));
    }
    Ok(())
}

/// The statement's first word, upper-cased.
fn keyword(sql: &str) -> String {
    sql.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Transaction control belongs to `/transactions`: a stateless call runs on a connection
/// shared with every other caller, which must never be left inside a transaction.
fn no_transaction_control(sql: &str) -> ApiResult<()> {
    if matches!(
        keyword(sql).as_str(),
        "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE"
    ) {
        return Err(ApiError::bad_request(
            "transactions are POST /v1/databases/{id}/transactions (then .../execute, .../commit, \
             .../rollback), or /batch for a list of statements",
        ));
    }
    Ok(())
}

/// Inside an interactive transaction, savepoints are fine; ending it is `/commit` or
/// `/rollback`.
fn no_transaction_end(sql: &str) -> ApiResult<()> {
    let word = keyword(sql);
    let rollback_to = word == "ROLLBACK" && sql.to_ascii_uppercase().contains(" TO ");
    if matches!(word.as_str(), "BEGIN" | "COMMIT" | "END") || (word == "ROLLBACK" && !rollback_to) {
        return Err(ApiError::bad_request(
            "end the transaction with .../commit or .../rollback",
        ));
    }
    Ok(())
}

/// The database `id` for `auth`, open.
async fn served(server: &Arc<Server>, auth: &Auth, id: &str) -> ApiResult<Arc<Served>> {
    let record = server.database_for(&auth.user, id).await?;
    server.open(&record).await
}

/// After a call: count it, and notice whether the database moved to a new generation.
async fn after(server: &Arc<Server>, served: &Served, commits: i64) -> ApiResult<()> {
    let record = served.record.lock().unwrap().clone();
    server.note_usage(record.installation, commits);
    server.note_location(&record, &served.db).await
}

/// Run `work`, and say which commit it made, if any.
fn committing<R>(
    conn: &served::Conn,
    work: impl FnOnce(&served::Conn) -> octopage::Result<R>,
) -> octopage::Result<(R, Option<octopage::ObjectId>)> {
    let before = conn.last_commit();
    let result = work(conn)?;
    let after = conn.last_commit();
    let commit = after
        .filter(|c| Some(*c) != before && c.published)
        .map(|c| c.head);
    Ok((result, commit))
}

async fn statement(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
    Json(body): Json<StatementBody>,
) -> ApiResult<Json<Json_>> {
    one_statement(&body.sql)?;
    no_transaction_control(&body.sql)?;
    let served = served(&server, &auth, &id).await?;
    let params = values::params(body.params.as_ref()).map_err(ApiError::bad_request)?;
    let sql = body.sql;
    let (outcome, commit) = if served::reads_only(&sql) {
        let outcome = served.read(move |c| c.run_statement(&sql, &params)).await?;
        (outcome, None)
    } else {
        served
            .write(move |c| committing(c, |c| c.run_statement(&sql, &params)))
            .await?
    };
    after(&server, &served, commit.is_some() as i64).await?;
    Ok(Json(outcome_json(&outcome, commit)))
}

#[derive(Deserialize)]
struct BatchBody {
    #[serde(default)]
    statements: Vec<StatementBody>,
    /// Or a script: statements separated by semicolons, without parameters.
    #[serde(default)]
    sql: Option<String>,
}

async fn batch(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
    Json(body): Json<BatchBody>,
) -> ApiResult<Json<Json_>> {
    let listed = match body.sql {
        None => body.statements,
        Some(script) if body.statements.is_empty() => octopage::statements(&script)
            .into_iter()
            .map(|sql| StatementBody {
                sql: sql.to_string(),
                params: None,
            })
            .collect(),
        Some(_) => return Err(ApiError::bad_request("send statements or sql, not both")),
    };
    let served = served(&server, &auth, &id).await?;
    let statements = listed
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            // The batch is the transaction: statements inside it cannot end it early.
            no_transaction_control(&s.sql).map_err(|_| {
                ApiError::bad_request(format!(
                    "statement {}: a batch is one transaction already, without BEGIN or COMMIT",
                    i + 1
                ))
            })?;
            let params = values::params(s.params.as_ref())
                .map_err(|e| ApiError::bad_request(format!("statement {}: {e}", i + 1)))?;
            Ok(Statement::new(s.sql, &params))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let (outcomes, commit) = served
        .write(move |c| committing(c, |c| c.run_transaction(&statements)))
        .await?;
    after(&server, &served, commit.is_some() as i64).await?;
    Ok(Json(json!({
        "results": outcomes
            .iter()
            .map(|o| outcome_json(o, None))
            .collect::<Vec<_>>(),
        "commit": commit.map(|c| c.to_hex()),
    })))
}

// ------------------------------------------------------------------------------ transactions

async fn begin(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<Json_>)> {
    let served = served(&server, &auth, &id).await?;
    let tx = Tx::begin(&served, auth.user.id, server.config.transaction_idle).await?;
    let answer = json!({
        "transaction": tx.id,
        "idle_timeout": server.config.transaction_idle.as_secs(),
    });
    server.transactions.insert(tx);
    Ok((StatusCode::CREATED, Json(answer)))
}

/// Transaction `tx` of `auth`, on database `id`.
fn transaction(server: &Server, auth: &Auth, id: &str, tx: &str) -> ApiResult<Arc<Tx>> {
    match server.transactions.get(tx) {
        Some(t) if t.user == auth.user.id && t.database == id => Ok(t),
        _ => Err(ApiError::not_found(format!(
            "no open transaction {tx} (it may have timed out and rolled back)"
        ))),
    }
}

async fn tx_execute(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path((id, tx)): Path<(String, String)>,
    Json(body): Json<StatementBody>,
) -> ApiResult<Json<Json_>> {
    one_statement(&body.sql)?;
    no_transaction_end(&body.sql)?;
    let tx = transaction(&server, &auth, &id, &tx)?;
    let params = values::params(body.params.as_ref()).map_err(ApiError::bad_request)?;
    let sql = body.sql;
    let outcome = tx.run(move |c| c.run_statement(&sql, &params)).await?;
    Ok(Json(outcome_json(&outcome, None)))
}

async fn tx_commit(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path((id, tx)): Path<(String, String)>,
) -> ApiResult<Json<Json_>> {
    let open = transaction(&server, &auth, &id, &tx)?;
    let result = open.finish("COMMIT").await;
    server.transactions.remove(&tx);
    let commit = result?;
    if let Ok(served) = served(&server, &auth, &id).await {
        after(&server, &served, commit.is_some() as i64).await?;
    }
    Ok(Json(json!({ "commit": commit.map(|c| c.to_hex()) })))
}

async fn tx_rollback(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path((id, tx)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let open = transaction(&server, &auth, &id, &tx)?;
    let result = open.finish("ROLLBACK").await;
    server.transactions.remove(&tx);
    result?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------ history, branches

async fn log(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Json_>> {
    let served = served(&server, &auth, &id).await?;
    let limit = query
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(20usize)
        .min(1000);
    served.db.refresh().await?;
    let entries = served.db.log(limit).await?;
    Ok(Json(json!({
        "commits": entries.iter().map(|e| json!({
            "commit": e.commit.to_hex(),
            "parent": e.parent.map(|p| p.to_hex()),
            "time": e.time,
            "message": e.message,
            "statements": e.changelog.statements.iter().map(|s| json!({
                "sql": s.sql,
                "params": s.params.iter().map(values::to_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>()
    })))
}

async fn branches(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<Json_>> {
    let served = served(&server, &auth, &id).await?;
    let branches = served.db.branches().await?;
    Ok(Json(json!({
        "branches": branches.iter().map(|(name, head)| json!({
            "name": name, "head": head.to_hex()
        })).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
struct NewBranch {
    name: String,
    #[serde(default)]
    from: Option<String>,
}

async fn create_branch(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
    Json(body): Json<NewBranch>,
) -> ApiResult<(StatusCode, Json<Json_>)> {
    let served = served(&server, &auth, &id).await?;
    let head = served
        .db
        .create_branch(&body.name, body.from.as_deref())
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "name": body.name, "head": head.to_hex() })),
    ))
}

async fn drop_branch(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let served = served(&server, &auth, &id).await?;
    served.db.drop_branch(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MergeBody {
    from: String,
}

async fn merge(
    State(server): State<Arc<Server>>,
    auth: Auth,
    Path(id): Path<String>,
    Json(body): Json<MergeBody>,
) -> ApiResult<Json<Json_>> {
    let served = served(&server, &auth, &id).await?;
    let source = served.db.open_branch(&body.from).await?;
    let report = served.write(move |c| c.merge_from(&source)).await?;
    after(&server, &served, report.merged.len() as i64).await?;
    Ok(Json(json!({
        "merged": report.merged.iter().map(|(from, made)| json!({
            "from": from.to_hex(), "commit": made.to_hex()
        })).collect::<Vec<_>>(),
        "skipped": report.skipped,
    })))
}
