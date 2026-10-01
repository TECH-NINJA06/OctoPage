use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use octopage_git::{Credentials, HttpConfig, SmartHttp, StaticToken};
use tokio::io::AsyncWriteExt;

pub const TOKEN: &str = "local-test-token";

pub struct LocalRemote {
    dir: tempfile::TempDir,
    base: String,
    state: Arc<ServerState>,
}

struct ServerState {
    root: PathBuf,
    expected_auth: String,
    /// Repositories that accept only their own tokens (as a GitHub App installation token
    /// reaches only the installation's repositories), by repository name.
    grants: std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// Requests served, by repository name.
    requests: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    /// Pushes are applied one at a time, and reads wait while one is applied. On Windows,
    /// concurrent receive-packs that bring the same object fail ("unable to migrate objects to
    /// permanent storage"), and a fetch that races a push moving objects in can fail too
    /// ("possible repository corruption on the remote side"). GitHub has neither problem;
    /// serialising keeps the race semantics, since a stale push still loses its CAS.
    pushes: tokio::sync::RwLock<()>,
}

impl LocalRemote {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(ServerState {
            root: dir.path().to_path_buf(),
            expected_auth: format!(
                "Basic {}",
                base64(format!("x-access-token:{TOKEN}").as_bytes())
            ),
            pushes: tokio::sync::RwLock::new(()),
            grants: Default::default(),
            requests: Default::default(),
        });
        let app = axum::Router::new().fallback(cgi).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        LocalRemote {
            dir,
            base: format!("http://{addr}"),
            state,
        }
    }

    /// The server's root URL: repository `name` is at `{base}/{name}.git`.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// From now on repository `name` accepts `token` (user `x-access-token`), and only the
    /// tokens granted to it: no longer the shared test token.
    pub fn grant(&self, name: &str, token: &str) {
        self.state
            .grants
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(token.to_string());
    }

    /// Requests served for repository `name` so far.
    pub fn requests(&self, name: &str) -> usize {
        self.state
            .requests
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(0)
    }

    /// A bare repo configured like a GitHub repo, which also fscks every object pushed to it.
    pub fn create_repo(&self, name: &str) -> String {
        let path = self.repo_path(name);
        git(None, &["init", "--bare", "-q", path.to_str().unwrap()]);
        for (key, value) in [
            ("http.receivepack", "true"),
            ("uploadpack.allowFilter", "true"),
            ("uploadpack.allowAnySHA1InWant", "true"),
            ("receive.fsckObjects", "true"),
        ] {
            git(Some(&path), &["config", key, value]);
        }
        self.url(name)
    }

    pub fn repo_path(&self, name: &str) -> PathBuf {
        self.dir.path().join(format!("{name}.git"))
    }

    pub fn url(&self, name: &str) -> String {
        format!("{}/{name}.git", self.base)
    }

    pub fn transport(&self, name: &str) -> SmartHttp {
        SmartHttp::new(&self.url(name), Some(creds()), HttpConfig::default()).unwrap()
    }

    /// Run git inside the bare repo `name`.
    pub fn git(&self, name: &str, args: &[&str]) -> String {
        git(Some(&self.repo_path(name)), args)
    }

    /// Install a git hook (`pre-receive`, say) in repo `name`: a shell script, which git runs
    /// with `sh` (Git for Windows ships one).
    pub fn set_hook(&self, name: &str, hook: &str, script: &str) {
        let path = self.repo_path(name).join("hooks").join(hook);
        std::fs::write(&path, script.replace("\r\n", "\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Imitate GitHub's push protection in repo `name`: refuse any push with a blob holding
    /// `secret`, and name each such blob's path the way GitHub does (`GH013`, `path: …`).
    pub fn protect_pushes(&self, name: &str, secret: &str) {
        let script = format!(
            r#"#!/bin/sh
found=""
while read old new ref; do
  [ "$new" = 0000000000000000000000000000000000000000 ] && continue
  for entry in $(git rev-list --objects "$new" --not --all | tr ' ' ':'); do
    sha="${{entry%%:*}}"
    path="${{entry#*:}}"
    [ "$path" = "$sha" ] && continue
    [ "$(git cat-file -t "$sha")" = blob ] || continue
    if git cat-file blob "$sha" | grep -a -q '{secret}'; then
      found="$found $path"
    fi
  done
done
[ -z "$found" ] && exit 0
echo "error: GH013: Repository rule violations found for refs/heads/main."
echo "- GITHUB PUSH PROTECTION"
echo "  Push cannot contain secrets"
echo "  —— Test Secret ——"
echo "  locations:"
for path in $found; do
  echo "    - commit: 0000000000000000000000000000000000000000"
  echo "      path: $path:1"
done
echo "  https://github.com/example/example/security/secret-scanning/unblock-secret/test"
exit 1
"#
        );
        self.set_hook(name, "pre-receive", &script);
    }
}

pub fn creds() -> Credentials {
    Credentials::git("x-access-token", Arc::new(StaticToken::new(TOKEN)))
}

/// Run git, independent of the user's global config for anything that matters here.
pub fn git(dir: Option<&Path>, args: &[&str]) -> String {
    git_with_input(dir, args, None)
}

pub fn git_with_input(dir: Option<&Path>, args: &[&str], input: Option<&[u8]>) -> String {
    String::from_utf8(git_bytes(dir, args, input)).unwrap()
}

pub fn git_bytes(dir: Option<&Path>, args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "commit.gpgsign=false",
        "-c",
        "user.name=octopage-test",
        "-c",
        "user.email=test@example.invalid",
    ]);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let mut child = cmd.spawn().expect("git must be installed");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        if let Some(input) = input {
            stdin.write_all(input).unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

async fn cgi(State(state): State<Arc<ServerState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = to_bytes(body, 512 << 20).await.unwrap();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    // `/owner/name.git/info/refs` is repository `owner/name`.
    let path = parts.uri.path();
    let repo = path
        .find(".git/")
        .map(|end| path[1..end].to_string())
        .unwrap_or_default();
    let authorized = {
        let grants = state.grants.lock().unwrap();
        match grants.get(&repo) {
            Some(tokens) => tokens.iter().any(|t| {
                header("authorization")
                    == format!("Basic {}", base64(format!("x-access-token:{t}").as_bytes()))
            }),
            None => header("authorization") == state.expected_auth,
        }
    };
    if !authorized {
        return Response::builder()
            .status(401)
            .header("WWW-Authenticate", "Basic realm=\"test\"")
            .body(Body::empty())
            .unwrap();
    }
    *state.requests.lock().unwrap().entry(repo).or_default() += 1;
    let (_push_turn, _read_turn) = if parts.uri.path().ends_with("/git-receive-pack") {
        (Some(state.pushes.write().await), None)
    } else {
        (None, Some(state.pushes.read().await))
    };
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", &state.root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", parts.uri.path())
        .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("REMOTE_USER", "octopage-test")
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let protocol = header("git-protocol");
    if !protocol.is_empty() {
        cmd.env("HTTP_GIT_PROTOCOL", protocol);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });
    let output = child.wait_with_output().await.unwrap();
    let _ = writer.await;

    let raw = output.stdout;
    let (head, payload) = match find(&raw, b"\r\n\r\n") {
        Some(at) => (&raw[..at], raw[at + 4..].to_vec()),
        None => match find(&raw, b"\n\n") {
            Some(at) => (&raw[..at], raw[at + 2..].to_vec()),
            None => (&raw[..0], raw.clone()),
        },
    };
    let mut builder = Response::builder();
    let mut status = 200;
    for line in String::from_utf8_lossy(head).lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("status") {
                status = value.trim()[..3].parse().unwrap();
            } else {
                builder = builder.header(key.trim(), value.trim());
            }
        }
    }
    builder.status(status).body(Body::from(payload)).unwrap()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                TABLE[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

// ------------------------------------------------------------------------ transport wrappers

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use octopage_git::{
    Error as GitError, NewObject, Object, ObjectId, Ref, RefUpdate, Result as GitResult, Transport,
};

/// Adds a fixed delay to every request, like a network round trip, so races between clients
/// play out over time as they do against GitHub rather than instantly.
pub struct Latency<T> {
    pub inner: T,
    delay: std::time::Duration,
}

impl<T> Latency<T> {
    pub fn new(inner: T, delay: std::time::Duration) -> Self {
        Latency { inner, delay }
    }
}

impl<T: Transport> Transport for Latency<T> {
    async fn list_refs(&self, prefixes: &[&str]) -> GitResult<Vec<Ref>> {
        tokio::time::sleep(self.delay).await;
        self.inner.list_refs(prefixes).await
    }

    async fn fetch(&self, ids: &[ObjectId]) -> GitResult<Vec<Object>> {
        tokio::time::sleep(self.delay).await;
        self.inner.fetch(ids).await
    }

    async fn history(
        &self,
        tip: ObjectId,
        have: Option<ObjectId>,
        with_trees: bool,
    ) -> GitResult<Vec<Object>> {
        tokio::time::sleep(self.delay).await;
        self.inner.history(tip, have, with_trees).await
    }

    async fn push(&self, updates: &[RefUpdate], objects: &[NewObject]) -> GitResult<()> {
        tokio::time::sleep(self.delay).await;
        self.inner.push(updates, objects).await
    }

    fn location(&self) -> Option<String> {
        self.inner.location()
    }

    fn relocate(&self, location: &str) -> GitResult<Self> {
        Ok(Latency::new(self.inner.relocate(location)?, self.delay))
    }
}

/// Counts the round trips made through a transport.
#[derive(Default)]
pub struct Counting<T> {
    pub inner: T,
    pub fetches: AtomicUsize,
    pub fetched_ids: AtomicUsize,
    pub histories: AtomicUsize,
    pub pushes: AtomicUsize,
    pub ref_reads: AtomicUsize,
}

impl<T> Counting<T> {
    pub fn new(inner: T) -> Self {
        Counting {
            inner,
            fetches: AtomicUsize::new(0),
            fetched_ids: AtomicUsize::new(0),
            histories: AtomicUsize::new(0),
            pushes: AtomicUsize::new(0),
            ref_reads: AtomicUsize::new(0),
        }
    }

    /// Round trips of every kind so far.
    pub fn round_trips(&self) -> usize {
        [
            &self.fetches,
            &self.histories,
            &self.pushes,
            &self.ref_reads,
        ]
        .iter()
        .map(|c| c.load(Ordering::SeqCst))
        .sum()
    }
}

impl<T: Transport> Transport for Counting<T> {
    async fn list_refs(&self, prefixes: &[&str]) -> GitResult<Vec<Ref>> {
        self.ref_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.list_refs(prefixes).await
    }

    async fn fetch(&self, ids: &[ObjectId]) -> GitResult<Vec<Object>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        self.fetched_ids.fetch_add(ids.len(), Ordering::SeqCst);
        self.inner.fetch(ids).await
    }

    async fn history(
        &self,
        tip: ObjectId,
        have: Option<ObjectId>,
        with_trees: bool,
    ) -> GitResult<Vec<Object>> {
        self.histories.fetch_add(1, Ordering::SeqCst);
        self.inner.history(tip, have, with_trees).await
    }

    async fn push(&self, updates: &[RefUpdate], objects: &[NewObject]) -> GitResult<()> {
        self.pushes.fetch_add(1, Ordering::SeqCst);
        self.inner.push(updates, objects).await
    }

    fn location(&self) -> Option<String> {
        self.inner.location()
    }

    /// Counts afresh for the other repository.
    fn relocate(&self, location: &str) -> GitResult<Self> {
        Ok(Counting::new(self.inner.relocate(location)?))
    }
}

/// Makes a fraction of pushes fail the ways a network does: dropped before reaching the server,
/// applied but with the response lost, or throttled. Reads pass through untouched.
pub struct Chaos<T> {
    pub inner: T,
    rate: f64,
    rng: Mutex<fastrand::Rng>,
    pub dropped_before: AtomicUsize,
    pub dropped_after: AtomicUsize,
    pub throttled: AtomicUsize,
    pub rejected: AtomicUsize,
}

impl<T> Chaos<T> {
    /// Each push fails with probability `rate`, reproducibly for a given `seed`.
    pub fn new(inner: T, rate: f64, seed: u64) -> Self {
        Chaos {
            inner,
            rate,
            rng: Mutex::new(fastrand::Rng::with_seed(seed)),
            dropped_before: AtomicUsize::new(0),
            dropped_after: AtomicUsize::new(0),
            throttled: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
        }
    }

    pub fn injected(&self) -> usize {
        [
            &self.dropped_before,
            &self.dropped_after,
            &self.throttled,
            &self.rejected,
        ]
        .iter()
        .map(|c| c.load(Ordering::SeqCst))
        .sum()
    }
}

impl<T: Transport> Transport for Chaos<T> {
    async fn list_refs(&self, prefixes: &[&str]) -> GitResult<Vec<Ref>> {
        self.inner.list_refs(prefixes).await
    }

    async fn fetch(&self, ids: &[ObjectId]) -> GitResult<Vec<Object>> {
        self.inner.fetch(ids).await
    }

    async fn history(
        &self,
        tip: ObjectId,
        have: Option<ObjectId>,
        with_trees: bool,
    ) -> GitResult<Vec<Object>> {
        self.inner.history(tip, have, with_trees).await
    }

    async fn push(&self, updates: &[RefUpdate], objects: &[NewObject]) -> GitResult<()> {
        let roll = {
            let mut rng = self.rng.lock().unwrap();
            (rng.f64() < self.rate).then(|| rng.u8(0..4))
        };
        match roll {
            Some(0) => {
                self.dropped_before.fetch_add(1, Ordering::SeqCst);
                Err(GitError::PushOutcomeUnknown(
                    "chaos: connection dropped before the push arrived".into(),
                ))
            }
            Some(1) => {
                self.inner.push(updates, objects).await?;
                self.dropped_after.fetch_add(1, Ordering::SeqCst);
                Err(GitError::PushOutcomeUnknown(
                    "chaos: push applied, response lost".into(),
                ))
            }
            Some(2) => {
                self.throttled.fetch_add(1, Ordering::SeqCst);
                Err(GitError::RateLimited { retry_after: None })
            }
            Some(_) => {
                // github.com's bare "failed": rejected, nothing applied.
                self.rejected.fetch_add(1, Ordering::SeqCst);
                Err(GitError::Rejected("chaos: failed".into()))
            }
            None => self.inner.push(updates, objects).await,
        }
    }

    fn location(&self) -> Option<String> {
        self.inner.location()
    }

    /// The same failure rate for the other repository, with a seed of its own.
    fn relocate(&self, location: &str) -> GitResult<Self> {
        let seed = self.rng.lock().unwrap().u64(..);
        Ok(Chaos::new(self.inner.relocate(location)?, self.rate, seed))
    }
}
