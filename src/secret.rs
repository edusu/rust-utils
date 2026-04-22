//! Opaque wrapper that redacts secrets in `Debug` and `Display` output.
//!
//! Use [`Secret`] to wrap API tokens, passwords, cryptographic keys,
//! and anything else that must not end up in logs. Access to the inner
//! value is only possible through the explicit [`Secret::expose`] and
//! [`Secret::into_inner`] methods.
//!
//! # What this protects against
//! * Accidental `format!("{:?}", …)` / `format!("{}", …)`.
//! * `dbg!(secret)` statements left in during development.
//! * Logs via `tracing` / `log` where fields are rendered with `Debug`.
//!
//! # What this does NOT protect against
//! * Serialization via `serde`. `Secret` intentionally does not
//!   implement `Serialize` / `Deserialize`.
//! * Core dumps, debugger inspection, or `/proc/<pid>/mem`.
//! * Memory residue after drop. For zero-on-drop semantics, wrap the
//!   inner value in `Zeroizing<T>` from the `zeroize` crate.
//! * Cloning the inner value out with [`expose`](Secret::expose) or
//!   [`into_inner`](Secret::into_inner) and then formatting it.
//!
//! # Example
//! ```
//! use rust_utils::secret::Secret;
//!
//! let token = Secret::new(String::from("sk_live_abcdef"));
//! assert_eq!(format!("{token}"), "[REDACTED]");
//! assert!(!format!("{token:?}").contains("sk_live"));
//! // Explicit access when you really need the value:
//! assert!(token.expose().starts_with("sk_"));
//! ```

use std::fmt;

/// Wrapper that masks its inner value in `Debug`/`Display` output.
///
/// Construction is via [`Secret::new`] or `Secret::from(value)`.
/// Access to the inner value requires the explicit
/// [`expose`](Self::expose) or [`into_inner`](Self::into_inner)
/// method, making accidental leaks visible at the call site.
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wrap a value as a secret.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Borrow the inner value.
    ///
    /// Prefer short-lived access: the longer a reference lives, the
    /// higher the chance it ends up formatted by mistake.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Consume the wrapper and return the inner value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Clone> Clone for Secret<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T: AsRef<[u8]>> Secret<T> {
    /// Constant-time equality comparison of two byte-convertible
    /// secrets.
    ///
    /// Returns `false` immediately when the two byte slices have
    /// different lengths — length is not considered secret. When the
    /// lengths match, the comparison runs in time proportional to
    /// `len`, independently of where the bytes differ, which prevents
    /// timing side channels on the content.
    pub fn ct_eq(&self, other: &Self) -> bool {
        constant_time_eq(self.0.as_ref(), other.0.as_ref())
    }
}

/// XOR-accumulated comparison. Runs for `len` iterations; never
/// short-circuits on a byte mismatch.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_reveal_inner_value() {
        let s = Secret::new(String::from("super-secret-token"));
        let rendered = format!("{s:?}");
        assert!(!rendered.contains("super-secret-token"));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn display_shows_redacted_placeholder() {
        let s = Secret::new(String::from("super-secret-token"));
        assert_eq!(format!("{s}"), "[REDACTED]");
    }

    #[test]
    fn expose_returns_inner_reference() {
        let s = Secret::new(42_u32);
        assert_eq!(*s.expose(), 42);
    }

    #[test]
    fn into_inner_yields_ownership() {
        let s = Secret::new(String::from("value"));
        assert_eq!(s.into_inner(), "value");
    }

    #[test]
    fn clone_preserves_inner_value() {
        let s = Secret::new(String::from("x"));
        let c = s.clone();
        assert_eq!(c.expose(), "x");
    }

    #[test]
    fn from_value_constructs_secret() {
        let s: Secret<&str> = "hi".into();
        assert_eq!(*s.expose(), "hi");
    }

    #[test]
    fn ct_eq_matches_equal_secrets() {
        let a: Secret<Vec<u8>> = Secret::new(b"hello".to_vec());
        let b: Secret<Vec<u8>> = Secret::new(b"hello".to_vec());
        assert!(a.ct_eq(&b));
    }

    #[test]
    fn ct_eq_rejects_differing_content() {
        let a: Secret<Vec<u8>> = Secret::new(b"hello".to_vec());
        let b: Secret<Vec<u8>> = Secret::new(b"world".to_vec());
        assert!(!a.ct_eq(&b));
    }

    #[test]
    fn ct_eq_rejects_differing_lengths() {
        let a: Secret<Vec<u8>> = Secret::new(b"hello".to_vec());
        let b: Secret<Vec<u8>> = Secret::new(b"hello!".to_vec());
        assert!(!a.ct_eq(&b));
    }
}
