use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use tokio::sync::Semaphore;

use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct HttpConfig {
    /// Requests in flight per client. The spec: at most 8, well under GitHub's secondary limit of 100.
    pub max_in_flight: usize,
    /// Budget for opening a connection. When a host resolves to several addresses, it is split
    /// between them, so one unreachable CDN address cannot stall a request for long.
    pub connect_timeout: Duration,
    /// Budget for a whole request, including uploading a large pack.
    pub request_timeout: Duration,
    /// Attempts per request, including the first.
    pub max_attempts: u32,
    /// First back-off step; doubles per attempt, with full jitter.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Longest rate-limit wait to sit out; beyond it the caller gets `Error::RateLimited`.
    pub max_rate_limit_wait: Duration,
    pub user_agent: String,
    /// Speak HTTP/1.1 only, instead of negotiating HTTP/2 where the server offers it.
    pub http1_only: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            max_in_flight: 8,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(300),
            max_attempts: 5,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
            max_rate_limit_wait: Duration::from_secs(60),
            user_agent: concat!("octopage/", env!("CARGO_PKG_VERSION")).to_string(),
            http1_only: false,
        }
    }
}

pub type TokenFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// Supplies the token for each request, so short-lived tokens (GitHub App installation tokens
/// expire after an hour) can be refreshed mid-session. OctoPage never stores tokens itself.
pub trait TokenProvider: Send + Sync {
    fn token(&self) -> TokenFuture<'_>;

    /// Called when the server rejected the current token (HTTP 401); the next `token()` should refresh it.
    fn invalidate(&self) {}
}

/// A fixed token, such as a fine-grained personal access token.
pub struct StaticToken(String);

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        StaticToken(token.into())
    }
}

impl TokenProvider for StaticToken {
    fn token(&self) -> TokenFuture<'_> {
        let token = self.0.clone();
        Box::pin(async move { Ok(token) })
    }
}

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken(***)")
    }
}

#[derive(Clone)]
enum Scheme {
    /// HTTPS basic auth, as git uses: username plus token as password.
    Basic(String),
    Bearer,
    /// `Authorization: token <t>`, which raw.githubusercontent.com expects.
    Token,
}

/// How to authenticate: an HTTP scheme plus a token source.
#[derive(Clone)]
pub struct Credentials {
    scheme: Scheme,
    provider: Arc<dyn TokenProvider>,
}

impl Credentials {
    /// For git endpoints. GitHub App and Actions tokens use the username `x-access-token`;
    /// personal access tokens use the account's username.
    pub fn git(username: impl Into<String>, provider: Arc<dyn TokenProvider>) -> Self {
        Credentials {
            scheme: Scheme::Basic(username.into()),
            provider,
        }
    }

    /// For the REST API.
    pub fn bearer(provider: Arc<dyn TokenProvider>) -> Self {
        Credentials {
            scheme: Scheme::Bearer,
            provider,
        }
    }

    /// For raw.githubusercontent.com.
    pub fn raw(provider: Arc<dyn TokenProvider>) -> Self {
        Credentials {
            scheme: Scheme::Token,
            provider,
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = match &self.scheme {
            Scheme::Basic(user) => format!("Basic({user})"),
            Scheme::Bearer => "Bearer".into(),
            Scheme::Token => "Token".into(),
        };
        write!(f, "Credentials({scheme}, ***)")
    }
}

/// The REST quota as last reported by GitHub, and whether the client is currently being throttled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateLimitState {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    /// Unix time at which the quota resets.
    pub reset: Option<u64>,
    /// Set while GitHub is rate-limiting this client; cleared by the next successful request.
    pub degraded: bool,
}

impl RateLimitState {
    /// True when less than `fraction` of the quota remains (the spec reserves 20%).
    pub fn below(&self, fraction: f64) -> bool {
        match (self.limit, self.remaining) {
            (Some(limit), Some(remaining)) if limit > 0 => {
                (remaining as f64) < fraction * limit as f64
            }
            _ => false,
        }
    }
}

/// Whether a request may be sent again after an unclear failure.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Replay {
    /// Reads: resend on any transient failure.
    Safe,
    /// Pushes and ref writes: resend only when the request certainly never reached the server.
    OnlyIfNotSent,
}

pub(crate) struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<Bytes>,
    pub replay: Replay,
}

#[derive(Clone)]
pub(crate) struct HttpClient {
    client: reqwest::Client,
    config: Arc<HttpConfig>,
    permits: Arc<Semaphore>,
    rate: Arc<Mutex<RateLimitState>>,
}

enum Step {
    Done(Result<Bytes>),
    Retry {
        error: Error,
        wait: Option<Duration>,
    },
}

impl HttpClient {
    pub(crate) fn new(config: HttpConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(config.user_agent.clone())
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout);
        if config.http1_only {
            builder = builder.http1_only();
        }
        let client = builder.build().map_err(Error::Network)?;
        Ok(HttpClient {
            client,
            permits: Arc::new(Semaphore::new(config.max_in_flight.max(1))),
            config: Arc::new(config),
            rate: Arc::new(Mutex::new(RateLimitState::default())),
        })
    }

    pub(crate) fn rate_limit(&self) -> RateLimitState {
        *self.rate.lock().unwrap()
    }

    /// Send a request. Successful (2xx) responses are returned; everything else becomes an `Error`.
    pub(crate) async fn execute(
        &self,
        req: &Request,
        creds: Option<&Credentials>,
    ) -> Result<Bytes> {
        let mut attempt = 0;
        let mut reauthenticated = false;
        loop {
            attempt += 1;
            let step = self.attempt(req, creds, &mut reauthenticated).await;
            match step {
                Step::Done(result) => return result,
                Step::Retry { error, wait } => {
                    if attempt >= self.config.max_attempts {
                        return Err(error);
                    }
                    let wait = wait.unwrap_or_else(|| self.backoff(attempt));
                    tracing::debug!(url = %req.url, attempt, ?wait, %error, "retrying request");
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    async fn attempt(
        &self,
        req: &Request,
        creds: Option<&Credentials>,
        reauthenticated: &mut bool,
    ) -> Step {
        let token = match creds {
            Some(c) => match c.provider.token().await {
                Ok(t) => Some(t),
                Err(e) => return Step::Done(Err(e)),
            },
            None => None,
        };
        let mut builder = self.client.request(req.method.clone(), &req.url);
        for (name, value) in &req.headers {
            builder = builder.header(*name, value);
        }
        if let (Some(c), Some(token)) = (creds, token) {
            builder = match &c.scheme {
                Scheme::Basic(user) => builder.basic_auth(user, Some(token)),
                Scheme::Bearer => builder.bearer_auth(token),
                Scheme::Token => builder.header("Authorization", format!("token {token}")),
            };
        }
        if let Some(body) = &req.body {
            builder = builder.body(body.clone());
        }

        let permit = self
            .permits
            .acquire()
            .await
            .expect("semaphore is never closed");
        let sent = builder.send().await;
        let (status, headers, body) = match sent {
            Ok(resp) => {
                let status = resp.status();
                let headers = resp.headers().clone();
                match resp.bytes().await {
                    Ok(body) => (status, headers, body),
                    Err(e) => return self.network_failure(req, e, true),
                }
            }
            Err(e) => {
                let sent = !e.is_connect();
                return self.network_failure(req, e, sent);
            }
        };
        drop(permit);
        self.record_rate_limit(&headers);

        if status.is_success() {
            self.rate.lock().unwrap().degraded = false;
            return Step::Done(Ok(body));
        }
        let snippet = String::from_utf8_lossy(&body[..body.len().min(300)])
            .trim()
            .to_string();
        if status == StatusCode::UNAUTHORIZED {
            if let (Some(c), false) = (creds, *reauthenticated) {
                *reauthenticated = true;
                c.provider.invalidate();
                return Step::Retry {
                    error: Error::Auth {
                        status: 401,
                        message: snippet,
                    },
                    wait: Some(Duration::ZERO),
                };
            }
            return Step::Done(Err(Error::Auth {
                status: 401,
                message: snippet,
            }));
        }
        if let Some(wait) = self.rate_limit_wait(status, &headers) {
            self.rate.lock().unwrap().degraded = true;
            let error = Error::RateLimited {
                retry_after: Some(wait),
            };
            if wait > self.config.max_rate_limit_wait {
                return Step::Done(Err(error));
            }
            return Step::Retry {
                error,
                wait: Some(wait),
            };
        }
        match status {
            StatusCode::FORBIDDEN => Step::Done(Err(Error::Auth {
                status: 403,
                message: snippet,
            })),
            StatusCode::NOT_FOUND => {
                Step::Done(Err(Error::NotFound(format!("{} ({snippet})", req.url))))
            }
            s if s.is_server_error() => {
                let error = Error::Http {
                    status: s.as_u16(),
                    url: req.url.clone(),
                    body: snippet,
                };
                if req.replay == Replay::Safe {
                    Step::Retry { error, wait: None }
                } else {
                    Step::Done(Err(Error::PushOutcomeUnknown(error.to_string())))
                }
            }
            s => Step::Done(Err(Error::Http {
                status: s.as_u16(),
                url: req.url.clone(),
                body: snippet,
            })),
        }
    }

    fn network_failure(&self, req: &Request, error: reqwest::Error, sent: bool) -> Step {
        if sent && req.replay == Replay::OnlyIfNotSent {
            return Step::Done(Err(Error::PushOutcomeUnknown(error.to_string())));
        }
        Step::Retry {
            error: Error::Network(error),
            wait: None,
        }
    }

    /// GitHub signals throttling with 429, or 403 plus `retry-after` or an exhausted quota.
    fn rate_limit_wait(&self, status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
        };
        let retry_after = header(RETRY_AFTER.as_str())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let exhausted = header("x-ratelimit-remaining") == Some("0");
        let limited = status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN && (retry_after.is_some() || exhausted));
        if !limited {
            return None;
        }
        let until_reset = header("x-ratelimit-reset")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|reset| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                Duration::from_secs(reset.saturating_sub(now))
            });
        Some(
            retry_after
                .or(if exhausted { until_reset } else { None })
                .unwrap_or(self.config.initial_backoff * 4),
        )
    }

    fn record_rate_limit(&self, headers: &HeaderMap) {
        let number = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        if let (Some(limit), Some(remaining)) =
            (number("x-ratelimit-limit"), number("x-ratelimit-remaining"))
        {
            let mut rate = self.rate.lock().unwrap();
            rate.limit = Some(limit as u32);
            rate.remaining = Some(remaining as u32);
            rate.reset = number("x-ratelimit-reset");
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let exp = self
            .config
            .initial_backoff
            .saturating_mul(1 << (attempt - 1).min(16));
        exp.min(self.config.max_backoff).mul_f64(fastrand::f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_reserve() {
        let state = RateLimitState {
            limit: Some(5000),
            remaining: Some(900),
            reset: None,
            degraded: false,
        };
        assert!(state.below(0.2));
        assert!(
            !RateLimitState {
                remaining: Some(1001),
                ..state
            }
            .below(0.2)
        );
        assert!(!RateLimitState::default().below(0.2));
    }

    #[test]
    fn secrets_are_not_printed() {
        let creds = Credentials::git("x-access-token", Arc::new(StaticToken::new("ghs_secret")));
        assert!(!format!("{creds:?}").contains("ghs_secret"));
        assert!(!format!("{:?}", StaticToken::new("ghs_secret")).contains("ghs_secret"));
    }
}
