use crate::error::{Error, Result, invalid, protocol};
use crate::oid::ObjectId;
use crate::pktline::{self, Packet, Reader};
use crate::transport::{Ref, RefUpdate};

pub(crate) const AGENT: &str = concat!("agent=octopage/", env!("CARGO_PKG_VERSION"));

fn check_ref_name(name: &str) -> Result<()> {
    let ok = name.starts_with("refs/")
        && !name.ends_with('/')
        && !name.contains("..")
        && !name.contains("//")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("unsupported ref name {name:?}")))
    }
}

pub(crate) fn receive_pack_request(updates: &[RefUpdate], pack: &[u8]) -> Result<Vec<u8>> {
    if updates.is_empty() {
        return Err(invalid("a push needs at least one ref update"));
    }
    // side-band-64k carries the server's error messages; with `atomic`, the report itself only
    // says "atomic transaction failed", and the reason ("is at X but expected Y") is in those messages.
    // `atomic` only when several refs must move together. For a single ref it adds nothing, and
    // github.com then reports a lost compare-and-swap as a bare "failed" instead of
    // "cannot lock ref ... is at X but expected Y".
    let atomic = if updates.len() > 1 { " atomic" } else { "" };
    let mut caps = format!("report-status side-band-64k quiet{atomic} {AGENT}");
    if updates.iter().any(|u| u.new.is_none()) {
        caps.push_str(" delete-refs");
    }
    let mut out = Vec::new();
    for (i, u) in updates.iter().enumerate() {
        check_ref_name(&u.name)?;
        if u.old.is_none() && u.new.is_none() {
            return Err(invalid(format!(
                "update of {} neither expects nor sets a value",
                u.name
            )));
        }
        let old = u.old.unwrap_or(ObjectId::ZERO);
        let new = u.new.unwrap_or(ObjectId::ZERO);
        let line = if i == 0 {
            format!("{old} {new} {}\0{caps}\n", u.name)
        } else {
            format!("{old} {new} {}\n", u.name)
        };
        pktline::write_str(&mut out, &line);
    }
    pktline::flush(&mut out);
    // The protocol sends a pack only when some update sets a value.
    if updates.iter().any(|u| u.new.is_some()) {
        out.extend_from_slice(pack);
    }
    Ok(out)
}

/// Words git and GitHub use when a ref did not hold the expected value.
fn is_conflict(reason: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "cannot lock ref",
        "failed to update ref",
        "failed to lock",
        "stale info",
        "fetch first",
        "non-fast-forward",
        "already exists",
        "but expected",
    ];
    MARKERS.iter().any(|m| reason.contains(m))
}

/// With `atomic`, a ref transaction that fails at commit time (an expected value that no longer
/// holds, or a ref locked by a concurrent writer) is reported with this phrase on every ref.
const ATOMIC_TRANSACTION_FAILED: &str = "atomic transaction failed";

/// Split a side-band response into the report (channel 1) and server messages (channel 2).
/// A response without side-band framing is returned whole.
fn demux_sideband(body: &[u8]) -> Result<(Vec<u8>, Vec<String>)> {
    let mut reader = Reader::new(body);
    let mut report = Vec::new();
    let mut messages = Vec::new();
    while let Some(packet) = reader.next_packet()? {
        match packet {
            Packet::Data(data) => match data.first() {
                Some(1) => report.extend_from_slice(&data[1..]),
                // Drop control characters: index-pack signals the end of its input with a NUL.
                Some(2) => messages.extend(
                    String::from_utf8_lossy(&data[1..])
                        .split(['\n', '\r'])
                        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c.is_control()))
                        .filter(|l| !l.is_empty())
                        .map(String::from),
                ),
                Some(3) => {
                    return Err(Error::Remote(
                        String::from_utf8_lossy(&data[1..]).trim().to_string(),
                    ));
                }
                _ => return Ok((body.to_vec(), messages)), // plain report-status
            },
            Packet::Flush => break,
            _ => {}
        }
    }
    Ok((report, messages))
}

/// Turn a report-status response into the push result.
pub(crate) fn parse_report_status(body: &[u8], updates: &[RefUpdate]) -> Result<()> {
    let (report, messages) = demux_sideband(body)?;
    let mut reader = Reader::new(&report);
    let mut unpack = None;
    let mut statuses: Vec<(String, Option<String>)> = Vec::new();
    while let Some(packet) = reader.next_packet()? {
        let Some(line) = packet.text() else {
            if packet == Packet::Flush {
                break;
            }
            continue;
        };
        if let Some(result) = line.strip_prefix("unpack ") {
            unpack = Some(result.to_string());
        } else if let Some(name) = line.strip_prefix("ok ") {
            statuses.push((name.to_string(), None));
        } else if let Some(rest) = line.strip_prefix("ng ") {
            let (name, reason) = rest.split_once(' ').unwrap_or((rest, "rejected"));
            statuses.push((name.to_string(), Some(reason.to_string())));
        } else if let Some(message) = line.strip_prefix("ERR ") {
            return Err(Error::Remote(message.to_string()));
        }
    }
    match unpack.as_deref() {
        Some("ok") => {}
        Some(error) => {
            return Err(Error::Rejected(format!(
                "remote could not unpack the objects: {error}"
            )));
        }
        None => return Err(protocol("push response has no unpack status")),
    }
    let detail = |reason: &str| {
        if messages.is_empty() {
            reason.to_string()
        } else {
            format!("{reason} ({})", messages.join("; "))
        }
    };
    if let Some((name, reason)) = statuses
        .iter()
        .find_map(|(n, r)| r.as_ref().filter(|r| is_conflict(r)).map(|r| (n, r)))
    {
        return Err(Error::Conflict {
            refname: name.clone(),
            reason: detail(reason),
        });
    }
    // "atomic push failure" marks refs rejected only because a sibling failed; report the sibling.
    let rejected: Vec<String> = statuses
        .iter()
        .filter_map(|(n, r)| {
            r.as_ref()
                .filter(|r| !r.contains("atomic"))
                .map(|r| format!("{n}: {r}"))
        })
        .collect();
    if !rejected.is_empty() {
        return Err(Error::Rejected(detail(&rejected.join("; "))));
    }
    if let Some((name, _)) = statuses
        .iter()
        .find(|(_, r)| r.as_deref() == Some(ATOMIC_TRANSACTION_FAILED))
    {
        // Name the ref the server's message blames, if it does.
        let blamed = messages.iter().find_map(|m| {
            let rest = &m[m.find("cannot lock ref '")? + "cannot lock ref '".len()..];
            Some(rest[..rest.find('\'')?].to_string())
        });
        return Err(Error::Conflict {
            refname: blamed.unwrap_or_else(|| name.clone()),
            reason: detail(ATOMIC_TRANSACTION_FAILED),
        });
    }
    if let Some((name, Some(reason))) = statuses.iter().find(|(_, r)| r.is_some()) {
        return Err(Error::Rejected(detail(&format!("{name}: {reason}"))));
    }
    for u in updates {
        if !statuses.iter().any(|(n, _)| n == &u.name) {
            return Err(protocol(format!(
                "push response has no status for {}",
                u.name
            )));
        }
    }
    Ok(())
}

fn v2_command(command: &str, args: impl IntoIterator<Item = String>) -> Vec<u8> {
    let mut out = Vec::new();
    pktline::write_str(&mut out, &format!("command={command}\n"));
    pktline::write_str(&mut out, &format!("{AGENT}\n"));
    pktline::delim(&mut out);
    for arg in args {
        pktline::write_str(&mut out, &format!("{arg}\n"));
    }
    pktline::flush(&mut out);
    out
}

pub(crate) fn ls_refs_request(prefixes: &[&str]) -> Vec<u8> {
    v2_command(
        "ls-refs",
        prefixes.iter().map(|p| format!("ref-prefix {p}")),
    )
}

pub(crate) fn parse_ls_refs(body: &[u8], prefixes: &[&str]) -> Result<Vec<Ref>> {
    let mut reader = Reader::new(body);
    let mut refs = Vec::new();
    while let Some(packet) = reader.next_packet()? {
        let Some(line) = packet.text() else {
            if packet == Packet::Flush {
                break;
            }
            continue;
        };
        if let Some(message) = line.strip_prefix("ERR ") {
            return Err(Error::Remote(message.to_string()));
        }
        if line.starts_with("unborn ") {
            continue;
        }
        let mut parts = line.split(' ');
        let (Some(id), Some(name)) = (parts.next(), parts.next()) else {
            return Err(protocol(format!("malformed ls-refs line {line:?}")));
        };
        // ref-prefix is a hint the server may ignore, so filter here too.
        if prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p)) {
            refs.push(Ref {
                name: name.to_string(),
                id: ObjectId::from_hex(id)?,
            });
        }
    }
    Ok(refs)
}

/// Which reachable objects a fetch brings along besides the ones it names.
#[derive(Clone, Copy)]
pub(crate) enum Filter {
    /// `tree:0`: blobs and trees come back exactly; commits come back without their trees.
    Exact,
    /// `blob:none`: commits come back with every tree they reach, but no blobs.
    Trees,
}

/// A fetch of `wants`. `haves` and `deepen` bound which ancestor commits come along.
pub(crate) fn fetch_request(
    wants: &[ObjectId],
    haves: &[ObjectId],
    deepen: Option<u32>,
    filter: Filter,
) -> Vec<u8> {
    let filter = match filter {
        Filter::Exact => "filter tree:0",
        Filter::Trees => "filter blob:none",
    };
    let args = [
        "no-progress".to_string(),
        "ofs-delta".to_string(),
        filter.to_string(),
    ]
    .into_iter()
    .chain(deepen.map(|d| format!("deepen {d}")))
    .chain(wants.iter().map(|id| format!("want {id}")))
    .chain(haves.iter().map(|id| format!("have {id}")))
    .chain(["done".to_string()]);
    v2_command("fetch", args)
}

/// Extract the packfile from a v2 fetch response (demultiplexing side-band channel 1).
pub(crate) fn parse_fetch_response(body: &[u8]) -> Result<Vec<u8>> {
    let mut reader = Reader::new(body);
    let mut pack = Vec::new();
    let mut in_pack = false;
    while let Some(packet) = reader.next_packet()? {
        match packet {
            Packet::Data(data) if in_pack => match data.first() {
                Some(1) => pack.extend_from_slice(&data[1..]),
                Some(2) => {} // progress
                Some(3) => return Err(remote_error(&String::from_utf8_lossy(&data[1..]))),
                _ => {
                    return Err(protocol(
                        "packfile section has an unknown side-band channel",
                    ));
                }
            },
            Packet::Data(_) => {
                let line = packet.text().unwrap_or_default();
                if let Some(message) = line.strip_prefix("ERR ") {
                    return Err(remote_error(message));
                }
                if line == "packfile" {
                    in_pack = true;
                }
                // Other sections (acknowledgments, shallow-info, ...) are skipped.
            }
            Packet::Flush if in_pack => break,
            _ => {}
        }
    }
    if !in_pack {
        return Err(protocol("fetch response has no packfile section"));
    }
    Ok(pack)
}

/// `upload-pack: not our ref <id>` means the object does not exist (or is unreachable).
fn remote_error(message: &str) -> Error {
    let message = message.trim();
    if let Some(pos) = message.find("not our ref") {
        let ids: Vec<ObjectId> = message[pos..]
            .split_whitespace()
            .filter_map(|w| ObjectId::from_hex(w).ok())
            .collect();
        if !ids.is_empty() {
            return Error::MissingObjects(ids);
        }
    }
    Error::Remote(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt_body(lines: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for l in lines {
            pktline::write_str(&mut out, &format!("{l}\n"));
        }
        pktline::flush(&mut out);
        out
    }

    fn id(n: u8) -> ObjectId {
        ObjectId::from_array([n; 20])
    }

    #[test]
    fn push_request_layout() {
        let updates = [
            RefUpdate::update("refs/heads/db", id(1), id(2)),
            RefUpdate::delete("refs/octopage/stage/t1", id(3)),
        ];
        let body = receive_pack_request(&updates, b"PACK...").unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(&format!(
            "{} {} refs/heads/db\0report-status side-band-64k quiet atomic {AGENT} delete-refs\n",
            id(1),
            id(2)
        )));
        assert!(text.contains(&format!(
            "{} {} refs/octopage/stage/t1\n",
            id(3),
            ObjectId::ZERO
        )));
        assert!(text.ends_with("0000PACK..."));
        // A single ref: no `atomic`.
        let body = receive_pack_request(&updates[..1], b"PACK...").unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("atomic"));
        // Deletes only: no pack.
        let body = receive_pack_request(&updates[1..], b"PACK...").unwrap();
        assert!(body.ends_with(b"0000"));
        assert!(receive_pack_request(&[RefUpdate::create("HEAD", id(1))], b"").is_err());
        assert!(receive_pack_request(&[RefUpdate::create("refs/heads/../x", id(1))], b"").is_err());
    }

    #[test]
    fn github_conflict_is_typed() {
        // Verbatim from the Phase 0 spike against github.com.
        let body = pkt_body(&[
            "unpack ok",
            "ng refs/heads/db cannot lock ref 'refs/heads/db': is at 0af48a3bcdb9ab4e07eb22cc89d818cfee2598ab but expected dbcdf82972451e90da5d0cbc43ef121b6a9219a2",
        ]);
        let updates = [RefUpdate::update("refs/heads/db", id(1), id(2))];
        match parse_report_status(&body, &updates) {
            Err(Error::Conflict { refname, reason }) => {
                assert_eq!(refname, "refs/heads/db");
                assert!(reason.contains("but expected"));
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        // Older git wording, from the local spike run.
        let body = pkt_body(&["unpack ok", "ng refs/heads/db failed to update ref"]);
        assert!(matches!(
            parse_report_status(&body, &updates),
            Err(Error::Conflict { .. })
        ));
    }

    #[test]
    fn atomic_collateral_reports_the_real_conflict() {
        let updates = [
            RefUpdate::update("refs/heads/db", id(1), id(2)),
            RefUpdate::create("refs/octopage/lease", id(3)),
        ];
        let body = pkt_body(&[
            "unpack ok",
            "ng refs/octopage/lease atomic push failed",
            "ng refs/heads/db failed to update ref",
        ]);
        match parse_report_status(&body, &updates) {
            Err(Error::Conflict { refname, .. }) => assert_eq!(refname, "refs/heads/db"),
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    fn sidebanded(report: &[&str], messages: &str) -> Vec<u8> {
        let mut body = Vec::new();
        let mut channel1 = vec![1u8];
        channel1.extend(pkt_body(report));
        pktline::write(&mut body, &channel1);
        pktline::write(&mut body, format!("\x02{messages}").as_bytes());
        pktline::flush(&mut body);
        body
    }

    #[test]
    fn atomic_transaction_failure_is_a_conflict_with_the_servers_reason() {
        // What git's receive-pack sends when an atomic push loses a compare-and-swap.
        let updates = [
            RefUpdate::update("refs/heads/db", id(1), id(2)),
            RefUpdate::create("refs/octopage/lease", id(2)),
        ];
        let body = sidebanded(
            &[
                "unpack ok",
                "ng refs/heads/db atomic transaction failed",
                "ng refs/octopage/lease atomic transaction failed",
            ],
            "error: cannot lock ref 'refs/heads/db': is at 0101 but expected 0202\n",
        );
        match parse_report_status(&body, &updates) {
            Err(Error::Conflict { refname, reason }) => {
                assert_eq!(refname, "refs/heads/db");
                assert!(reason.contains("is at 0101 but expected 0202"), "{reason}");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        // A sibling's real rejection is not hidden behind "atomic push failure".
        let body = sidebanded(
            &[
                "unpack ok",
                "ng refs/heads/db atomic push failure",
                "ng refs/octopage/lease pre-receive hook declined",
            ],
            "",
        );
        assert!(
            matches!(parse_report_status(&body, &updates), Err(Error::Rejected(r)) if r.contains("hook declined"))
        );
        let body = sidebanded(
            &["unpack ok", "ok refs/heads/db", "ok refs/octopage/lease"],
            "",
        );
        assert!(parse_report_status(&body, &updates).is_ok());
    }

    #[test]
    fn fetch_requests_ask_for_exact_objects() {
        let text =
            String::from_utf8(fetch_request(&[id(1)], &[id(2)], Some(1), Filter::Exact)).unwrap();
        for arg in [
            "filter tree:0\n",
            "deepen 1\n",
            &format!("want {}\n", id(1)),
            &format!("have {}\n", id(2)),
            "done\n",
        ] {
            assert!(text.contains(arg), "missing {arg:?} in {text:?}");
        }
    }

    #[test]
    fn other_rejections_and_success() {
        let updates = [RefUpdate::update("refs/heads/db", id(1), id(2))];
        let body = pkt_body(&["unpack ok", "ng refs/heads/db pre-receive hook declined"]);
        assert!(matches!(
            parse_report_status(&body, &updates),
            Err(Error::Rejected(_))
        ));
        let body = pkt_body(&[
            "unpack index-pack abnormal exit",
            "ng refs/heads/db unpacker error",
        ]);
        assert!(matches!(
            parse_report_status(&body, &updates),
            Err(Error::Rejected(_))
        ));
        let body = pkt_body(&["unpack ok", "ok refs/heads/db"]);
        assert!(parse_report_status(&body, &updates).is_ok());
        let body = pkt_body(&["unpack ok"]);
        assert!(matches!(
            parse_report_status(&body, &updates),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn ls_refs_parsing() {
        let body = pkt_body(&[
            &format!("{} refs/heads/db", id(1)),
            &format!("{} refs/heads/dbx symref-target:x", id(2)),
            &format!("{} refs/tags/v1", id(3)),
            "unborn HEAD",
        ]);
        let refs = parse_ls_refs(&body, &["refs/heads/"]).unwrap();
        assert_eq!(
            refs,
            vec![
                Ref {
                    name: "refs/heads/db".into(),
                    id: id(1)
                },
                Ref {
                    name: "refs/heads/dbx".into(),
                    id: id(2)
                }
            ]
        );
        assert_eq!(parse_ls_refs(&body, &[]).unwrap().len(), 3);
    }

    #[test]
    fn fetch_response_demux() {
        let mut body = Vec::new();
        pktline::write_str(&mut body, "packfile\n");
        pktline::write(&mut body, b"\x02Enumerating objects\n");
        pktline::write(&mut body, b"\x01PACK-part-1");
        pktline::write(&mut body, b"\x01-part-2");
        pktline::flush(&mut body);
        assert_eq!(parse_fetch_response(&body).unwrap(), b"PACK-part-1-part-2");

        let mut body = Vec::new();
        pktline::write_str(
            &mut body,
            &format!("ERR upload-pack: not our ref {}\n", id(9)),
        );
        assert!(
            matches!(parse_fetch_response(&body), Err(Error::MissingObjects(ids)) if ids == vec![id(9)])
        );

        let mut body = Vec::new();
        pktline::write_str(&mut body, "packfile\n");
        pktline::write(&mut body, b"\x03fatal: out of memory");
        assert!(matches!(parse_fetch_response(&body), Err(Error::Remote(_))));
    }
}
