use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use octopage_git::{
    Commit, Credentials, HttpConfig, NewObject, Object, ObjectId, RefUpdate, Signature, SmartHttp,
    StaticToken, TokenProvider, Transport, Tree, TreeEntry, pack,
};
use octopage_kv::Kv;
use octopage_pagestore::{Config, PageStore};

fn commit(tree: ObjectId, parent: Option<ObjectId>, message: &str) -> Object {
    let sig = Signature::new("octopage-probe", "probe@example.invalid", 1_790_000_000);
    Commit {
        tree,
        parents: parent.into_iter().collect(),
        author: sig.clone(),
        committer: sig,
        message: format!("{message}\n").into_bytes(),
    }
    .to_object()
    .unwrap()
}

fn tree(entries: &[(String, ObjectId)]) -> Object {
    let mut t = Tree::new();
    for (name, id) in entries {
        t.insert(TreeEntry::blob(name.as_str(), *id)).unwrap();
    }
    t.to_object().unwrap()
}

fn random_blob() -> Object {
    let mut bytes = vec![0u8; 256];
    fastrand::fill(&mut bytes);
    Object::blob(bytes).unwrap()
}

struct Report {
    failed: usize,
}

impl Report {
    fn line<T, E: std::fmt::Debug>(
        &mut self,
        name: &str,
        started: Instant,
        result: &Result<T, E>,
        note: &str,
    ) {
        let ms = started.elapsed().as_millis();
        match result {
            Ok(_) => println!("PASS {name:<52} {ms:>6} ms  {note}"),
            Err(e) => {
                self.failed += 1;
                println!("FAIL {name:<52} {ms:>6} ms  {note}\n     {e:?}");
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(repo) = args
        .iter()
        .position(|a| a == "--repo")
        .and_then(|i| args.get(i + 1))
        .cloned()
    else {
        eprintln!("usage: github_probe --repo OWNER/NAME   (token in OCTOPAGE_GITHUB_TOKEN)");
        std::process::exit(2);
    };
    let token = std::env::var("OCTOPAGE_GITHUB_TOKEN").expect("set OCTOPAGE_GITHUB_TOKEN");
    let owner = repo.split('/').next().unwrap().to_string();
    let user = if token.starts_with("ghs_") {
        "x-access-token".to_string()
    } else {
        owner
    };
    let provider: Arc<dyn TokenProvider> = Arc::new(StaticToken::new(token));
    // `--url` points the probe at another git server, for a dry run.
    let url = args
        .iter()
        .position(|a| a == "--url")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| format!("https://github.com/{repo}.git"));
    let git = |http1_only: bool| {
        let config = HttpConfig {
            http1_only,
            ..HttpConfig::default()
        };
        SmartHttp::new(
            &url,
            Some(Credentials::git(user.clone(), provider.clone())),
            config,
        )
        .unwrap()
    };
    let t = git(false);
    let run = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let prefix = format!("refs/heads/octopage-probe/{run}");
    let mut r = Report { failed: 0 };
    println!("OctoPage push probe against {url}\n");

    // P1: create a ref; 200 small blobs in one tree; every object whole.
    let blobs: Vec<Object> = (0..200).map(|_| random_blob()).collect();
    let entries = |blobs: &[Object]| -> Vec<(String, ObjectId)> {
        blobs
            .iter()
            .enumerate()
            .map(|(i, b)| (format!("{i:03}"), b.id()))
            .collect()
    };
    let t1 = tree(&entries(&blobs));
    let c1 = commit(t1.id(), None, "p1");
    let mut objs: Vec<NewObject> = blobs.iter().cloned().map(NewObject::from).collect();
    objs.push(t1.clone().into());
    objs.push(c1.clone().into());
    let ref1 = format!("{prefix}/p1");
    let s = Instant::now();
    let res = t
        .push_pack(
            &[RefUpdate::create(&ref1, c1.id())],
            &pack::build_whole(&objs),
        )
        .await;
    r.line("P1 create a ref, whole objects", s, &res, "");

    // P2: create a ref whose tree holds the empty blob (the page store's first changelog).
    let empty = Object::blob(Vec::new()).unwrap();
    let t2 = tree(&[("changelog".to_string(), empty.id())]);
    let c2 = commit(t2.id(), None, "p2");
    let objs2: Vec<NewObject> = [empty, t2, c2.clone()]
        .into_iter()
        .map(NewObject::from)
        .collect();
    let s = Instant::now();
    let res = t
        .push_pack(
            &[RefUpdate::create(format!("{prefix}/p2"), c2.id())],
            &pack::build_whole(&objs2),
        )
        .await;
    r.line("P2 create a ref holding the empty blob", s, &res, "");

    // P3: update P1's ref; the changed tree goes as a delta against P1's tree (thin pack).
    let mut blobs3 = blobs.clone();
    blobs3[7] = random_blob();
    let t3 = tree(&entries(&blobs3));
    let c3 = commit(t3.id(), Some(c1.id()), "p3");
    let objs3 = vec![
        NewObject::from(blobs3[7].clone()),
        NewObject::with_delta_base(t3.clone(), t1.clone()),
        NewObject::from(c3.clone()),
    ];
    let (thin, whole) = (pack::build(&objs3), pack::build_whole(&objs3));
    let s = Instant::now();
    let res = t
        .push_pack(&[RefUpdate::update(&ref1, c1.id(), c3.id())], &thin)
        .await;
    let delta_ok = res.is_ok();
    r.line(
        "P3 update, tree as a delta (thin pack)",
        s,
        &res,
        &format!("{} bytes vs {} whole", thin.len(), whole.len()),
    );

    // P4: the same kind of update with every object whole.
    let parent4 = if delta_ok {
        (c3.clone(), t3.clone(), blobs3.clone())
    } else {
        (c1.clone(), t1.clone(), blobs.clone())
    };
    let mut blobs4 = parent4.2.clone();
    blobs4[8] = random_blob();
    let t4 = tree(&entries(&blobs4));
    let c4 = commit(t4.id(), Some(parent4.0.id()), "p4");
    let objs4 = vec![
        NewObject::from(blobs4[8].clone()),
        NewObject::with_delta_base(t4.clone(), parent4.1.clone()),
        NewObject::from(c4.clone()),
    ];
    let s = Instant::now();
    let res = t
        .push_pack(
            &[RefUpdate::update(&ref1, parent4.0.id(), c4.id())],
            &pack::build_whole(&objs4),
        )
        .await;
    let head = if res.is_ok() {
        (c4, t4, blobs4)
    } else {
        parent4
    };
    r.line("P4 update, every object whole", s, &res, "");

    // P5: P3 again over HTTP/1.1 instead of HTTP/2.
    let mut blobs5 = head.2.clone();
    blobs5[9] = random_blob();
    let t5 = tree(&entries(&blobs5));
    let c5 = commit(t5.id(), Some(head.0.id()), "p5");
    let objs5 = vec![
        NewObject::from(blobs5[9].clone()),
        NewObject::with_delta_base(t5, head.1.clone()),
        NewObject::from(c5.clone()),
    ];
    let s = Instant::now();
    let res = git(true)
        .push_pack(
            &[RefUpdate::update(&ref1, head.0.id(), c5.id())],
            &pack::build(&objs5),
        )
        .await;
    r.line("P5 update, tree as a delta, over HTTP/1.1", s, &res, "");

    // P6-P8: the real page store and key-value paths (Transport::push, with its fallback).
    let config = Config {
        branch: format!("{prefix}/db"),
        head_poll: None,
        batch_window: Duration::from_millis(5),
        ..Config::default()
    };
    let s = Instant::now();
    let store = PageStore::create(t.clone(), config.clone(), 4096).await;
    r.line("P6 page store: create a database", s, &store, "");
    if let Ok(store) = store {
        let s = Instant::now();
        let kv = Kv::create(store, 16).await;
        r.line(
            "P7 key-value: init (a commit with tree deltas)",
            s,
            &kv,
            &format!("whole-object resends so far: {}", t.whole_resends()),
        );
        if let Ok(kv) = kv {
            let s = Instant::now();
            let mut result = Ok(());
            for i in 0..5 {
                if let Err(e) = kv.put(format!("k{i}").as_bytes(), b"v").await {
                    result = Err(e);
                    break;
                }
            }
            r.line(
                "P8 key-value: 5 puts",
                s,
                &result,
                &format!("whole-object resends so far: {}", t.whole_resends()),
            );
        }
    }

    // Clean up every probe ref.
    match t.list_refs(&[&format!("{prefix}/")]).await {
        Ok(refs) if !refs.is_empty() => {
            let deletes: Vec<RefUpdate> = refs
                .iter()
                .map(|x| RefUpdate::delete(&x.name, x.id))
                .collect();
            match t.push_pack(&deletes, &[]).await {
                Ok(()) => println!("\ncleaned up {} probe ref(s)", deletes.len()),
                Err(e) => println!("\ncleanup failed ({e:?}); delete refs under {prefix}/ by hand"),
            }
        }
        Ok(_) => println!("\nno probe refs to clean up"),
        Err(e) => println!("\ncould not list probe refs: {e:?}"),
    }
    println!("{} experiment(s) failed", r.failed);
}
