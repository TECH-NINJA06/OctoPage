use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use octopage_git::{Credentials, HttpConfig, SmartHttp, TokenFuture, TokenProvider};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use serde::Deserialize;

use crate::meta::{Repository, now};

/// How to reach GitHub as the App.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// The App's id.
    pub app_id: u64,
    /// The App's private key (PEM, as GitHub hands it out).
    pub private_key: String,
    /// For signing users in (the App's OAuth client).
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// The REST API root: `https://api.github.com`.
    pub api: String,
    /// The web root, for signing in: `https://github.com`.
    pub web: String,
    /// Where repositories are served over git: `https://github.com`.
    pub git: String,
}

impl AppConfig {
    /// The App on github.com.
    pub fn github(app_id: u64, private_key: String) -> Self {
        AppConfig {
            app_id,
            private_key,
            client_id: None,
            client_secret: None,
            api: "https://api.github.com".into(),
            web: "https://github.com".into(),
            git: "https://github.com".into(),
        }
    }
}

/// A user, as GitHub described them when they signed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedIn {
    pub github_id: i64,
    pub login: String,
    /// The App's installations they can use: (installation id, account).
    pub installations: Vec<(i64, String)>,
}

/// The App, with a cache of installation tokens.
pub struct GitHubApp {
    config: AppConfig,
    key: rsa::pkcs1v15::SigningKey<rsa::sha2::Sha256>,
    http: reqwest::Client,
    /// Installation tokens, and when each expires (seconds since the Unix epoch).
    tokens: Mutex<HashMap<i64, (String, i64)>>,
    minted: AtomicU64,
}

impl std::fmt::Debug for GitHubApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubApp")
            .field("app_id", &self.config.app_id)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Token {
    token: String,
    expires_at: String,
}

/// Seconds since the Unix epoch for an RFC 3339 UTC time (`2026-09-29T12:00:00Z`).
fn parse_time(text: &str) -> Option<i64> {
    let (date, time) = text.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time
        .split(':')
        .map(|p| p.split('.').next()?.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

impl GitHubApp {
    pub fn new(config: AppConfig) -> Result<Arc<Self>, String> {
        let key = rsa::RsaPrivateKey::from_pkcs1_pem(&config.private_key)
            .or_else(|_| rsa::RsaPrivateKey::from_pkcs8_pem(&config.private_key))
            .map_err(|e| format!("the App's private key: {e}"))?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("octopage-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(GitHubApp {
            config,
            key: rsa::pkcs1v15::SigningKey::new(key),
            http,
            tokens: Mutex::new(HashMap::new()),
            minted: AtomicU64::new(0),
        }))
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Installation tokens minted so far.
    pub fn minted(&self) -> u64 {
        self.minted.load(Ordering::Relaxed)
    }

    /// A JSON Web Token that signs in as the App, for ten minutes (less a minute for clock skew).
    pub fn jwt(&self) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let now = now();
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({ "iat": now - 60, "exp": now + 540, "iss": self.config.app_id.to_string() })
                .to_string(),
        );
        let signing_input = format!("{header}.{claims}");
        let signature = self.key.sign(signing_input.as_bytes()).to_vec();
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn api(&self, path: &str) -> String {
        format!("{}{path}", self.config.api.trim_end_matches('/'))
    }

    /// A token for `installation`: from the cache until five minutes before it expires, then a
    /// new one.
    pub async fn installation_token(&self, installation: i64) -> Result<String, String> {
        if let Some((token, expires)) = self.tokens.lock().unwrap().get(&installation)
            && *expires - 300 > now()
        {
            return Ok(token.clone());
        }
        let response = self
            .http
            .post(self.api(&format!("/app/installations/{installation}/access_tokens")))
            .bearer_auth(self.jwt())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|e| format!("minting a token for installation {installation}: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "GitHub refused a token for installation {installation} ({status}): {body}"
            ));
        }
        let token: Token = response
            .json()
            .await
            .map_err(|e| format!("an installation token: {e}"))?;
        let expires = parse_time(&token.expires_at).unwrap_or_else(|| now() + 3000);
        self.minted.fetch_add(1, Ordering::Relaxed);
        self.tokens
            .lock()
            .unwrap()
            .insert(installation, (token.token.clone(), expires));
        Ok(token.token)
    }

    /// Drop `installation`'s cached token (GitHub refused it).
    pub fn forget_token(&self, installation: i64) {
        self.tokens.lock().unwrap().remove(&installation);
    }

    /// A git transport to `full_name`, signing in as `installation`.
    pub fn transport(
        self: &Arc<Self>,
        installation: i64,
        full_name: &str,
    ) -> Result<SmartHttp, String> {
        let url = format!("{}/{full_name}.git", self.config.git.trim_end_matches('/'));
        let token = Arc::new(InstallationToken {
            app: self.clone(),
            installation,
        });
        SmartHttp::new(
            &url,
            Some(Credentials::git("x-access-token", token)),
            HttpConfig::default(),
        )
        .map_err(|e| e.to_string())
    }

    /// The repositories `installation` reaches.
    pub async fn installation_repositories(
        &self,
        installation: i64,
    ) -> Result<Vec<Repository>, String> {
        #[derive(Deserialize)]
        struct Page {
            repositories: Vec<Repo>,
        }
        #[derive(Deserialize)]
        struct Repo {
            id: i64,
            full_name: String,
            private: bool,
        }
        let token = self.installation_token(installation).await?;
        let mut out = Vec::new();
        for page in 1.. {
            let listed: Page = self
                .http
                .get(self.api(&format!(
                    "/installation/repositories?per_page=100&page={page}"
                )))
                .bearer_auth(&token)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|e| format!("listing installation {installation}'s repositories: {e}"))?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            let done = listed.repositories.len() < 100;
            out.extend(listed.repositories.into_iter().map(|r| Repository {
                id: r.id,
                installation,
                full_name: r.full_name,
                private: r.private,
            }));
            if done {
                break;
            }
        }
        Ok(out)
    }

    /// Where to send a user to sign in with GitHub, if the App's OAuth client is set.
    pub fn authorize_url(&self, state: &str, redirect: &str) -> Option<String> {
        let client = self.config.client_id.as_ref()?;
        Some(format!(
            "{}/login/oauth/authorize?client_id={client}&state={state}&redirect_uri={}",
            self.config.web.trim_end_matches('/'),
            percent(redirect)
        ))
    }

    /// Finish a sign-in: exchange GitHub's `code` for the user's token, read who they are and
    /// which installations they can use, and let the token go.
    pub async fn sign_in(&self, code: &str) -> Result<SignedIn, String> {
        #[derive(Deserialize)]
        struct Exchanged {
            access_token: Option<String>,
            error_description: Option<String>,
        }
        #[derive(Deserialize)]
        struct Me {
            id: i64,
            login: String,
        }
        #[derive(Deserialize)]
        struct Account {
            login: String,
        }
        #[derive(Deserialize)]
        struct Listed {
            id: i64,
            account: Account,
        }
        #[derive(Deserialize)]
        struct Installations {
            installations: Vec<Listed>,
        }
        let (Some(client_id), Some(secret)) = (&self.config.client_id, &self.config.client_secret)
        else {
            return Err("signing in with GitHub is not set up (no OAuth client)".into());
        };
        let exchanged: Exchanged = self
            .http
            .post(format!(
                "{}/login/oauth/access_token",
                self.config.web.trim_end_matches('/')
            ))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", secret.as_str()),
                ("code", code),
            ])
            .send()
            .await
            .map_err(|e| format!("signing in: {e}"))?
            .json()
            .await
            .map_err(|e| format!("signing in: {e}"))?;
        let token = exchanged.access_token.ok_or_else(|| {
            exchanged
                .error_description
                .unwrap_or_else(|| "GitHub did not sign the user in".into())
        })?;
        let get = |path: &str| {
            self.http
                .get(self.api(path))
                .bearer_auth(&token)
                .header("Accept", "application/vnd.github+json")
                .send()
        };
        let me: Me = get("/user")
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("reading the user: {e}"))?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let installations: Installations = get("/user/installations?per_page=100")
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("reading the user's installations: {e}"))?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        Ok(SignedIn {
            github_id: me.id,
            login: me.login,
            installations: installations
                .installations
                .into_iter()
                .map(|i| (i.id, i.account.login))
                .collect(),
        })
    }
}

fn percent(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// An installation's token, minted and refreshed as git needs it.
struct InstallationToken {
    app: Arc<GitHubApp>,
    installation: i64,
}

impl TokenProvider for InstallationToken {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(async move {
            self.app
                .installation_token(self.installation)
                .await
                .map_err(octopage_git::Error::Invalid)
        })
    }

    fn invalidate(&self) {
        self.app.forget_token(self.installation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-09-01T00:00:00Z"), Some(1_788_220_800));
        assert_eq!(
            parse_time("2026-09-01T14:30:05.25Z"),
            Some(1_788_220_800 + 52_205)
        );
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(percent("http://a/b c"), "http%3A%2F%2Fa%2Fb%20c");
    }
}
