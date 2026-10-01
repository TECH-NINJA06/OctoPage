use crate::error::{Result, protocol};

/// Largest payload a single pkt-line can carry.
pub const MAX_DATA: usize = 65516;

pub fn write(out: &mut Vec<u8>, data: &[u8]) {
    assert!(
        data.len() <= MAX_DATA,
        "pkt-line payload of {} bytes is too long",
        data.len()
    );
    out.extend_from_slice(format!("{:04x}", data.len() + 4).as_bytes());
    out.extend_from_slice(data);
}

pub fn write_str(out: &mut Vec<u8>, line: &str) {
    write(out, line.as_bytes());
}

pub fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

pub fn delim(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0001");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    ResponseEnd,
}

impl<'a> Packet<'a> {
    /// The payload as text without its trailing newline, if this is a data packet.
    pub fn text(&self) -> Option<&'a str> {
        match self {
            Packet::Data(d) => std::str::from_utf8(d)
                .ok()
                .map(|s| s.strip_suffix('\n').unwrap_or(s)),
            _ => None,
        }
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn next_packet(&mut self) -> Result<Option<Packet<'a>>> {
        if self.pos == self.buf.len() {
            return Ok(None);
        }
        let head = self
            .buf
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| protocol("truncated pkt-line length"))?;
        let len = std::str::from_utf8(head)
            .ok()
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .ok_or_else(|| {
                protocol(format!(
                    "invalid pkt-line length {:?}",
                    String::from_utf8_lossy(head)
                ))
            })?;
        self.pos += 4;
        let packet = match len {
            0 => Packet::Flush,
            1 => Packet::Delim,
            2 => Packet::ResponseEnd,
            3 => return Err(protocol("invalid pkt-line length 3")),
            n => {
                let data = self
                    .buf
                    .get(self.pos..self.pos + n - 4)
                    .ok_or_else(|| protocol("truncated pkt-line"))?;
                self.pos += n - 4;
                Packet::Data(data)
            }
        };
        Ok(Some(packet))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut out = Vec::new();
        write_str(&mut out, "command=fetch\n");
        delim(&mut out);
        write(&mut out, b"");
        flush(&mut out);
        assert_eq!(&out[..4], b"0012");
        let mut r = Reader::new(&out);
        assert_eq!(
            r.next_packet().unwrap().unwrap().text(),
            Some("command=fetch")
        );
        assert_eq!(r.next_packet().unwrap(), Some(Packet::Delim));
        assert_eq!(r.next_packet().unwrap(), Some(Packet::Data(b"")));
        assert_eq!(r.next_packet().unwrap(), Some(Packet::Flush));
        assert_eq!(r.next_packet().unwrap(), None);
    }

    #[test]
    fn rejects_malformed() {
        assert!(Reader::new(b"00").next_packet().is_err());
        assert!(Reader::new(b"zzzz").next_packet().is_err());
        assert!(Reader::new(b"0003").next_packet().is_err());
        assert!(Reader::new(b"0009abc").next_packet().is_err());
        assert!(Reader::new(b"+01a").next_packet().is_err());
    }
}
