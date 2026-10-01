use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::{Database, Error, ObjectId, Result, Transport};

/// Whether `signature` (the `X-Hub-Signature-256` header, `sha256=<hex>`) is GitHub's
/// HMAC-SHA256 of `body` with the webhook's `secret`. Compared in constant time.
pub fn verify_signature(secret: &[u8], body: &[u8], signature: &str) -> bool {
    let Some(hex) = signature.trim().strip_prefix("sha256=") else {
        return false;
    };
    let Some(expected) = decode_hex(hex) else {
        return false;
    };
    let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// What a push event says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushEvent {
    /// The ref that moved, such as `refs/heads/main`.
    pub git_ref: String,
    /// Where it was before (all zeros: it was created).
    pub before: ObjectId,
    /// Where it moved to (all zeros: it was deleted).
    pub after: ObjectId,
    /// The repository, `OWNER/NAME`.
    pub repository: String,
}

impl PushEvent {
    /// Parse a push event's body (the delivery's `X-GitHub-Event` is `push`).
    pub fn parse(body: &[u8]) -> Result<PushEvent> {
        #[derive(Deserialize)]
        struct Repository {
            full_name: String,
        }
        #[derive(Deserialize)]
        struct Push {
            #[serde(rename = "ref")]
            git_ref: String,
            before: String,
            after: String,
            repository: Repository,
        }
        let push: Push = serde_json::from_slice(body)
            .map_err(|e| Error::Invalid(format!("not a push event: {e}")))?;
        let id = |hex: &str| {
            ObjectId::from_hex(hex).map_err(|e| Error::Invalid(format!("a push event: {e}")))
        };
        Ok(PushEvent {
            before: id(&push.before)?,
            after: id(&push.after)?,
            git_ref: push.git_ref,
            repository: push.repository.full_name,
        })
    }
}

impl<T: Transport + 'static> Database<T> {
    /// Handle a push webhook delivery: `body` as received, `signature` its
    /// `X-Hub-Signature-256` header, `secret` the webhook's secret. Returns whether it
    /// concerned this database. A move of its branch updates the known head at once; a
    /// generation pointer (the database moved to a new repository) makes it follow.
    pub async fn on_webhook(&self, secret: &[u8], body: &[u8], signature: &str) -> Result<bool> {
        if !verify_signature(secret, body, signature) {
            return Err(Error::Invalid(
                "the webhook delivery's signature does not match".into(),
            ));
        }
        let event = PushEvent::parse(body)?;
        if self
            .location()
            .is_some_and(|here| !here.eq_ignore_ascii_case(&event.repository))
        {
            return Ok(false);
        }
        if event.git_ref.starts_with(octopage_pagestore::GEN_PREFIX) {
            self.refresh().await?;
            return Ok(true);
        }
        if event.git_ref != self.config.store.branch {
            return Ok(false);
        }
        if event.after.is_zero() {
            self.refresh().await?; // the branch went away: find out properly
        } else {
            self.store().notify_head(event.after);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        // GitHub's documented example: secret "It's a Secret to Everybody", body "Hello, World!".
        let secret = b"It's a Secret to Everybody";
        let body = b"Hello, World!";
        let good = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(verify_signature(secret, body, good));
        assert!(!verify_signature(secret, b"Hello, World?", good));
        assert!(!verify_signature(b"another secret", body, good));
        assert!(!verify_signature(secret, body, "sha1=757107ea"));
        assert!(!verify_signature(secret, body, "sha256=zz"));
    }

    #[test]
    fn push_events() {
        let body =
            br#"{"ref":"refs/heads/main","before":"0000000000000000000000000000000000000000",
            "after":"a94a8fe5ccb19ba61c4c0873d391e987982fbbd3","repository":{"full_name":"o/db"},
            "pusher":{"name":"x"}}"#;
        let event = PushEvent::parse(body).unwrap();
        assert_eq!(event.git_ref, "refs/heads/main");
        assert!(event.before.is_zero());
        assert_eq!(
            event.after.to_hex(),
            "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"
        );
        assert_eq!(event.repository, "o/db");
        assert!(PushEvent::parse(b"{}").is_err());
    }
}
