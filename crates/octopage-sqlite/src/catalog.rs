use serde::{Deserialize, Serialize};

/// One row of `sqlite_schema`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaObject {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub tbl_name: String,
    pub rootpage: i64,
    pub sql: Option<String>,
}

/// The catalog blob's contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// Always `sqlite3`: the pages are SQLite database pages.
    pub format: String,
    /// SQLite's schema cookie, which every schema change bumps.
    pub schema_version: u32,
    /// `sqlite_schema` in rowid order.
    pub objects: Vec<SchemaObject>,
}

impl Catalog {
    pub fn parse(bytes: &[u8]) -> serde_json::Result<Catalog> {
        serde_json::from_slice(bytes)
    }
}

impl SchemaObject {
    /// `sqlite_schema` read through a connection, in the catalog's form and order.
    pub fn all(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<SchemaObject>> {
        let mut query =
            conn.prepare("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema")?;
        query
            .query_map([], |r| {
                Ok(SchemaObject {
                    kind: r.get(0)?,
                    name: r.get(1)?,
                    tbl_name: r.get(2)?,
                    rootpage: r.get(3)?,
                    sql: r.get(4)?,
                })
            })?
            .collect()
    }
}

pub(crate) fn encode(schema_version: u32, objects: Vec<SchemaObject>) -> Result<Vec<u8>, String> {
    let catalog = Catalog {
        format: "sqlite3".into(),
        schema_version,
        objects,
    };
    let mut json = serde_json::to_vec_pretty(&catalog).map_err(|e| e.to_string())?;
    json.push(b'\n');
    Ok(json)
}

pub(crate) type Page = Vec<u8>;

pub(crate) fn be16(page: &[u8], at: usize) -> Result<usize, String> {
    page.get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
        .ok_or_else(|| format!("offset {at} is outside the page"))
}

pub(crate) fn be32(page: &[u8], at: usize) -> Result<u32, String> {
    page.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| format!("offset {at} is outside the page"))
}

/// A SQLite varint at `bytes[at..]`: its value and length.
pub(crate) fn varint(bytes: &[u8], at: usize) -> Result<(u64, usize), String> {
    let mut value = 0u64;
    for i in 0..9 {
        let byte = *bytes.get(at + i).ok_or("truncated varint")?;
        if i == 8 {
            return Ok(((value << 8) | byte as u64, 9));
        }
        value = (value << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    unreachable!()
}

/// Every row of `sqlite_schema`, in rowid order. `page(n)` returns SQLite page `n`; `usable` is
/// the page size minus the reserved bytes.
pub(crate) fn read_schema(
    usable: usize,
    mut page: impl FnMut(u32) -> Result<Page, String>,
) -> Result<Vec<SchemaObject>, String> {
    let mut objects = Vec::new();
    let mut stack = vec![1u32];
    let mut visited = 0usize;
    while let Some(n) = stack.pop() {
        visited += 1;
        if visited > 1_000_000 {
            return Err("the schema B-tree does not end".into());
        }
        let data = page(n)?;
        let h = if n == 1 { 100 } else { 0 }; // page 1 starts with the database header
        let cells = be16(&data, h + 3)?;
        match data.get(h).copied() {
            Some(0x05) => {
                // Interior: children in key order, the right-most last. Push them reversed so
                // the rows come out in rowid order.
                let mut children = Vec::with_capacity(cells + 1);
                for k in 0..cells {
                    children.push(be32(&data, be16(&data, h + 12 + 2 * k)?)?);
                }
                children.push(be32(&data, h + 8)?);
                stack.extend(children.into_iter().rev());
            }
            Some(0x0d) => {
                for k in 0..cells {
                    let at = be16(&data, h + 8 + 2 * k)?;
                    let (length, a) = varint(&data, at)?;
                    let (_rowid, b) = varint(&data, at + a)?;
                    let payload = payload(&data, at + a + b, length as usize, usable, &mut page)?;
                    objects.push(schema_row(&payload)?);
                }
            }
            other => return Err(format!("page {n} is not a table B-tree page ({other:?})")),
        }
    }
    Ok(objects)
}

/// A table-leaf cell's payload, following its overflow chain if it has one.
fn payload(
    data: &[u8],
    start: usize,
    length: usize,
    usable: usize,
    page: &mut impl FnMut(u32) -> Result<Page, String>,
) -> Result<Vec<u8>, String> {
    // How much of the payload sits on the leaf itself (the file format's rule for table leaves).
    let max_local = usable - 35;
    let local = if length <= max_local {
        length
    } else {
        let min_local = (usable - 12) * 32 / 255 - 23;
        let k = min_local + (length - min_local) % (usable - 4);
        if k <= max_local { k } else { min_local }
    };
    let mut out = data
        .get(start..start + local)
        .ok_or("a cell runs past the page")?
        .to_vec();
    if local < length {
        let mut next = be32(data, start + local)?;
        while out.len() < length {
            if next == 0 {
                return Err("an overflow chain ends early".into());
            }
            let overflow = page(next)?;
            let take = (length - out.len()).min(usable - 4);
            out.extend_from_slice(overflow.get(4..4 + take).ok_or("short overflow page")?);
            next = be32(&overflow, 0)?;
        }
    }
    Ok(out)
}

/// Decode a `sqlite_schema` record: type, name, tbl_name, rootpage, sql.
fn schema_row(record: &[u8]) -> Result<SchemaObject, String> {
    enum Value {
        Null,
        Int(i64),
        Text(String),
    }
    let (header_len, mut at) = varint(record, 0)?;
    let mut types = Vec::new();
    while at < header_len as usize {
        let (t, len) = varint(record, at)?;
        types.push(t);
        at += len;
    }
    fn take<'a>(record: &'a [u8], body: &mut usize, n: usize) -> Result<&'a [u8], String> {
        let bytes = record.get(*body..*body + n).ok_or("a record runs short")?;
        *body += n;
        Ok(bytes)
    }
    let mut body = header_len as usize;
    let mut values = Vec::new();
    for t in types {
        let value = match t {
            0 => Value::Null,
            1..=6 => {
                let n = [0, 1, 2, 3, 4, 6, 8][t as usize];
                let bytes = take(record, &mut body, n)?;
                let mut v = if bytes[0] & 0x80 != 0 { -1i64 } else { 0 };
                for &b in bytes {
                    v = (v << 8) | b as i64;
                }
                Value::Int(v)
            }
            8 => Value::Int(0),
            9 => Value::Int(1),
            t if t >= 13 && t % 2 == 1 => {
                let bytes = take(record, &mut body, ((t - 13) / 2) as usize)?;
                Value::Text(String::from_utf8_lossy(bytes).into_owned())
            }
            t => return Err(format!("unexpected serial type {t} in sqlite_schema")),
        };
        values.push(value);
    }
    let text = |v: Option<&Value>| match v {
        Some(Value::Text(s)) => Ok(s.clone()),
        _ => Err("sqlite_schema: expected text".to_string()),
    };
    Ok(SchemaObject {
        kind: text(values.first())?,
        name: text(values.get(1))?,
        tbl_name: text(values.get(2))?,
        rootpage: match values.get(3) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        },
        sql: match values.get(4) {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::varint;

    #[test]
    fn varints() {
        assert_eq!(varint(&[0x05], 0).unwrap(), (5, 1));
        assert_eq!(varint(&[0x81, 0x00], 0).unwrap(), (128, 2));
        assert_eq!(varint(&[0xff; 9], 0).unwrap(), (u64::MAX, 9));
        assert!(varint(&[0x81], 0).is_err());
    }
}
