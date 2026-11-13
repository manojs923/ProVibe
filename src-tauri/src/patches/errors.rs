// src-tauri/src/patches/errors.rs
//
// Error surface for the patch subsystem.
//
// Patches mutate a live guest — a bad one can leave a VM half-spoofed and
// unusable. Failures therefore carry which stage died and which patch, so a
// caller can decide between retry, rollback, and giving up. `String` was the
// original shape; the conversions at the bottom keep those call sites working.

use thiserror::Error;

/// A patch-subsystem failure.
#[derive(Debug, Error)]
pub enum PatchError {
    /// A patch definition could not be read or parsed.
    #[error("failed to load patch '{patch}': {reason}")]
    Load { patch: String, reason: String },

    /// A patch ran but did not complete.
    #[error("failed to apply patch '{patch}': {reason}")]
    Apply { patch: String, reason: String },

    /// A patch ran, but read-back confirmed it did not take effect.
    #[error("patch '{patch}' applied but verification failed: {reason}")]
    Verify { patch: String, reason: String },

    /// No patch exists under that identifier.
    #[error("patch not found: {0}")]
    NotFound(String),

    /// The target was not in a state that permits the operation.
    #[error("invalid state for '{operation}': {state}")]
    InvalidState { operation: String, state: String },

    /// Host I/O fault, usually while reading a patch payload off disk.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Anything that doesn't warrant its own variant yet.
    #[error("{0}")]
    Other(String),
}

/// Result alias for patch operations.
pub type PatchResult<T> = Result<T, PatchError>;

impl PatchError {
    /// Convenience for the `Other` variant.
    pub fn other(msg: impl Into<String>) -> Self {
        PatchError::Other(msg.into())
    }

    /// True when retrying the same operation could plausibly succeed.
    ///
    /// `Load` and `InvalidState` are deterministic — retrying re-runs the same
    /// failing path. `Io` and `Apply` often are not.
    pub fn is_retryable(&self) -> bool {
        matches!(self, PatchError::Io(_) | PatchError::Apply { .. })
    }

    /// True when the failure leaves the target in a modified state, so a
    /// rollback is warranted rather than a plain retry.
    pub fn requires_rollback(&self) -> bool {
        matches!(
            self,
            PatchError::Apply { .. } | PatchError::Verify { .. }
        )
    }
}

impl From<String> for PatchError {
    fn from(value: String) -> Self {
        PatchError::Other(value)
    }
}

impl From<&str> for PatchError {
    fn from(value: &str) -> Self {
        PatchError::Other(value.to_string())
    }
}

/// Downgrade to the legacy string form, for callers still returning `String`.
impl From<PatchError> for String {
    fn from(value: PatchError) -> Self {
        value.to_string()
    }
}

/// Legacy result type: `Result<(), String>`.
///
/// Kept because existing call sites import it. Prefer [`PatchResult`] for new
/// code — this form discards the variant, so `is_retryable` and
/// `requires_rollback` can't be consulted.
#[allow(non_camel_case_types)]
pub type patch_error = Result<(), String>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_errors_are_retryable_and_rollback() {
        let err = PatchError::Apply {
            patch: "smbios".into(),
            reason: "WMI timeout".into(),
        };
        assert!(err.is_retryable());
        assert!(err.requires_rollback());
    }

    #[test]
    fn verify_errors_roll_back_but_do_not_retry() {
        let err = PatchError::Verify {
            patch: "mac".into(),
            reason: "readback mismatch".into(),
        };
        assert!(!err.is_retryable());
        assert!(err.requires_rollback());
    }

    #[test]
    fn load_and_state_errors_are_deterministic() {
        assert!(!PatchError::NotFound("x".into()).is_retryable());
        assert!(!PatchError::InvalidState {
            operation: "start".into(),
            state: "Off".into()
        }
        .requires_rollback());
    }

    #[test]
    fn io_errors_convert_and_stay_retryable() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "slow");
        let err: PatchError = io.into();
        assert!(err.is_retryable());
        assert!(!err.requires_rollback());
    }

    #[test]
    fn display_names_the_patch() {
        let err = PatchError::Apply {
            patch: "openprocess_hook".into(),
            reason: "Add-Type failed".into(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("openprocess_hook"));
        assert!(rendered.contains("Add-Type failed"));
    }

    #[test]
    fn string_conversions_round_trip() {
        let err: PatchError = "boom".into();
        let as_string: String = err.into();
        assert_eq!(as_string, "boom");

        let from_owned: PatchError = String::from("owned").into();
        assert!(matches!(from_owned, PatchError::Other(_)));
    }

    #[test]
    fn legacy_alias_still_usable() {
        fn legacy() -> patch_error {
            Ok(())
        }
        assert!(legacy().is_ok());
    }
}