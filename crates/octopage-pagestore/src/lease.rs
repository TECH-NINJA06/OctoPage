use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use octopage_git::{Commit, Object, ObjectId, RefUpdate, Transport, Tree};

use crate::error::Result;
use crate::store::{PageStore, now};

/// The lease's ref.
pub const LEASE_REF: &str = "refs/octopage/lease";

/// Who holds the writer lease, and until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// The holder's client id ([`PageStore::client_id`]).
    pub holder: String,
    /// Milliseconds since the Unix epoch.
    pub expires: i64,
    commit: ObjectId,
}

impl Lease {
    /// Whether the lease has not expired yet (by this machine's clock).
    pub fn is_live(&self) -> bool {
        self.expires > unix_millis()
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A page store's side of the lease.
pub(crate) struct LeaseState {
    /// This client's id, as lease holders are named.
    client: String,
    /// The lease as last read (or written) by this client.
    known: Mutex<Option<Lease>>,
    /// Lost races in a row.
    losses: AtomicU32,
    /// This client took the lease itself to get a turn: release it once a commit lands.
    for_fairness: AtomicBool,
    taken: AtomicU64,
    waits: AtomicU64,
}

impl LeaseState {
    /// Forget the lease as read: the database moved, and the new repository has its own.
    pub(crate) fn forget(&self) {
        *self.known.lock().unwrap() = None;
        self.losses.store(0, Ordering::Relaxed);
        self.for_fairness.store(false, Ordering::Relaxed);
    }
}

impl Default for LeaseState {
    fn default() -> Self {
        LeaseState {
            client: format!("{:016x}", fastrand::u64(..)),
            known: Mutex::new(None),
            losses: AtomicU32::new(0),
            for_fairness: AtomicBool::new(false),
            taken: AtomicU64::new(0),
            waits: AtomicU64::new(0),
        }
    }
}

fn parse(id: ObjectId, object: &Object) -> Option<Lease> {
    let commit = Commit::decode(object.data()).ok()?;
    let message = String::from_utf8_lossy(&commit.message);
    let field = |name: &str| {
        message
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
    };
    Some(Lease {
        holder: field("holder:")?.to_string(),
        expires: field("expires:")?.parse().ok()?,
        commit: id,
    })
}

fn not_applied(e: &octopage_git::Error) -> bool {
    matches!(
        e,
        octopage_git::Error::Conflict { .. }
            | octopage_git::Error::Rejected(_)
            | octopage_git::Error::PushOutcomeUnknown(_)
    )
}

impl<T: Transport + 'static> PageStore<T> {
    /// This client's id, as the lease names its holder.
    pub fn client_id(&self) -> &str {
        &self.inner.lease.client
    }

    /// The writer lease as this client last read it.
    pub fn lease(&self) -> Option<Lease> {
        self.inner.lease.known.lock().unwrap().clone()
    }

    /// Whether this client holds a live lease.
    pub fn holds_lease(&self) -> bool {
        self.lease()
            .is_some_and(|l| l.holder == self.inner.lease.client && l.is_live())
    }

    /// How many times this client took the lease, and how many times it waited for another's.
    pub fn lease_counts(&self) -> (u64, u64) {
        let state = &self.inner.lease;
        (
            state.taken.load(Ordering::Relaxed),
            state.waits.load(Ordering::Relaxed),
        )
    }

    /// Record the lease ref as read (`None`: no lease).
    pub(crate) async fn learn_lease(&self, id: Option<ObjectId>) -> Result<()> {
        let known = self.lease();
        let lease = match id {
            None => None,
            Some(id) if known.as_ref().is_some_and(|l| l.commit == id) => known,
            Some(id) => parse(id, &self.object(id).await?),
        };
        *self.inner.lease.known.lock().unwrap() = lease;
        Ok(())
    }

    /// Take (or renew) the lease for `lease_ttl`, unless another client holds a live one.
    /// Returns whether this client holds it now. A steady writer can hold it across many
    /// commits; call [`PageStore::release_lease`] when done.
    pub async fn acquire_lease(&self) -> Result<bool> {
        self.take_lease(false).await
    }

    async fn take_lease(&self, for_fairness: bool) -> Result<bool> {
        let state = &self.inner.lease;
        let known = self.lease();
        if known
            .as_ref()
            .is_some_and(|l| l.holder != state.client && l.is_live())
        {
            return Ok(false);
        }
        let expires = unix_millis() + self.inner.config.lease_ttl.as_millis() as i64;
        let tree = Tree::new().to_object()?; // the empty tree
        let message = format!(
            "octopage lease\n\nholder: {}\nexpires: {expires}",
            state.client
        );
        let commit = self.commit_object(tree.id(), Vec::new(), &message, now())?;
        let update = match &known {
            Some(l) => RefUpdate::update(LEASE_REF, l.commit, commit.id()),
            None => RefUpdate::create(LEASE_REF, commit.id()),
        };
        self.remember(&commit, true);
        let pushed = self
            .transport()
            .push(&[update], &[tree.into(), commit.clone().into()])
            .await;
        match pushed {
            Ok(()) => {
                *state.known.lock().unwrap() = Some(Lease {
                    holder: state.client.clone(),
                    expires,
                    commit: commit.id(),
                });
            }
            // Someone else moved the lease first, or the outcome is unknown: read it.
            Err(e) if not_applied(&e) => {
                self.refresh().await?;
            }
            Err(e) => return Err(e.into()),
        }
        let held = self.holds_lease();
        if held {
            state.taken.fetch_add(1, Ordering::Relaxed);
            state.for_fairness.store(for_fairness, Ordering::Relaxed);
        }
        Ok(held)
    }

    /// Give the lease up, if this client holds it. A failure is harmless: it expires.
    pub async fn release_lease(&self) -> Result<()> {
        let state = &self.inner.lease;
        state.for_fairness.store(false, Ordering::Relaxed);
        let Some(lease) = self.lease().filter(|l| l.holder == state.client) else {
            return Ok(());
        };
        match self
            .transport()
            .push(&[RefUpdate::delete(LEASE_REF, lease.commit)], &[])
            .await
        {
            Ok(()) => {
                *state.known.lock().unwrap() = None;
                Ok(())
            }
            Err(e) if not_applied(&e) => self.refresh().await.map(|_| ()),
            Err(e) => Err(e.into()),
        }
    }

    /// Before pushing a commit: wait while another client holds a live lease, then take the
    /// lease if this client has lost too many races in a row.
    pub(crate) async fn before_commit(&self) -> Result<()> {
        let config = &self.inner.config;
        let Some(after) = config.lease_after_losses else {
            return Ok(());
        };
        let state = &self.inner.lease;
        let started = Instant::now();
        let mut waited = false;
        // The lease cannot outlive its ttl; the extra second covers clock skew.
        let limit = config.lease_ttl + std::time::Duration::from_secs(1);
        while self
            .lease()
            .is_some_and(|l| l.holder != state.client && l.is_live())
            && started.elapsed() < limit
        {
            waited = true;
            tokio::time::sleep(config.lease_poll).await;
            self.refresh().await?;
        }
        if waited {
            state.waits.fetch_add(1, Ordering::Relaxed);
        }
        if state.losses.load(Ordering::Relaxed) >= after
            && !self.holds_lease()
            && let Err(error) = self.take_lease(true).await
        {
            tracing::debug!(%error, "could not take the writer lease");
        }
        Ok(())
    }

    /// After a commit attempt: count lost races, and give back a lease taken for fairness once
    /// a commit lands.
    pub(crate) async fn after_commit(&self, landed: bool) {
        let state = &self.inner.lease;
        if !landed {
            state.losses.fetch_add(1, Ordering::Relaxed);
            return;
        }
        state.losses.store(0, Ordering::Relaxed);
        if state.for_fairness.load(Ordering::Relaxed)
            && let Err(error) = self.release_lease().await
        {
            tracing::debug!(%error, "could not release the writer lease");
        }
    }
}
