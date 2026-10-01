use reqwest::Method;

use crate::error::{Error, Result, invalid};
use crate::http::{Credentials, HttpClient, HttpConfig, Replay, Request};
use crate::object::Object;
use crate::oid::ObjectId;

/// raw.githubusercontent.com, for public read-mostly datasets and the browser build.
/// Always addressed by commit id, never by branch name, so every response is immutable
/// and every body is checked against the blob id the caller expects.
#[derive(Clone)]
pub struct RawCdn {
    http: HttpClient,
    base: String,
    repo: String,
    creds: Option<Credentials>,
}

impl RawCdn {
    pub fn new(repo: &str, creds: Option<Credentials>, config: HttpConfig) -> Result<Self> {
        RawCdn::with_base_url("https://raw.githubusercontent.com", repo, creds, config)
    }

    pub fn with_base_url(
        base: &str,
        repo: &str,
        creds: Option<Credentials>,
        config: HttpConfig,
    ) -> Result<Self> {
        Ok(RawCdn {
            http: HttpClient::new(config)?,
            base: base.trim_end_matches('/').to_string(),
            repo: repo.to_string(),
            creds,
        })
    }

    /// The blob at `path` in `commit`'s tree, verified to have id `expected`.
    pub async fn get(&self, commit: ObjectId, path: &str, expected: ObjectId) -> Result<Object> {
        let safe = !path.is_empty()
            && !path.starts_with('/')
            && path
                .split('/')
                .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
            && path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b));
        if !safe {
            return Err(invalid(format!("unsupported path {path:?}")));
        }
        let req = Request {
            method: Method::GET,
            url: format!("{}/{}/{commit}/{path}", self.base, self.repo),
            headers: Vec::new(),
            body: None,
            replay: Replay::Safe,
        };
        let body = self.http.execute(&req, self.creds.as_ref()).await?;
        let blob = Object::blob(body)?;
        if blob.id() != expected {
            return Err(Error::Integrity(format!(
                "{path} at {commit}: expected blob {expected}, got {}",
                blob.id()
            )));
        }
        Ok(blob)
    }
}
