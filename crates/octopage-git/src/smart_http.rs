use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use reqwest::Method;

use crate::error::{Error, Result};
use crate::http::{Credentials, HttpClient, HttpConfig, RateLimitState, Replay, Request};
use crate::object::{Object, ObjectKind};
use crate::oid::ObjectId;
use crate::protocol::Filter;
use crate::transport::{NewObject, Ref, RefUpdate, Transport};
use crate::{pack, protocol};

/// Git's smart HTTP protocol, the path for all bulk data. Every call is one POST on a kept-alive
/// connection: pushes skip ref discovery because the caller already knows the expected old values.
#[derive(Clone)]
pub struct SmartHttp {
    http: HttpClient,
    base: String,
    creds: Option<Credentials>,
    whole_resends: Arc<AtomicUsize>,
}

impl SmartHttp {
    /// `remote_url` is the repository's HTTPS URL, such as `https://github.com/owner/name.git`.
    pub fn new(remote_url: &str, creds: Option<Credentials>, config: HttpConfig) -> Result<Self> {
        Ok(SmartHttp {
            http: HttpClient::new(config)?,
            base: remote_url.trim_end_matches('/').to_string(),
            creds,
            whole_resends: Arc::default(),
        })
    }

    /// Pushes that were rejected with deltas and then sent again with whole objects.
    pub fn whole_resends(&self) -> usize {
        self.whole_resends.load(Ordering::Relaxed)
    }

    /// The repository `owner/name` on github.com.
    pub fn github(repo: &str, creds: Option<Credentials>, config: HttpConfig) -> Result<Self> {
        SmartHttp::new(&format!("https://github.com/{repo}.git"), creds, config)
    }

    pub fn rate_limit(&self) -> RateLimitState {
        self.http.rate_limit()
    }

    async fn fetch_once(&self, wants: &[ObjectId]) -> Result<Vec<Object>> {
        let body = self
            .post(
                "git-upload-pack",
                protocol::fetch_request(wants, &[], None, Filter::Exact),
                true,
                Replay::Safe,
            )
            .await?;
        pack::parse(&protocol::parse_fetch_response(&body)?)
    }

    async fn post(&self, service: &str, body: Vec<u8>, v2: bool, replay: Replay) -> Result<Bytes> {
        let mut headers = vec![
            ("Content-Type", format!("application/x-{service}-request")),
            ("Accept", format!("application/x-{service}-result")),
        ];
        if v2 {
            headers.push(("Git-Protocol", "version=2".to_string()));
        }
        let req = Request {
            method: Method::POST,
            url: format!("{}/{service}", self.base),
            headers,
            body: Some(Bytes::from(body)),
            replay,
        };
        match self.http.execute(&req, self.creds.as_ref()).await {
            Ok(body) => Ok(body),
            Err(Error::NotFound(_)) => Err(Error::NotFound(format!(
                "{}: repository not found, or the token cannot see it",
                self.base
            ))),
            Err(e) => Err(e),
        }
    }
}

impl SmartHttp {
    /// Send a pack exactly as built, with no fallback: for tools and diagnostics.
    pub async fn push_pack(&self, updates: &[RefUpdate], pack: &[u8]) -> Result<()> {
        let request = protocol::receive_pack_request(updates, pack)?;
        let body = self
            .post("git-receive-pack", request, false, Replay::OnlyIfNotSent)
            .await?;
        protocol::parse_report_status(&body, updates)
    }
}

/// `https://host/a/b.git` as (`https://host`, `a/b.git`).
fn split_url(url: &str) -> Option<(&str, &str)> {
    let scheme = url.find("://")? + 3;
    let slash = scheme + url[scheme..].find('/')?;
    Some((&url[..slash], url[slash + 1..].trim_end_matches('/')))
}

/// Add what the push carried to a rejection, so the server's complaint can be matched to it.
fn describe(error: Error, objects: &[NewObject]) -> Error {
    match error {
        Error::Rejected(reason) => {
            let count =
                |kind: ObjectKind| objects.iter().filter(|o| o.object.kind() == kind).count();
            let deltas = objects.iter().filter(|o| o.delta_base.is_some()).count();
            Error::Rejected(format!(
                "{reason} [sent {} commits, {} trees, {} blobs; {deltas} offered as deltas]",
                count(ObjectKind::Commit),
                count(ObjectKind::Tree),
                count(ObjectKind::Blob)
            ))
        }
        other => other,
    }
}

impl Transport for SmartHttp {
    async fn list_refs(&self, prefixes: &[&str]) -> Result<Vec<Ref>> {
        let body = self
            .post(
                "git-upload-pack",
                protocol::ls_refs_request(prefixes),
                true,
                Replay::Safe,
            )
            .await?;
        protocol::parse_ls_refs(&body, prefixes)
    }

    async fn fetch(&self, ids: &[ObjectId]) -> Result<Vec<Object>> {
        let mut seen = HashSet::new();
        let wants: Vec<ObjectId> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        if wants.is_empty() {
            return Ok(Vec::new());
        }
        let mut objects = self.fetch_once(&wants).await?;
        let got: HashSet<ObjectId> = objects.iter().map(Object::id).collect();
        let missing: Vec<ObjectId> = wants.into_iter().filter(|id| !got.contains(id)).collect();
        if !missing.is_empty() {
            // The tree:0 filter drops a wanted tree that is also reachable from another wanted
            // commit (git marks it seen, then filters it). Asking for the rest alone gets them.
            let rest = self.fetch_once(&missing).await?;
            let got: HashSet<ObjectId> = rest.iter().map(Object::id).collect();
            let still: Vec<ObjectId> = missing.into_iter().filter(|id| !got.contains(id)).collect();
            if !still.is_empty() {
                return Err(Error::MissingObjects(still));
            }
            objects.extend(rest);
        }
        Ok(objects)
    }

    async fn history(
        &self,
        tip: ObjectId,
        have: Option<ObjectId>,
        with_trees: bool,
    ) -> Result<Vec<Object>> {
        let filter = if with_trees {
            Filter::Trees
        } else {
            Filter::Exact
        };
        let request = match have {
            Some(have) => protocol::fetch_request(&[tip], &[have], None, filter),
            None => protocol::fetch_request(&[tip], &[], Some(1), filter),
        };
        let body = self
            .post("git-upload-pack", request, true, Replay::Safe)
            .await?;
        let objects: Vec<Object> = pack::parse(&protocol::parse_fetch_response(&body)?)?
            .into_iter()
            .filter(|o| {
                o.kind() == ObjectKind::Commit || (with_trees && o.kind() == ObjectKind::Tree)
            })
            .collect();
        // With a `have`, an empty answer just means `tip` is already reachable from it.
        if have.is_none() && !objects.iter().any(|c| c.id() == tip) {
            return Err(Error::MissingObjects(vec![tip]));
        }
        Ok(objects)
    }

    async fn push(&self, updates: &[RefUpdate], objects: &[NewObject]) -> Result<()> {
        let thin = objects.iter().any(|o| o.delta_base.is_some());
        match self.push_pack(updates, &pack::build(objects)).await {
            // github.com has rejected pushes whose trees were deltas against objects it holds
            // ("missing necessary objects"), which stock git accepts. A rejected push applied
            // nothing, so sending the same objects again, whole, is always safe.
            Err(Error::Rejected(reason))
                if thin && reason.contains("missing necessary objects") =>
            {
                tracing::warn!(%reason, "push with deltas rejected; resending whole objects");
                self.whole_resends.fetch_add(1, Ordering::Relaxed);
                self.push_pack(updates, &pack::build_whole(objects))
                    .await
                    .map_err(|e| describe(e, objects))
            }
            other => other.map_err(|e| describe(e, objects)),
        }
    }

    /// The repository's path on its host, without `.git`: `OWNER/NAME` on github.com.
    fn location(&self) -> Option<String> {
        let (_, path) = split_url(&self.base)?;
        Some(path.trim_end_matches(".git").to_string())
    }

    /// Another repository on the same host: `location` replaces as many trailing segments of
    /// this one's path as it has (`OWNER/NAME` on github.com), or is a whole URL.
    fn relocate(&self, location: &str) -> Result<Self> {
        let base = if location.contains("://") {
            location.trim_end_matches('/').to_string()
        } else {
            let (root, path) = split_url(&self.base)
                .ok_or_else(|| Error::Invalid(format!("cannot relocate {}", self.base)))?;
            let suffix = if path.ends_with(".git") { ".git" } else { "" };
            let mut segments: Vec<&str> = path.trim_end_matches(".git").split('/').collect();
            let new: Vec<&str> = location.trim_matches('/').split('/').collect();
            if new.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
                return Err(Error::Invalid(format!(
                    "{location:?} is not a repository location"
                )));
            }
            segments.truncate(segments.len().saturating_sub(new.len()));
            segments.extend(new);
            format!("{root}/{}{suffix}", segments.join("/"))
        };
        Ok(SmartHttp {
            http: self.http.clone(),
            base,
            creds: self.creds.clone(),
            whole_resends: Arc::default(),
        })
    }

    /// Asks for the refs without credentials: a private repository refuses (or claims not to
    /// exist), a public one answers.
    async fn is_public(&self) -> Result<Option<bool>> {
        let anonymous = SmartHttp {
            creds: None,
            ..self.clone()
        };
        match anonymous.list_refs(&["refs/heads/"]).await {
            Ok(_) => Ok(Some(true)),
            Err(Error::Auth { .. } | Error::NotFound(_)) => Ok(Some(false)),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations() {
        let github = SmartHttp::github("owner/name", None, HttpConfig::default()).unwrap();
        assert_eq!(github.location().as_deref(), Some("owner/name"));
        let next = github.relocate("owner/name-g2").unwrap();
        assert_eq!(next.base, "https://github.com/owner/name-g2.git");
        let local =
            SmartHttp::new("http://127.0.0.1:9/db.git", None, HttpConfig::default()).unwrap();
        assert_eq!(local.location().as_deref(), Some("db"));
        assert_eq!(
            local.relocate("db-g2").unwrap().base,
            "http://127.0.0.1:9/db-g2.git"
        );
        assert_eq!(
            local.relocate("https://example.com/x/y.git").unwrap().base,
            "https://example.com/x/y.git"
        );
        assert!(github.relocate("../other").is_err());
    }
}
