use std::collections::HashMap;

use crate::catalog::{self, Page, be16, be32, varint};

/// How much of a cell's payload sits on its B-tree page (the file format's rule); the rest is
/// in an overflow chain.
fn local_size(payload: usize, usable: usize, table: bool) -> usize {
    let max_local = if table {
        usable - 35
    } else {
        (usable - 12) * 64 / 255 - 23
    };
    if payload <= max_local {
        return payload;
    }
    let min_local = (usable - 12) * 32 / 255 - 23;
    let k = min_local + (payload - min_local) % (usable - 4);
    if k <= max_local { k } else { min_local }
}

/// The pages of an overflow chain starting at `first`.
fn chain(
    first: u32,
    payload: usize,
    local: usize,
    usable: usize,
    page: &mut impl FnMut(u32) -> Result<Page, String>,
) -> Result<Vec<u32>, String> {
    let mut pages = Vec::new();
    let mut left = payload - local;
    let mut next = first;
    while left > 0 && next != 0 && pages.len() < 100_000 {
        pages.push(next);
        left = left.saturating_sub(usable - 4);
        next = be32(&page(next)?, 0)?;
    }
    Ok(pages)
}

fn rows(rowids: &[i64]) -> String {
    match rowids {
        [] => "no rows".into(),
        [one] => format!("the row with rowid {one}"),
        [first, .., last] => format!("the rows with rowids {first} to {last}"),
    }
}

/// For each of `wanted` (SQLite page numbers), a description of what holds it, found by
/// walking the schema and then every table's and index's B-tree from its root.
pub(crate) fn locate(
    wanted: &[u32],
    usable: usize,
    mut page: impl FnMut(u32) -> Result<Page, String>,
) -> Result<HashMap<u32, String>, String> {
    let mut found = HashMap::new();
    let mut roots = vec![("the schema".to_string(), 1u32)];
    for object in catalog::read_schema(usable, &mut page)? {
        if object.rootpage > 0 {
            roots.push((
                format!("{} {}", object.kind, object.name),
                object.rootpage as u32,
            ));
        }
    }
    for (name, root) in roots {
        let mut stack = vec![root];
        let mut visited = 0;
        while let Some(n) = stack.pop() {
            visited += 1;
            if visited > 10_000_000 {
                return Err(format!("{name}: the B-tree does not end"));
            }
            let data = page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            let kind = *data.get(h).ok_or("an empty page")?;
            let interior = kind == 0x02 || kind == 0x05;
            let cells = be16(&data, h + 3)?;
            let cell_array = h + if interior { 12 } else { 8 };
            let mut rowids = Vec::new();
            for k in 0..cells {
                let at = be16(&data, cell_array + 2 * k)?;
                match kind {
                    0x05 => stack.push(be32(&data, at)?),
                    0x02 => stack.push(be32(&data, at)?),
                    0x0d | 0x0a => {
                        let (length, a) = varint(&data, at)?;
                        let mut start = at + a;
                        let mut rowid = None;
                        if kind == 0x0d {
                            let (id, b) = varint(&data, start)?;
                            rowid = Some(id as i64);
                            rowids.push(id as i64);
                            start += b;
                        }
                        let length = length as usize;
                        let local = local_size(length, usable, kind == 0x0d);
                        if local < length {
                            let first = be32(&data, start + local)?;
                            for p in chain(first, length, local, usable, &mut page)? {
                                if wanted.contains(&p) {
                                    let what = match rowid {
                                        Some(id) => {
                                            format!("{name}, {} (a long value)", rows(&[id]))
                                        }
                                        None => format!("{name} (a long key)"),
                                    };
                                    found.insert(p, what);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if interior {
                stack.push(be32(&data, h + 8)?); // the right-most child
            }
            if wanted.contains(&n) {
                let what = match kind {
                    0x0d => format!("{name}, {}", rows(&rowids)),
                    _ if interior => format!("{name} (an interior page)"),
                    _ => name.clone(),
                };
                found.insert(n, what);
            }
            if found.len() == wanted.len() {
                return Ok(found);
            }
        }
    }
    Ok(found)
}
