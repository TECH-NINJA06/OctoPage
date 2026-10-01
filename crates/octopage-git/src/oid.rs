use std::fmt;
use std::str::FromStr;

use crate::error::{Result, invalid};

/// A git object id: the SHA-1 of the object's header and content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ObjectId([u8; ObjectId::LEN]);

impl ObjectId {
    pub const LEN: usize = 20;
    /// The all-zero id git uses for "no object" in ref updates.
    pub const ZERO: ObjectId = ObjectId([0; ObjectId::LEN]);

    pub const fn from_array(bytes: [u8; ObjectId::LEN]) -> Self {
        ObjectId(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let array: [u8; ObjectId::LEN] = bytes.try_into().map_err(|_| {
            invalid(format!(
                "object id must be {} bytes, got {}",
                ObjectId::LEN,
                bytes.len()
            ))
        })?;
        Ok(ObjectId(array))
    }

    pub fn as_bytes(&self) -> &[u8; ObjectId::LEN] {
        &self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; ObjectId::LEN]
    }

    pub fn from_hex(hex: &str) -> Result<Self> {
        let hex = hex.as_bytes();
        if hex.len() != ObjectId::LEN * 2 {
            return Err(invalid(format!(
                "object id must be 40 hex digits, got {}",
                hex.len()
            )));
        }
        let mut out = [0u8; ObjectId::LEN];
        for (i, pair) in hex.chunks_exact(2).enumerate() {
            out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Ok(ObjectId(out))
    }

    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(ObjectId::LEN * 2);
        for b in self.0 {
            s.push(DIGITS[(b >> 4) as usize] as char);
            s.push(DIGITS[(b & 15) as usize] as char);
        }
        s
    }
}

fn nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(invalid(format!("invalid hex digit {:?}", c as char))),
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

impl FromStr for ObjectId {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self> {
        ObjectId::from_hex(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let hex = "ce013625030ba8dba906f756967f9e9ca394464a";
        let id = ObjectId::from_hex(hex).unwrap();
        assert_eq!(id.to_hex(), hex);
        assert_eq!(id.to_string(), hex);
        assert_eq!(ObjectId::from_hex(&hex.to_uppercase()).unwrap(), id);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(ObjectId::from_hex("abc").is_err());
        assert!(ObjectId::from_hex(&"g".repeat(40)).is_err());
        assert!(ObjectId::from_bytes(&[0; 19]).is_err());
    }

    #[test]
    fn zero() {
        assert!(ObjectId::ZERO.is_zero());
        assert_eq!(ObjectId::ZERO.to_hex(), "0".repeat(40));
    }
}
