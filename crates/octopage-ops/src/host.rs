use std::future::Future;
use std::sync::Mutex;

use octopage::{Error, InMemory, Result};
use octopage_git::{Credentials, HttpConfig, Rest, StaticToken};

/// A git host's administrative side.
pub trait Host: Send + Sync {
    /// Create an empty repository at `location` (`OWNER/NAME` on GitHub), private unless
    /// `public`. Fails if it exists.
    fn create_repository(
        &self,
        location: &str,
        public: bool,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Delete the repository at `location`.
    fn delete_repository(&self, location: &str) -> impl Future<Output = Result<()>> + Send;

    /// Whether a repository exists at `location` (and can be seen).
    fn exists(&self, location: &str) -> impl Future<Output = Result<bool>> + Send;

    /// The repository's size in bytes, if the host says.
    fn repository_size(&self, location: &str) -> impl Future<Output = Result<Option<u64>>> + Send;

    /// The repository's default branch, without `refs/heads/`.
    fn default_branch(&self, location: &str)
    -> impl Future<Output = Result<Option<String>>> + Send;

    /// Make `branch` (without `refs/heads/`) the repository's default branch.
    fn set_default_branch(
        &self,
        location: &str,
        branch: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Open an issue, unless one with the same title is open already. Returns whether it opened
    /// one.
    fn open_issue(
        &self,
        location: &str,
        title: &str,
        body: &str,
    ) -> impl Future<Output = Result<bool>> + Send;
}

/// github.com (or GitHub Enterprise Server) through its REST API.
///
/// Two tokens: `token` reads repositories and opens issues (in Actions, the job's
/// `GITHUB_TOKEN`); `admin` creates and deletes repositories, which `GITHUB_TOKEN` cannot (spec
/// issue 4): a fine-grained token with Administration, Contents and Workflows write on all
/// the owner's repositories, or a GitHub App installation token.
#[derive(Clone)]
pub struct GitHub {
    api: String,
    token: Credentials,
    admin: Option<Credentials>,
}

impl GitHub {
    pub fn new(token: &str, admin: Option<&str>) -> Self {
        GitHub::with_api("https://api.github.com", token, admin)
    }

    /// Another API root, such as GitHub Enterprise Server's `https://HOST/api/v3`.
    pub fn with_api(api: &str, token: &str, admin: Option<&str>) -> Self {
        let bearer = |t: &str| Credentials::bearer(std::sync::Arc::new(StaticToken::new(t)));
        GitHub {
            api: api.to_string(),
            token: bearer(token),
            admin: admin.map(bearer),
        }
    }

    fn rest(&self, location: &str, admin: bool) -> Result<Rest> {
        let creds = if admin {
            self.admin.clone().ok_or_else(|| {
                Error::Invalid(
                    "creating or deleting a repository needs an admin token \
                     (OCTOPAGE_ADMIN_TOKEN): the job's own token cannot"
                        .into(),
                )
            })?
        } else {
            self.token.clone()
        };
        Ok(Rest::with_base_url(
            &self.api,
            location,
            creds,
            HttpConfig::default(),
        )?)
    }
}

impl Host for GitHub {
    async fn create_repository(&self, location: &str, public: bool) -> Result<()> {
        let rest = self.rest(location, true)?;
        Ok(rest
            .create_repository(
                !public,
                "An OctoPage database (a newer generation; the maintenance job made it)",
            )
            .await?)
    }

    async fn delete_repository(&self, location: &str) -> Result<()> {
        Ok(self.rest(location, true)?.delete_repository().await?)
    }

    async fn exists(&self, location: &str) -> Result<bool> {
        Ok(self.rest(location, false)?.repository().await?.is_some())
    }

    async fn repository_size(&self, location: &str) -> Result<Option<u64>> {
        let repo = self.rest(location, false)?.repository().await?;
        Ok(repo.map(|r| r.size * 1024))
    }

    async fn default_branch(&self, location: &str) -> Result<Option<String>> {
        let repo = self.rest(location, false)?.repository().await?;
        Ok(repo.and_then(|r| r.default_branch))
    }

    async fn set_default_branch(&self, location: &str, branch: &str) -> Result<()> {
        // Changing settings takes the admin token when there is one.
        let admin = self.admin.is_some();
        Ok(self
            .rest(location, admin)?
            .set_default_branch(branch)
            .await?)
    }

    async fn open_issue(&self, location: &str, title: &str, body: &str) -> Result<bool> {
        let rest = self.rest(location, false)?;
        if rest.open_issues().await?.iter().any(|i| i.title == title) {
            return Ok(false);
        }
        rest.create_issue(title, body).await?;
        Ok(true)
    }
}

/// The in-memory host of an [`InMemory`] remote, for tests. It records issues.
pub struct Memory {
    remote: InMemory,
    issues: Mutex<Vec<(String, String, String)>>,
    defaults: Mutex<Vec<(String, String)>>,
}

impl Memory {
    /// The host of `remote` (any repository on it).
    pub fn new(remote: &InMemory) -> Self {
        use octopage::Transport;
        Memory {
            remote: remote
                .relocate(remote.name())
                .expect("the repository is on its own host"),
            issues: Mutex::new(Vec::new()),
            defaults: Mutex::new(Vec::new()),
        }
    }

    /// Issues opened so far: (location, title, body).
    pub fn issues(&self) -> Vec<(String, String, String)> {
        self.issues.lock().unwrap().clone()
    }
}

impl Host for Memory {
    async fn create_repository(&self, location: &str, _public: bool) -> Result<()> {
        self.remote.create_repository(location)?;
        Ok(())
    }

    async fn delete_repository(&self, location: &str) -> Result<()> {
        Ok(self.remote.delete_repository(location)?)
    }

    async fn exists(&self, location: &str) -> Result<bool> {
        Ok(self.remote.repositories().iter().any(|r| r == location))
    }

    async fn repository_size(&self, location: &str) -> Result<Option<u64>> {
        use octopage::Transport;
        Ok(match self.remote.relocate(location) {
            Ok(repo) => Some(repo.object_bytes()),
            Err(_) => None,
        })
    }

    async fn default_branch(&self, location: &str) -> Result<Option<String>> {
        let set = self.defaults.lock().unwrap();
        Ok(set
            .iter()
            .rev()
            .find(|(l, _)| l == location)
            .map(|(_, b)| b.clone())
            .or_else(|| Some("main".into())))
    }

    async fn set_default_branch(&self, location: &str, branch: &str) -> Result<()> {
        self.defaults
            .lock()
            .unwrap()
            .push((location.to_string(), branch.to_string()));
        Ok(())
    }

    async fn open_issue(&self, location: &str, title: &str, body: &str) -> Result<bool> {
        let mut issues = self.issues.lock().unwrap();
        if issues.iter().any(|(l, t, _)| l == location && t == title) {
            return Ok(false);
        }
        issues.push((location.to_string(), title.to_string(), body.to_string()));
        Ok(true)
    }
}
