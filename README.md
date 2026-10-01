# OctoPage

An embedded relational database whose only storage device is a GitHub repository: every 4 KB page is a git
blob, every committed transaction is a git commit, and the database head is a git ref.

- Design: `OctoPage Relational DB Engine on GitHub Block Storage — Specification.pdf`
- Build plan (phases 0–9), tech stack and hosting: [ROADMAP.md](ROADMAP.md)
- Phase 0 (transport spike): findings in [spike/REPORT.md](spike/REPORT.md).
- Phase 1 (git object layer and transport): `crates/octopage-git`.
- Phase 2 (page store): `crates/octopage-pagestore`, `crates/octopage-kv`. Gate passed on github.com.
- Engine decision: SQL runs on SQLite, through a VFS on the page store
  ([spike/sqlite_vfs/REPORT.md](spike/sqlite_vfs/REPORT.md)).
- Phase 3 (production SQLite VFS): `crates/octopage-sqlite`. Gate passed in memory with fault injection and on
  real git.
- Phase 4 (engine API and shell): `crates/octopage`, `crates/octopage-cli`. Gate passed: SQLite's logic tests
  (9,221 queries) on one connection.
- Phase 5 (transactions): deterministic re-execution, statement-list transactions, changelog replay, and the
  advisory writer lease. Gate passed: the logic tests in transactions, and bank transfers that stay
  serializable (checked by replaying the whole history).
- Phase 6 (security, compression, branching): encrypted databases (passphrase, recovery key, key providers),
  compression, branches and merges, push-protection reports, signed commits. Gate passed.
- Phase 7 (operations): `crates/octopage-ops`.
  - Generation rollover to a new repository, with retention.
  - A maintenance workflow for GitHub Actions: staging cleanup, integrity scans, size projection, retiring old
    generations.
  - `fsck`, `stats`, `reconcile`, `settings`, `migrate --page-size`.
  - The 800 MB live-size limit, and push webhooks.
  - Gate passed: two rollovers under running writers and readers, with no client errors.
- Phase 8 (bindings and the service):
  - Python (`crates/octopage-python`), Node (`crates/octopage-node`) and C (`crates/octopage-ffi`) bindings;
  - `crates/octopage-server`, the multi-tenant HTTP API behind a GitHub App;
  - `web/octopage-browser`, read-only SQL in the browser on public repositories.
  - Gate passed on the local git server: the same SQL suite through Rust, Python, Node and HTTP; two tenants
    isolated; a browser querying a dataset. Not yet run against github.com.
- Phase 9 (the hosted service), built and tested locally:
  - the dashboard (`web/dashboard`), served by the service;
  - HTTP client SDKs (`sdk/typescript`, `sdk/python`);
  - deployment to Fly.io (`Dockerfile`, `fly.toml`) and release workflows;
  - Not yet deployed. The gate, three pilot users for 30 days, is ahead.

## The `octopage` shell

```powershell
$env:OCTOPAGE_GITHUB_TOKEN = "github_pat_..."
cargo run --release -p octopage-cli -- --repo YOUR_USER/octopage-spike --branch refs/heads/sql --create
```

```text
octopage> CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);
committed 4a349b24e5e9
octopage> INSERT INTO notes(body) VALUES('pages are git blobs'), ('a transaction is a commit');
2 rows changed
committed 6b66f21a1168
octopage> SELECT count(*) FROM notes AS OF '4a349b2';
count(*)
--------
0
(1 row)
octopage> .log 2
```

Each statement outside `BEGIN … COMMIT` is a commit of its own, and `.log` shows the statements each commit
ran. `-c "SQL"` runs one command, SQL piped on stdin runs as a script, and `--memory` gives a scratch
database that never leaves the machine. `.help` lists the dot commands.

- **Encrypted:** `--create --encrypt` makes an encrypted database. Pages, schema and changelogs are unreadable
  in the repository. The passphrase comes from `OCTOPAGE_PASSPHRASE` or a prompt, and the recovery key is
  printed once: store it. `--recovery-key` opens the database with it.
- **Branches:** `CREATE BRANCH feature;`, `MERGE BRANCH feature INTO main;` and `DROP BRANCH feature;`, and
  `.branches` lists them. Open a branch with `--branch refs/heads/feature`.
- **Signed commits:** `--sign-with ~/.ssh/id_ed25519` signs every commit; register the key on GitHub as a
  signing key to see them verified.

## Using the API

```rust
use octopage::{Database, params};

let db = Database::open(octopage::github("OWNER/NAME", Some(&token))?, Default::default()).await?;
tokio::task::spawn_blocking(move || -> octopage::Result<()> {
    let conn = db.connect()?; // SQLite is synchronous: use connections off the async runtime
    conn.transaction(|tx| {
        tx.execute("UPDATE acct SET balance = balance - ?1 WHERE id = ?2", params![10, 1])?;
        tx.execute("UPDATE acct SET balance = balance + ?1 WHERE id = ?2", params![10, 2])?;
        Ok(())
    })?; // run again automatically if another client's commit got in first
    let then = conn.query("SELECT * FROM acct AS OF '2026-09-01'", params![])?;
    Ok(())
});
```

Transactions see one time throughout (`datetime('now')`), and `random()` and `randomblob()` draw from a seed
the changelog records, so a re-run or a replay computes the same values. `conn.run_transaction(&statements)`
runs a list of statements as one transaction, re-running them after a refused commit; `conn.replay(&changelog)`
runs a commit's changelog again. Writers that keep losing races take the advisory writer lease
(`refs/octopage/lease`) and the others wait for them.

The Phase 5 gate on real git, for an hour:
`$env:OCTOPAGE_BANK_SECONDS = "3600"; cargo test --release -p octopage --test bank -- --ignored --nocapture`.

## Maintenance

```powershell
octopage --repo YOUR_USER/octopage-spike fsck        # check every page (and SQLite's integrity_check)
octopage --repo YOUR_USER/octopage-spike stats       # size, growth, days until the size budget
octopage --repo YOUR_USER/octopage-spike settings --retention "keep_days 30" --live-limit-mb 800
octopage --repo YOUR_USER/octopage-spike maintain --rollover never
octopage --repo YOUR_USER/octopage-spike workflows install --cli-source https://github.com/YOUR_USER/octopage
```

- **The workflow.** `workflows install` commits `.github/workflows/octopage-maintenance.yml`. It runs
  `octopage maintain` weekly:
  - removes abandoned staging refs;
  - scans every page;
  - opens an issue two weeks before the repository reaches its size budget;
  - deletes a previous generation after its grace period;
  - rolls over when needed.
- **Its secrets**, both optional:
  - `OCTOPAGE_ADMIN_TOKEN`: a token that may create and delete repositories, for rollovers;
  - `OCTOPAGE_PASSPHRASE`: for full scans of an encrypted database.
- **Rollover.** `octopage rollover` moves every database in the repository to `NAME-g2` (then `-g3`, …),
  keeping only the history each one's retention keeps. Running clients follow by themselves.
  - Clients' tokens must be able to reach the new repository. A fine-grained token limited to selected
    repositories needs it added.
  - The old repository stays for the grace period (7 days by default).
- **Other commands:**
  - `octopage migrate --page-size 16384` moves a database to other pages in the same way; clients then open it
    again.
  - `octopage reconcile` repairs a history someone rewrote by hand.

## Python, Node and C

Each opens `OWNER/NAME` on github.com, the URL of any smart-HTTP git remote, or `:memory:`.

```python
import octopage  # DB-API 2.0; built with maturin from crates/octopage-python

conn = octopage.connect("OWNER/NAME", token, branch="main")
conn.execute("INSERT INTO notes(body) VALUES(?)", ("from Python",))
conn.commit()  # a git commit; conn.head is its id
rows = conn.execute("SELECT * FROM notes").fetchall()
```

```js
const { Database } = require('octopage'); // napi-rs: crates/octopage-node

const db = await Database.open('OWNER/NAME', { token, branch: 'main' });
const conn = await db.connect();
await conn.transaction(async (tx) => {  // runs again if another client's commit got in first
  await tx.execute('UPDATE acct SET balance = balance - 10 WHERE id = 1');
  await tx.execute('UPDATE acct SET balance = balance + 10 WHERE id = 2');
});
const notes = await conn.query('SELECT * FROM notes WHERE id = ?', [1]); // [{ id, body }]
```

- Python: calls release the GIL while the database works. Errors are the PEP 249 classes, plus
  `ConflictError` for a commit another client's beat.
- Node: every call returns a promise and runs on libuv's thread pool. Errors carry a `code` (`conflict`,
  `constraint`, `sql`, `locked`, …).
- C: `crates/octopage-ffi` builds a shared library, with the header generated at
  `crates/octopage-ffi/include/octopage.h` (`octo_open`, `octo_connect`, `octo_execute`, `octo_query`,
  `octo_run_transaction`, …). Every function returns a status; `octo_last_error()` says why.

## The service

`octopage-server` is the hosted API: users sign in with GitHub and install the OctoPage GitHub App on their
repositories, and applications call it with API keys. It serves the dashboard (`web/dashboard`, built with
`npm run build`) when `OCTOPAGE_DASHBOARD` points at it. Fly.io deployment is configured by `Dockerfile` and
`fly.toml`; the SDKs for calling it are in `sdk/typescript` and `sdk/python`.

```powershell
$env:GITHUB_APP_ID = "123456"; $env:GITHUB_APP_PRIVATE_KEY_FILE = "app.pem"
$env:GITHUB_WEBHOOK_SECRET = "..."; $env:GITHUB_CLIENT_ID = "..."; $env:GITHUB_CLIENT_SECRET = "..."
$env:OCTOPAGE_KMS = "local:<64 hex digits>"    # or aws:<region>:<key id>
cargo run --release -p octopage-server
```

```text
POST /v1/databases                  {"repository": "OWNER/NAME", "branch": "main", "create": true}
POST /v1/databases/{id}/query       {"sql": "SELECT * FROM notes WHERE id = ?", "params": [1]}
POST /v1/databases/{id}/execute     one statement, one commit
POST /v1/databases/{id}/batch       statements as one transaction
POST /v1/databases/{id}/transactions, …/{tx}/execute, …/{tx}/commit, …/{tx}/rollback
GET  /v1/databases/{id}/log, branches, POST merge; /v1/keys; /healthz, /readyz, /metrics
```

- **Tokens.** The service reaches repositories with the App's installation tokens, minted when needed. It
  stores no user tokens. API keys are stored hashed.
- **Writes.** Each database has one writer connection, and the service's own writes queue for it instead of
  racing. Queries run on a pool of readers.
- **Keys.** An encrypted database's key is wrapped by the service's key service. Alternatively, the user
  supplies a passphrase per session (`/unlock`).
- **Isolation.** Each repository's pages are cached apart, and each installation's GitHub requests have a
  budget of their own (`OCTOPAGE_BUDGET_PER_HOUR`, 4000 by default).
- **Configuration.** The variables are listed in `crates/octopage-server/src/main.rs`, and the routes in
  `src/api.rs`. Service metadata is kept in SQLite in `OCTOPAGE_DATA`.

## In the browser

`web/octopage-browser` runs read-only SQL on a database in a public repository, with no server. SQLite's
WebAssembly build reads pages from GitHub's raw CDN as queries need them, and caches them in IndexedDB. See
the browser package source for usage details.

## Checking the transport against GitHub

With a fine-grained token that has *Contents: read and write* on a throwaway repo:

```powershell
$env:OCTOPAGE_GITHUB_TOKEN = "github_pat_..."
cargo run -p octopage-git --example github_check -- --repo YOUR_USER/octopage-spike
```

It uses temporary refs, deletes them at the end, and signs in only with that token.

## Trying the key-value store on GitHub

```powershell
$env:OCTOPAGE_GITHUB_TOKEN = "github_pat_..."
cargo run -p octopage-kv -- --repo YOUR_USER/octopage-spike --branch refs/heads/kv init
cargo run -p octopage-kv -- --repo YOUR_USER/octopage-spike --branch refs/heads/kv put greeting hello
cargo run -p octopage-kv -- --repo YOUR_USER/octopage-spike --branch refs/heads/kv get greeting
cargo run -p octopage-kv -- --repo YOUR_USER/octopage-spike --branch refs/heads/kv log
cargo run --release -p octopage-kv -- --repo YOUR_USER/octopage-spike soak   # the Phase 2 gate
```

## Running the SQL soak on GitHub

Three concurrent SQL writers on a temporary branch, then every check of the Phase 3 gate; the branch is deleted
at the end:

```powershell
$env:OCTOPAGE_GITHUB_TOKEN = "github_pat_..."
cargo run --release -p octopage-sqlite --example sql_soak -- --repo YOUR_USER/octopage-spike
```

Locally, the full gate is
`$env:OCTOPAGE_SOAK_PER_WRITER = "334"; cargo test --release -p octopage-sqlite --test soak -- --nocapture`, and
`$env:OCTOPAGE_MODEL_STEPS = "3000"` makes the differential test (`--test model`) run longer.

## Layout

```
crates/            Rust workspace (engine crates are added phase by phase)
  octopage-git/        git objects, packfiles, and the GitHub transport (push, fetch, refs, REST, CDN)
  octopage-pagestore/  pages in git blobs, the fan-out page map, caches, transactions, rebase, staging
  octopage-kv/         a key-value store on the page store, the octopage-kv CLI, and the soak test
  octopage-sqlite/     SQLite on the page store through a VFS: catalog, read-ahead, the SQL soak
  octopage/            the database API: connections, transactions, changelog, AS OF; SQLite's logic tests
  octopage-ops/        maintenance: rollover, retention, integrity scans, hygiene, size, migrate, reconcile
  octopage-cli/        the octopage shell and the maintenance commands
  octopage-ffi/        the C ABI (and its generated header)
  octopage-python/     the Python binding (PyO3, DB-API 2.0)
  octopage-node/       the Node.js binding (napi-rs)
  octopage-server/     the multi-tenant HTTP service: GitHub App, tenants, keys, webhooks, metrics
  octopage-conformance/ the SQL suite every interface runs, and the browser test
  octopage-testkit/    test helpers: a local GitHub-shaped git remote, round-trip counting, fault injection
web/octopage-browser/  read-only SQL in the browser (SQLite's WebAssembly build in a worker)
web/dashboard/     the service's dashboard (React, TypeScript, Vite)
sdk/               HTTP clients for the service: typescript/ and python/
deploy/, Dockerfile, fly.toml   the service's image and its Fly.io deployment
spike/             Phase 0 measurement harness and findings
tools/             Development tools; local_git_remote.py serves a GitHub-shaped git remote for tests
.github/workflows/ ci.yml: format, lint, test, image build; deploy.yml; release.yml
```

## Checks

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python spike/octospike.py selftest
```

The binding, SDK, browser and dashboard tests need Python 3.9+, Node 22+, a C compiler, and Edge or Chrome. They
run `npm install` in `web/octopage-browser`, `web/dashboard` and `sdk/typescript` the first time.
