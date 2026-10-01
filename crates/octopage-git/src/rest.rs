use bytes::Bytes;
use reqwest::Method;
use serde::Deserialize;

use crate::error::{Error, Result, invalid};
use crate::http::{Credentials, HttpClient, HttpConfig, RateLimitState, Replay, Request};
use crate::oid::ObjectId;

/// The GitHub REST API, for the jobs the spec gives it: reading and moving refs where the git
/// protocol is unavailable (the browser build), and metadata. Every call spends REST quota.
#[derive(Clone)]
pub struct Rest {
    http: HttpClient,
    api: String,
    repo: String,
    creds: Credentials,
}

#[derive(Deserialize)]
struct RefBody {
    object: RefObject,
}

#[derive(Deserialize)]
struct RefObject {
    sha: String,
}

/// What GitHub says about a repository.
#[derive(Clone, Debug, Deserialize)]
pub struct Repository {
    /// `OWNER/NAME`.
    pub full_name: String,
    pub private: bool,
    pub default_branch: Option<String>,
    /// Kilobytes, as GitHub last measured it (it lags behind pushes).
    pub size: u64,
}

/// An issue, as far as OctoPage cares.
#[derive(Clone, Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
}

#[derive(Deserialize)]
struct Account {
    #[serde(rename = "type")]
    kind: String,
}

impl Rest {
    /// The repository `owner/name` on api.github.com.
    pub fn new(repo: &str, creds: Credentials, config: HttpConfig) -> Result<Self> {
        Rest::with_base_url("https://api.github.com", repo, creds, config)
    }

    /// Another API root, such as GitHub Enterprise Server's `https://host/api/v3`.
    pub fn with_base_url(
        api: &str,
        repo: &str,
        creds: Credentials,
        config: HttpConfig,
    ) -> Result<Self> {
        Ok(Rest {
            http: HttpClient::new(config)?,
            api: api.trim_end_matches('/').to_string(),
            repo: repo.to_string(),
            creds,
        })
    }

    pub fn rate_limit(&self) -> RateLimitState {
        self.http.rate_limit()
    }

    /// The same API, for another repository.
    pub fn for_repo(&self, repo: &str) -> Rest {
        Rest {
            repo: repo.to_string(),
            ..self.clone()
        }
    }

    /// This client's repository, `OWNER/NAME`.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    fn ref_path(name: &str) -> Result<&str> {
        let short = name
            .strip_prefix("refs/")
            .ok_or_else(|| invalid(format!("ref name {name:?} must start with refs/")))?;
        if short.is_empty()
            || !short
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
            || short.contains("..")
        {
            return Err(invalid(format!("unsupported ref name {name:?}")));
        }
        Ok(short)
    }

    /// A call on the API: `path` follows the API root.
    async fn call_api(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Bytes> {
        let replay = if method == Method::GET {
            Replay::Safe
        } else {
            Replay::OnlyIfNotSent
        };
        let mut headers = vec![
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", "2022-11-28".to_string()),
        ];
        if body.is_some() {
            headers.push(("Content-Type", "application/json".to_string()));
        }
        let req = Request {
            method,
            url: format!("{}{path}", self.api),
            headers,
            body: body.map(|b| Bytes::from(b.to_string())),
            replay,
        };
        self.http.execute(&req, Some(&self.creds)).await
    }

    /// A call on this client's repository: `path` follows `/repos/OWNER/NAME`.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Bytes> {
        self.call_api(method, &format!("/repos/{}{path}", self.repo), body)
            .await
    }

    fn parse<'a, T: Deserialize<'a>>(body: &'a [u8], what: &str) -> Result<T> {
        serde_json::from_slice(body).map_err(|e| Error::Protocol(format!("{what}: {e}")))
    }

    /// The repository's metadata, or `None` if it does not exist (or the token cannot see it).
    pub async fn repository(&self) -> Result<Option<Repository>> {
        match self.call(Method::GET, "", None).await {
            Ok(body) => Ok(Some(Rest::parse(&body, "repository")?)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Create this client's repository, empty, under its owner (a user, which must be the
    /// token's, or an organization). The token needs to be allowed to create repositories.
    pub async fn create_repository(&self, private: bool, description: &str) -> Result<()> {
        let (owner, name) = self
            .repo
            .split_once('/')
            .ok_or_else(|| invalid(format!("{:?} is not OWNER/NAME", self.repo)))?;
        let account: Account = Rest::parse(
            &self
                .call_api(Method::GET, &format!("/users/{owner}"), None)
                .await?,
            "account",
        )?;
        let path = if account.kind == "Organization" {
            format!("/orgs/{owner}/repos")
        } else {
            "/user/repos".to_string()
        };
        let body = serde_json::json!({
            "name": name,
            "private": private,
            "description": description,
            "auto_init": false,
            "has_wiki": false,
        });
        self.call_api(Method::POST, &path, Some(body))
            .await
            .map(|_| ())
    }

    /// Delete this client's repository. The token needs administration rights on it.
    pub async fn delete_repository(&self) -> Result<()> {
        self.call(Method::DELETE, "", None).await.map(|_| ())
    }

    /// Make `branch` (a name, without `refs/heads/`) the default branch.
    pub async fn set_default_branch(&self, branch: &str) -> Result<()> {
        let body = serde_json::json!({ "default_branch": branch });
        self.call(Method::PATCH, "", Some(body)).await.map(|_| ())
    }

    /// The first hundred open issues (pull requests excluded).
    pub async fn open_issues(&self) -> Result<Vec<Issue>> {
        #[derive(Deserialize)]
        struct Listed {
            number: u64,
            title: String,
            pull_request: Option<serde_json::Value>,
        }
        let body = self
            .call(Method::GET, "/issues?state=open&per_page=100", None)
            .await?;
        let listed: Vec<Listed> = Rest::parse(&body, "issues")?;
        Ok(listed
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .map(|i| Issue {
                number: i.number,
                title: i.title,
            })
            .collect())
    }

    /// Open an issue; returns its number.
    pub async fn create_issue(&self, title: &str, body: &str) -> Result<u64> {
        let request = serde_json::json!({ "title": title, "body": body });
        let issue: Issue = Rest::parse(
            &self.call(Method::POST, "/issues", Some(request)).await?,
            "issue",
        )?;
        Ok(issue.number)
    }

    /// The value of ref `name` (such as `refs/heads/main`), or `None` if it does not exist.
    pub async fn get_ref(&self, name: &str) -> Result<Option<ObjectId>> {
        match self
            .call(
                Method::GET,
                &format!("/git/ref/{}", Rest::ref_path(name)?),
                None,
            )
            .await
        {
            Ok(body) => {
                let parsed: RefBody = serde_json::from_slice(&body)
                    .map_err(|e| Error::Protocol(format!("ref response: {e}")))?;
                Ok(Some(ObjectId::from_hex(&parsed.object.sha)?))
            }
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Create ref `name` at `id`. An existing ref is a `Conflict`.
    pub async fn create_ref(&self, name: &str, id: ObjectId) -> Result<()> {
        Rest::ref_path(name)?;
        let body = serde_json::json!({ "ref": name, "sha": id.to_hex() });
        self.call(Method::POST, "/git/refs", Some(body))
            .await
            .map(|_| ())
            .map_err(|e| conflict_on_422(e, name))
    }

    /// Move ref `name` to `id`. Without `force`, GitHub accepts only a fast-forward, which is a
    /// compare-and-swap as long as every new commit's parent is the value the writer read.
    pub async fn update_ref(&self, name: &str, id: ObjectId, force: bool) -> Result<()> {
        let path = format!("/git/refs/{}", Rest::ref_path(name)?);
        let body = serde_json::json!({ "sha": id.to_hex(), "force": force });
        self.call(Method::PATCH, &path, Some(body))
            .await
            .map(|_| ())
            .map_err(|e| conflict_on_422(e, name))
    }

    pub async fn delete_ref(&self, name: &str) -> Result<()> {
        let path = format!("/git/refs/{}", Rest::ref_path(name)?);
        match self.call(Method::DELETE, &path, None).await {
            Ok(_) => Ok(()),
            Err(Error::Http { status: 422, .. }) => Err(Error::NotFound(name.to_string())),
            Err(e) => Err(e),
        }
    }
}

fn conflict_on_422(error: Error, name: &str) -> Error {
    match error {
        Error::Http {
            status: 422, body, ..
        } => Error::Conflict {
            refname: name.to_string(),
            reason: body,
        },
        other => other,
    }
}
