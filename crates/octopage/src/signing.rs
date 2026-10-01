use std::fmt;
use std::path::Path;

use octopage_pagestore::CommitSigner;
use ssh_key::{HashAlg, LineEnding, PrivateKey};

use crate::{Error, Result};

/// Signs commits with an SSH key (Ed25519).
pub struct SshSigner {
    key: PrivateKey,
}

impl SshSigner {
    /// Read an OpenSSH private key, as `ssh-keygen` writes it, decrypting it with `passphrase`
    /// if it has one.
    pub fn from_file(path: impl AsRef<Path>, passphrase: Option<&str>) -> Result<Self> {
        let invalid = |e: ssh_key::Error| Error::Invalid(format!("the SSH key: {e}"));
        let key = PrivateKey::read_openssh_file(path.as_ref()).map_err(invalid)?;
        let key = if key.is_encrypted() {
            let passphrase = passphrase.ok_or_else(|| {
                Error::Invalid("the SSH key is encrypted: give its passphrase".into())
            })?;
            key.decrypt(passphrase).map_err(invalid)?
        } else {
            key
        };
        Ok(SshSigner { key })
    }

    /// The public key, as `ssh-keygen -y` prints it: register it on GitHub as a signing key.
    pub fn public_key(&self) -> String {
        self.key.public_key().to_openssh().unwrap_or_default()
    }
}

impl fmt::Debug for SshSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SshSigner({})",
            self.key.public_key().fingerprint(HashAlg::Sha256)
        )
    }
}

impl CommitSigner for SshSigner {
    fn sign(&self, commit: &[u8]) -> std::result::Result<String, String> {
        let signature = self
            .key
            .sign("git", HashAlg::Sha512, commit)
            .map_err(|e| e.to_string())?;
        signature.to_pem(LineEnding::LF).map_err(|e| e.to_string())
    }
}
