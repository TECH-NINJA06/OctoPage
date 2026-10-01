use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The service's counters.
#[derive(Default)]
pub struct Metrics {
    /// HTTP requests by route and status.
    requests: Mutex<BTreeMap<(String, u16), u64>>,
    pub commits: AtomicU64,
    pub webhooks: AtomicU64,
}

impl Metrics {
    pub fn request(&self, route: &str, status: u16) {
        *self
            .requests
            .lock()
            .unwrap()
            .entry((route.to_string(), status))
            .or_default() += 1;
    }

    /// Requests so far with `status`, over all routes.
    pub fn requests_with(&self, status: u16) -> u64 {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|((_, s), _)| *s == status)
            .map(|(_, n)| n)
            .sum()
    }

    /// Render everything, with the gauges and per-installation counters the caller adds.
    pub fn render(
        &self,
        served: usize,
        transactions: usize,
        installations: &[(i64, u64, u64)],
        tokens_minted: u64,
    ) -> String {
        let mut out = String::new();
        out += "# HELP octopage_http_requests_total HTTP requests by route and status.\n";
        out += "# TYPE octopage_http_requests_total counter\n";
        for ((route, status), n) in self.requests.lock().unwrap().iter() {
            let _ = writeln!(
                out,
                "octopage_http_requests_total{{route=\"{route}\",status=\"{status}\"}} {n}"
            );
        }
        let gauge = |out: &mut String, name: &str, help: &str, value: u64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
            );
        };
        gauge(
            &mut out,
            "octopage_served_databases",
            "Databases open.",
            served as u64,
        );
        gauge(
            &mut out,
            "octopage_open_transactions",
            "Interactive transactions open.",
            transactions as u64,
        );
        let counter = |out: &mut String, name: &str, help: &str, value: u64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
            );
        };
        counter(
            &mut out,
            "octopage_commits_total",
            "Commits made through the API.",
            self.commits.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "octopage_webhooks_total",
            "Webhook deliveries accepted.",
            self.webhooks.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "octopage_installation_tokens_minted_total",
            "GitHub App installation tokens minted.",
            tokens_minted,
        );
        out += "# HELP octopage_github_requests_total Requests to GitHub, by installation.\n";
        out += "# TYPE octopage_github_requests_total counter\n";
        for (installation, requests, _) in installations {
            let _ = writeln!(
                out,
                "octopage_github_requests_total{{installation=\"{installation}\"}} {requests}"
            );
        }
        out += "# HELP octopage_github_waits_total Requests to GitHub that waited for their installation's budget.\n";
        out += "# TYPE octopage_github_waits_total counter\n";
        for (installation, _, waited) in installations {
            let _ = writeln!(
                out,
                "octopage_github_waits_total{{installation=\"{installation}\"}} {waited}"
            );
        }
        out
    }
}
