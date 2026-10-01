use std::future::Future;

use crate::error::{Error, Result};
use crate::object::Object;
use crate::oid::ObjectId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub id: ObjectId,
}

/// One ref change in a push. `old` is the value the ref must hold for the push to apply
/// (`None`: must not exist); `new` is the value to set (`None`: delete).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    pub name: String,
    pub old: Option<ObjectId>,
    pub new: Option<ObjectId>,
}

impl RefUpdate {
    pub fn create(name: impl Into<String>, new: ObjectId) -> Self {
        RefUpdate {
            name: name.into(),
            old: None,
            new: Some(new),
        }
    }

    pub fn update(name: impl Into<String>, old: ObjectId, new: ObjectId) -> Self {
        RefUpdate {
            name: name.into(),
            old: Some(old),
            new: Some(new),
        }
    }

    pub fn delete(name: impl Into<String>, old: ObjectId) -> Self {
        RefUpdate {
            name: name.into(),
            old: Some(old),
            new: None,
        }
    }
}

/// An object to upload. `delta_base`, if given, is an object the remote already has (for trees,
/// usually the previous version at the same path); the transport may send a delta against it.
#[derive(Clone, Debug)]
pub struct NewObject {
    pub object: Object,
    pub delta_base: Option<Object>,
}

impl NewObject {
    pub fn with_delta_base(object: Object, base: Object) -> Self {
        NewObject {
            object,
            delta_base: Some(base),
        }
    }
}

impl From<Object> for NewObject {
    fn from(object: Object) -> Self {
        NewObject {
            object,
            delta_base: None,
        }
    }
}

/// Where OctoPage's objects and refs live. Every operation is one round trip.
pub trait Transport: Send + Sync {
    /// Refs whose names start with one of `prefixes` (all refs if `prefixes` is empty).
    fn list_refs(&self, prefixes: &[&str]) -> impl Future<Output = Result<Vec<Ref>>> + Send;

    /// Fetch objects by id. Every returned object has been hashed, so its bytes match its id.
    /// Blobs and trees come back exactly (a tree without its entries); a requested commit may
    /// bring ancestor commits along, so use `history` to fetch commits.
    /// Fails with `Error::MissingObjects` if any requested object is absent.
    fn fetch(&self, ids: &[ObjectId]) -> impl Future<Output = Result<Vec<Object>>> + Send;

    /// The commits reachable from `tip` but not from `have` (with `have = None`, just `tip`),
    /// in no particular order. With `with_trees`, also every tree those commits reach that `have`
    /// does not: for a new head, exactly the page-map trees that changed, in one round trip; with
    /// `have = None`, the head's whole page map. Never blobs. If `tip` is reachable from `have`
    /// (an older snapshot), the answer is empty.
    ///
    /// This is how a reader catches up with a new head, and how a writer that lost a race learns
    /// what happened between the head it saw and the new one.
    fn history(
        &self,
        tip: ObjectId,
        have: Option<ObjectId>,
        with_trees: bool,
    ) -> impl Future<Output = Result<Vec<Object>>> + Send;

    /// Upload `objects` and apply `updates` atomically: all of them or none, each a
    /// compare-and-swap on its `old` value. A lost race is `Error::Conflict`.
    fn push(
        &self,
        updates: &[RefUpdate],
        objects: &[NewObject],
    ) -> impl Future<Output = Result<()>> + Send;

    /// Whether anyone can read the repository without credentials: `Some(true)` public,
    /// `Some(false)` private, `None` if this transport cannot tell.
    fn is_public(&self) -> impl Future<Output = Result<Option<bool>>> + Send {
        async { Ok(None) }
    }

    /// Where this transport points: `OWNER/NAME` on GitHub, the repository's path on other
    /// hosts, if it can tell.
    fn location(&self) -> Option<String> {
        None
    }

    /// A transport to another repository on the same host, signing in the same way: where a
    /// database that moved to a new generation now lives. `location` is `OWNER/NAME` on GitHub
    /// (a path, as [`Transport::location`] gives it, elsewhere) or a whole URL.
    fn relocate(&self, location: &str) -> Result<Self>
    where
        Self: Sized,
    {
        Err(Error::Invalid(format!(
            "this transport cannot follow a database to {location}"
        )))
    }
}

/// Several owners (for example several page stores in one process) can share one transport.
impl<T: Transport> Transport for std::sync::Arc<T> {
    fn list_refs(&self, prefixes: &[&str]) -> impl Future<Output = Result<Vec<Ref>>> + Send {
        (**self).list_refs(prefixes)
    }

    fn fetch(&self, ids: &[ObjectId]) -> impl Future<Output = Result<Vec<Object>>> + Send {
        (**self).fetch(ids)
    }

    fn history(
        &self,
        tip: ObjectId,
        have: Option<ObjectId>,
        with_trees: bool,
    ) -> impl Future<Output = Result<Vec<Object>>> + Send {
        (**self).history(tip, have, with_trees)
    }

    fn push(
        &self,
        updates: &[RefUpdate],
        objects: &[NewObject],
    ) -> impl Future<Output = Result<()>> + Send {
        (**self).push(updates, objects)
    }

    fn is_public(&self) -> impl Future<Output = Result<Option<bool>>> + Send {
        (**self).is_public()
    }

    fn location(&self) -> Option<String> {
        (**self).location()
    }

    fn relocate(&self, location: &str) -> Result<Self> {
        Ok(std::sync::Arc::new((**self).relocate(location)?))
    }
}
