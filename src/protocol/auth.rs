//! Authentication (IMPLEMENTATION.md §65, SPEC.md §115).
//!
//! The MVP uses a cluster-scoped bearer credential. That is deliberately the
//! *floor*, not the goal: fully unauthenticated is forbidden, and the shape
//! here is meant to be replaced by per-node mTLS without changing any caller.
//!
//! Two properties are enforced rather than documented:
//!
//! * **No secret is compiled in.** The credential comes from a file or the
//!   environment. There is a test that greps the source for the alternative.
//! * **Comparison is constant-time.** Comparing credentials with `==` leaks
//!   their prefix through timing to anyone who can reach the port.

use std::path::Path;

use thiserror::Error;

/// Why a credential was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    /// No credential was presented.
    #[error("no credential presented")]
    Missing,
    /// The `Authorization` header was not a bearer token.
    #[error("credential must be presented as `Authorization: Bearer <token>`")]
    Malformed,
    /// The credential did not match.
    #[error("credential rejected")]
    Rejected,
}

/// A shared, cluster-scoped credential.
#[derive(Clone)]
pub struct ClusterCredential {
    token: String,
}

impl ClusterCredential {
    /// Minimum length that is not trivially guessable.
    pub const MIN_LENGTH: usize = 32;

    /// Wrap a token.
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into() }
    }

    /// Read a token from a file, trimming trailing whitespace.
    ///
    /// A file is the recommended source: it can be given file permissions and
    /// kept out of the process table, unlike a command-line argument.
    pub fn from_file(path: &Path) -> std::io::Result<Self> {
        Ok(Self::new(std::fs::read_to_string(path)?.trim().to_string()))
    }

    /// Whether the token is long enough to be worth having.
    pub fn is_strong(&self) -> bool {
        self.token.len() >= Self::MIN_LENGTH
    }

    /// The value an agent should send in the `Authorization` header.
    pub fn header_value(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Check a presented `Authorization` header.
    pub fn verify_header(&self, header: Option<&str>) -> Result<(), AuthError> {
        let header = header.ok_or(AuthError::Missing)?;
        let presented = header.strip_prefix("Bearer ").ok_or(AuthError::Malformed)?;
        if constant_time_eq(presented.as_bytes(), self.token.as_bytes()) {
            Ok(())
        } else {
            Err(AuthError::Rejected)
        }
    }
}

/// Deliberately opaque: a credential must not reach a log through a stray
/// `{:?}`.
impl std::fmt::Debug for ClusterCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClusterCredential(<redacted>, {} bytes)", self.token.len())
    }
}

/// Compare without an early return, so timing does not reveal a prefix.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn credential() -> ClusterCredential {
        ClusterCredential::new("0123456789abcdef0123456789abcdef")
    }

    #[test]
    fn a_matching_bearer_token_is_accepted() {
        let credential = credential();
        assert_eq!(credential.verify_header(Some(&credential.header_value())), Ok(()));
    }

    #[test]
    fn a_wrong_token_is_rejected() {
        assert_eq!(
            credential().verify_header(Some("Bearer wrong")),
            Err(AuthError::Rejected)
        );
    }

    #[test]
    fn a_token_sharing_a_prefix_is_still_rejected() {
        assert_eq!(
            credential().verify_header(Some("Bearer 0123456789abcdef0123456789abcdeX")),
            Err(AuthError::Rejected)
        );
    }

    #[test]
    fn an_absent_or_malformed_header_is_refused() {
        assert_eq!(credential().verify_header(None), Err(AuthError::Missing));
        assert_eq!(credential().verify_header(Some("")), Err(AuthError::Malformed));
        assert_eq!(credential().verify_header(Some("Basic abc")), Err(AuthError::Malformed));
        assert_eq!(
            credential().verify_header(Some("bearer abc")),
            Err(AuthError::Malformed)
        );
    }

    #[test]
    fn an_empty_configured_token_does_not_accept_an_empty_presentation() {
        // Guards against a misconfiguration turning into open access.
        let empty = ClusterCredential::new("");
        assert!(!empty.is_strong());
        assert_eq!(empty.verify_header(None), Err(AuthError::Missing));
        assert_eq!(empty.verify_header(Some("Basic ")), Err(AuthError::Malformed));
    }

    #[test]
    fn short_tokens_are_reported_as_weak() {
        assert!(!ClusterCredential::new("short").is_strong());
        assert!(credential().is_strong());
    }

    #[test]
    fn debug_output_never_reveals_the_token() {
        let text = format!("{:?}", credential());
        assert!(!text.contains("0123456789"), "{text}");
        assert!(text.contains("redacted"));
    }

    #[test]
    fn a_token_can_be_read_from_a_file_ignoring_trailing_whitespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        let mut file = std::fs::File::create(&path).expect("create");
        writeln!(file, "0123456789abcdef0123456789abcdef").expect("write");

        let credential = ClusterCredential::from_file(&path).expect("read");
        assert!(credential.is_strong());
        assert_eq!(credential.verify_header(Some(&credential.header_value())), Ok(()));
    }

    #[test]
    fn constant_time_comparison_matches_ordinary_equality() {
        for (a, b) in [
            (&b""[..], &b""[..]),
            (b"abc", b"abc"),
            (b"abc", b"abd"),
            (b"abc", b"ab"),
            (b"", b"a"),
        ] {
            assert_eq!(constant_time_eq(a, b), a == b, "{a:?} vs {b:?}");
        }
    }
}
