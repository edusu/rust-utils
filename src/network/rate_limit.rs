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

impl RateLimitWindow {
    /// - `<n>s` → PerSecond(n)
    /// - `<n>m` → PerMinute(n)
    /// - `<n>h` → Custom { period = Duration::from_secs(n * 3600) }
    /// - `<n>d` → Custom { period = Duration::from_secs(n * 86400) }
    pub fn from_string(s: &str) -> Option<Self> {
        if s.is_empty() {
            return None;
        }

        let (num_str, unit) = s.split_at(s.len() - 1);
        let number: u32 = match num_str.parse() {
            Ok(n) if n > 0 => n,
            _ => return None,
        };
        let nonzero = NonZeroU32::new(number)?;

        match unit {
            "s" => Some(RateLimitWindow::PerSecond(nonzero)),
            "m" => Some(RateLimitWindow::PerMinute(nonzero)),
            "h" => {
                let secs = number as u64 * 3600;
                Some(RateLimitWindow::Custom {
                    period: Duration::from_secs(secs),
                })
            }
            "d" => {
                let secs = number as u64 * 86400;
                Some(RateLimitWindow::Custom {
                    period: Duration::from_secs(secs),
                })
            }
            _ => None,
        }
    }
}
