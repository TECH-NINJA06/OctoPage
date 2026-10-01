use octopage::{Result, Settings, Transport};
use octopage_pagestore::PageStore;

const DAY: f64 = 86_400.0;
const COMMIT_OVERHEAD: u64 = 2048;

/// Commits looked at, at most.
const WINDOW_COMMITS: usize = 2000;

/// How a database grows.
#[derive(Clone, Debug, PartialEq)]
pub struct Growth {
    /// Pages and their size at the head.
    pub pages: usize,
    pub live_bytes: u64,
    /// Commits in the window, and the days it spans.
    pub commits: usize,
    pub days: f64,
    /// Bytes a day the history grows by, from the pages the window's commits changed.
    pub bytes_per_day: f64,
}

/// A repository's size against its budget.
#[derive(Clone, Debug, PartialEq)]
pub struct Projection {
    /// The repository's size, as the host reports it (`None`: it does not say; then the
    /// projection starts from the live data, which understates it).
    pub repository_bytes: Option<u64>,
    pub budget: u64,
    /// Growth of every database in the repository together.
    pub bytes_per_day: f64,
    /// Days until the repository reaches its budget at that rate (`None`: it is not growing).
    pub days_left: Option<f64>,
}

impl Projection {
    /// Whether to warn: the budget is reached within `settings.warn_days`.
    pub fn warns(&self, settings: &Settings) -> bool {
        self.days_left
            .is_some_and(|d| d < settings.warn_days as f64)
    }
}

/// How the database on `store` grew over the last 30 days (at `now`).
pub async fn growth<T: Transport + 'static>(store: &PageStore<T>, now: i64) -> Result<Growth> {
    let head = store.latest().await?;
    let pages = head.page_ids().await?.len();
    let page_size = head.page_size() as u64;
    let history = store.history(WINDOW_COMMITS).await?;
    let since = now - 30 * 86_400;
    let window: Vec<_> = history.iter().filter(|c| c.time >= since).collect();
    let mut added = 0u64;
    let mut commits = 0usize;
    for commit in &window {
        let Some(parent) = commit.parent else {
            continue;
        };
        let a = store.snapshot(parent).await?;
        let b = store.snapshot(commit.commit).await?;
        added += store.diff(&a, &b).await?.len() as u64 * page_size + COMMIT_OVERHEAD;
        commits += 1;
    }
    let days = match (window.first(), window.last()) {
        (Some(newest), Some(oldest)) if commits > 0 => {
            ((newest.time - oldest.time) as f64 / DAY).max(1.0)
        }
        _ => 30.0,
    };
    Ok(Growth {
        pages,
        live_bytes: pages as u64 * page_size,
        commits,
        days,
        bytes_per_day: added as f64 / days,
    })
}

/// Project the repository's size from its databases' growth.
pub fn project(
    growth: &[Growth],
    repository_bytes: Option<u64>,
    settings: &Settings,
) -> Projection {
    let bytes_per_day: f64 = growth.iter().map(|g| g.bytes_per_day).sum();
    let size = repository_bytes.unwrap_or_else(|| growth.iter().map(|g| g.live_bytes).sum());
    let room = settings.repository_budget.saturating_sub(size) as f64;
    let days_left = (bytes_per_day > 0.0).then(|| room / bytes_per_day);
    Projection {
        repository_bytes,
        budget: settings.repository_budget,
        bytes_per_day,
        days_left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection() {
        let growth = Growth {
            pages: 1000,
            live_bytes: 4_096_000,
            commits: 300,
            days: 30.0,
            bytes_per_day: 10_000_000.0,
        };
        let settings = Settings::default();
        let p = project(
            std::slice::from_ref(&growth),
            Some(1_000_000_000),
            &settings,
        );
        let left = p.days_left.unwrap();
        assert!((left - 7.3741824).abs() < 1e-6, "{left}");
        assert!(p.warns(&settings));
        let p = project(&[growth], Some(100_000_000), &settings);
        assert!(!p.warns(&settings));
        assert_eq!(project(&[], None, &settings).days_left, None);
    }
}
