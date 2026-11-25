// src-tauri/src/util/mod.rs
//
// Shared, dependency-light helpers used across subsystems.
//
// Scope rules for this module:
//   - Host-side plumbing only: process invocation, quoting, unit and time
//     conversion, path shaping.
//   - No Hyper-V, RDP, or stealth policy. Those modules may use these
//     helpers; the reverse dependency is not allowed.
//   - Anything that can block must say so in its doc comment, so callers
//     know whether it belongs inside an async fn.

pub mod powershell;

pub use powershell::PowerShellScript;

// ---------------------------------------------------------------------------
// Process invocation
// ---------------------------------------------------------------------------

/// `CREATE_NO_WINDOW`.
///
/// This binary is GUI-subsystem and owns no console of its own, so every
/// `powershell.exe` it launches gets a brand new conhost window. On a status
/// poll that reads as a terminal flickering open and closed every second and a
/// half. Every spawn in the crate goes through [`hidden_command`] — or applies
/// this flag to its own tokio command — so none of them flash.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A process builder that never allocates a console window.
pub fn hidden_command(program: &str) -> std::process::Command {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new(program);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd
}

// ---------------------------------------------------------------------------
// PowerShell literal escaping
//
// Every host script in this crate interpolates guest-controlled or
// config-controlled text. These helpers exist so the escaping rules live in
// one place instead of being re-derived per call site — `artifact.rs`,
// `cpuid.rs`, `examity.rs`, and `vm/mod.rs` all hand-rolled a variant of this
// and got it subtly different.
// ---------------------------------------------------------------------------

/// Escape text for use inside a PowerShell **single-quoted** string.
///
/// Inside single quotes the only metacharacter is `'` itself, escaped by
/// doubling. Everything else — `$`, backtick, `\` — is literal.
pub fn escape_ps_single_quote(input: &str) -> String {
    input.replace('\'', "''")
}

/// Wrap text as a ready-to-embed single-quoted PowerShell literal.
pub fn ps_literal(input: &str) -> String {
    format!("'{}'", escape_ps_single_quote(input))
}

/// Escape text for use inside a PowerShell **double-quoted** string.
///
/// Double quotes interpolate, so backtick, `$`, and `"` all need escaping.
/// Backtick is escaped first, otherwise the escapes introduced for `$` and `"`
/// would themselves be escaped.
pub fn escape_ps_double_quote(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '`' => out.push_str("``"),
            '$' => out.push_str("`$"),
            '"' => out.push_str("`\""),
            '\r' => {}
            '\n' => out.push_str("`n"),
            _ => out.push(ch),
        }
    }
    out
}

/// Wrap text as a ready-to-embed double-quoted PowerShell string.
pub fn ps_interpolated(input: &str) -> String {
    format!("\"{}\"", escape_ps_double_quote(input))
}

/// Neutralise a single-quoted here-string (`@'` … `'@`) body.
///
/// A here-string body is entirely literal, so nothing needs escaping — the one
/// hazard is a body line beginning with `'@`, which terminates the string
/// early. Those lines get a single leading space, which is the least invasive
/// fix available (a here-string has no escape syntax).
pub fn escape_ps_here_string(input: &str) -> String {
    if !input.contains("'@") {
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len() + 8);
    for (idx, line) in input.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.starts_with("'@") {
            out.push(' ');
        }
        out.push_str(line);
    }
    out
}

/// PowerShell boolean literal.
pub fn ps_bool(value: bool) -> &'static str {
    if value {
        "$true"
    } else {
        "$false"
    }
}

// ---------------------------------------------------------------------------
// Units
// ---------------------------------------------------------------------------

const BYTES_PER_MB: u64 = 1024 * 1024;

/// Convert a byte count to whole mebibytes.
pub fn bytes_to_mb(bytes: u64) -> u64 {
    bytes / BYTES_PER_MB
}

/// Convert mebibytes to bytes, saturating rather than wrapping.
pub fn mb_to_bytes(mb: u64) -> u64 {
    mb.saturating_mul(BYTES_PER_MB)
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Seconds since the Unix epoch, or 0 if the clock is before it.
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
pub fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Render a path as a single-quoted PowerShell literal, lossily for non-UTF-8.
pub fn path_to_ps_literal(path: &std::path::Path) -> String {
    ps_literal(&path.to_string_lossy())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quote_doubles_quotes_only() {
        assert_eq!(escape_ps_single_quote("a'b"), "a''b");
        assert_eq!(escape_ps_single_quote("$var`tick"), "$var`tick");
    }

    #[test]
    fn literal_wraps_and_escapes() {
        assert_eq!(ps_literal("it's"), "'it''s'");
    }

    #[test]
    fn double_quote_escapes_interpolation() {
        assert_eq!(escape_ps_double_quote("a$b"), "a`$b");
        assert_eq!(escape_ps_double_quote("a\"b"), "a`\"b");
        assert_eq!(escape_ps_double_quote("a`b"), "a``b");
    }

    #[test]
    fn double_quote_backtick_escaped_before_dollar() {
        // A backtick before a dollar must not produce a bare `$`.
        assert_eq!(escape_ps_double_quote("`$"), "```$");
    }

    #[test]
    fn double_quote_collapses_newlines() {
        assert_eq!(escape_ps_double_quote("a\r\nb"), "a`nb");
    }

    #[test]
    fn here_string_untouched_when_no_terminator() {
        let body = "line one\nline two";
        assert_eq!(escape_ps_here_string(body), body);
    }

    #[test]
    fn here_string_indents_terminator_lines() {
        let body = "keep\n'@ sneaky";
        assert_eq!(escape_ps_here_string(body), "keep\n '@ sneaky");
    }

    #[test]
    fn bool_literals() {
        assert_eq!(ps_bool(true), "$true");
        assert_eq!(ps_bool(false), "$false");
    }

    #[test]
    fn byte_conversions_round_trip() {
        assert_eq!(bytes_to_mb(mb_to_bytes(64)), 64);
        assert_eq!(bytes_to_mb(0), 0);
        assert_eq!(bytes_to_mb(BYTES_PER_MB - 1), 0);
    }

    #[test]
    fn mb_conversion_saturates() {
        assert_eq!(mb_to_bytes(u64::MAX), u64::MAX);
    }

    #[test]
    fn path_literal_is_quoted() {
        let rendered = path_to_ps_literal(std::path::Path::new(r"C:\Temp\a b.log"));
        assert_eq!(rendered, r"'C:\Temp\a b.log'");
    }

    #[test]
    fn timestamps_are_plausible() {
        // Later than 2020-01-01, earlier than 2100.
        assert!(unix_seconds() > 1_577_836_800);
        assert!(unix_millis() > 1_577_836_800_000);
    }
}