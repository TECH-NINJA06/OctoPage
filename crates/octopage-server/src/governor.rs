use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use octopage_git::{
    NewObject, Object, ObjectId, Ref, RefUpdate, Result as GitResult, SmartHttp, Transport,
};

/// The pace of one installation's requests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    /// Requests a second, sustained.
    pub per_second: f64,
    /// Requests allowed at once, after a quiet spell.
    pub burst: f64,
}

impl Default for Budget {
    /// About 4,000 requests an hour, with bursts of 60: under GitHub's 5,000 an hour for an
    /// installation, with room for the user's own tools.
    fn default() -> Self {
        Budget {
            per_second: 4000.0 / 3600.0,
            burst: 60.0,
        }
    }
}

/// One installation's token bucket, and its counters.
#[derive(Debug)]
pub struct Governor {
    budget: Budget,
    state: Mutex<(f64, Instant)>,
    /// Requests let through, and how many had to wait.
    pub requests: AtomicU64,
    pub waited: AtomicU64,
}

impl Governor {
    pub fn new(budget: Budget) -> Arc<Self> {
        Arc::new(Governor {
            budget,
            state: Mutex::new((budget.burst, Instant::now())),
            requests: AtomicU64::new(0),
            waited: AtomicU64::new(0),
        })
    }

    pub fn budget(&self) -> Budget {
        self.budget
    }

    /// Take one request's worth, waiting for it if the bucket is empty. `low`: GitHub says the
    /// quota is running out, so go at half pace.
    pub async fn acquire(&self, low: bool) {
        let rate = if low {
            self.budget.per_second / 2.0
        } else {
            self.budget.per_second
        };
        let mut waited = false;
        loop {
            let wait = {
                let mut state = self.state.lock().unwrap();
                let (tokens, last) = &mut *state;
                let now = Instant::now();
                *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * rate)
                    .min(self.budget.burst);
                *last = now;
                if *tokens >= 1.0 {
                    *tokens -= 1.0;
                    None
                } else {
                    Some(Duration::from_secs_f64((1.0 - *tokens) / rate))
                }
            };
            match wait {
                None => break,
                Some(wait) => {
                    waited = true;
                    tokio::time::sleep(wait).await;
                }
            }
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        if waited {
            self.waited.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Whether a transport's quota at GitHub is running out.
pub trait QuotaAware {
    fn quota_low(&self) -> bool {
        false
    }
}

impl QuotaAware for SmartHttp {
    fn quota_low(&self) -> bool {
        self.rate_limit().below(0.2)
    }
}

/// A transport whose every request passes through a governor.
pub struct Governed<T> {
    inner: T,
    governor: Arc<Governor>,
}

impl<T> Governed<T> {
    pub fn new(inner: T, governor: Arc<Governor>) -> Self {
        Governed { inner, governor }
    }

    pub fn governor(&self) -> &Arc<Governor> {
        &self.governor
    }
}

impl<T: Transport + QuotaAware> Governed<T> {
    async fn turn(&self) {
        self.governor.acquire(self.inner.quota_low()).await;
    }
}

impl<T: Transport + QuotaAware> Transport for Governed<T> {
    async fn list_refs(&self, prefixes: &[&str]) -> GitResult<Vec<Ref>> {
        self.turn().await;
        self.inner.list_refs(prefixes).await
    }

    async fn fetch(&self, ids: &[ObjectId]) -> GitResult<Vec<Object>> {
        self.turn().await;
        self.inner.fetch(ids).await
    }

    async fn history(
        &self,
        tip: ObjectId,
        have: Option<ObjectId>,
        with_trees: bool,
    ) -> GitResult<Vec<Object>> {
        self.turn().await;
        self.inner.history(tip, have, with_trees).await
    }

    async fn push(&self, updates: &[RefUpdate], objects: &[NewObject]) -> GitResult<()> {
        self.turn().await;
        self.inner.push(updates, objects).await
    }

    async fn is_public(&self) -> GitResult<Option<bool>> {
        self.turn().await;
        self.inner.is_public().await
    }

    fn location(&self) -> Option<String> {
        self.inner.location()
    }

    /// The same installation, so the same governor.
    fn relocate(&self, location: &str) -> GitResult<Self> {
        Ok(Governed {
            inner: self.inner.relocate(location)?,
            governor: self.governor.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_bucket_paces_requests() {
        let governor = Governor::new(Budget {
            per_second: 10.0,
            burst: 5.0,
        });
        let started = tokio::time::Instant::now();
        for _ in 0..5 {
            governor.acquire(false).await; // the burst
        }
        assert_eq!(started.elapsed(), Duration::ZERO);
        for _ in 0..10 {
            governor.acquire(false).await; // then 10 a second
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(990) && elapsed <= Duration::from_millis(1100),
            "{elapsed:?}"
        );
        assert_eq!(governor.requests.load(Ordering::Relaxed), 15);
        assert_eq!(governor.waited.load(Ordering::Relaxed), 10);
        // Low on quota: half pace.
        let before = tokio::time::Instant::now();
        governor.acquire(true).await;
        assert!(
            before.elapsed() >= Duration::from_millis(190),
            "{:?}",
            before.elapsed()
        );
    }
}
