//! Shared exponential-backoff helper used by the retry and supervisor
//! modules.
//!
//! Crate-internal: the public API surfaces its own tuning knobs
//! (`RetryPolicy`, `Supervisor`) and delegates the actual math to
//! [`compute_delay`] so that the formula exists in exactly one place.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Compute one step of exponential backoff with optional full jitter.
///
/// Returns `base * 2^(attempt-1)`, capped at `max`, optionally reduced
/// to a uniform random value in `[0, capped]` via
/// [`full_jitter`].
///
/// # Arguments
/// * `base`    — minimum duration (returned at `attempt == 1` with
///   jitter disabled).
/// * `max`     — upper cap applied after the exponential expansion.
/// * `attempt` — 1-based attempt count. `0` is treated as `1`.
/// * `jitter`  — when `true`, pass the capped value through
///   [`full_jitter`].
pub(crate) fn compute_delay(base: Duration, max: Duration, attempt: u32, jitter: bool) -> Duration {
    // Left-shift is equivalent to multiplying by 2^(attempt-1).
    // `checked_shl` returns `None` when the shift count exceeds 31,
    // which saturates to `u32::MAX`. `saturating_sub` prevents
    // underflow when `attempt` is 0.
    let factor = 1u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX);
    let uncapped = base.saturating_mul(factor);
    let capped = uncapped.min(max);
    if jitter { full_jitter(capped) } else { capped }
}

/// Draw a duration uniformly from `[0, capped]` using the current
/// wall-clock nanoseconds as a cheap entropy source.
///
/// Not cryptographically random. Known limitation: two call sites
/// firing within the same nanosecond (e.g. synchronised replicas
/// crash-looping) produce the same delay, which dilutes the
/// desynchronisation property of full jitter.
fn full_jitter(capped: Duration) -> Duration {
    let capped_nanos = u64::try_from(capped.as_nanos()).unwrap_or(u64::MAX);
    if capped_nanos == 0 {
        return Duration::ZERO;
    }
    let source = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Duration::from_nanos(source % capped_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturates_at_cap_without_jitter() {
        let base = Duration::from_millis(100);
        let cap = Duration::from_secs(1);
        assert_eq!(
            compute_delay(base, cap, 1, false),
            Duration::from_millis(100)
        );
        assert_eq!(
            compute_delay(base, cap, 2, false),
            Duration::from_millis(200)
        );
        assert_eq!(
            compute_delay(base, cap, 3, false),
            Duration::from_millis(400)
        );
        assert_eq!(
            compute_delay(base, cap, 4, false),
            Duration::from_millis(800)
        );
        assert_eq!(compute_delay(base, cap, 5, false), cap);
        assert_eq!(compute_delay(base, cap, 1_000, false), cap);
    }

    #[test]
    fn zero_attempt_is_treated_as_one() {
        let base = Duration::from_millis(50);
        let cap = Duration::from_secs(1);
        assert_eq!(compute_delay(base, cap, 0, false), base);
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let base = Duration::from_millis(100);
        let cap = Duration::from_secs(1);
        for attempt in 1..=8 {
            let d = compute_delay(base, cap, attempt, true);
            assert!(d <= cap, "jitter must not exceed cap: {d:?}");
        }
    }
}
