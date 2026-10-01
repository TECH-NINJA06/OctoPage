use octopage_git::ObjectId;
use octopage_pagestore::CommitInfo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// A lower-case hex prefix of a commit id, 4 to 40 digits.
    Commit(String),
    /// Seconds since the Unix epoch.
    Time(i64),
}

pub(crate) fn parse(text: &str) -> Result<Target, String> {
    let text = text.trim();
    if (4..=40).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Target::Commit(text.to_ascii_lowercase()));
    }
    parse_time(text).map(Target::Time).ok_or_else(|| {
        "expected a commit id (at least 4 hex digits) or a UTC time like '2026-09-01 14:30:00'"
            .to_string()
    })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_time(text: &str) -> Option<i64> {
    let text = text
        .strip_suffix(" UTC")
        .or_else(|| text.strip_suffix('Z'))
        .or_else(|| text.strip_suffix("+00:00"))
        .unwrap_or(text);
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let number = |s: &str, digits: usize| -> Option<i64> {
        (s.len() == digits && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let mut parts = date.split('-');
    let year = number(parts.next()?, 4)?;
    let month = number(parts.next()?, 2)?;
    let day = number(parts.next()?, 2)?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut seconds = days_from_civil(year, month, day) * 86_400;
    if let Some(time) = time {
        let time = time.split('.').next()?; // fractions of a second are ignored
        let mut parts = time.split(':');
        let hour = number(parts.next()?, 2)?;
        let minute = number(parts.next()?, 2)?;
        let second = match parts.next() {
            Some(s) => number(s, 2)?,
            None => 0,
        };
        if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        seconds += hour * 3600 + minute * 60 + second;
    }
    Some(seconds)
}

pub(crate) fn find(history: &[CommitInfo], target: &Target) -> Result<ObjectId, String> {
    match target {
        Target::Commit(prefix) => {
            let names = |c: &&CommitInfo| {
                c.commit.to_hex().starts_with(prefix.as_str())
                    || c.rewritten_from
                        .is_some_and(|o| o.to_hex().starts_with(prefix.as_str()))
            };
            let mut matches = history.iter().filter(names);
            match (matches.next(), matches.next()) {
                (Some(c), None) => Ok(c.commit),
                (Some(_), Some(_)) => Err("more than one commit starts with that".into()),
                (None, _) => Err("no commit in this database's history starts with that".into()),
            }
        }
        Target::Time(at) => history
            .iter()
            .find(|c| c.time <= *at)
            .map(|c| c.commit)
            .ok_or_else(|| "the database did not exist yet".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(parse("A1B2C3D"), Ok(Target::Commit("a1b2c3d".into())));
        assert_eq!(parse("2026-09-01"), Ok(Target::Time(1_788_220_800)));
        assert_eq!(
            parse("2026-09-01 14:30"),
            Ok(Target::Time(1_788_220_800 + 14 * 3600 + 30 * 60))
        );
        assert_eq!(
            parse("2026-09-01T14:30:05.250Z"),
            Ok(Target::Time(1_788_220_800 + 14 * 3600 + 30 * 60 + 5))
        );
        assert_eq!(parse("1970-01-01 00:00:00 UTC"), Ok(Target::Time(0)));
        assert!(parse("yesterday").is_err());
        assert!(parse("2026-13-01").is_err());
        assert!(parse("abc").is_err(), "too short for a commit id");
    }

    #[test]
    fn finding_commits() {
        let id = |b: u8| ObjectId::from_array([b; 20]);
        let history: Vec<CommitInfo> = [(0xcc, 300), (0xcb, 200), (0xaa, 100)]
            .into_iter()
            .map(|(b, time)| CommitInfo {
                commit: id(b),
                parent: None,
                time,
                message: String::new(),
                rewritten_from: (b == 0xaa).then(|| id(0x11)),
            })
            .collect();
        assert_eq!(find(&history, &Target::Time(250)), Ok(id(0xcb)));
        assert_eq!(find(&history, &Target::Time(300)), Ok(id(0xcc)));
        assert!(find(&history, &Target::Time(99)).is_err());
        assert_eq!(find(&history, &Target::Commit("aaaa".into())), Ok(id(0xaa)));
        assert_eq!(find(&history, &Target::Commit("cccc".into())), Ok(id(0xcc)));
        assert!(
            find(&history, &Target::Commit("c".into())).is_err(),
            "ambiguous"
        );
        assert!(find(&history, &Target::Commit("dddd".into())).is_err());
        assert_eq!(
            find(&history, &Target::Commit("1111".into())),
            Ok(id(0xaa)),
            "by the id it had before it was copied"
        );
    }
}
