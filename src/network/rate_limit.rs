use std::num::NonZeroU32;
use std::time::Duration;

/// Time window used to express a rate limit.
///
/// A window specifies how many cells (requests) are replenished over a
/// given period. Combined with an optional burst, it is turned into a
/// [`governor::Quota`].
///
/// Marked `#[non_exhaustive]` so new variants can be added in the future
/// without breaking downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RateLimitWindow {
    /// Replenish `allowed` cells every second.
    PerSecond(NonZeroU32),
    /// Replenish `allowed` cells every minute.
    PerMinute(NonZeroU32),
    /// Replenish exactly one cell every `period`. The bucket size can be
    /// grown with a `burst` value at construction time. `period` must be
    /// strictly greater than zero.
    Custom { period: Duration },
}
