// src-tauri/src/util/powershell.rs
//
// A PowerShell script body, wrapped so signatures read as intent rather than
// `&str` / `String`.
//
// This is deliberately a thin type — it does not execute anything. Execution
// lives in `stealth::cpuid` (host) and `hyper_v::vm_manager` (guest), which
// take this so a bare path or VM name can never be passed where a script is
// expected.
//
// It is also the mock boundary for guest execution: `VmManagerOps` takes it by
// value, so tests construct scripts and assert on their contents.

use std::fmt;
use std::ops::Deref;

use crate::util::{escape_ps_here_string, escape_ps_single_quote, ps_literal};

/// A PowerShell script body.
///
/// Construct with [`PowerShellScript::new`]. Use [`Self::as_string`] for
/// inspection or [`Self::into_string`] to hand ownership to an executor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PowerShellScript {
    source: String,
    label: Option<String>,
}

impl PowerShellScript {
    /// Wrap a script body.
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            label: None,
        }
    }

    /// Attach a short label for tracing. The label never reaches PowerShell.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The script body.
    #[must_use]
    pub fn as_string(&self) -> &str {
        &self.source
    }

    /// Consume the wrapper, yielding the body.
    #[must_use]
    pub fn into_string(self) -> String {
        self.source
    }

    /// Optional tracing label.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// Body length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.source.len()
    }

    /// True when the body is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    /// True when the body contains `needle`, for cheap assertions and dispatch.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.source.contains(needle)
    }

    /// Rewrite a single-quoted host-side value into this script.
    ///
    /// Replaces every `{{name}}` placeholder with a correctly escaped
    /// single-quoted literal. Keeps escaping out of the format string, where a
    /// stray apostrophe in a path or VM name would break the script.
    #[must_use]
    pub fn bind(mut self, name: &str, value: &str) -> Self {
        let placeholder = format!("{{{{{}}}}}", name);
        self.source = self.source.replace(&placeholder, &ps_literal(value));
        self
    }

    /// Escape the body for embedding in a single-quoted here-string.
    ///
    /// Use when nesting this script inside another script, as `examity.rs` does
    /// when it ships a guest payload through a host wrapper.
    #[must_use]
    pub fn as_here_string_body(&self) -> String {
        escape_ps_here_string(&self.source)
    }

    /// Escape the body for embedding in a single-quoted PowerShell string.
    #[must_use]
    pub fn as_quoted_literal(&self) -> String {
        escape_ps_single_quote(&self.source)
    }
}

impl Deref for PowerShellScript {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.source
    }
}

impl AsRef<str> for PowerShellScript {
    fn as_ref(&self) -> &str {
        &self.source
    }
}

impl From<String> for PowerShellScript {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for PowerShellScript {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for PowerShellScript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_exposes_source() {
        let script = PowerShellScript::new("Get-VM");
        assert_eq!(script.as_string(), "Get-VM");
        assert_eq!(script.into_string(), "Get-VM");
    }

    #[test]
    fn accepts_owned_and_borrowed() {
        let owned = PowerShellScript::new(String::from("Get-VM"));
        let borrowed = PowerShellScript::new("Get-VM");
        assert_eq!(owned, borrowed);
        assert_eq!(PowerShellScript::from("Get-VM"), borrowed);
    }

    #[test]
    fn label_is_separate_from_body() {
        let script = PowerShellScript::new("Get-VM").with_label("probe");
        assert_eq!(script.label(), Some("probe"));
        assert_eq!(script.as_string(), "Get-VM");
    }

    #[test]
    fn empty_detection() {
        assert!(PowerShellScript::new("").is_empty());
        assert!(!PowerShellScript::new("x").is_empty());
    }

    #[test]
    fn contains_and_len() {
        let script = PowerShellScript::new("artifactPaths");
        assert!(script.contains("artifact"));
        assert_eq!(script.len(), 13);
    }

    #[test]
    fn deref_reaches_str_methods() {
        let script = PowerShellScript::new("Get-VM");
        assert!(script.starts_with("Get"));
    }

    #[test]
    fn bind_escapes_single_quotes() {
        let script = PowerShellScript::new("Set-VM -Name {{vm}}").bind("vm", "it's a vm");
        assert_eq!(script.as_string(), "Set-VM -Name 'it''s a vm'");
    }

    #[test]
    fn bind_replaces_every_occurrence() {
        let script = PowerShellScript::new("{{a}} and {{a}}").bind("a", "x");
        assert_eq!(script.as_string(), "'x' and 'x'");
    }

    #[test]
    fn bind_leaves_unknown_placeholders_alone() {
        let script = PowerShellScript::new("{{a}} {{b}}").bind("a", "x");
        assert_eq!(script.as_string(), "'x' {{b}}");
    }

    #[test]
    fn bind_does_not_interpret_replacement_metacharacters() {
        // A `$` in the value must not become a backreference.
        let script = PowerShellScript::new("{{p}}").bind("p", "$1\\x");
        assert_eq!(script.as_string(), "'$1\\x'");
    }

    #[test]
    fn here_string_body_neutralises_terminator() {
        let script = PowerShellScript::new("one\n'@ two");
        assert_eq!(script.as_here_string_body(), "one\n '@ two");
    }

    #[test]
    fn quoted_literal_doubles_quotes() {
        let script = PowerShellScript::new("it's");
        assert_eq!(script.as_quoted_literal(), "it''s");
    }

    #[test]
    fn display_matches_source() {
        assert_eq!(PowerShellScript::new("Get-VM").to_string(), "Get-VM");
    }
}