use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use octopage_git::{Object, ObjectId, Transport};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::store::Current;

type Reply = oneshot::Sender<Result<Object>>;
type InFlight = Arc<Mutex<HashMap<ObjectId, Vec<Reply>>>>;

pub(crate) struct Fetcher {
    tx: mpsc::UnboundedSender<(ObjectId, Reply)>,
}

fn stopped() -> Error {
    Error::Invalid("the page store's fetcher has stopped".into())
}

impl Fetcher {
    /// Start the coalescing task. It stops when the `Fetcher` is dropped.
    pub fn spawn<T: Transport + 'static>(
        transport: Arc<Current<T>>,
        window: Duration,
        max_batch: usize,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(transport, rx, window, max_batch.max(1)));
        Fetcher { tx }
    }

    pub async fn get(&self, id: ObjectId) -> Result<Object> {
        let (reply, wait) = oneshot::channel();
        self.tx.send((id, reply)).map_err(|_| stopped())?;
        wait.await.map_err(|_| stopped())?
    }

    /// Fetch several objects; they join the same batch (or batches, beyond `max_batch`).
    pub async fn get_many(&self, ids: &[ObjectId]) -> Result<Vec<Object>> {
        let mut waits = Vec::with_capacity(ids.len());
        for &id in ids {
            let (reply, wait) = oneshot::channel();
            self.tx.send((id, reply)).map_err(|_| stopped())?;
            waits.push(wait);
        }
        let mut out = Vec::with_capacity(ids.len());
        for wait in waits {
            out.push(wait.await.map_err(|_| stopped())??);
        }
        Ok(out)
    }
}

/// Queue `id` for the current batch, or attach to a fetch already in flight.
fn enqueue(inflight: &InFlight, batch: &mut Vec<ObjectId>, id: ObjectId, reply: Reply) {
    let mut waiting = inflight.lock().unwrap();
    match waiting.get_mut(&id) {
        Some(replies) => replies.push(reply),
        None => {
            waiting.insert(id, vec![reply]);
            batch.push(id);
        }
    }
}

async fn run<T: Transport + 'static>(
    transport: Arc<Current<T>>,
    mut rx: mpsc::UnboundedReceiver<(ObjectId, Reply)>,
    window: Duration,
    max_batch: usize,
) {
    let inflight: InFlight = Arc::default();
    while let Some((id, reply)) = rx.recv().await {
        let mut batch = Vec::new();
        enqueue(&inflight, &mut batch, id, reply);
        let deadline = tokio::time::Instant::now() + window;
        while batch.len() < max_batch {
            tokio::select! {
                next = rx.recv() => match next {
                    Some((id, reply)) => enqueue(&inflight, &mut batch, id, reply),
                    None => break,
                },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        if batch.is_empty() {
            continue; // everything asked for was already on its way
        }
        let transport = transport.get(); // the repository the database is in now
        let inflight = inflight.clone();
        tokio::spawn(async move {
            let result = transport.fetch(&batch).await;
            let mut waiting = inflight.lock().unwrap();
            match result {
                Ok(objects) => {
                    let by_id: HashMap<ObjectId, Object> =
                        objects.into_iter().map(|o| (o.id(), o)).collect();
                    for id in batch {
                        for reply in waiting.remove(&id).unwrap_or_default() {
                            let answer = by_id.get(&id).cloned().ok_or_else(|| {
                                Error::from(octopage_git::Error::MissingObjects(vec![id]))
                            });
                            let _ = reply.send(answer);
                        }
                    }
                }
                Err(e) => {
                    let e = Error::from(e);
                    for id in batch {
                        for reply in waiting.remove(&id).unwrap_or_default() {
                            let _ = reply.send(Err(e.clone()));
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octopage_git::{InMemory, NewObject, Ref, RefUpdate};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts fetch calls and the ids in each.
    struct Counting {
        inner: InMemory,
        calls: AtomicUsize,
        largest: AtomicUsize,
    }

    impl Transport for Counting {
        async fn list_refs(&self, prefixes: &[&str]) -> octopage_git::Result<Vec<Ref>> {
            self.inner.list_refs(prefixes).await
        }
        async fn fetch(&self, ids: &[ObjectId]) -> octopage_git::Result<Vec<Object>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.largest.fetch_max(ids.len(), Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await; // a network round trip
            self.inner.fetch(ids).await
        }
        async fn history(
            &self,
            tip: ObjectId,
            have: Option<ObjectId>,
            with_trees: bool,
        ) -> octopage_git::Result<Vec<Object>> {
            self.inner.history(tip, have, with_trees).await
        }
        async fn push(
            &self,
            updates: &[RefUpdate],
            objects: &[NewObject],
        ) -> octopage_git::Result<()> {
            self.inner.push(updates, objects).await
        }
    }

    #[tokio::test]
    async fn concurrent_reads_share_round_trips() {
        let inner = InMemory::new();
        let blobs: Vec<Object> = (0..100u8)
            .map(|i| Object::blob(vec![i; 64]).unwrap())
            .collect();
        // Store the blobs on the fake remote through a ref-less push of a tree holding them.
        let mut tree = octopage_git::Tree::new();
        for (i, b) in blobs.iter().enumerate() {
            tree.insert(octopage_git::TreeEntry::blob(format!("{i:03}"), b.id()))
                .unwrap();
        }
        let tree = tree.to_object().unwrap();
        let mut objects: Vec<NewObject> = blobs.iter().cloned().map(NewObject::from).collect();
        objects.push(tree.clone().into());
        inner
            .push(&[RefUpdate::create("refs/heads/t", tree.id())], &objects)
            .await
            .unwrap();

        let transport = Arc::new(Counting {
            inner,
            calls: AtomicUsize::new(0),
            largest: AtomicUsize::new(0),
        });
        let fetcher = Fetcher::spawn(
            Arc::new(Current::new(transport.clone())),
            Duration::from_millis(20),
            64,
        );
        // 200 requests for 100 distinct ids, each id asked for twice.
        let ids: Vec<ObjectId> = blobs.iter().chain(blobs.iter()).map(Object::id).collect();
        let got = fetcher.get_many(&ids).await.unwrap();
        assert_eq!(got.iter().map(Object::id).collect::<Vec<_>>(), ids);
        // Two batches (64 + 36); the repeats joined fetches already in flight.
        assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
        assert_eq!(transport.largest.load(Ordering::SeqCst), 64);

        let missing = fetcher.get(ObjectId::from_array([1; 20])).await;
        assert!(matches!(missing, Err(Error::Transport(_))));
    }
}
