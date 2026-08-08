//! What the status bar says while the agent is working.
//!
//! Three things a waiting user actually wants: that something is still
//! happening, how long it has been, and how much has been spent. The verb and
//! spinner answer the first, and rotate so a stalled render is distinguishable
//! from a slow turn.
//!
//! The token count steps rather than ticks, because that is the truth available
//! — providers report usage once per model reply, not per token, so a smooth
//! counter would have to be invented from the character stream. A number that
//! holds still and then jumps at a real boundary is worth more than a
//! plausible one that is made up.

use std::time::{Duration, Instant};

/// Rotated rather than fixed, so a wedged UI is visibly different from a turn
/// that is merely taking a while.
const VERBS: [&str; 12] = [
    "Thinking",
    "Pondering",
    "Noodling",
    "Working",
    "Cooking",
    "Musing",
    "Churning",
    "Digging",
    "Puzzling",
    "Brewing",
    "Whirring",
    "Tinkering",
];

const VERB_ROTATE: Duration = Duration::from_secs(6);
const SECS_PER_MIN: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MIN;

/// Live state of the turn in progress.
#[derive(Clone, Copy)]
pub struct Activity {
    pub started: Instant,
    /// Output tokens across every model reply in this turn so far.
    pub output_tokens: u32,
    /// The run this belongs to, so consecutive turns do not open on the same
    /// word.
    pub seed: u64,
}

impl Activity {
    pub fn label(&self, now: Instant) -> String {
        let elapsed = now.saturating_duration_since(self.started);
        let verb = verb(self.seed, elapsed);
        let time = format_elapsed(elapsed);
        if self.output_tokens == 0 {
            // Nothing has come back yet, and "↓ 0 tokens" reads like a fault
            // rather than a turn that has only just started.
            return format!("{verb}… ({time})");
        }
        format!(
            "{verb}… ({time} · ↓ {} tokens)",
            format_tokens(self.output_tokens)
        )
    }
}

fn verb(seed: u64, elapsed: Duration) -> &'static str {
    let step = elapsed.as_secs() / VERB_ROTATE.as_secs();
    VERBS[(seed.wrapping_add(step) % VERBS.len() as u64) as usize]
}

/// Minutes appear only once there are minutes, so a short turn is not padded
/// with a leading `0m`.
fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < SECS_PER_MIN {
        return format!("{secs}s");
    }
    if secs < SECS_PER_HOUR {
        return format!("{}m {}s", secs / SECS_PER_MIN, secs % SECS_PER_MIN);
    }
    format!(
        "{}h {}m",
        secs / SECS_PER_HOUR,
        (secs % SECS_PER_HOUR) / SECS_PER_MIN
    )
}

/// One decimal place, and never a trailing `.0`: the count is a sense of
/// scale, not a figure anyone reconciles against a bill.
fn format_tokens(tokens: u32) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => trim_decimal(tokens as f64 / 1_000.0, 'k'),
        _ => trim_decimal(tokens as f64 / 1_000_000.0, 'M'),
    }
}

fn trim_decimal(value: f64, suffix: char) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if (rounded.fract()).abs() < f64::EPSILON {
        format!("{}{suffix}", rounded as u64)
    } else {
        format!("{rounded:.1}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(0,     "0s"      ; "just_started")]
    #[test_case(45,    "45s"     ; "under_a_minute")]
    #[test_case(60,    "1m 0s"   ; "exactly_a_minute")]
    #[test_case(85,    "1m 25s"  ; "the_example_from_the_ask")]
    #[test_case(3599,  "59m 59s" ; "just_under_an_hour")]
    #[test_case(3600,  "1h 0m"   ; "exactly_an_hour")]
    #[test_case(3960,  "1h 6m"   ; "over_an_hour")]
    fn elapsed_reads_naturally(secs: u64, expected: &str) {
        assert_eq!(format_elapsed(Duration::from_secs(secs)), expected);
    }

    #[test_case(0,         "0"     ; "none")]
    #[test_case(999,       "999"   ; "under_a_thousand")]
    #[test_case(1_000,     "1k"    ; "exactly_a_thousand")]
    #[test_case(5_700,     "5.7k"  ; "the_example_from_the_ask")]
    #[test_case(5_749,     "5.7k"  ; "rounds_down")]
    #[test_case(5_750,     "5.8k"  ; "rounds_up")]
    #[test_case(12_000,    "12k"   ; "no_trailing_zero")]
    #[test_case(999_999,   "1000k" ; "just_under_a_million")]
    #[test_case(2_500_000, "2.5M"  ; "millions")]
    fn tokens_read_as_a_scale(tokens: u32, expected: &str) {
        assert_eq!(format_tokens(tokens), expected);
    }

    fn activity(secs: u64, tokens: u32) -> (Activity, Instant) {
        let now = Instant::now();
        let started = now - Duration::from_secs(secs);
        (
            Activity {
                started,
                output_tokens: tokens,
                seed: 0,
            },
            now,
        )
    }

    /// The verb is deliberately not pinned here: it rotates, and a test that
    /// froze it would fail for the wrong reason.
    #[test]
    fn the_label_matches_the_shape_asked_for() {
        let (a, now) = activity(85, 5_700);
        let label = a.label(now);
        assert!(label.ends_with("… (1m 25s · ↓ 5.7k tokens)"), "{label}");
        assert!(VERBS.iter().any(|v| label.starts_with(v)), "{label}");
    }

    /// Zero is not yet news, and reads like a fault rather than a young turn.
    #[test]
    fn no_tokens_yet_means_no_token_clause() {
        let (a, now) = activity(3, 0);
        let label = a.label(now);
        assert!(label.ends_with("… (3s)"), "{label}");
        assert!(!label.contains('↓'), "{label}");
    }

    #[test]
    fn the_verb_rotates_while_the_turn_runs() {
        let (a, now) = activity(0, 0);
        let first = verb(a.seed, Duration::ZERO);
        let later = verb(a.seed, VERB_ROTATE);
        assert_ne!(first, later);
        // And holds still between rotations, so it is not a flicker.
        assert_eq!(
            verb(a.seed, Duration::ZERO),
            verb(a.seed, VERB_ROTATE - Duration::from_millis(1))
        );
        let _ = now;
    }

    #[test]
    fn consecutive_turns_do_not_open_on_the_same_word() {
        assert_ne!(verb(7, Duration::ZERO), verb(8, Duration::ZERO));
    }

    /// A turn long enough to wrap the list must not index out of bounds.
    #[test]
    fn a_very_long_turn_keeps_rotating() {
        let far = VERB_ROTATE * (VERBS.len() as u32 * 3 + 1);
        assert!(VERBS.contains(&verb(u64::MAX, far)));
    }
}
