use std::collections::HashMap;
use std::io::{Read, Write};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use sha1_checked::{Digest, Sha1};

use crate::delta;
use crate::error::{Result, protocol};
use crate::object::{Object, ObjectKind};
use crate::oid::ObjectId;
use crate::transport::NewObject;

const OFS_DELTA: u8 = 6;
const REF_DELTA: u8 = 7;
const MAX_PREALLOC: usize = 64 << 20;

/// Builds a packfile entry by entry.
#[derive(Default)]
pub struct PackWriter {
    entries: Vec<u8>,
    count: u32,
}

impl PackWriter {
    pub fn new() -> Self {
        PackWriter::default()
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Add a whole object.
    pub fn add(&mut self, object: &Object) {
        entry_header(
            &mut self.entries,
            object.kind().pack_code(),
            object.data().len(),
        );
        deflate_into(&mut self.entries, object.data());
        self.count += 1;
    }

    /// Add an object as a delta against `base`, which may live in this pack or (thin pack) on the remote.
    pub fn add_ref_delta(&mut self, base: ObjectId, delta: &[u8]) {
        entry_header(&mut self.entries, REF_DELTA, delta.len());
        self.entries.extend_from_slice(base.as_bytes());
        deflate_into(&mut self.entries, delta);
        self.count += 1;
    }

    pub fn finish(self) -> Vec<u8> {
        let mut pack = Vec::with_capacity(12 + self.entries.len() + 20);
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&self.count.to_be_bytes());
        pack.extend_from_slice(&self.entries);
        let trailer = Sha1::digest(&pack);
        pack.extend_from_slice(&trailer);
        pack
    }
}

/// Build the pack for a push. Objects with a delta base of the same kind are sent as deltas when
/// that saves at least half their size. Bases must already exist on the remote (a thin pack).
pub fn build(objects: &[NewObject]) -> Vec<u8> {
    let mut writer = PackWriter::new();
    for new in objects {
        let object = &new.object;
        match &new.delta_base {
            Some(base) if base.kind() == object.kind() && base.id() != object.id() => {
                let d = delta::encode(base.data(), object.data());
                if d.len() * 2 <= object.data().len() {
                    writer.add_ref_delta(base.id(), &d);
                } else {
                    writer.add(object);
                }
            }
            _ => writer.add(object),
        }
    }
    writer.finish()
}

/// Build the pack for a push with every object whole: no deltas, so the pack stands alone.
pub fn build_whole(objects: &[NewObject]) -> Vec<u8> {
    let mut writer = PackWriter::new();
    for new in objects {
        writer.add(&new.object);
    }
    writer.finish()
}

fn entry_header(out: &mut Vec<u8>, code: u8, size: usize) {
    let mut byte = (code << 4) | (size & 0x0f) as u8;
    let mut rest = size >> 4;
    while rest > 0 {
        out.push(byte | 0x80);
        byte = (rest & 0x7f) as u8;
        rest >>= 7;
    }
    out.push(byte);
}

fn deflate_into(out: &mut Vec<u8>, data: &[u8]) {
    // Encrypted pages are incompressible; the fast level avoids wasting time trying.
    let mut encoder = ZlibEncoder::new(out, Compression::fast());
    encoder
        .write_all(data)
        .expect("writing to a Vec cannot fail");
    encoder.finish().expect("writing to a Vec cannot fail");
}

enum Base {
    None,
    Offset(usize),
    Id(ObjectId),
}

struct Entry {
    offset: usize,
    code: u8,
    base: Base,
    data: Vec<u8>,
}

/// Parse a packfile, resolve its deltas and hash every object.
/// Checks the trailing checksum; bases missing from the pack are an error (fetches are never thin).
pub fn parse(pack: &[u8]) -> Result<Vec<Object>> {
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        return Err(protocol("not a packfile"));
    }
    let version = u32::from_be_bytes(pack[4..8].try_into().unwrap());
    if version != 2 && version != 3 {
        return Err(protocol(format!("unsupported pack version {version}")));
    }
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize;
    let body_end = pack.len() - 20;
    if Sha1::digest(&pack[..body_end]).as_slice() != &pack[body_end..] {
        return Err(protocol("pack checksum mismatch"));
    }
    if count > body_end / 2 {
        return Err(protocol("pack claims more objects than it can hold"));
    }

    let mut entries = Vec::with_capacity(count);
    let mut pos = 12;
    for _ in 0..count {
        let offset = pos;
        let mut byte = *pack.get(pos).ok_or_else(truncated)?;
        pos += 1;
        let code = (byte >> 4) & 7;
        let mut size = (byte & 0x0f) as usize;
        let mut shift = 4;
        while byte & 0x80 != 0 {
            byte = *pack.get(pos).ok_or_else(truncated)?;
            pos += 1;
            if shift > 57 {
                return Err(protocol("pack entry size too large"));
            }
            size |= ((byte & 0x7f) as usize) << shift;
            shift += 7;
        }
        let base = match code {
            OFS_DELTA => {
                let mut byte = *pack.get(pos).ok_or_else(truncated)?;
                pos += 1;
                let mut back = (byte & 0x7f) as usize;
                while byte & 0x80 != 0 {
                    byte = *pack.get(pos).ok_or_else(truncated)?;
                    pos += 1;
                    back = back
                        .checked_add(1)
                        .and_then(|b| b.checked_mul(128))
                        .ok_or_else(|| protocol("bad delta offset"))?
                        | (byte & 0x7f) as usize;
                }
                Base::Offset(
                    offset
                        .checked_sub(back)
                        .ok_or_else(|| protocol("delta base before start of pack"))?,
                )
            }
            REF_DELTA => {
                let id = ObjectId::from_bytes(
                    pack.get(pos..pos + ObjectId::LEN).ok_or_else(truncated)?,
                )?;
                pos += ObjectId::LEN;
                Base::Id(id)
            }
            1..=4 => Base::None,
            other => return Err(protocol(format!("unknown pack entry type {other}"))),
        };
        let (data, used) = inflate(&pack[pos..body_end], size)?;
        pos += used;
        entries.push(Entry {
            offset,
            code,
            base,
            data,
        });
    }

    let by_offset: HashMap<usize, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.offset, i))
        .collect();
    let mut resolved: Vec<Option<Object>> = vec![None; entries.len()];
    let mut by_id: HashMap<ObjectId, usize> = HashMap::new();
    let mut pending: Vec<usize> = (0..entries.len()).collect();
    while !pending.is_empty() {
        let before = pending.len();
        let mut still = Vec::new();
        for i in pending {
            let entry = &entries[i];
            let base_index = match entry.base {
                Base::None => None,
                Base::Offset(o) => Some(
                    *by_offset
                        .get(&o)
                        .ok_or_else(|| protocol("delta base offset is not an entry"))?,
                ),
                Base::Id(id) => match by_id.get(&id) {
                    Some(&b) => Some(b),
                    None => {
                        still.push(i);
                        continue;
                    }
                },
            };
            let object = match base_index {
                None => Object::new(
                    ObjectKind::from_pack_code(entry.code).unwrap(),
                    entry.data.clone(),
                )?,
                Some(b) => match &resolved[b] {
                    Some(base) => {
                        Object::new(base.kind(), delta::apply(base.data(), &entry.data)?)?
                    }
                    None => {
                        still.push(i);
                        continue;
                    }
                },
            };
            by_id.insert(object.id(), i);
            resolved[i] = Some(object);
        }
        if still.len() == before {
            return Err(protocol("pack has deltas whose bases are not in the pack"));
        }
        pending = still;
    }
    Ok(resolved.into_iter().map(|o| o.unwrap()).collect())
}

/// Inflate one zlib stream from the front of `input`. Returns the data and the compressed length.
fn inflate(input: &[u8], expected: usize) -> Result<(Vec<u8>, usize)> {
    let mut decoder = flate2::bufread::ZlibDecoder::new(input);
    let mut out = Vec::with_capacity(expected.min(MAX_PREALLOC));
    (&mut decoder)
        .take(expected as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| protocol(format!("corrupt compressed pack entry: {e}")))?;
    if out.len() != expected {
        return Err(protocol(format!(
            "pack entry inflated to {} bytes, expected {expected}",
            out.len()
        )));
    }
    Ok((out, decoder.total_in() as usize))
}

fn truncated() -> crate::Error {
    protocol("pack is truncated")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn blob(data: &[u8]) -> Object {
        Object::blob(data.to_vec()).unwrap()
    }

    #[test]
    fn round_trip_full_objects() {
        let objects = vec![blob(b""), blob(b"hello\n"), blob(&vec![9u8; 100_000])];
        let pack = build(
            &objects
                .iter()
                .cloned()
                .map(NewObject::from)
                .collect::<Vec<_>>(),
        );
        assert_eq!(parse(&pack).unwrap(), objects);
    }

    #[test]
    fn deltas_against_objects_in_the_same_pack_resolve() {
        let base = blob(&vec![1u8; 5000]);
        let mut changed = vec![1u8; 5000];
        changed[2500] = 2;
        let target = blob(&changed);
        let mut w = PackWriter::new();
        w.add_ref_delta(base.id(), &delta::encode(base.data(), target.data())); // delta before its base
        w.add(&base);
        let parsed = parse(&w.finish()).unwrap();
        assert_eq!(parsed, vec![target, base]);
    }

    #[test]
    fn thin_delta_is_used_only_when_it_pays() {
        let base = blob(&vec![3u8; 4096]);
        let mut next = vec![3u8; 4096];
        next[10] = 4;
        let similar = NewObject::with_delta_base(blob(&next), base.clone());
        let unrelated = NewObject::with_delta_base(blob(&[0xfa; 4096]), base.clone());
        assert!(build(std::slice::from_ref(&similar)).len() < 200);
        // A thin pack cannot be parsed alone: its base is on the remote.
        assert!(parse(&build(&[similar])).is_err());
        // An unrelated object goes in whole, so its pack stands alone.
        assert_eq!(
            parse(&build(std::slice::from_ref(&unrelated))).unwrap(),
            vec![unrelated.object]
        );
    }

    #[test]
    fn rejects_corruption() {
        let mut pack = build(&[NewObject::from(blob(b"hello\n"))]);
        assert!(parse(&pack[..pack.len() - 1]).is_err());
        pack[14] ^= 0xff;
        assert!(parse(&pack).is_err());
        assert!(parse(b"not a pack at all, not at all, no").is_err());
    }

    proptest! {
        #[test]
        fn round_trips_random_blobs(blobs in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..3000), 0..20)) {
            let objects: Vec<Object> = blobs.iter().map(|b| blob(b)).collect();
            let pack = build(&objects.iter().cloned().map(NewObject::from).collect::<Vec<_>>());
            prop_assert_eq!(parse(&pack).unwrap(), objects);
        }

        #[test]
        fn never_panics_on_garbage(mut bytes in proptest::collection::vec(any::<u8>(), 32..400)) {
            bytes[..4].copy_from_slice(b"PACK");
            bytes[4..8].copy_from_slice(&2u32.to_be_bytes());
            let end = bytes.len() - 20;
            let sum = Sha1::digest(&bytes[..end]);
            bytes[end..].copy_from_slice(&sum);
            let _ = parse(&bytes);
        }
    }
}
