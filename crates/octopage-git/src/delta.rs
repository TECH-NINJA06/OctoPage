use crate::error::{Result, protocol};

const MAX_COPY: usize = 0x10000; // the largest copy every git version accepts in one instruction
const MAX_INSERT: usize = 0x7f;
const MAX_PREALLOC: usize = 64 << 20;

/// Rebuild the target from `base` and `delta`. Malformed deltas are errors, never panics.
pub fn apply(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;
    let source_size = read_size(delta, &mut pos)?;
    let target_size = read_size(delta, &mut pos)?;
    if source_size != base.len() {
        return Err(protocol(format!(
            "delta expects a {source_size}-byte base, got {}",
            base.len()
        )));
    }
    let mut out = Vec::with_capacity(target_size.min(MAX_PREALLOC));
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    offset |= (*delta.get(pos).ok_or_else(truncated)? as usize) << (8 * i);
                    pos += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    size |= (*delta.get(pos).ok_or_else(truncated)? as usize) << (8 * i);
                    pos += 1;
                }
            }
            if size == 0 {
                size = MAX_COPY;
            }
            let chunk = offset
                .checked_add(size)
                .and_then(|end| base.get(offset..end))
                .ok_or_else(|| protocol("delta copies outside its base"))?;
            out.extend_from_slice(chunk);
        } else if op != 0 {
            let chunk = delta.get(pos..pos + op as usize).ok_or_else(truncated)?;
            out.extend_from_slice(chunk);
            pos += op as usize;
        } else {
            return Err(protocol("delta contains reserved opcode 0"));
        }
        if out.len() > target_size {
            return Err(protocol("delta produces more bytes than it declares"));
        }
    }
    if out.len() != target_size {
        return Err(protocol(format!(
            "delta produced {} bytes, declared {target_size}",
            out.len()
        )));
    }
    Ok(out)
}

/// Encode `target` as a delta against `base` using their common prefix and suffix.
pub fn encode(base: &[u8], target: &[u8]) -> Vec<u8> {
    let prefix = base.iter().zip(target).take_while(|(a, b)| a == b).count();
    let room = base.len().min(target.len()) - prefix;
    let suffix = base[prefix..]
        .iter()
        .rev()
        .zip(target[prefix..].iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();

    let mut out = Vec::with_capacity(32 + target.len() - prefix - suffix);
    write_size(&mut out, base.len());
    write_size(&mut out, target.len());
    push_copies(&mut out, 0, prefix);
    for chunk in target[prefix..target.len() - suffix].chunks(MAX_INSERT) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    push_copies(&mut out, base.len() - suffix, suffix);
    out
}

fn push_copies(out: &mut Vec<u8>, mut offset: usize, mut len: usize) {
    while len > 0 {
        let size = len.min(MAX_COPY);
        let mut op = 0x80u8;
        let mut args = Vec::with_capacity(7);
        for i in 0..4 {
            let byte = (offset >> (8 * i)) as u8;
            if byte != 0 {
                op |= 1 << i;
                args.push(byte);
            }
        }
        let encoded_size = if size == MAX_COPY { 0 } else { size };
        for i in 0..3 {
            let byte = (encoded_size >> (8 * i)) as u8;
            if byte != 0 {
                op |= 0x10 << i;
                args.push(byte);
            }
        }
        out.push(op);
        out.extend_from_slice(&args);
        offset += size;
        len -= size;
    }
}

fn read_size(delta: &[u8], pos: &mut usize) -> Result<usize> {
    let mut value = 0usize;
    let mut shift = 0;
    loop {
        let byte = *delta.get(*pos).ok_or_else(truncated)?;
        *pos += 1;
        if shift > 56 {
            return Err(protocol("delta size header too long"));
        }
        value |= ((byte & 0x7f) as usize) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
}

fn write_size(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn truncated() -> crate::Error {
    protocol("delta is truncated")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn one_changed_tree_entry_is_tiny() {
        let base: Vec<u8> = (0..8000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut target = base.clone();
        target[4000..4020].copy_from_slice(&[0xAB; 20]);
        let delta = encode(&base, &target);
        assert!(delta.len() < 40, "delta was {} bytes", delta.len());
        assert_eq!(apply(&base, &delta).unwrap(), target);
    }

    #[test]
    fn copies_longer_than_one_instruction() {
        let base = vec![7u8; 3 * MAX_COPY + 5];
        let mut target = base.clone();
        target.push(1);
        assert_eq!(apply(&base, &encode(&base, &target)).unwrap(), target);
    }

    #[test]
    fn rejects_bad_deltas() {
        assert!(apply(b"abc", &[]).is_err());
        assert!(apply(b"abc", &[3, 3, 0x91, 0, 9]).is_err()); // copy past the end of the base
        assert!(apply(b"abc", &[3, 1, 0]).is_err()); // reserved opcode
        assert!(apply(b"abc", &[4, 3, 0x90, 3]).is_err()); // wrong base size
        assert!(apply(b"abc", &[3, 2, 3, b'x', b'y', b'z']).is_err()); // longer than declared
    }

    proptest! {
        #[test]
        fn round_trips(base in proptest::collection::vec(any::<u8>(), 0..2000),
                       edit_at in any::<prop::sample::Index>(),
                       insert in proptest::collection::vec(any::<u8>(), 0..300),
                       remove in 0usize..300) {
            let at = edit_at.index(base.len() + 1);
            let end = (at + remove).min(base.len());
            let mut target = base[..at].to_vec();
            target.extend_from_slice(&insert);
            target.extend_from_slice(&base[end..]);
            prop_assert_eq!(apply(&base, &encode(&base, &target)).unwrap(), target);
        }

        #[test]
        fn never_panics_on_garbage(base in proptest::collection::vec(any::<u8>(), 0..64),
                                   delta in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = apply(&base, &delta);
        }
    }
}
