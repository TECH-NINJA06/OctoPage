//! Phase 1 exit gate: run the transport against a real GitHub repository.
//!
//! ```text
//! $env:OCTOPAGE_GITHUB_TOKEN = "github_pat_..."
//! cargo run -p octopage-git --example github_check -- --repo OWNER/NAME
//! ```
//!
//! Uses only `refs/heads/octopage-check/<run>` and `refs/octopage/check-<run>/...` and deletes them
//! at the end. Authenticates only with the given token (never with saved git credentials).
//!
//! `--git-url URL` points the git checks at another smart-HTTP remote (such as
//! `tools/local_git_remote.py`) and skips the REST and CDN checks, which need github.com.

#[path = "../tests/support/fanout.rs"]
mod fanout;

use std::collections::HashSet;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fanout::{Snapshot, page_path, random_page};
use octopage_git::{
    Credentials, Error, HttpConfig, Object, ObjectId, RawCdn, RefUpdate, Rest, SmartHttp,
    StaticToken, TokenProvider, Transport, pack,
};

struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, started: Instant, detail: impl std::fmt::Display) {
        let ms = started.elapsed().as_millis();
        println!(
            "{} {name:<50} {ms:>6} ms  {detail}",
            if ok { "PASS" } else { "FAIL" }
        );
        if ok {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
    }
}

struct Ctx {
    t: SmartHttp,
    t2: SmartHttp,
    rest: Option<Rest>,
    cdn: Option<RawCdn>,
    head: String,
    stage: String,
    lease: String,
}

fn pages(ids: impl IntoIterator<Item = u32>) -> Vec<(String, Object)> {
    ids.into_iter()
        .map(|p| (page_path(p), random_page()))
        .collect()
}

fn is_conflict<T>(r: &Result<T, Error>) -> bool {
    matches!(r, Err(Error::Conflict { .. }))
}

fn fail(e: Error) -> String {
    format!("cannot continue: {e}")
}

async fn checks(c: &Ctx, r: &mut Report) -> Result<(), String> {
    let s = Instant::now();
    let (a, objects) = Snapshot::default().next(&pages(1..=100), "seed");
    let pushed =
        c.t.push(&[RefUpdate::create(&c.head, a.id())], &objects)
            .await;
    r.check(
        "push: create a ref with 100 pages",
        pushed.is_ok(),
        s,
        format!("{pushed:?}"),
    );
    pushed.map_err(fail)?;

    let s = Instant::now();
    let refs = c.t.list_refs(&[&c.head]).await.map_err(fail)?;
    r.check(
        "ls-refs: reads the head",
        refs.len() == 1 && refs[0].id == a.id(),
        s,
        "",
    );

    let s = Instant::now();
    let want: Vec<ObjectId> = a.files.values().take(64).map(Object::id).collect();
    let got = c.t.fetch(&want).await.map_err(fail)?;
    let ok = got.iter().map(Object::id).collect::<HashSet<_>>() == want.iter().copied().collect();
    r.check(
        "fetch: 64 pages by id, each verified",
        ok,
        s,
        format!("{} objects", got.len()),
    );

    let s = Instant::now();
    let root = a.trees[""].id();
    let got = c.t.fetch(&[root]).await.map_err(fail)?;
    let ok = got.len() == 1 && got[0].id() == root;
    r.check(
        "fetch: a tree alone (tree:0 filter)",
        ok,
        s,
        format!("{} objects", got.len()),
    );

    let s = Instant::now();
    let (b, objects) = a.next(&pages([7]), "b");
    let size = pack::build(&objects).len();
    let pushed =
        c.t.push(&[RefUpdate::update(&c.head, a.id(), b.id())], &objects)
            .await;
    r.check(
        "push: one-page commit with tree deltas",
        pushed.is_ok(),
        s,
        format!("{size} bytes, {pushed:?}"),
    );
    pushed.map_err(fail)?;

    let s = Instant::now();
    let between =
        c.t.history(b.id(), Some(a.id()), false)
            .await
            .map_err(fail)?;
    let tip = c.t.history(b.id(), None, false).await.map_err(fail)?;
    let ok =
        between.len() == 1 && between[0].id() == b.id() && tip.len() == 1 && tip[0].id() == b.id();
    r.check(
        "fetch: commits between two heads",
        ok,
        s,
        format!("{} and {} commits", between.len(), tip.len()),
    );

    let s = Instant::now();
    let (stale, objects) = a.next(&pages([8]), "stale");
    let result =
        c.t.push(&[RefUpdate::update(&c.head, a.id(), stale.id())], &objects)
            .await;
    r.check(
        "cas: a stale push is a Conflict",
        is_conflict(&result),
        s,
        format!("{result:?}"),
    );

    let s = Instant::now();
    let (x, ox) = b.next(&pages([9]), "x");
    let (y, oy) = b.next(&pages([10]), "y");
    let ux = [RefUpdate::update(&c.head, b.id(), x.id())];
    let uy = [RefUpdate::update(&c.head, b.id(), y.id())];
    let (rx, ry) = tokio::join!(c.t.push(&ux, &ox), c.t2.push(&uy, &oy));
    let winner = match (&rx, &ry) {
        (Ok(()), Err(Error::Conflict { .. })) => Some(x),
        (Err(Error::Conflict { .. }), Ok(())) => Some(y),
        _ => None,
    };
    r.check(
        "cas: two racing writers, exactly one wins",
        winner.is_some(),
        s,
        format!("{rx:?} / {ry:?}"),
    );
    let current = winner.ok_or("cannot continue after a failed race")?;

    let s = Instant::now();
    let (w, objects) = current.next(&pages([11]), "w");
    let result =
        c.t.push(&[RefUpdate::update(&c.head, a.id(), w.id())], &objects)
            .await;
    r.check(
        "cas: wrong expected value, even for a fast-forward",
        is_conflict(&result),
        s,
        format!("{result:?}"),
    );

    let s = Instant::now();
    let (staged, objects) = current.next(&pages([12, 13, 14]), "staged");
    let first =
        c.t.push(&[RefUpdate::create(&c.stage, staged.id())], &objects)
            .await;
    let commit = [
        RefUpdate::update(&c.head, current.id(), staged.id()),
        RefUpdate::delete(&c.stage, staged.id()),
    ];
    let second = c.t.push(&commit, &[]).await;
    let refs = c.t.list_refs(&[&c.head, &c.stage]).await.map_err(fail)?;
    let ok = first.is_ok() && second.is_ok() && refs.len() == 1 && refs[0].id == staged.id();
    r.check(
        "atomic: move head and drop staging ref together",
        ok,
        s,
        format!("{first:?} / {second:?}"),
    );
    let current = staged;

    let s = Instant::now();
    let (z, objects) = current.next(&pages([15]), "z");
    let both = [
        RefUpdate::update(&c.head, a.id(), z.id()),
        RefUpdate::create(&c.lease, z.id()),
    ];
    let result = c.t.push(&both, &objects).await;
    let lease = c.t.list_refs(&[&c.lease]).await.map_err(fail)?;
    let ok = is_conflict(&result) && lease.is_empty();
    r.check(
        "atomic: one stale ref fails the whole push",
        ok,
        s,
        format!("{result:?}"),
    );

    if let (Some(rest), Some(cdn)) = (&c.rest, &c.cdn) {
        let s = Instant::now();
        let via_rest = rest.get_ref(&c.head).await;
        let ok = matches!(via_rest, Ok(Some(id)) if id == current.id());
        r.check("rest: reads the head", ok, s, format!("{via_rest:?}"));

        let s = Instant::now();
        let result = rest.update_ref(&c.head, a.id(), false).await;
        r.check(
            "rest: a non-fast-forward move is a Conflict",
            is_conflict(&result),
            s,
            format!("{result:?}"),
        );

        let s = Instant::now();
        let (path, blob) = current.files.iter().next().unwrap();
        let got = cdn.get(current.id(), path, blob.id()).await;
        let ok = matches!(&got, Ok(o) if o == blob);
        r.check(
            "cdn: page by commit id, verified",
            ok,
            s,
            format!("{:?}", got.map(|o| o.id())),
        );
    } else {
        println!("SKIP rest and cdn checks (not github.com)");
    }

    let s = Instant::now();
    let ghost = ObjectId::from_array([0x42; 20]);
    let result = c.t.fetch(&[ghost]).await;
    let ok = matches!(&result, Err(Error::MissingObjects(ids)) if ids == &vec![ghost]);
    r.check(
        "fetch: a missing object is named",
        ok,
        s,
        format!("{result:?}"),
    );
    Ok(())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let Some(repo) = arg(&args, "--repo") else {
        eprintln!(
            "usage: github_check --repo OWNER/NAME [--git-url URL]   (token in OCTOPAGE_GITHUB_TOKEN)"
        );
        return ExitCode::FAILURE;
    };
    let Ok(token) =
        std::env::var("OCTOPAGE_GITHUB_TOKEN").or_else(|_| std::env::var("OCTOSPIKE_TOKEN"))
    else {
        eprintln!(
            "Set OCTOPAGE_GITHUB_TOKEN to a fine-grained token with Contents: read and write on {repo}."
        );
        return ExitCode::FAILURE;
    };
    // Personal access tokens log in as the repo owner; Actions and App tokens as x-access-token.
    let owner = repo.split('/').next().unwrap_or_default().to_string();
    let git_user = if token.starts_with("ghs_") {
        "x-access-token".to_string()
    } else {
        owner
    };
    let provider: Arc<dyn TokenProvider> = Arc::new(StaticToken::new(token));
    let git_url = arg(&args, "--git-url");
    let on_github = git_url.is_none();
    let url = git_url.unwrap_or_else(|| format!("https://github.com/{repo}.git"));
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let build = || -> octopage_git::Result<Ctx> {
        let git = || {
            SmartHttp::new(
                &url,
                Some(Credentials::git(git_user.clone(), provider.clone())),
                HttpConfig::default(),
            )
        };
        let rest = || {
            Rest::new(
                &repo,
                Credentials::bearer(provider.clone()),
                HttpConfig::default(),
            )
        };
        let cdn = || {
            RawCdn::new(
                &repo,
                Some(Credentials::raw(provider.clone())),
                HttpConfig::default(),
            )
        };
        Ok(Ctx {
            t: git()?,
            t2: git()?,
            rest: on_github.then(rest).transpose()?,
            cdn: on_github.then(cdn).transpose()?,
            head: format!("refs/heads/octopage-check/{run_id}"),
            stage: format!("refs/octopage/check-{run_id}/stage"),
            lease: format!("refs/octopage/check-{run_id}/lease"),
        })
    };
    let ctx = match build() {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("setup failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("OctoPage Phase 1 check against {url}\n");
    let mut report = Report {
        passed: 0,
        failed: 0,
    };
    if let Err(stopped) = checks(&ctx, &mut report).await {
        println!("STOP {stopped}");
        report.failed += 1;
    }

    let prefix = format!("refs/octopage/check-{run_id}/");
    match ctx.t.list_refs(&[&ctx.head, &prefix]).await {
        Ok(refs) if !refs.is_empty() => {
            let deletes: Vec<RefUpdate> = refs
                .iter()
                .map(|r| RefUpdate::delete(&r.name, r.id))
                .collect();
            match ctx.t.push(&deletes, &[]).await {
                Ok(()) => println!("\ncleaned up {} ref(s)", deletes.len()),
                Err(e) => println!("\ncleanup failed ({e}); delete {} by hand", ctx.head),
            }
        }
        Ok(_) => {}
        Err(e) => println!("\ncould not list refs for cleanup: {e}"),
    }
    if let Some(rest) = &ctx.rest {
        let quota = rest.rate_limit();
        println!(
            "REST quota: {:?} of {:?} left",
            quota.remaining, quota.limit
        );
    }
    println!("\n{} passed, {} failed", report.passed, report.failed);
    if report.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
