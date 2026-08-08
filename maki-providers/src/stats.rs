//! How each provider actually performed this session.
//!
//! Token counts already live in the session; what was missing is the shape of
//! the wait. Time to first token is the number you feel, and tokens per second
//! is what decides whether a long answer is worth asking for. Both are recorded
//! at the one place every provider's stream passes through, so no provider has
//! to remember to instrument itself.
//!
//! Session-scoped and in memory: these describe this run, not a verdict on a
//! provider forever.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

const MILLIS_PER_SEC: f64 = 1000.0;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderStats {
    pub requests: u64,
    pub errors: u64,
    ttft_millis_total: u64,
    ttft_samples: u64,
    pub ttft_millis_best: Option<u64>,
    output_tokens: u64,
    stream_millis: u64,
}

impl ProviderStats {
    /// `None` until a stream actually produces something, so a provider that
    /// only ever errored does not report a flattering zero.
    pub fn mean_ttft_millis(&self) -> Option<u64> {
        (self.ttft_samples > 0).then(|| self.ttft_millis_total / self.ttft_samples)
    }

    /// Measured from first token to end of stream, not from request start:
    /// including the wait would blend latency into a throughput number.
    pub fn tokens_per_sec(&self) -> Option<f64> {
        (self.stream_millis > 0 && self.output_tokens > 0)
            .then(|| self.output_tokens as f64 * MILLIS_PER_SEC / self.stream_millis as f64)
    }
}

fn store() -> &'static RwLock<BTreeMap<String, ProviderStats>> {
    static STATS: OnceLock<RwLock<BTreeMap<String, ProviderStats>>> = OnceLock::new();
    STATS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

fn entry(slug: &str, f: impl FnOnce(&mut ProviderStats)) {
    let mut guard = store().write().unwrap();
    f(guard.entry(slug.to_string()).or_default());
}

pub fn record_first_token(slug: &str, ttft: Duration) {
    let millis = ttft.as_millis() as u64;
    entry(slug, |s| {
        s.ttft_millis_total += millis;
        s.ttft_samples += 1;
        s.ttft_millis_best = Some(s.ttft_millis_best.map_or(millis, |b| b.min(millis)));
    });
}

/// `stream` is the span from first token to the end, so it pairs with the
/// tokens that arrived in it.
pub fn record_success(slug: &str, output_tokens: u32, stream: Duration) {
    entry(slug, |s| {
        s.requests += 1;
        s.output_tokens += u64::from(output_tokens);
        s.stream_millis += stream.as_millis() as u64;
    });
}

/// Only terminal failures. A retryable blip that later succeeds is not an
/// outage the user suffered.
pub fn record_error(slug: &str) {
    entry(slug, |s| {
        s.requests += 1;
        s.errors += 1;
    });
}

pub fn snapshot() -> BTreeMap<String, ProviderStats> {
    store().read().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLUG: &str = "stats-test-provider";
    const OTHER_SLUG: &str = "stats-test-other";

    #[test]
    fn ttft_tracks_mean_and_best() {
        record_first_token(SLUG, Duration::from_millis(300));
        record_first_token(SLUG, Duration::from_millis(100));

        let stats = snapshot().remove(SLUG).expect("recorded");
        assert_eq!(stats.mean_ttft_millis(), Some(200));
        assert_eq!(stats.ttft_millis_best, Some(100));
    }

    #[test]
    fn throughput_divides_tokens_by_stream_time() {
        record_success(OTHER_SLUG, 250, Duration::from_millis(500));

        let stats = snapshot().remove(OTHER_SLUG).expect("recorded");
        assert_eq!(stats.tokens_per_sec(), Some(500.0));
    }

    #[test]
    fn untouched_provider_reports_nothing_rather_than_zero() {
        let stats = ProviderStats::default();
        assert_eq!(stats.mean_ttft_millis(), None);
        assert_eq!(stats.tokens_per_sec(), None);
    }
}
