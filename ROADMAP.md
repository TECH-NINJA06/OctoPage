# OctoPage build plan: Phases 0–9

Source: `OctoPage Relational DB Engine on GitHub Block Storage — Specification.pdf` (27 Sep 2026).

The spec's roadmap has 5 phases and 5 gates over about 28 weeks for one full-time engineer. This plan splits
that into 10 phases, keeps every one of the spec's gates, and adds a hosting phase at the end. Week numbers are
estimates for one full-time engineer. A phase does not start until the previous phase's exit gate passes.

## At a glance

| # | Phase | Weeks | Main deliverable | Exit gate |
|---|-------|-------|------------------|-----------|
| 0 | Setup + transport spike | 1–3 | Measured GitHub latency report, repo skeleton, CI | Batch sizes, page size and transport approach fixed from real numbers *(spec gate)* |
| 1 | Git object layer + transport | 4–5 | `octopage-git` crate: objects, packfiles, push/fetch, REST, raw CDN | Push / fetch / CAS round trip works against a local remote and GitHub |
| 2 | Page store | 6–8 | `get` / `put` / `commit(expected_head)`, fan-out trees, caches, KV CLI | 1,000-commit soak from 3 writers, zero lost or duplicated writes *(spec gate)* |
| 3 | Production SQLite VFS | 9–11 | `octopage-sqlite` hardened; torture suite; scan read-ahead; `sqlite_schema` mirrored into `catalog` | The Phase 2 soak, rerun as concurrent SQL with fault injection, loses nothing; `PRAGMA integrity_check` clean |
| 4 | Engine API + shell | 12–13 | `Database` / `Connection` / `Transaction`, automatic retry, changelog, `AS OF`, `octopage` shell | Chosen sqllogictest subset passes on a single connection |
| 5 | Transactions | 14–15 | Changelog re-execution, deterministic `now()` / `random()`, writer lease | sqllogictest subset passes with transactions, plus the bank-transfer test *(spec gate)* |
| 6 | Security, compression, branching | 16–18 | zstd, encryption modes, `CREATE BRANCH` / `MERGE BRANCH` | Encrypted DB passes the Phase 5 suite; no plaintext in the repo |
| 7 | Operations on GitHub Actions | 19–21 | Retention, rollover, integrity scan, `fsck` / `reconcile` / `migrate` | Generation rollover under load with no client errors *(spec gate)* |
| 8 | Bindings + multi-tenant server | 22–25 | Python, Node, C ABI, WASM build, GitHub App auth, multi-tenant HTTP API | Same test suite passes through every binding and the HTTP API; tenants are isolated |
| 9 | Hosted service, docs, pilots, v1 | 26–30 + 30-day pilot | Web dashboard, deployed service, SDKs, published packages | Three pilot users on the hosted service for 30 days *(spec gate)* |

**Product decision (27 Sep 2026):** OctoPage ships as a hosted service. Each user signs in with GitHub and
installs the OctoPage GitHub App on their own repo, so their data and their API usage stay in their own account.
This adds about five weeks to the spec's estimate (phases 8 and 9).

**Engine decision (28 Sep 2026):** the SQL engine is SQLite, running on the page store through a VFS, instead of
the native B+tree and SQL engine the spec assumes (`spike/sqlite_vfs/REPORT.md`). Phases 3–5 were replanned
around it, which saves about 3 weeks and brings all of SQLite's SQL in place of a SQL-92 subset.

## Spec issues to settle before or during the build

These came up while mapping the spec to phases. None of them sinks the design, but each one changes code.

1. **Page 0 conflicts with every commit (Phase 2). Resolved in Phase 2 as proposed.** The superblock holds the epoch and the next free page id,
   and the free-map changes on every allocation, so every commit writes page 0. Two concurrent transactions
   would then always overlap and always take the slow re-execute path. Fix: leave page 0 and free-map pages out
   of the conflict sets and recompute them at rebase time (epoch = parent epoch + 1, allocations redone on H2).
2. **What "copy-on-write" means for B+tree page ids (Phase 3). Resolved by the engine decision.** If every
   modified page gets a fresh page id, the root page id changes on every write. The catalog holds root page ids,
   and the catalog SHA is in every read set, so every pair of transactions would conflict. SQLite already does
   what the fix proposed: it updates B-tree pages in place by page number (git keeps each version as a new blob)
   and allocates new pages only for splits.
3. **Deriving the encryption nonce from (page id, epoch) is unsafe (Phase 6).** Two racing writers can both
   write page X at the same epoch with different bytes, and a rebased retry can do the same. That reuses a
   nonce under one key, which breaks XChaCha20-Poly1305. Fix: use random 192-bit nonces (safe with XChaCha) in
   `random` mode, and in `convergent` mode derive the nonce from a keyed hash of the plaintext.
4. **`GITHUB_TOKEN` cannot create repositories (Phase 7). Resolved in Phase 7 as proposed.** Generation
   rollover creates `<name>-g<n+1>`, but the Actions `GITHUB_TOKEN` is scoped to the one repo it runs in.
   Rollover needs a GitHub App installation token (or a PAT stored as a secret) that can create repos.
   Retention and integrity scans can still use `GITHUB_TOKEN`. The workflow reads the secret
   `OCTOPAGE_ADMIN_TOKEN`, and uses it for git during a rollover too, because `GITHUB_TOKEN` cannot push to the
   new repository either.
5. **Stale epoch in reused pages (Phase 5). Resolved in Phase 2: pages are stamped with the epoch when each commit attempt is built, so a rebased commit's pages carry its own epoch.** On a disjoint rebase the spec rebuilds only the tree path and
   reuses page bytes, so those pages' header epoch names the losing attempt. Decide whether the header epoch is
   advisory, or rewrite it on rebase (which touches every page in the write set).
6. **Library choice for the git protocol. Settled in Phases 0 and 1.** As of 27 Sep 2026, gitoxide still has no
   push client and no promisor (lazy blob) fetch. Phase 1 went further and uses no gitoxide at all. The pieces
   OctoPage would borrow (object encoding, packs, pkt-lines) are small and well specified, gitoxide's 0.x APIs
   change often, and it has no delta encoder, which the one-page-commit wire size depends on. `octopage-git`
   implements the slice it needs, with `sha1-checked` for collision-detecting SHA-1, and is tested against real
   git with strict object checks.
7. **The raw CDN is not the default read path.** Phase 0 measured private-repo raw reads: every first read
   missed the CDN (338 ms) and every repeat hit it (27 ms). But one miss costs as much as a whole 64-page git
   fetch, and a client's repeat reads come from its own local cache anyway. So reads default to batched git
   fetches, and the CDN serves only the public, read-mostly browser build.
8. **The changelog path is circular and grows without bound (Phase 2). Resolved in Phase 2 as proposed.** A commit cannot contain its own SHA in
   `changelog/<commit-sha-prefix>`, and a per-commit entry would grow the `changelog/` tree forever. Fix: one
   root `changelog` blob that each commit replaces. See `spike/REPORT.md` finding 8.
9. **The rollover job has to move the head (Phase 7). Resolved in Phase 7.**
   - The spec's steps: copy the history, write the pointer, and "the job never touches the head". But a writer
     can commit to the old repository after the copy and before clients switch, and that commit would be lost.
   - The job therefore takes the writer lease, catches up, and in one atomic push moves every database's head
     to a commit that adds a `moved` blob and creates `refs/octopage/gen/<n+1>`.
   - That compare-and-swap is the fence: a commit that got in first makes the push fail, and the job copies it
     and tries again.
   - Writers whose push the seal refused see the pointer and carry their transaction over. The gate shows it
     happening with no client errors.

## Tech stack

**Engine (Rust, per the spec)**

| Concern | Choice |
|---------|--------|
| Language / build | Rust stable, Cargo workspace |
| Async runtime / HTTP | `tokio`, `reqwest` with `rustls` (reqwest also compiles to WASM) |
| Git objects, packfiles, protocol | Own implementation in `octopage-git` (spec issue 6): only the narrow slice OctoPage needs |
| Hashing / zlib | `sha1-checked` (collision-detecting SHA-1), `flate2` |
| SQL engine | SQLite 3.53 (bundled through `rusqlite`) on the page store, through OctoPage's VFS (`octopage-sqlite`) |
| Page compression | `zstd` |
| Encryption | `chacha20poly1305` (XChaCha20-Poly1305), `argon2`, `hmac` + `sha2` (convergent nonces), `getrandom` (RustCrypto) |
| Signed commits | `ssh-key` (SSH signatures, as `ssh-keygen -Y sign` makes them) |
| SQL parsing | SQLite's; a small tokenizer in `octopage` recognises OctoPage's own clauses (`AS OF`; later `CREATE BRANCH`, `MERGE BRANCH`) |
| Catalog / changelog format | `serde` + `serde_json` (canonical JSON; the catalog mirrors `sqlite_schema`) |
| CLI / shell | `clap`, `rustyline` |
| Errors / logging | `thiserror`, `tracing` |

**Testing**

| Concern | Choice |
|---------|--------|
| Property tests | `proptest` |
| Differential tests | Random SQL on OctoPage and on SQLite's own in-memory database must agree |
| SQL conformance | SQLite's logic tests (sqllogictest files), with a runner that follows SQLite's reference runner (`md-5` for result hashes) |
| Benchmarks | `criterion` |
| Local git remote | `git http-backend` (no GitHub quota in CI) |

**Bindings**

| Target | Choice |
|--------|--------|
| C ABI | `cbindgen` |
| Python | PyO3 + `maturin` |
| Node | `napi-rs` |
| Browser | SQLite's official WASM build (`@sqlite.org/sqlite-wasm`) in a Web Worker, with the read-only VFS in JavaScript (Phase 8: no Rust WASM build); page cache in IndexedDB |

**Hosted service**

| Concern | Choice |
|---------|--------|
| API server | Rust + `axum`, which embeds the engine directly (no FFI hop) |
| GitHub integration | A GitHub App: "Sign in with GitHub" plus installation tokens for repo access. Phase 8 signs the App's JWTs with `rsa` (RS256) and calls the REST API through `reqwest` |
| Service metadata DB | Postgres (managed: Neon or Supabase), via `sqlx`. Holds users, installations, databases, API keys and usage. It holds no user data. Phase 8 built it on embedded SQLite (`rusqlite`); Postgres comes before a second server instance. |
| Web dashboard | React + TypeScript + Vite; CodeMirror 6 for the SQL editor; the WASM build for in-browser read-only queries |
| Client SDKs for the hosted API | TypeScript (npm) and Python (PyPI) HTTP clients; they also work from serverless apps |
| API server hosting | Fly.io machines, each with a volume for the page cache |
| Dashboard + docs hosting | Cloudflare Pages (static) |
| Secrets | Fly secrets for the App private key and webhook secret; user tokens are minted per request and never stored |
| Data-key storage | Cloud KMS envelope encryption (AWS or GCP), with an optional passphrase mode (see below) |
| Observability | `tracing` logs, Prometheus metrics endpoint, Sentry for errors |
| CI/CD | GitHub Actions: test, release packages, deploy to Fly |
| Billing (later, optional) | Stripe |

## Suggested repository layout

```
octopage/
  Cargo.toml                 # workspace
  crates/
    octopage-git/            # objects, SHA-1, packfiles, pkt-line, smart-HTTP push/fetch, REST, raw CDN
    octopage-pagestore/      # page header, superblock, fan-out trees, caches, transactions, rebase, staging
    octopage-sqlite/         # the SQLite VFS: SQLite pages on the page store, catalog, read-ahead, soak
    octopage-kv/             # Phase 2 key-value store and its CLI (kept as the page store's test harness)
    octopage-testkit/        # local git remote, round-trip counting, fault injection
    octopage/                # public Rust API (Database, Connection, Transaction), changelog, branches + C ABI
    octopage-ops/            # maintenance: rollover, retention, scans, hygiene, size, migrate, reconcile
    octopage-cli/            # the `octopage` shell, and fsck, reconcile, migrate, stats, maintain
    octopage-server/         # multi-tenant HTTP API, GitHub App auth, webhook receiver
    octopage-ffi/            # the C ABI (built in Phase 8; the plan had it inside octopage)
    octopage-python/  octopage-node/   # the bindings (planned as bindings/python, bindings/node)
    octopage-conformance/    # the SQL suite every interface runs, and the browser test
  web/octopage-browser/      # browser read-only build (planned as crates/octopage-wasm)
  sdk/typescript/  sdk/python/   # HTTP clients for the hosted API
  web/dashboard/             # dashboard (React + TypeScript + Vite), served by octopage-server
  spike/                     # Phase 0 throwaway scripts and the latency report
  .github/workflows/         # CI, deploy, release
  deploy/  Dockerfile  fly.toml  # the service's image and its Fly.io deployment
  docs/                      # user and operator docs, legal drafts
```

## Test strategy (applies to every phase)

- **In-memory transport** for unit tests: no network, deterministic.
- **Local GitHub-shaped remote** for protocol tests in CI: `git http-backend` with `http.receivepack=true`,
  `uploadpack.allowFilter=true` and `uploadpack.allowAnySHA1InWant=true`. No quota, fast, reproducible.
- **Real GitHub** only for integration, soak and latency runs, on a nightly schedule against throwaway repos,
  so CI never spends the account's rate limit.
- **Fault injection** from Phase 2 on: kill the client mid-push, drop the push response, delay the ref read,
  return a corrupted blob.
- **Differential testing** from Phase 3 on: random statements on OctoPage and on SQLite's own in-memory
  database must give the same results, with an integrity check available after every step.

---

## Phase 0 — Setup and transport spike (weeks 1–3)

**Status (28 Sep 2026):** measurement gate passed in two regions; findings and decisions are in
`spike/REPORT.md`. The SQLite-VFS experiment is done (`spike/sqlite_vfs/REPORT.md`) and settled the last open
question: OctoPage builds on SQLite through a VFS (see the engine decision above). Still open: the GitHub App
checks (before Phases 7–8) and the trademark check.

**Goal:** learn what GitHub actually does under this workload before writing the engine. The spec allows this
phase to end the project if the numbers are far off.

- Throwaway GitHub setup: 2–3 private test repos and a fine-grained PAT with `contents: write` on those repos only.
- Cargo workspace skeleton, pinned toolchain, CI on GitHub Actions (fmt, clippy, test).
- Spike scripts (throwaway; shell or Python on top of the `git` CLI):
  - push packfiles of 1, 100 and 5,000 random 4 KB blobs with trees, a commit and `--force-with-lease`;
  - fetch 1, 64 and 512 blobs by SHA in one request from a blobless (`--filter=blob:none`) clone;
  - read pages from `raw.githubusercontent.com/<owner>/<repo>/<commit-sha>/pages/AA/BB/CC`, cold and warm;
  - REST `GET /git/ref` and `PATCH /git/refs` latency;
  - race two pushers with the same expected head and confirm the server rejects the stale one (this is the CAS
    the whole design rests on), and measure how long the rejection takes.
- Record p50 and p95 for each, from two regions. Your own machine plus a GitHub Actions runner gives you two.
- Register a development GitHub App and confirm three things: `git push` over HTTPS works with an installation
  token (`x-access-token:<token>`); what rate limit an installation gets; and whether an installation or user
  token can create repos in a personal account (needed for generation rollover).
- Two-day SQLite-VFS experiment, to confirm the spec's choice of a native engine.
- Check gitoxide push support and pick the transport implementation (see spec issue 6).
- Trademark check on the name "OctoPage".

**Exit gate:** a written report in `spike/REPORT.md` that settles the spec's open questions: page size, fetch
batch size, push vs. REST for the head move, raw CDN vs. git fetch for hot pages, and native vs. SQLite VFS.
**Stop and re-plan** if push or fetch latency is far worse than the spec's targets (push 300–900 ms, fetch
batch 150–400 ms), or if GitHub will not serve many blob SHAs in one request.

## Phase 1 — Git object layer and transport (weeks 4–5)

**Status (28 Sep 2026):** built in `crates/octopage-git`, with 53 tests passing against real git and a mock
GitHub. The exit gate's GitHub half waits on `cargo run -p octopage-git --example github_check -- --repo
OWNER/octopage-spike`, run with a personal token. Changes from the plan below:
- `Transport` has four operations: `list_refs`, `fetch`, `push`, and `commits_between(tip, have)`. The last one
  is for the Phase 5 validation step, which needs the commits between the head a writer saw and the new head.
- Fetches use git's `tree:0` filter so a tree or commit arrives alone rather than with everything under it.
- `Rest` and `RawCdn` are separate clients for their narrow jobs rather than `Transport` implementations. REST
  and the CDN cannot fetch arbitrary objects by id in one round trip.
- Pushes ask for side-band messages, because with `atomic` git reports a lost compare-and-swap only as
  "atomic transaction failed". The detail ("is at X but expected Y") arrives on that channel.

**Goal:** the only layer that knows about GitHub, behind one `Transport` interface.

- Blob, tree and commit encoding and hashing (SHA-1 with collision detection, as git does). Object ids are
  stored as raw bytes with a length prefix, so SHA-256 repositories can be added later.
- Packfile writer (undeltified first) and pack reader for fetch responses.
- pkt-line and smart HTTP: receive-pack push carrying the expected old SHA; protocol v2 fetch of blobs by SHA.
- REST client: read ref, compare-and-swap ref update, Git Data API blob/tree/commit (for the browser build).
- Raw CDN reader, addressed by commit SHA only, never by branch name.
- `Transport` trait with `SmartHttp`, `Rest`, `RawCdn` and `InMemory` implementations.
- `TokenProvider` trait instead of a fixed token string. GitHub App installation tokens expire after an hour,
  so a long-running server connection must be able to refresh its token mid-session.
- Short per-address connect timeout (or racing addresses in parallel). Phase 0 found a `raw.githubusercontent.com`
  address that never answers, which stalls a default connect for 21 s.
- Resolve `ofs-delta` and `ref-delta` in fetched packs. GitHub delta-compresses requested blobs against each
  other.
- Read heads with git protocol v2 `ls-refs` (306 ms vs. 347 ms for REST in Phase 0, and no REST quota).
- Move heads with the one-request `git-receive-pack` push, which carries the objects and an exact
  compare-and-swap (Phase 0 confirmed GitHub checks the expected-old SHA). Use its `atomic` capability to move
  the head and delete the staging ref in the same push.
- Request governor: at most 8 requests in flight, exponential back-off with jitter, honour `retry-after`, keep
  a 20% `x-ratelimit-remaining` reserve, and mark the client degraded when tripped.

**Exit gate:** push, fetch and CAS round trips pass against both the local remote and GitHub. A stale-head
push comes back as a typed `Conflict` error, not a generic failure.

## Phase 2 — Page store (weeks 6–8)

**Status (28 Sep 2026):** built as `crates/octopage-pagestore` (the page store), `crates/octopage-kv` (a
key-value store and the `octopage-kv` CLI) and `crates/octopage-testkit` (a shared local git remote and fault
injection). The soak gate passed three times:
- on the in-memory remote: 1,002 transactions from 3 writers, with 181 injected network faults;
- on real git (`git http-backend` with strict object checks): 1,002 transactions, 1,471 attempts, 157 rebases,
  312 re-executions and 57 transactions retried after 8 lost races, with zero lost or duplicated writes.
- on github.com (`octopage-kv --repo OWNER/NAME soak --per-writer 50`, from the development machine,
  28 Sep 2026): 150 transactions from 3 writers in 177 s, with 301 attempts, 97 rebases, 54 re-executions and
  12 transactions retried after 8 lost races. Every write was present exactly once and history held one commit
  per transaction.

Changes and findings:
- Transactions track read and write sets. After a lost race, `rebase` diffs the two page maps. Disjoint
  writes are re-applied on the new head without re-running anything; overlaps are re-executed by the layer above.
- The superblock and free-map are derived at commit time and left out of conflict detection (spec issue 1).
  Two writers allocating the same page id still conflict, because both write that page.
- A lost race costs 2 round trips before the retry: the new commits and their changed trees come in one
  `history` request, then the new superblock.
- **Writer starvation is real (spec edge case).** Under saturation, the writer that just won has everything
  cached and keeps winning; in one probe a writer lost 8 races in a row four times. No write was lost, but
  fairness needs the advisory writer lease (Phase 5). Phase 2 adds a randomized back-off between attempts.
- **github.com words lost races differently from stock git when a push uses `atomic`.** Racing writers
  got a bare "failed" or "missing necessary objects" instead of "cannot lock ref … is at X but expected Y".
  Pushes now use `atomic` only when several refs move together. After any rejection, the page store reads
  the head: if it moved, the rejection was a lost race (rebase); if not, it resends, a bounded number of
  times. A probe run on github.com confirmed that creating refs, delta-compressed trees, HTTP/1.1 and HTTP/2
  all work.
- Git for Windows cannot take concurrent pushes that carry the same object ("unable to migrate objects to
  permanent storage"). This affected only the local test server, which now applies pushes one at a time per
  repo. GitHub has no such limit.

**Goal:** a versioned, transactional page device (the spec's "page store" milestone).

- Page header (32 bytes: magic `OPG1`, type, flags, cell count, page id, free-space offset, epoch), checked on
  every read.
- Superblock at page 0; `catalog` and `changelog/<sha-prefix>` blobs in the root tree.
- Fan-out trees `pages/AA/BB/CC`: resolve page id to blob SHA, and on commit rebuild only the changed paths.
- API: `get(page_id, snapshot)`, `put(page_id, bytes)`, `commit(expected_head) -> NewHead | Conflict(H2)`.
- Free-map bitmap allocator. Settle spec issue 1 here.
- Buffer pool: blob SHA to bytes, 256 MB LRU, with a protected segment for the superblock, catalog and interior
  pages.
- On-disk content-addressed cache (2 GB default) shared across processes, plus cached trees for the last N heads.
- Read coalescer: one fetch every 20 ms or 64 SHAs, whichever comes first.
- Head watcher with a 30-second poll (webhooks come in Phase 7).
- Deterministic commits (timestamp fixed before the push), so after a lost response the client can recognise
  its own commit.
- Staging refs `refs/octopage/stage/<txid>` for write sets over 16 MB or transactions open over 10 minutes.
- `octopage-kv` CLI: `get`, `put`, `scan`, `log`.

**Exit gate (spec):** a soak test of 1,000 commits from 3 concurrent writers with zero lost or duplicated
writes, and the fault-injection cases above all resolving to "head moved" or "head did not move".

## Phase 3 — Production SQLite VFS (weeks 9–11)

These phases replace the spec's native B+tree, SQL front end and executor (the engine decision above). SQLite
brings the B-tree, overflow pages, indexes, parser, planner and executor; OctoPage's work is the VFS that puts
SQLite's pages on the page store, and the transaction layer around it.

**Status (29 Sep 2026): gate passed.** `crates/octopage-sqlite`:
- **The soak gate:** 3 writers, each its own client, ran 1,002 SQL transactions in memory with 196 injected
  network faults, in 9 s:
  - money transfers, inserts (one in ten with overflow pages), deletes, counter bumps, no-op rewrites;
  - 8 schema changes and a `VACUUM`;
  - 61 refused commits were run again and 9 lost races rebased; 50 commits of unknown outcome were checked
    first, and none had landed.

  Afterwards `integrity_check` was clean, no money appeared or vanished, every event was present exactly once
  with the right bytes, the counters added up, and history held one commit per transaction. Sampled old
  commits also read consistently. On real git (`git http-backend`, strict `fsck`), 600 transactions ran in
  5 minutes, with 191 refused, 98 rebased, 10 schema changes and a `VACUUM`. The same checks passed. Four commits
  failed with I/O errors, most likely while the development machine's disk was full; each was checked (none had
  landed) and run again.
- **Differential test:** 12,000 random units, compared with SQLite's own in-memory database after every
  statement, gave identical results. Four variants were run: a tiny page cache (so pages are written before
  the commit and put back on rollback), a large cache, and both with auto-vacuum. The units also covered
  savepoints, `VACUUM`, schema changes, reopening with cold caches, and reading old commits.
- **Read-ahead:** a cold scan of a 20,000-row table took 8 round trips instead of 104; a point lookup still
  takes 3.

Changes and findings:
- **SQLite shrinks the file after its commit.** `VACUUM` and auto-vacuum truncate the file in the second phase
  of SQLite's commit, after the sync that publishes it. So truncations were lost, and `integrity_check` later
  reported pages "never used". The differential test found this. The VFS now takes the final size from SQLite's
  own record in page 1.
- **Writes are held until the commit.** Pages SQLite rewrote with the bytes they already had are left out
  (`UPDATE t SET x = x` publishes nothing), and so is a rollback after SQLite wrote pages early.
- **Conflicts have their own error codes.** A refused commit is `CONFLICT`, safe to run again; a commit whose
  push outcome is unknown is `OUTCOME_UNKNOWN`, so check before running it again. Both are extended I/O
  errors, which make SQLite roll back and drop its cache (`is_conflict`, `is_outcome_unknown`).
- **Settings that would break commits are refused:** `journal_mode` other than `MEMORY`,
  `synchronous=OFF`, `locking_mode=EXCLUSIVE`, `page_size` other than 4096, and `ATTACH` of other files.
- **The `catalog` blob mirrors `sqlite_schema` as JSON.** Every commit that changes the schema rewrites it,
  so schema history reads as diffs in git.
- **Contention follows the data layout.** Rows that every transaction touches, such as a shared counter or a
  table everyone appends to, make concurrent writers collide. Rows on separate pages rebase.
- `examples/sql_soak.rs` runs the same soak against github.com (like `octopage-kv soak` in Phase 2).

**Exit gate:** the Phase 2 soak, rerun as concurrent SQL with fault injection, loses nothing, and
`PRAGMA integrity_check` is clean.

## Phase 4 — Engine API and shell (weeks 12–13)

**Status (29 Sep 2026): gate passed.** `crates/octopage` (the API) and `crates/octopage-cli` (the `octopage`
shell). SQLite's logic tests passed on one OctoPage connection with no failures: 9,221 queries and 2,025
statements, each statement outside a transaction a commit of its own. The subset is 18 files in
`crates/octopage/tests/slt`:
- `select1.test` … `select5.test`, the suite's five general query files (8,884 queries);
- all 12 files in `evidence/` (the SQL language: `IN`, aggregates, triggers, views, indexes, `REPLACE`,
  `UPDATE`, `DROP`, `REINDEX`);
- OctoPage's own `features.test`: transactions, savepoints, UPSERT with `RETURNING`, window functions,
  recursive CTEs, JSON, `STRICT` tables, generated columns, `WITHOUT ROWID`, partial indexes, full-text search
  (FTS5), R-tree, triggers, views and `VACUUM`.

Changes and findings:
- **The API** (`octopage` crate):
  - `Database::create` / `open`, `connect`, `log`, `resolve`, `at`, `catalog`, and `octopage::github(repo, token)`
    for the transport;
  - `Connection::execute` / `query` / `query_value` / `execute_batch` / `transaction`, with results as `Value`s;
  - the `params!` macro.
- **Retries:** a statement outside a transaction is its own transaction, and runs again by itself after a
  refused commit. `transaction(|tx| …)` runs its closure again. After `max_attempts` (8), `Error::Serialization`.
- **Unknown outcomes are settled below the API.** The page store's error now carries the commit, and the VFS
  reads the head up to three times: landed is success, and a head that moved elsewhere means the commit can never
  land, so it is refused and run again. Only a head still at the base after that is `Error::OutcomeUnknown`,
  which is left to the caller.
- **The changelog** is JSON: the statements that changed data, with their parameters, including the savepoint
  structure around them. Statements that failed are left out. `.log` in the shell and `Database::log` show it.
- **`AS OF`** takes a commit id prefix (4+ hex digits) or a UTC time (`'2026-09-01'`, `'2026-09-01 14:30'`), on
  queries. A time means the newest commit made then or before. Resolving reads the commit list (one round trip
  over smart HTTP); the connection keeps the last four past views open.
- **No `sqlparser` dependency:** a small tokenizer finds `AS OF`, splits batches (with SQLite's own
  `sqlite3_complete`, so trigger bodies stay whole) and spots transaction statements.
- **The runner is our own, not the `sqllogictest` crate.** SQLite's files expect each value printed by the
  column type the test declares (`I`, `T`, `R`), which a crate runner does not pass to the database. Ours
  follows SQLite's reference runner, and lets SQLite itself do the conversions.
- **`log` fetches the commits' root trees in batches** instead of one round trip each.
- **Debug builds carry line numbers only.** Full debug info made each test binary about 120 MB and filled
  the development machine's disk.

**Exit gate:** the exact sqllogictest subset above passes on a single connection.

## Phase 5 — Transactions (weeks 14–15)

**Status (29 Sep 2026): gate passed.**
- **SQLite's logic tests pass with transactions on:** all 17 of SQLite's files, run in explicit transactions of
  ten records, so queries also read their transaction's own uncommitted writes. No failures.
- **Bank transfers** (`crates/octopage/tests/bank.rs`): 4 writers move money between 50 accounts, and an auditor
  checks the total inside read-only transactions and at past commits (`AS OF`).
  - Two writers read balances in Rust and write back the values they computed: the lost-update pattern that
    only serializability makes safe.
  - Two send statement lists, which the engine runs again after a refused commit.
  - Every transfer records the balances it read. Afterwards the whole history is replayed, commit by commit,
    on a fresh database:
    - before each transfer, the replayed balances must equal what the writer read;
    - after each commit, the total must hold;
    - at the end, both databases must be identical.

    That checks serializability in commit order.
  - In memory, with 2 ms of latency and 5% of pushes failing, for 60 s:
    - 888 transfers, 251 refused commits run again, 26 rebases;
    - 14 injected rate limits reported as `Unavailable` and retried;
    - 61 leases taken;
    - all 891 commits replayed, and 792 recorded reads confirmed.
  - On real git (the local git server, 4 writers and the auditor) for an hour:
    - 2,443 transfers and 118 declined for lack of funds;
    - 2,671 refused commits run again, 557 rebases, 744 leases taken;
    - one transaction lost 8 races in a row and was retried; the unluckiest needed 13 attempts;
    - no unknown outcomes;
    - 1,707 audits of the total, 341 of them at past commits;
    - all 2,564 commits replayed in order, and 3,036 recorded reads confirmed.

    It passed on the third attempt. The first stopped when the laptop went into standby. The second ran the
    full hour, but a server error ended the audit early, before the check (see `Error::Unavailable` below).

Changes and findings:
- **Deterministic re-execution.** Each transaction has a clock: one time for all its statements (the VFS
  serves SQLite's `now` per thread, `octopage_sqlite::with_clock`) and a seed for `random()` and `randomblob()`,
  which each connection replaces with its own. Both go into the changelog and stay the same across re-runs.
  Each statement draws from its own stream, keyed by its place among the recorded statements, so a failed
  statement (not recorded) cannot shift later values. `Connection::replay` runs a changelog again; replaying a
  history onto a fresh database reproduces it exactly.
- **Statement lists** (`Connection::run_transaction`): the statements run as one transaction and run again on
  the new head after a refused commit, the spec's logical rebase. This is the hosted API's path.
- **The writer lease** (`refs/octopage/lease`, on by default):
  - a writer that loses 3 races in a row takes it; the lease names the holder and an expiry (10 s) and changes
    only by compare-and-swap;
  - cooperating writers wait for a live lease before pushing, and the holder releases it once its commit
    lands;
  - it is read in the same `ls-refs` request as the head, so it costs nothing extra;
  - `Database::take_lease` / `release_lease` let a steady writer hold it on purpose.

  In a contention test (4 writers on one row, 2 ms latency), across four runs the unluckiest transaction needed
  9 to 11 attempts without the lease and 6 to 7 with it.
- **`Error::Unavailable`**: a commit that failed without applying anything (unreachable, rate-limited) is its
  own error, distinct from a refused commit and from an unknown outcome. Network failures, rate limits and
  server errors are `Unavailable` on every path, including `AS OF` and opening branches, which call the page
  store directly. The second hour-long attempt found that path: a server error during an `AS OF` read came out
  as a raw storage error, so the auditor did not know it could retry.
- **The local test server** (`octopage-testkit`) now makes reads wait for a push in progress. On Windows,
  `git http-backend` serving a fetch while a push installed its objects answered "possible repository
  corruption on the remote side". That was the server's race, not a fault in the repository.
- **SNAPSHOT isolation is not offered** (the spec lists it for counters). With SQLite, the pages a transaction
  reads carry B-tree structure, not just data. Checking only the write set could let a transaction write a
  page that another had just freed, and corrupt the tree.

**Exit gate (spec's SQL-engine gate):** the sqllogictest subset passes with transactions on. A bank-transfer test:
N writers move money between accounts for an hour, and the total never changes and every committed history is
serializable.

## Phase 6 — Security, compression and branching (weeks 16–18)

**Status (29 Sep 2026): gate passed.**
- **An encrypted database passes the Phase 5 suite:**
  - all 17 of SQLite's logic-test files, in transactions of ten, with no failures;
  - the bank-transfer test (4 writers, faults, latency), with its full history replayed and checked.
- **No plaintext in the repo:** the test writes rows, parameters, table and column names and SQL that carry
  markers, then scans every object git holds (`git cat-file --batch-all-objects`, decompressed). The
  encrypted repository shows none of them, not even `SQLite format 3`; a plain control repository shows all
  of them.
- **Branch, diverge and merge** replay correctly in both directions, until both branches hold the same rows. A
  merge whose commit fails on the target (a unique key taken there) stops, names the commit and statement, and
  continues from there once fixed.

What was built:
- **A codec in the page store** for page, catalog and changelog blobs:
  - zstd level 3, then XChaCha20-Poly1305, with the magic, flags and blob kind as authenticated data;
  - the superblock stays as it is and says how the rest is stored;
  - the encryption overhead lives in the git blob, not inside the page, so SQLite's reserved bytes stay at 32;
    the planned 72-byte reserve is not needed.
- **Keys.** A random data key, wrapped in the root tree's `keys` blob once per way to open the database:
  - a passphrase (Argon2id, 64 MiB and 3 passes by default);
  - a recovery key, created with the database and returned once, which `Database::create_encrypted` hands back
    and the shell prints;
  - an external key provider (`KeyProvider`, for a KMS).

  The spec put the wrapped key in the superblock; a blob beside it keeps the superblock fixed-size.
- **Nonces (spec issue 3, settled):**
  - `random` (the default): 24 random bytes per blob;
  - `convergent`: a keyed hash of the content, which makes blobs deterministic and shows which are equal.

  Never the page id and epoch. Because every page carries its commit's epoch, convergent mode deduplicates
  little beyond pages that did not change.
- **Compression** is optional for plain databases (`Config::compression`; git compresses objects anyway) and
  always on for encrypted ones.
- **Public repositories.** The transport asks the repository for its refs without credentials:
  - a plain database in a public repository gets a warning, unless it is declared public (`--public`);
  - an encrypted one gets a warning about offline guessing, or a refusal with `--require-private`.
- **Branches** are git refs:
  - `CREATE BRANCH name [FROM branch]`, `DROP BRANCH name` and `MERGE BRANCH name [INTO branch]` in SQL, and
    `Database::create_branch`, `drop_branch`, `branches`, `open_branch` and `Connection::merge_from` in the API;
  - a merge replays the source's commits since the fork, oldest first, one commit each, with their recorded
    time and random seed;
  - each merged commit records its origin, so merges skip what was merged before, in either direction;
  - `merge_from` takes any database, so a fork in another repository merges the same way.
- **Push protection.** When GitHub's secret scanning blocks a push, the page store stops at once (resending
  cannot help) and the VFS says what holds the blocked pages. It walks the schema and every table and index
  from its root, including overflow chains, and reports, for example, "table notes, the row with rowid 52 (a
  long value)" and "the changelog". The test imitates GitHub with a pre-receive hook. An encrypted database
  never trips it.
- **Signed commits.** An SSH key (`SshSigner`, `--sign-with`) signs every commit in a `gpgsig` header, the way
  git does; `git verify-commit` accepts them.
- **A real bug the encrypted gate caught.** zstd's one-shot decompressor reserves its whole size limit up
  front, 256 MB per page read. Under concurrent readers an allocation failed and the process aborted. A
  streaming decoder fixed it.

**Exit gate:** an encrypted database passes the full Phase 5 suite. A scan of the repo finds none of the known
test plaintext. Branch, diverge and merge replays correctly and reports failed preconditions.

## Phase 7 — Operations and maintenance on GitHub Actions (weeks 19–21)

- Workflow templates that the CLI installs into the database repo:
  - retention (`keep_all`, `keep_days`, `keep_count`), never pruning commits younger than the 7-day maximum
    snapshot age;
  - staging-ref cleanup after 24 hours;
  - integrity scan (every page reachable from the head parses);
  - size projection, filing an issue when the limit is under 14 days away.
- Generation rollover: create `<name>-g<n+1>`, push the squashed tree and the retained chain, write
  `refs/octopage/gen/<n+1>`, have clients follow the pointer, and delete the old repo after a 7-day grace period.
  This needs a GitHub App or PAT (spec issue 4).
- Maintenance lease, so an overlapping job exits.
- CLI: `octopage fsck`, `reconcile`, `migrate --page-size`, `stats` (projected days to the size limit).
- Webhook-driven head watcher, keeping the 30-second poll as the fallback.
- Enforce the 800 MB soft limit on live pages.

**Exit gate (spec):** generation rollover while writers and readers are running, with no client errors.

**Status (29 Sep 2026): gate passed.** `crates/octopage-ops` (new), with changes to the page store, the API and
the CLI.

The gate (`crates/octopage-ops/tests/rollover.rs`): 4 writers move money between accounts and a reader checks
the total, now and at past commits (`AS OF`). Meanwhile the repository rolls over twice. Run in memory (2 ms
latency) and on real git (the local git server):
- no client errors;
- every client ends up in the third generation;
- every transfer that reported success is there exactly once, and the total never changed;
- the page scan is clean, and `git fsck --strict` passes on all three repositories.

| Run | Transfers | Audits (at past commits) | Rollover rounds |
|---|---|---|---|
| In memory | 185 | 143 (47) | 2 and 2 |
| Real git | 83 | 114 (38) | 2 and 3 |

In the real-git run's second rollover, a writer committed during the final catch-up. The seal's
compare-and-swap refused, and the job copied that commit and sealed on the next round. In each run one
transaction lost 8 races in a row on shared pages; that is reported as `Serialization` and retried, as in
Phase 5.

What was built:
- **Generations, in the page store.**
  - The root tree gains `settings`, `moved` and any other entries, which are carried from commit to commit.
  - Clients read `refs/octopage/gen/` in the same request as the head. On a pointer they switch transports
    (`Transport::relocate`), and `open` follows pointers too.
  - A transaction begun before the move carries over by comparing page maps, because the new repository knows
    the old commits under other ids.
  - Copied commits carry a `Rewritten-From:` trailer. `AS OF`, merges, and settling a commit whose push outcome
    was unknown all recognise the old id.
  - A commit in flight during the move is checked against the old repository's last head.
  - Creating a database in a repository that has moved on is refused: no client would find it.
- **Rollover** (`octopage_ops::rollover`):
  - copies the history each database's retention keeps as a first-parent chain; trees are kept, so page blobs
    and page-map trees are shared as they are;
  - copying is deterministic, so branches that share history share the copies;
  - other branches and tags are copied exactly;
  - large pushes go up in 32 MB batches under a stage ref;
  - an interrupted rollover resumes into the same repository.
  See spec issue 9 for the seal.
- **Retention** lives in the plain `settings` blob, not in the catalog, because the job holds no keys:
  - `keep_all`, `keep_days N` (every commit of the last N days, plus the newest commit of each older day for a
    year) or `keep_count N`;
  - never dropped: tagged commits, and commits younger than `snapshot_days` (7).
- **The maintenance job** (`octopage maintain`, run weekly by the installed workflow):
  - takes the maintenance lease, or exits if another job holds it;
  - deletes staging refs older than 24 hours;
  - scans every page: with the key, magic, id, type, size, epoch, a free-map that lists exactly the pages that
    exist, plus SQLite's `integrity_check`; without it, the encrypted framing. Problems open an issue;
  - checks history against a checkpoint ref;
  - projects the size (bytes a day from the pages the last 30 days changed, against the host's repository
    size) and opens an issue 14 days before the budget;
  - deletes the previous generation after its grace period, but only if its pointer names this repository;
  - rolls over when the budget is near and some retention drops history.

  The report is Markdown, for the workflow run's summary.
- **`reconcile`**, after a history rewritten by hand:
  - the checkpoint ref keeps the dropped commits reachable, and the job opens an issue;
  - `reconcile` lists what was dropped, then `--restore` replays their changelogs on the head, or `--accept`
    lets them go;
  - a rollover refuses until then.
- **`migrate --page-size`:**
  - the VFS now takes the page size from each database's superblock (4, 8 or 16 KB);
  - the migration copies schema and rows into a new generation that keeps the same keys (`create_like`), so
    the passphrase and recovery key still work;
  - commits made during the copy are replayed from their changelogs, then the old repository is sealed as in a
    rollover;
  - clients follow, then must open the database again: their SQLite connections know the old page size, and
    say so.
- **The live-size limit** (`Settings::live_limit`, 800 MB):
  - applied as SQLite's `max_page_count`, which `PRAGMA max_page_count` cannot raise;
  - a write past it fails with `Error::Full`, and nothing is committed.
- **Webhooks:**
  - `Database::on_webhook` checks `X-Hub-Signature-256` (tested against GitHub's documented example), then
    moves the known head or follows a generation pointer;
  - the 30-second poll stays as the fallback, and now follows moves too.
- **Workflows:** `octopage workflows install` commits `.github/workflows/octopage-maintenance.yml`. On a
  database's branch it goes beside the pages, and later commits keep it.
- **CLI:** `maintain`, `rollover`, `migrate`, `fsck`, `stats`, `reconcile`, `settings`, `workflows`.
- **Errors:** SQLite reports every page-store failure as "disk I/O error". The cause now reaches the caller:
  transient failures as `Unavailable`, others as themselves (for example, a repository that is not found).

Limits and findings:
- **Tokens.** The job's `GITHUB_TOKEN` can neither create a repository nor push to one (spec issue 4), so a
  rollover uses `OCTOPAGE_ADMIN_TOKEN` for git as well.
  - Clients need tokens that reach the new repository: a fine-grained token limited to selected repositories
    needs it added. A GitHub App installed on all repositories (Phase 8) has no such problem.
  - A client that sleeps through the grace period cannot follow, and gets "not found".
- **Copies:**
  - copied commits lose their signatures;
  - `AS OF` a commit id from two generations back no longer resolves;
  - annotated tags are not copied.
- **Race window:** a branch created in the old repository between the job's last listing and its seal is left
  behind, because a push cannot require that no new ref has appeared.
- **Scale:** rollover and migration keep one database's page ids and trees in memory. Neither has been run at
  hundreds of MB.
- **Not tried on github.com yet:** the workflow, the REST calls that create and delete repositories and open
  issues, and a rollover.

## Phase 8 — Bindings and multi-tenant server (weeks 22–25)

- Stable Rust API and a C ABI (via `cbindgen`).
- Python via PyO3/maturin (DB-API style) and Node via napi-rs. Go via cgo on the C ABI can slip to post-v1.
- WASM read-only build via `wasm-bindgen`: reads through the raw CDN and REST, with an IndexedDB page cache.
  github.com's git smart-HTTP endpoint does not allow browser (CORS) requests, so the browser cannot use the git
  protocol. That is why this build is REST-only and rate-capped. SQLite runs as its official WASM build, with
  the VFS in a Web Worker, where blocking on page fetches is allowed.
- Multi-tenant server (`octopage-server`, the spec's sidecar grown into a service):
  - GitHub App auth: users sign in with GitHub; repo access uses installation tokens minted on demand from the
    App's private key. No long-lived user tokens are stored.
  - Tenant model in Postgres: user → installation → repo → databases (branches), plus API keys (stored hashed)
    and usage counters.
  - HTTP JSON API: query, begin, commit, `AS OF`, branch and merge, authenticated by API key.
  - The server is the lease holder, and therefore the single writer, for each database it serves, so concurrent
    API calls queue instead of racing on compare-and-swap.
  - One GitHub App webhook endpoint receives push events for every installed repo and drives the head watcher
    for all tenants. It verifies `X-Hub-Signature-256` and handles installation added/removed events.
  - Isolation: a separate page cache namespace per repo, because a shared content-addressed cache would let one
    tenant's cache hit reveal another tenant's data. Also a separate request governor per installation, so one
    tenant cannot use up another's GitHub quota.
  - Data keys: KMS envelope encryption by default, with an optional passphrase mode (see Hosting).
  - Health and metrics endpoints; the App private key and webhook secret live only in the host's secret store.

**Exit gate:** the same SQL test suite passes through the Rust API, Python, Node and the HTTP API. A two-tenant
test shows no cross-tenant reads, cache hits or quota sharing. The WASM build queries a public dataset in a
browser.

**Status (30 Sep 2026): gate passed on the local git server.** Nothing in Phase 8 has run against github.com
yet (see the limits). New: `crates/octopage-ffi`, `octopage-python`, `octopage-node`, `octopage-server`,
`octopage-conformance`, and `web/octopage-browser`.

The gate:

| Condition | Test | Result |
|---|---|---|
| The same SQL suite through the Rust API, Python, Node and HTTP | `octopage-conformance/tests/rust_api.rs`, and `tests/conformance.rs` in `octopage-python`, `octopage-node` and `octopage-server` | 234 statements and 1,337 queries, 0 failures, through each of the four |
| Two tenants: no cross-tenant reads | `octopage-server/tests/tenants.rs` | Every route answers 404 for the other tenant's database (list, read, write, batch, transactions, log, branches, merge, delete). Adding the other tenant's repository is refused with 403, and the other installation's token cannot open it at the git level |
| … no cross-tenant cache hits | same | Each repository's pages are cached in a directory of their own. With the same bytes in both tenants' pages, a cold read fetches from its own repository and leaves the other cache untouched |
| … no quota sharing | same | Ada's burst of 12 queries waited 48 times for her installation's budget; Bob's 3 queries during the burst took 0.36 s and never waited |
| The browser build queries a public dataset | `octopage-conformance/tests/browser.rs` | Headless Edge, SQLite's WASM build, pages from a local stand-in for the raw CDN (5,000 rows, 72 pages). A lookup by key read 5 pages; a scan read each page once; time travel, errors, the IndexedDB cache (second visit: 0 fetches) and a rolled-over database all work |

The suite is a subset of SQLite's logic tests (the features file, 12 evidence files and `select1`), so that
four interfaces can run it on every test run; the Phase 4 gate ran the full 9,221 queries on the Rust API. The
Rust, Python and Node runs use the in-memory remote; the HTTP run goes through the local git server. Each
binding also has its own tests on real git.

What was built:
- **Rust API.** Every public item is documented (`#![deny(missing_docs)]`), and there is a prelude. Error kinds
  are the ones every binding maps.
- **C ABI** (`octopage-ffi`):
  - a shared library, with a header that `cbindgen` generates;
  - open, connect, execute, query, statement-list transactions, rows and values, and commit ids;
  - every call returns a status (`OCTO_CONFLICT`, `OCTO_LOCKED`, …), and `octo_last_error()` says why;
  - a C program checks it, compiled with the platform's compiler (MSVC `/W4 /WX` here).
- **Python** (`octopage-python`): PyO3 and maturin, abi3 wheels for Python 3.9 and later.
  - DB-API 2.0: the PEP 249 exceptions plus `ConflictError`, implicit transactions, `autocommit`, and
    `executescript`.
  - Calls release the GIL while the database works.
- **Node** (`octopage-node`): napi-rs.
  - Every call returns a promise and runs on libuv's thread pool.
  - `transaction()` runs its function again after a refused commit, one transaction at a time per connection.
  - Errors carry a `code`. Integers beyond 2^53 come back as bigints, and TypeScript types are included.
- **The service** (`octopage-server`, axum):
  - **Auth.** GitHub App JWTs; installation tokens minted on demand and cached until five minutes before they
    expire; "Sign in with GitHub" with sessions. API keys are stored as SHA-256 hashes.
  - **Metadata.** Users, installations, memberships, repositories, databases, keys and usage.
  - **Writes and reads.** One writer connection per database, with a first-come queue; queries run on a
    reader pool.
  - **Transactions.** Interactive transactions hold the writer's turn until they commit, roll back or time
    out. The stateless endpoints refuse `BEGIN`, `COMMIT` and `ROLLBACK`, because on the shared writer they
    would leak a transaction into other callers' turns.
  - **Isolation.** A token-bucket governor per installation, and a cache directory per repository.
  - **Encryption.** Data keys are wrapped by a local key or AWS KMS (SigV4, checked against AWS's documented
    example). Alternatively, a passphrase is given per session.
  - **Operations.** Webhooks for pushes and installation events; Prometheus metrics; health and readiness
    checks.
- **The suite** (`octopage-conformance`): the logic-test files, parsed once and checked as SQLite's reference
  runner checks them (value formatting, `rowsort` and `valuesort`, MD5 hashes above the threshold). Each
  interface implements one small trait.
- **The browser build** (`web/octopage-browser`):
  - SQLite's official WASM build runs in a Web Worker.
  - A JavaScript VFS fetches pages synchronously from the raw CDN by path at a commit, and presents them as
    Rust's VFS does.
  - An IndexedDB cache, time travel with `commit`, and following a `moved` database.

Deviations from the plan:
- **The browser VFS is JavaScript on SQLite's own WASM build**, not Rust through `wasm-bindgen`.
  - The browser only reads: fetch a page, strip its header, present page 1. That does not justify a second
    WASM toolchain.
  - It reads pages from the raw CDN, and makes one REST call per open, for the head.
- **Service metadata is in embedded SQLite, not Postgres.** No database server was used on the development
  machine, and one instance needs none. Postgres is needed before a second instance.
- **The service does not hold the writer lease permanently.** Its own writers queue for one connection, and the
  page store takes the lease under contention as in Phase 5. A lease held for good would lock out the user's
  other clients (the CLI, the maintenance workflow).
- **Go** (cgo on the C ABI) slipped to post-v1, as the plan allowed.

Findings:
- **The browser cannot trust page 1's size field.** The VFS stores page 1 only when something besides SQLite's
  bookkeeping changes (Phase 3), so the stored size is out of date after most commits. The first browser run
  trusted it, and SQLite reported corruption. The file's size comes from the page store's free map instead:
  the last page marked in use below the superblock's high-water mark. That costs one CDN read, and page 1 is
  shown with the right size.
- **A stateless `BEGIN` breaks a shared writer.** Found by running the logic tests over HTTP. Transaction
  control is now refused outside the transaction endpoints, and a rollback safety net runs after every
  stateless call.
- **Implicit transactions and scripts.** Python's `executescript` first commits a transaction the binding
  opened implicitly, as `sqlite3` does, but leaves one opened by an explicit `BEGIN` to the script. Over HTTP,
  `/execute` takes one statement and `/batch` takes several.

Limits:
- **Not tried on github.com:**
  - the App flow (JWTs, installation tokens, OAuth sign-in);
  - webhook deliveries from GitHub;
  - AWS KMS against real AWS;
  - the browser build against `raw.githubusercontent.com` and `api.github.com`.
  The test servers imitate their documented behaviour.
- **One instance only.** The metadata is local SQLite, and the queues and transactions live in memory.
- **The `rsa` crate** is not constant-time for private-key operations (RUSTSEC-2023-0071). The service signs
  only JWTs it composes itself, a few an hour per installation. Still, before production the App key should
  sign through a KMS or a constant-time library.
- **The browser build** reads plain databases in public repositories only.
  - It makes 60 unauthenticated REST calls an hour per address: one per open, none when `commit` is given.
  - Its worker must be served from the page's own origin.
- **The C ABI** is covered by a smoke test, not by the SQL suite.
- **Nothing is published yet** (wheels, npm packages, the C library). That is Phase 9.

## Phase 9 — Hosted service, docs, pilots and v1 (weeks 26–30 plus a 30-day pilot)

- Web dashboard (`web/`):
  - sign in with GitHub, install the App on a repo, create a database;
  - SQL console, table browser, commit history with the changelog of each commit, `AS OF` browsing;
  - branches and merges, API key management, and a repo-size meter with the projected days to the limit.
- HTTP client SDKs for the hosted API (`sdk/typescript`, `sdk/python`), usable from serverless apps.
- Maintenance for hosted users: the dashboard installs the Phase 7 workflows into the user's repo through the
  App. Rollover permissions depend on what Phase 0 found.
- Deploy: API server to Fly.io, dashboard and docs to Cloudflare Pages, Postgres on a managed provider.
- Release workflow: tagged builds publish to crates.io, PyPI and npm with checksums, and deploy the service.
- Docs: quick start, SQL reference, limits and "do not use it for", deletion latency for regulated data, and
  guidance on GitHub's terms of service. Do not market it as free storage.
- Legal basics: terms of service and a privacy policy stating what the service can see (it depends on the
  key-storage mode).
- Three pilot users on the hosted service, for example feature flags, a small CMS and a public dataset.
- Final product name.

**Exit gate (spec, adapted):** three pilot users on the hosted service for 30 days.

**Status (30 Sep 2026): built and tested locally; not deployed; the gate is ahead.** Everything that can be
built and tested on the development machine is done. What remains needs decisions or accounts that are the
owner's, listed at the end.

What was built:
- **The service's additions** (`crates/octopage-server`):
  - **Routes:**
    - `/v1/repositories`, where databases can go;
    - `/v1/usage` per installation and day;
    - size reports (`/stats`: growth over 30 days, repository size, days until the budget), cached for
      five minutes;
    - settings, read and changed as a commit;
    - installing the maintenance workflow through the App;
    - sign-out, and account deletion (`DELETE /v1/me`);
    - `/batch` takes a whole script, and several statements sent to `/execute` answer
      `multiple_statements`.
  - **The dashboard, from the same origin**, so it signs in with an ordinary session cookie:
    - its assets are cached for good, its page never;
    - a content security policy allows only its own scripts;
    - unknown `/v1` paths answer JSON 404s, not the dashboard.
  - **Whoever installs the App can use it at once**: the installation webhook grants the installer
    membership.
- **Security fixes found while building this:**
  - writes made with the session cookie must come from the service's own origin (cross-site request
    forgery);
  - `/auth/github/login?redirect=` accepted any URL, an open redirect: now only paths on the service;
  - `/metrics` would have been public, with every tenant's activity in it: `OCTOPAGE_METRICS_LISTEN` serves it
    on a private address instead;
  - removing the last database on a repository, or uninstalling the App, now deletes the service's page cache
    of it;
  - expired sessions are deleted, not just refused;
  - batches refuse `BEGIN` and `COMMIT` inside them.
- **HTTP client SDKs** (`sdk/typescript`, `sdk/python`):
  - no dependencies;
  - exact values (big integers, blobs, reals, non-finite numbers);
  - stable error codes;
  - `transaction()` runs the work again after a conflict;
  - batches, history, branches, settings, size, keys and maintenance.
- **The dashboard** (`web/dashboard`, React, TypeScript, Vite, CodeMirror 6):
  - sign-in, installing the App, and adding a database (encryption, with the recovery key shown once);
  - a SQL console (scripts run as one transaction);
  - a table browser that reads as of any commit or time;
  - history with each commit's statements;
  - branches and merges;
  - size and settings, with meters for the repository against its budget and live data against its limit;
  - the maintenance workflow, API keys, and usage;
  - light and dark, and phone width.
- **Deployment:**
  - one `Dockerfile` (the server and the dashboard; it runs unprivileged), with `fly.toml` for one machine
    with a volume near GitHub (`iad`);
  - `deploy.yml`, and `release.yml`: native builds, wheels, SDK packages, checksums and a draft release.
    Publishing and deploying are gated behind the `PUBLISH` variable.
- **Documentation** (`docs/`):
  - a quick start, the SQL and HTTP references;
  - limits and what not to use it for, and deleting data (for regulated data);
  - deploying, releasing, and the pilot plan;
  - drafts of the terms of service and privacy policy, for a lawyer.

Tests (all passing):

| Test | What it covers |
|---|---|
| `octopage-server/tests/dashboard.rs` | Every route the dashboard uses, the cookie's origin check, sign-in redirects, stats against a mocked repository size, settings validation, the workflow install (and refusing unsafe CLI sources), script batches, cache deletion, account deletion |
| `octopage-server/tests/sdks.rs` | The TypeScript client's 4 tests and the Python client's 5, against a running service |
| `octopage-server/tests/dashboard_e2e.rs` | The built dashboard in headless Edge (playwright-core), through a mocked GitHub sign-in: 13 steps from creating a database to signing out, in light and dark, at phone width, with no script errors or blocked resources |

Findings:
- **The end-to-end test caught a real bug.** Browsers compile an input's `pattern` with the regular
  expression `v` flag, which needs `/` escaped inside a character class. The branch-name check was silently
  ignored.
- **API tidy-ups before anyone depends on them:**
  - `POST /v1/databases` overwrote the `created` timestamp with a boolean; it is now `new`;
  - `/v1/me` says whether the service holds keys (`kms`), so the dashboard offers only what works.
- **Merges** stop at a commit that fails on the target (earlier ones stay merged); `skipped` counts commits the
  target already had. The docs said otherwise at first.

Deviations from the plan:
- **The service serves the dashboard**, instead of Cloudflare Pages. It is the same origin, so there are no
  cross-site cookies or CORS to get right. The docs are Markdown in `docs/`; a docs site can come later.
- **The metadata stays in SQLite on the volume**, not managed Postgres. The pilot needs one instance; Postgres
  is needed before a second.
- **Nothing is published or deployed.** That needs the owner's accounts and decisions, below.

Not tried:
- **The image build.** The Docker daemon was not running here, and the C: drive is nearly full, so the image
  was not built locally. CI builds it on every push to `main`.
- **The workflows.** The deploy and release workflows parse but have not run: they need the repository on
  GitHub.
- **Everything against github.com and Fly.io.** Everything above ran against the mock GitHub and the local
  git server.

What remains, the owner's to do or decide:
1. The final product name. It sets the package names, the App's name and the domain. "Octo" also echoes
   GitHub's Octocat; check the name with a trademark search.
2. A license.
3. Register the GitHub App, create the Fly app, volume and secrets, and deploy (`docs/deploy.md`).
4. Legal review of the terms and privacy policy.
5. Recruit three pilots and run the 30 days (`docs/pilot.md`).

Post-v1 (from the spec): cross-repo sharding with two-phase commit, Postgres wire protocol in the sidecar,
full-text search, expression indexes, and a GitHub App that installs the maintenance workflows in one click.

---

## Hosting (hosted multi-user service)

Each user's data lives in their own GitHub repo, reached through the OctoPage GitHub App installed on it. The
service stores only metadata; it keeps users' pages in a disposable cache.

```
user's app ──API key──▶ OctoPage API (Fly.io) ──installation token──▶ user's GitHub repo
                            │   ▲                                        │
browser ──▶ dashboard ──────┤   └────── App webhook (push events) ◀─────┘
(served by the API)         ▼
                    SQLite on the volume (users, installations, keys); Postgres before a 2nd instance
```

| What | Where | Cost to start | Notes |
|------|-------|---------------|-------|
| Users' databases | Each user's own GitHub repo | Free (the user's account) | Their data and their GitHub quota, not yours |
| API server | Fly.io machine with a volume | About $5–10/month | Long-running process with a persistent disk for the page cache |
| Service metadata | SQLite on the API server's volume (Phase 9); managed Postgres before a second instance | Included | No user data in it |
| Dashboard | Served by the API server (Phase 9) | Included | Static React build, same origin as the API |
| Docs | Markdown in `docs/`; a static site (Cloudflare Pages) later | Free | |
| Maintenance jobs | GitHub Actions in each user's repo | The user's Actions minutes | Installed by the App |
| Data keys | Cloud KMS | Cents per key per month | Only in KMS mode |
| Libraries + SDKs | crates.io, PyPI, npm | Free | |

**Host the API server in a US region, close to GitHub.** Phase 0 measured the same operations from the
development machine and from a GitHub Actions runner in the US:
- a 20 MB commit took 16.2 s from the development machine and 1.26 s from the runner;
- a one-page commit took 982 ms and 726 ms;
- a 64-page read took 386 ms and 139 ms.

From the US, every spec latency target is met. Pick a US Fly.io region (for example `iad`) and re-run
`spike/octospike.py` from the deployed machine to confirm.

**Avoid serverless for the API server** (Vercel functions, Netlify, Lambda). Without a persistent disk, every
invocation starts with a cold cache (200–600 ms per lookup), and a short-lived function cannot hold the writer
lease or receive webhooks reliably. Users' own apps can be serverless; they call the OctoPage API, which keeps
the cache warm.

**Why a GitHub App and not personal access tokens:**
- users click "Install" instead of pasting tokens;
- installation tokens expire after an hour, so a leak does limited damage;
- each installation has its own rate limit, so no single account carries everyone's traffic;
- one webhook covers every installed repo.

**Who holds the encryption keys. Resolved in Phases 8 and 9: both modes exist, and KMS mode is the default when
the service has a key service (a local master key, or AWS KMS). The privacy policy draft says what each lets the
service see.**
- **KMS mode (recommended default):** the service wraps each database's key with a cloud KMS and can run queries
  on its own. Simple for users, but the service can read their data, and the privacy policy must say so.
- **Passphrase mode (opt-in):** the user supplies the passphrase per session and the service holds the key only
  in memory. The service cannot read data at rest, but background work such as re-encryption cannot run without
  the user present.

**Terms of service.** The App acts only on repos its users installed it on, and the per-installation request
governor keeps each one well under GitHub's limits. Keep the public demo read-only, and never market the service
as free storage.
