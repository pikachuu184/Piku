//! Git-specific text handling: branch-name validation, plus a compatibility
//! alias for the shared display sanitizer.
//!
//! Repository-derived text (branch names, commit messages, authors, remote
//! URLs, paths inside a repo) is attacker-controlled, and every such string
//! passes through [`sanitize_git_text`] before reaching a renderable type.
//! The sanitizer itself now lives in [`crate::security::text`] because the
//! same problem applies to filenames, archive entries, and audio tags.

/// Strip control characters, ANSI escape sequences, and bidi overrides, then
/// cap the result at `max_chars` characters (appending `…` when truncated).
///
/// Thin alias for [`crate::security::text::sanitize_display`]. The git layer
/// keeps calling it under this name so that widening the sanitizer did not
/// require touching `services/git/*`, which is otherwise stable.
pub fn sanitize_git_text(raw: &str, max_chars: usize, keep_newlines: bool) -> String {
    crate::security::text::sanitize_display(raw, max_chars, keep_newlines)
}

/// Validate a branch name the *user* typed before it is handed to the git
/// backend. Deliberately stricter than git's own ref-name rules: ASCII
/// alphanumerics plus `-._/` only, no leading/trailing separators, no `..`,
/// no `@{`, no component starting with a dot, length-capped.
pub fn validate_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("branch name is empty".into());
    }
    if name.len() > 200 {
        return Err("branch name is too long".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '/'))
    {
        return Err("only letters, digits, `-`, `.`, `_` and `/` are allowed".into());
    }
    if name.starts_with(['-', '.', '/']) || name.ends_with(['.', '/']) {
        return Err("branch name cannot start or end with a separator".into());
    }
    if name.ends_with(".lock") {
        return Err("branch name cannot end with `.lock`".into());
    }
    if name.contains("..") || name.contains("//") || name.contains("@{") {
        return Err("branch name contains a forbidden sequence".into());
    }
    if name
        .split('/')
        .any(|part| part.is_empty() || part.starts_with('.'))
    {
        return Err("branch name has an empty or dot-leading component".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_escape_sequences() {
        // ESC is a control char, so the sequence loses its trigger byte.
        let s = sanitize_git_text("main\u{1b}[31mevil\u{1b}[0m", 100, false);
        assert_eq!(s, "main[31mevil[0m");
    }

    #[test]
    fn strips_bidi_overrides() {
        let s = sanitize_git_text("safe\u{202E}txt.exe\u{202C}", 100, false);
        assert_eq!(s, "safetxt.exe");
    }

    #[test]
    fn strips_c0_c1_and_del() {
        let s = sanitize_git_text("a\u{0007}b\u{007F}c\u{0085}d", 100, false);
        assert_eq!(s, "abcd");
    }

    #[test]
    fn caps_length_with_ellipsis() {
        let s = sanitize_git_text(&"x".repeat(50), 10, false);
        assert_eq!(s.chars().count(), 11);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn multiline_only_when_asked() {
        assert_eq!(sanitize_git_text("a\nb\tc\r", 100, false), "a b c");
        assert_eq!(sanitize_git_text("a\nb\tc\r", 100, true), "a\nb\tc");
    }

    #[test]
    fn multibyte_truncation_is_clean() {
        let s = sanitize_git_text("héllo wörld", 5, false);
        assert_eq!(s, "héllo…");
    }

    #[test]
    fn branch_name_validation() {
        assert!(validate_branch_name("feature/login-2").is_ok());
        assert!(validate_branch_name("v1.2.3").is_ok());
        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("-flag").is_err());
        assert!(validate_branch_name("a..b").is_err());
        assert!(validate_branch_name("a//b").is_err());
        assert!(validate_branch_name("HEAD@{1}").is_err());
        assert!(validate_branch_name("refs/../escape").is_err());
        assert!(validate_branch_name("name.lock").is_err());
        assert!(validate_branch_name("dir/.hidden").is_err());
        assert!(validate_branch_name("spa ce").is_err());
        assert!(validate_branch_name("uni\u{202E}code").is_err());
    }
}
