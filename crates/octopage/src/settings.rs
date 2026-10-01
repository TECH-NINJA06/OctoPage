#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Retention {
    /// Never drop a commit (small or audit-critical databases).
    KeepAll,
    /// Every commit of the last `days` days, plus tagged commits, plus the last commit of each
    /// day for a year.
    KeepDays {
        /// How many days of history to keep whole.
        days: u32,
    },
    /// The last `count` commits.
    KeepCount {
        /// How many commits to keep.
        count: u32,
    },
}

impl Default for Retention {
    fn default() -> Self {
        Retention::KeepDays { days: 30 }
    }
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Retention::KeepAll => write!(f, "keep_all"),
            Retention::KeepDays { days } => write!(f, "keep_days {days}"),
            Retention::KeepCount { count } => write!(f, "keep_count {count}"),
        }
    }
}

impl std::str::FromStr for Retention {
    type Err = String;

    /// `keep_all`, `keep_days N` or `keep_count N`.
    fn from_str(s: &str) -> Result<Self, String> {
        let mut words = s.split_whitespace();
        let policy = words.next().unwrap_or_default().to_ascii_lowercase();
        let number = |w: Option<&str>| -> Result<u32, String> {
            w.and_then(|w| w.parse().ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{policy} needs a positive number"))
        };
        let retention = match policy.as_str() {
            "keep_all" => Retention::KeepAll,
            "keep_days" => Retention::KeepDays {
                days: number(words.next())?,
            },
            "keep_count" => Retention::KeepCount {
                count: number(words.next())?,
            },
            _ => {
                return Err(format!(
                    "{s:?}: the retention is keep_all, keep_days N or keep_count N"
                ));
            }
        };
        if words.next().is_some() {
            return Err(format!("{s:?}: too many words"));
        }
        Ok(retention)
    }
}

/// A database's maintenance settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Which commits compaction keeps.
    pub retention: Retention,
    /// The longest a reader may hold a snapshot, in days: compaction never drops a commit
    /// younger than this.
    pub snapshot_days: u32,
    /// The most live data the database may hold, in bytes (spec: the 800 MB soft limit). A
    /// write that would grow it further fails with [`crate::Error::Full`].
    pub live_limit: u64,
    /// The repository size to stay under, in bytes. The maintenance job opens an issue when it
    /// projects crossing it within [`Settings::warn_days`], and compacts before it does.
    pub repository_budget: u64,
    /// How early to warn about the budget, in days.
    pub warn_days: u32,
    /// How long a repository the database moved out of stays readable before the maintenance
    /// job deletes it, in days.
    pub grace_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            retention: Retention::default(),
            snapshot_days: 7,
            live_limit: 800 << 20,
            repository_budget: 1 << 30,
            warn_days: 14,
            grace_days: 7,
        }
    }
}

impl Settings {
    /// The settings in a `settings` blob; defaults for anything it leaves out.
    pub fn parse(bytes: &[u8]) -> Result<Settings, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("the database's settings: {e}"))
    }

    /// The contents of a `settings` blob (JSON).
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("serializes")
    }

    /// Check that the settings make sense.
    pub fn validate(&self) -> Result<(), String> {
        if self.snapshot_days == 0 {
            return Err("snapshot_days must be at least 1".into());
        }
        if self.live_limit < (1 << 20) {
            return Err("live_limit must be at least 1 MB".into());
        }
        if self.repository_budget <= self.live_limit {
            return Err("repository_budget must be larger than live_limit".into());
        }
        Ok(())
    }

    /// The live-size limit in pages of `page_size` bytes.
    pub(crate) fn max_pages(&self, page_size: usize) -> u32 {
        (self.live_limit / page_size as u64).clamp(1, u32::MAX as u64) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_default() {
        let settings = Settings {
            retention: Retention::KeepCount { count: 500 },
            ..Settings::default()
        };
        assert_eq!(Settings::parse(&settings.to_bytes()).unwrap(), settings);
        // Missing fields take their defaults.
        let partial = Settings::parse(br#"{"retention":{"policy":"keep_all"}}"#).unwrap();
        assert_eq!(partial.retention, Retention::KeepAll);
        assert_eq!(partial.live_limit, 800 << 20);
        assert_eq!(Settings::default().max_pages(4096), 204_800);
        assert_eq!(Settings::default().max_pages(16384), 51_200);
    }

    #[test]
    fn retention_from_text() {
        assert_eq!("keep_all".parse(), Ok(Retention::KeepAll));
        assert_eq!("keep_days 30".parse(), Ok(Retention::KeepDays { days: 30 }));
        assert_eq!(
            "KEEP_COUNT 9".parse(),
            Ok(Retention::KeepCount { count: 9 })
        );
        assert!("keep_days".parse::<Retention>().is_err());
        assert!("keep_days 0".parse::<Retention>().is_err());
        assert!("forever".parse::<Retention>().is_err());
        for r in [
            Retention::KeepAll,
            Retention::KeepDays { days: 3 },
            Retention::KeepCount { count: 4 },
        ] {
            assert_eq!(r.to_string().parse(), Ok(r));
        }
    }
}
