//! Sanitizer for repository-derived text. Branch names, commit messages,
//! authors, remote names/URLs, tags and paths inside a repository are all
//! attacker-controlled: a hostile repo can embed ANSI escapes, C0/C1 control
//! characters, or Unicode bidi overrides to spoof or scramble the UI. Every
//! such string passes through here before it reaches a renderable type.

/// Explicit bidi/invisible formatting characters that can reorder or hide
/// rendered text (Trojan Source class attacks).
fn is_bidi_or_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}'   // LRE, RLE, PDF, LRO, RLO
        | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
        | '\u{061C}'              // Arabic letter mark
        | '\u{200E}' | '\u{200F}' // LRM, RLM
        | '\u{FEFF}'              // zero-width no-break space / BOM
    )
}

/// Strip control characters, ANSI escape sequences, and bidi overrides, then
/// cap the result at `max_chars` characters (appending `…` when truncated).
///
/// - C0 controls (including ESC, so ANSI sequences lose their trigger byte),
///   C1 controls, and DEL are removed. `\r` is always removed.
/// - `\n` and `\t` are kept only when `keep_newlines` is true (commit message
///   bodies); single-line fields collapse them to a space.
/// - The cap counts `char`s, not bytes, so multi-byte text truncates cleanly.
pub fn sanitize_git_text(raw: &str, max_chars: usize, keep_newlines: bool) -> String {
    let mut out = String::with_capacity(raw.len().min(max_chars * 4));
    let mut count = 0usize;
    let mut truncated = false;

    for c in raw.chars() {
        let mapped = match c {
            '\r' => continue,
            '\n' | '\t' if keep_newlines => c,
            '\n' | '\t' => ' ',
            c if c.is_control() => continue, // C0 (incl. ESC), C1, DEL
            c if is_bidi_or_invisible(c) => continue,
            c => c,
        };
        if count >= max_chars {
            truncated = true;
            break;
        }
        out.push(mapped);
        count += 1;
    }
    if truncated {
        out.push('…');
    }
    out
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
    if name.split('/').any(|part| part.is_empty() || part.starts_with('.')) {
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
