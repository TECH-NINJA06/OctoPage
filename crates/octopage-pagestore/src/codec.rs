use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::error::{Error, Result, format, invalid};

const PACKED: &[u8; 4] = b"OPX1";
const COMPRESSED: u8 = 1;
const ENCRYPTED: u8 = 2;
const NONCE_LEN: usize = 24;
const ZSTD_LEVEL: i32 = 3;
/// The most a packed blob may expand to (a changelog can be large; pages are 16 KB at most).
const MAX_PLAIN: usize = 256 << 20;

/// What a blob holds, bound into its encryption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Page,
    Catalog,
    Changelog,
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Kind::Page => b'p',
            Kind::Catalog => b'c',
            Kind::Changelog => b'l',
        }
    }
}

/// How nonces are chosen (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Nonces {
    #[default]
    Random,
    Convergent,
}

/// Something that wraps and unwraps data keys, such as a cloud KMS.
pub trait KeyProvider: Send + Sync {
    /// Names the provider and key (for example a KMS key id); stored beside the wrapped key.
    fn id(&self) -> String;
    fn wrap(&self, key: &[u8; 32]) -> std::result::Result<Vec<u8>, String>;
    fn unwrap(&self, wrapped: &[u8]) -> std::result::Result<[u8; 32], String>;
}

/// A way to open an encrypted database.
#[derive(Clone)]
pub enum Unlock {
    Passphrase(String),
    /// The recovery key printed when the database was created.
    RecoveryKey(String),
    Provider(Arc<dyn KeyProvider>),
}

impl fmt::Debug for Unlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unlock::Passphrase(_) => f.write_str("Unlock::Passphrase(***)"),
            Unlock::RecoveryKey(_) => f.write_str("Unlock::RecoveryKey(***)"),
            Unlock::Provider(p) => write!(f, "Unlock::Provider({})", p.id()),
        }
    }
}

/// Argon2id cost for passphrase slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kdf {
    pub memory_kib: u32,
    pub passes: u32,
}

impl Default for Kdf {
    fn default() -> Self {
        // OWASP's Argon2id recommendation family: 64 MiB, 3 passes, one lane.
        Kdf {
            memory_kib: 64 * 1024,
            passes: 3,
        }
    }
}

/// How to encrypt a new database.
#[derive(Clone, Debug)]
pub struct Encryption {
    /// The first way to open it: a passphrase or a key provider. A recovery key is always made
    /// as well and returned once, when the database is created.
    pub key: Unlock,
    pub nonces: Nonces,
    pub kdf: Kdf,
}

impl Encryption {
    pub fn passphrase(passphrase: impl Into<String>) -> Self {
        Encryption {
            key: Unlock::Passphrase(passphrase.into()),
            nonces: Nonces::default(),
            kdf: Kdf::default(),
        }
    }
}

/// The recovery key of an encrypted database: 32 random bytes, shown as 16 groups of 4 hex
/// digits. Anyone with it can read the database; whoever creates the database must store it.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryKey(Zeroizing<[u8; 32]>);

impl RecoveryKey {
    fn generate() -> Result<Self> {
        Ok(RecoveryKey(Zeroizing::new(random_bytes()?)))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let hex: String = text.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
        let bytes = unhex(&hex)
            .filter(|b| b.len() == 32)
            .ok_or_else(|| invalid("a recovery key is 64 hex digits"))?;
        Ok(RecoveryKey(Zeroizing::new(bytes.try_into().unwrap())))
    }
}

impl fmt::Display for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = hex(&self.0[..]);
        let groups: Vec<&str> = (0..hex.len()).step_by(4).map(|i| &hex[i..i + 4]).collect();
        f.write_str(&groups.join("-"))
    }
}

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryKey(***)")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|e| invalid(format!("no system randomness: {e}")))?;
    Ok(out)
}

fn nonce(bytes: &[u8]) -> Option<XNonce> {
    XNonce::try_from(bytes).ok()
}

fn keyed_hash(key: &[u8]) -> Hmac<Sha256> {
    <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes keys of any size")
}

/// Encrypt `key` under `kek`: nonce followed by ciphertext.
fn wrap(kek: &[u8; 32], key: &[u8; 32]) -> Result<Vec<u8>> {
    let nonce = random_bytes::<NONCE_LEN>()?;
    let cipher = XChaCha20Poly1305::new(kek.into());
    let sealed = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: key,
                aad: b"octopage data key",
            },
        )
        .map_err(|_| invalid("could not wrap the data key"))?;
    Ok([nonce.as_slice(), &sealed].concat())
}

fn unwrap_key(kek: &[u8; 32], wrapped: &[u8]) -> Option<Zeroizing<[u8; 32]>> {
    let (n, sealed) = wrapped.split_at_checked(NONCE_LEN)?;
    let key = XChaCha20Poly1305::new(kek.into())
        .decrypt(
            &nonce(n)?,
            Payload {
                msg: sealed,
                aad: b"octopage data key",
            },
        )
        .ok()?;
    Some(Zeroizing::new(key.try_into().ok()?))
}

fn stretch(passphrase: &str, salt: &[u8], kdf: Kdf) -> Result<Zeroizing<[u8; 32]>> {
    let params = argon2::Params::new(kdf.memory_kib, kdf.passes, 1, Some(32))
        .map_err(|e| invalid(format!("bad Argon2 parameters: {e}")))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut out[..])
        .map_err(|e| invalid(format!("Argon2 failed: {e}")))?;
    Ok(out)
}

/// One way to unwrap the data key.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Slot {
    Passphrase {
        salt: String,
        memory_kib: u32,
        passes: u32,
        wrapped: String,
    },
    Recovery {
        wrapped: String,
    },
    Provider {
        provider: String,
        wrapped: String,
    },
}

/// The `keys` blob.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct KeyFile {
    format: String,
    cipher: String,
    nonces: Nonces,
    slots: Vec<Slot>,
}

const KEYS_FORMAT: &str = "octopage-keys-1";

impl KeyFile {
    /// A new data key, wrapped for `encryption.key` and a fresh recovery key.
    pub(crate) fn create(encryption: &Encryption) -> Result<(KeyFile, Codec, RecoveryKey)> {
        let data_key = Zeroizing::new(random_bytes::<32>()?);
        let recovery = RecoveryKey::generate()?;
        let first = match &encryption.key {
            Unlock::Passphrase(passphrase) => {
                let salt = random_bytes::<16>()?;
                let kek = stretch(passphrase, &salt, encryption.kdf)?;
                Slot::Passphrase {
                    salt: hex(&salt),
                    memory_kib: encryption.kdf.memory_kib,
                    passes: encryption.kdf.passes,
                    wrapped: hex(&wrap(&kek, &data_key)?),
                }
            }
            Unlock::Provider(provider) => Slot::Provider {
                provider: provider.id(),
                wrapped: hex(&provider
                    .wrap(&data_key)
                    .map_err(|e| invalid(format!("the key provider failed: {e}")))?),
            },
            Unlock::RecoveryKey(_) => {
                return Err(invalid(
                    "a recovery key is made with the database, not given to it",
                ));
            }
        };
        let file = KeyFile {
            format: KEYS_FORMAT.into(),
            cipher: "xchacha20poly1305".into(),
            nonces: encryption.nonces,
            slots: vec![
                first,
                Slot::Recovery {
                    wrapped: hex(&wrap(&recovery.0, &data_key)?),
                },
            ],
        };
        let codec = Codec::encrypted(&data_key, encryption.nonces);
        Ok((file, codec, recovery))
    }

    pub(crate) fn parse(bytes: &[u8]) -> Result<KeyFile> {
        let file: KeyFile =
            serde_json::from_slice(bytes).map_err(|e| format(format!("the keys blob: {e}")))?;
        if file.format != KEYS_FORMAT || file.cipher != "xchacha20poly1305" {
            return Err(format(format!(
                "unsupported keys format {} / {}",
                file.format, file.cipher
            )));
        }
        Ok(file)
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut json = serde_json::to_vec_pretty(self).expect("serializable");
        json.push(b'\n');
        json
    }

    /// The codec for this database, if `unlock` opens one of its slots.
    pub(crate) fn unlock(&self, unlock: &Unlock) -> Result<Codec> {
        let data_key = self
            .slots
            .iter()
            .find_map(|slot| match (slot, unlock) {
                (
                    Slot::Passphrase {
                        salt,
                        memory_kib,
                        passes,
                        wrapped,
                    },
                    Unlock::Passphrase(passphrase),
                ) => {
                    let kdf = Kdf {
                        memory_kib: *memory_kib,
                        passes: *passes,
                    };
                    let kek = stretch(passphrase, &unhex(salt)?, kdf).ok()?;
                    unwrap_key(&kek, &unhex(wrapped)?)
                }
                (Slot::Recovery { wrapped }, Unlock::RecoveryKey(text)) => {
                    let key = RecoveryKey::parse(text).ok()?;
                    unwrap_key(&key.0, &unhex(wrapped)?)
                }
                (Slot::Provider { provider, wrapped }, Unlock::Provider(p))
                    if *provider == p.id() =>
                {
                    p.unwrap(&unhex(wrapped)?).ok().map(Zeroizing::new)
                }
                _ => None,
            })
            .ok_or(Error::WrongKey)?;
        Ok(Codec::encrypted(&data_key, self.nonces))
    }
}

struct Cipher {
    aead: XChaCha20Poly1305,
    /// For convergent nonces: the key of the hash they come from.
    nonce_key: Option<Zeroizing<[u8; 32]>>,
}

/// What is wrong with the framing of an encrypted database's stored blob, checked without the
/// key: it must be an encrypted frame long enough for its nonce and tag.
pub(crate) fn frame_problem(blob: &[u8]) -> Option<String> {
    const TAG_LEN: usize = 16;
    if blob.len() < 5 || &blob[..4] != PACKED {
        return Some("not an encrypted frame (stored in the clear?)".into());
    }
    let flags = blob[4];
    if flags & !(COMPRESSED | ENCRYPTED) != 0 {
        return Some(format!("unknown frame flags {flags:#x}"));
    }
    if flags & ENCRYPTED == 0 {
        return Some("a frame that is not encrypted".into());
    }
    if blob.len() < 5 + NONCE_LEN + TAG_LEN {
        return Some(format!("an encrypted frame of only {} bytes", blob.len()));
    }
    None
}

/// How this database's blobs are packed.
#[derive(Clone)]
pub(crate) struct Codec {
    compress: bool,
    cipher: Option<Arc<Cipher>>,
    /// An encrypted database opened without its key: nothing can be sealed or opened.
    locked: bool,
}

impl fmt::Debug for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Codec")
            .field("compress", &self.compress)
            .field("encrypted", &self.cipher.is_some())
            .finish()
    }
}

impl Codec {
    pub(crate) fn plain(compress: bool) -> Self {
        Codec {
            compress,
            cipher: None,
            locked: false,
        }
    }

    /// An encrypted database's codec without the key, for work on its structure only (a
    /// maintenance job that holds no keys): sealing or opening anything fails with `Locked`.
    pub(crate) fn locked() -> Self {
        Codec {
            compress: true,
            cipher: None,
            locked: true,
        }
    }

    fn encrypted(data_key: &[u8; 32], nonces: Nonces) -> Self {
        let nonce_key = (nonces == Nonces::Convergent).then(|| {
            let mut mac = keyed_hash(data_key);
            mac.update(b"octopage convergent nonces");
            Zeroizing::new(mac.finalize().into_bytes().into())
        });
        Codec {
            compress: true, // ciphertext does not compress, so compress first
            cipher: Some(Arc::new(Cipher {
                aead: XChaCha20Poly1305::new(data_key.into()),
                nonce_key,
            })),
            locked: false,
        }
    }

    pub(crate) fn is_encrypted(&self) -> bool {
        self.cipher.is_some() || self.locked
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.locked
    }

    pub(crate) fn compresses(&self) -> bool {
        self.compress
    }

    /// The stored form of `data`.
    pub(crate) fn seal(&self, kind: Kind, data: &[u8]) -> Result<Bytes> {
        if self.locked {
            return Err(Error::Locked);
        }
        if !self.compress && self.cipher.is_none() {
            return Ok(Bytes::copy_from_slice(data));
        }
        let mut flags = 0;
        let mut body = std::borrow::Cow::Borrowed(data);
        if self.compress {
            let packed = zstd::bulk::compress(data, ZSTD_LEVEL)
                .map_err(|e| invalid(format!("zstd: {e}")))?;
            if packed.len() < data.len() {
                flags |= COMPRESSED;
                body = packed.into();
            }
        }
        let Some(cipher) = &self.cipher else {
            if flags == 0 {
                return Ok(Bytes::copy_from_slice(data)); // incompressible: stored as it is
            }
            return Ok([PACKED.as_slice(), &[flags], &body].concat().into());
        };
        flags |= ENCRYPTED;
        let aad = [PACKED.as_slice(), &[flags, kind.tag()]].concat();
        let nonce: [u8; NONCE_LEN] = match &cipher.nonce_key {
            None => random_bytes()?,
            Some(key) => {
                let mut mac = keyed_hash(&key[..]);
                mac.update(&aad);
                mac.update(&body);
                mac.finalize().into_bytes()[..NONCE_LEN].try_into().unwrap()
            }
        };
        let sealed = cipher
            .aead
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &body,
                    aad: &aad,
                },
            )
            .map_err(|_| invalid("encryption failed"))?;
        Ok([PACKED.as_slice(), &[flags], &nonce, &sealed]
            .concat()
            .into())
    }

    /// The contents of a stored blob. Blobs stored as they are pass through unchanged.
    pub(crate) fn open(&self, kind: Kind, blob: &Bytes) -> Result<Bytes> {
        if self.locked {
            return Err(Error::Locked);
        }
        if blob.len() < 5 || &blob[..4] != PACKED {
            return Ok(blob.clone());
        }
        let flags = blob[4];
        let mut body = std::borrow::Cow::Borrowed(&blob[5..]);
        if flags & ENCRYPTED != 0 {
            let cipher = self.cipher.as_ref().ok_or(Error::Locked)?;
            let (n, sealed) = body
                .split_at_checked(NONCE_LEN)
                .ok_or_else(|| format("a short encrypted blob"))?;
            let aad = [PACKED.as_slice(), &[flags, kind.tag()]].concat();
            let plain = cipher
                .aead
                .decrypt(
                    &nonce(n).expect("split at the nonce length"),
                    Payload {
                        msg: sealed,
                        aad: &aad,
                    },
                )
                .map_err(|_| {
                    format(
                        "an encrypted blob failed authentication (damaged, or not this database's)",
                    )
                })?;
            body = plain.into();
        }
        if flags & COMPRESSED != 0 {
            // A streaming decoder grows its output as needed. (`zstd::bulk::decompress` would
            // reserve the whole limit up front, 256 MB per page read.)
            use std::io::Read;
            let zstd = |e: std::io::Error| format(format!("zstd: {e}"));
            let mut plain = Vec::new();
            zstd::stream::read::Decoder::new(&body[..])
                .map_err(zstd)?
                .take(MAX_PLAIN as u64 + 1)
                .read_to_end(&mut plain)
                .map_err(zstd)?;
            if plain.len() > MAX_PLAIN {
                return Err(format("a compressed blob expands past the limit"));
            }
            body = plain.into();
        }
        Ok(Bytes::from(body.into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap() -> Kdf {
        Kdf {
            memory_kib: 64,
            passes: 1,
        }
    }

    #[test]
    fn plain_and_compressed_blobs() {
        let page = [vec![7u8; 100], vec![0u8; 3996]].concat();
        let plain = Codec::plain(false);
        assert_eq!(plain.seal(Kind::Page, &page).unwrap(), page);
        let packed = Codec::plain(true).seal(Kind::Page, &page).unwrap();
        assert!(packed.len() < 100, "{} bytes", packed.len());
        assert_eq!(Codec::plain(false).open(Kind::Page, &packed).unwrap(), page);
        // Incompressible data is stored as it is.
        let noise: Vec<u8> = (0..4096)
            .map(|i| (i * 7919 % 251) as u8 ^ (i >> 3) as u8)
            .collect();
        let _ = Codec::plain(true).seal(Kind::Page, &noise).unwrap();
    }

    #[test]
    fn encryption_round_trips_and_binds_the_kind() {
        let encryption = Encryption {
            key: Unlock::Passphrase("correct horse".into()),
            nonces: Nonces::Random,
            kdf: cheap(),
        };
        let (file, codec, recovery) = KeyFile::create(&encryption).unwrap();
        let secret = b"SECRET-ROW-CONTENTS repeated SECRET-ROW-CONTENTS".repeat(20);
        let sealed = codec.seal(Kind::Changelog, &secret).unwrap();
        assert!(!sealed.windows(6).any(|w| w == b"SECRET"), "no plaintext");
        assert_eq!(codec.open(Kind::Changelog, &sealed).unwrap(), secret);
        assert!(
            codec.open(Kind::Page, &sealed).is_err(),
            "a changelog is not a page"
        );
        assert!(matches!(
            Codec::plain(false).open(Kind::Changelog, &sealed),
            Err(Error::Locked)
        ));
        // Random nonces: the same content seals differently each time.
        assert_ne!(sealed, codec.seal(Kind::Changelog, &secret).unwrap());

        // The key file round-trips and opens with the passphrase or the recovery key only.
        let file = KeyFile::parse(&file.to_bytes()).unwrap();
        let by_passphrase = file
            .unlock(&Unlock::Passphrase("correct horse".into()))
            .unwrap();
        assert_eq!(
            by_passphrase.open(Kind::Changelog, &sealed).unwrap(),
            secret
        );
        let by_recovery = file
            .unlock(&Unlock::RecoveryKey(recovery.to_string()))
            .unwrap();
        assert_eq!(by_recovery.open(Kind::Changelog, &sealed).unwrap(), secret);
        assert!(matches!(
            file.unlock(&Unlock::Passphrase("wrong".into())),
            Err(Error::WrongKey)
        ));
        assert_eq!(recovery.to_string().len(), 64 + 15);
        assert_eq!(RecoveryKey::parse(&recovery.to_string()).unwrap(), recovery);
    }

    #[test]
    fn convergent_nonces_are_deterministic() {
        let encryption = Encryption {
            key: Unlock::Passphrase("p".into()),
            nonces: Nonces::Convergent,
            kdf: cheap(),
        };
        let (_, codec, _) = KeyFile::create(&encryption).unwrap();
        let page = vec![3u8; 4096];
        let a = codec.seal(Kind::Page, &page).unwrap();
        assert_eq!(a, codec.seal(Kind::Page, &page).unwrap());
        assert_ne!(a, codec.seal(Kind::Catalog, &page).unwrap(), "kinds differ");
        assert_eq!(codec.open(Kind::Page, &a).unwrap(), page);
    }

    struct Xor;

    impl KeyProvider for Xor {
        fn id(&self) -> String {
            "test:xor".into()
        }
        fn wrap(&self, key: &[u8; 32]) -> std::result::Result<Vec<u8>, String> {
            Ok(key.iter().map(|b| b ^ 0x5a).collect())
        }
        fn unwrap(&self, wrapped: &[u8]) -> std::result::Result<[u8; 32], String> {
            let key: Vec<u8> = wrapped.iter().map(|b| b ^ 0x5a).collect();
            key.try_into().map_err(|_| "bad length".to_string())
        }
    }

    #[test]
    fn key_providers() {
        let encryption = Encryption {
            key: Unlock::Provider(Arc::new(Xor)),
            nonces: Nonces::Random,
            kdf: cheap(),
        };
        let (file, codec, _) = KeyFile::create(&encryption).unwrap();
        let sealed = codec.seal(Kind::Page, b"hello").unwrap();
        let reopened = file.unlock(&Unlock::Provider(Arc::new(Xor))).unwrap();
        assert_eq!(&reopened.open(Kind::Page, &sealed).unwrap()[..], b"hello");
    }
}
