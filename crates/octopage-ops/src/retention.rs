use std::collections::HashSet;

use octopage::{ObjectId, Retention, Settings};

const DAY: i64 = 86_400;

/// One commit of a database's history, for deciding what to keep.
#[derive(Clone, Copy, Debug)]
pub struct Dated {
    pub commit: ObjectId,
    /// Commit time, seconds since the Unix epoch.
    pub time: i64,
}

/// The commits of `chain` (a first-parent history, newest first) to keep under `settings` at
/// time `now`. Always kept: the head, every commit younger than the longest snapshot age, and
/// tagged commits (`tagged`).
pub fn kept(
    chain: &[Dated],
    settings: &Settings,
    tagged: &HashSet<ObjectId>,
    now: i64,
) -> HashSet<ObjectId> {
    let snapshot_floor = now - settings.snapshot_days as i64 * DAY;
    let mut keep: HashSet<ObjectId> = chain
        .iter()
        .enumerate()
        .filter(|(i, c)| *i == 0 || c.time >= snapshot_floor || tagged.contains(&c.commit))
        .map(|(_, c)| c.commit)
        .collect();
    match settings.retention {
        Retention::KeepAll => keep.extend(chain.iter().map(|c| c.commit)),
        Retention::KeepCount { count } => {
            keep.extend(chain.iter().take(count as usize).map(|c| c.commit));
        }
        Retention::KeepDays { days } => {
            let cutoff = now - days.max(settings.snapshot_days) as i64 * DAY;
            let year = now - 365 * DAY;
            let mut days_seen = HashSet::new();
            for c in chain {
                if c.time >= cutoff {
                    keep.insert(c.commit);
                } else if c.time >= year && days_seen.insert(c.time.div_euclid(DAY)) {
                    // The newest commit of each older day (the chain is newest first).
                    keep.insert(c.commit);
                }
            }
        }
        // A policy this version does not know drops nothing.
        _ => keep.extend(chain.iter().map(|c| c.commit)),
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(times: &[i64]) -> Vec<Dated> {
        times
            .iter()
            .enumerate()
            .map(|(i, t)| Dated {
                commit: ObjectId::from_array([i as u8 + 1; 20]),
                time: *t,
            })
            .collect()
    }

    fn settings(retention: Retention) -> Settings {
        Settings {
            retention,
            ..Settings::default()
        }
    }

    #[test]
    fn keep_days_keeps_recent_commits_and_one_per_older_day() {
        let now = 1000 * DAY;
        // Newest first: two today, two 40 days ago (same day), one 400 days ago.
        let c = chain(&[
            now - 10,
            now - 20,
            now - 40 * DAY + 50,
            now - 40 * DAY + 10,
            now - 400 * DAY,
        ]);
        let kept = kept(
            &c,
            &settings(Retention::KeepDays { days: 30 }),
            &HashSet::new(),
            now,
        );
        assert!(kept.contains(&c[0].commit) && kept.contains(&c[1].commit));
        assert!(kept.contains(&c[2].commit), "the newest of its day");
        assert!(!kept.contains(&c[3].commit), "an older one of the same day");
        assert!(!kept.contains(&c[4].commit), "older than a year");
    }

    #[test]
    fn keep_count_never_drops_young_commits_or_tags() {
        let now = 1000 * DAY;
        let c = chain(&[
            now - 10,
            now - DAY,
            now - 2 * DAY,
            now - 20 * DAY,
            now - 30 * DAY,
        ]);
        let tagged = HashSet::from([c[4].commit]);
        let kept = kept(
            &c,
            &settings(Retention::KeepCount { count: 1 }),
            &tagged,
            now,
        );
        // The last commit, the three younger than 7 days, and the tag.
        assert_eq!(kept.len(), 4);
        assert!(!kept.contains(&c[3].commit));
        assert!(kept.contains(&c[4].commit));
    }

    #[test]
    fn keep_all_keeps_everything() {
        let c = chain(&[5, 4, 3, 2, 1]);
        let kept = kept(
            &c,
            &settings(Retention::KeepAll),
            &HashSet::new(),
            1000 * DAY,
        );
        assert_eq!(kept.len(), 5);
    }
}
