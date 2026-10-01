use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use octopage_git::Transport;

use crate::{Error, Kv, Op, Result};

#[derive(Clone, Debug, Default)]
pub struct SoakReport {
    pub writers: usize,
    pub transactions: usize,
    /// Commit attempts across all transactions (one per transaction with no contention).
    pub attempts: u64,
    /// Lost races resolved by moving the writes onto the new head.
    pub rebases: u64,
    /// Lost races that required running the statements again.
    pub reexecutions: u64,
    /// Transactions that failed as a whole and were started again by the writer: serialization
    /// errors, throttling, or pushes that did not land.
    pub retries: u64,
    /// Transactions whose push outcome was unknown but which had in fact landed.
    pub late_landings: u64,
    /// Transactions that lost eight races in a row and were started again (included in `retries`).
    pub serialization_failures: u64,
    pub elapsed: Duration,
}

fn item_key(writer: usize, i: usize) -> Vec<u8> {
    format!("item/{writer}/{i:06}").into_bytes()
}

fn is_unknown(e: &Error) -> bool {
    matches!(
        e,
        Error::Store(octopage_pagestore::Error::OutcomeUnknown { .. })
    )
}

fn is_throttled(e: &Error) -> bool {
    matches!(e, Error::Store(octopage_pagestore::Error::Transport(t)) if matches!(**t, octopage_git::Error::RateLimited { .. }))
}

/// Each writer commits `per_writer` transactions of `add count/<w> 1` and
/// `put item/<w>/<i> <i>`; every third also does `add total 1`, a counter all writers fight
/// over. So some races end in a rebase (disjoint pages) and some in re-execution (the counter).
/// Prints progress every `progress` transactions if asked.
pub async fn run<T: Transport + 'static>(
    writers: Vec<Kv<T>>,
    per_writer: usize,
    progress: Option<usize>,
) -> Result<SoakReport> {
    let started = Instant::now();
    let count = writers.len();
    let done = Arc::new(AtomicUsize::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for (w, kv) in writers.into_iter().enumerate() {
        let done = done.clone();
        tasks.spawn(async move {
            let mut report = SoakReport::default();
            for i in 0..per_writer {
                let mut ops = vec![
                    Op::Add(format!("count/{w}").into_bytes(), 1),
                    Op::Put(item_key(w, i), i.to_string().into_bytes()),
                ];
                if i % 3 == 0 {
                    ops.push(Op::Add(b"total".to_vec(), 1));
                }
                let mut failures = 0u32;
                loop {
                    match kv.apply(&ops).await {
                        Ok((_, stats)) => {
                            report.attempts += stats.attempts as u64;
                            report.rebases += stats.rebases as u64;
                            report.reexecutions += stats.reexecutions as u64;
                            break;
                        }
                        Err(e)
                            if matches!(e, Error::Serialization(_))
                                || is_throttled(&e)
                                || is_unknown(&e) =>
                        {
                            if let Error::Serialization(stats) = &e {
                                report.attempts += stats.attempts as u64;
                                report.rebases += stats.rebases as u64;
                                report.reexecutions += stats.reexecutions as u64;
                                report.serialization_failures += 1;
                            }
                            // A push whose outcome stayed unknown may still land late. Never redo
                            // work without checking: this transaction's item says whether it did.
                            if is_unknown(&e) {
                                kv.store().refresh().await?;
                                if kv.get(&item_key(w, i)).await?.is_some() {
                                    report.late_landings += 1;
                                    break;
                                }
                            }
                            report.retries += 1;
                            failures += 1;
                            tokio::time::sleep(Duration::from_millis(
                                20 * failures.min(100) as u64,
                            ))
                            .await;
                        }
                        Err(e) => return Err(e),
                    }
                }
                report.transactions += 1;
                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                if progress.is_some_and(|every| n.is_multiple_of(every)) {
                    eprintln!("  {n} transactions committed");
                }
            }
            Ok::<SoakReport, Error>(report)
        });
    }
    let mut total = SoakReport {
        writers: count,
        ..SoakReport::default()
    };
    while let Some(joined) = tasks.join_next().await {
        let r = joined.map_err(|e| Error::NotKv(format!("writer task failed: {e}")))??;
        total.transactions += r.transactions;
        total.attempts += r.attempts;
        total.rebases += r.rebases;
        total.reexecutions += r.reexecutions;
        total.retries += r.retries;
        total.late_landings += r.late_landings;
        total.serialization_failures += r.serialization_failures;
    }
    total.elapsed = started.elapsed();
    Ok(total)
}

/// Check the database after `run`. `commits_before` is the history length before the soak.
pub async fn verify<T: Transport + 'static>(
    kv: &Kv<T>,
    writers: usize,
    per_writer: usize,
    commits_before: usize,
) -> Result<()> {
    let fail = |msg: String| Err(Error::Verify(msg));
    kv.store().refresh().await?;
    let number = |v: Option<Vec<u8>>| {
        v.and_then(|v| String::from_utf8(v).ok())
            .and_then(|s| s.parse::<usize>().ok())
    };
    let expected = writers * per_writer;
    let contended = writers * per_writer.div_ceil(3);
    let total = number(kv.get(b"total").await?);
    if total != Some(contended) {
        return fail(format!(
            "total is {total:?}, expected {contended}: a write was lost or applied twice"
        ));
    }
    for w in 0..writers {
        let n = number(kv.get(format!("count/{w}").as_bytes()).await?);
        if n != Some(per_writer) {
            return fail(format!("count/{w} is {n:?}, expected {per_writer}"));
        }
    }
    let items: BTreeMap<Vec<u8>, Vec<u8>> = kv.scan(b"item/").await?.into_iter().collect();
    if items.len() != expected {
        return fail(format!("{} items, expected {expected}", items.len()));
    }
    for w in 0..writers {
        for i in 0..per_writer {
            if items.get(&item_key(w, i)) != Some(&i.to_string().into_bytes()) {
                return fail(format!("item {w}/{i} is missing or wrong"));
            }
        }
    }
    let commits = kv.store().log(usize::MAX).await?.len();
    if commits != commits_before + expected {
        return fail(format!(
            "{commits} commits in history, expected {}: one per transaction",
            commits_before + expected
        ));
    }
    Ok(())
}
