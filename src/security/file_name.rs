//! Validation for a single user-typed path component (rename / new folder).
//! Lives in the security layer so both the UI dialogs and the job queue can
//! enforce the same rules (defense in depth).

use crate::security::path_guard::is_reserved_name;

/// Windows-invalid filename characters.
const INVALID_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Maximum length for a single path component (NTFS limit).
const MAX_NAME_LEN: usize = 255;

/// Validate a single path component typed by the user. Returns a
/// user-presentable error message on failure.
pub fn validate_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Name cannot be empty".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("Name cannot be `.` or `..`".into());
    }
    if trimmed.chars().count() > MAX_NAME_LEN {
        return Err(format!("Names are limited to {MAX_NAME_LEN} characters"));
    }
    if trimmed.chars().any(|c| INVALID_CHARS.contains(&c)) {
        return Err("Names cannot contain  < > : \" / \\ | ? *".into());
    }
    if trimmed.chars().any(char::is_control) {
        return Err("Names cannot contain control characters".into());
    }
    // `is_control` is Unicode category `Cc` only, so every bidi override and
    // zero-width character passed the check above. Those are exactly what a
    // name like `invoice<RLO>gpj.exe` uses to render as `invoice.exe.jpg` —
    // including in the dialog that asks whether to run it.
    if let Some(bad) = trimmed
        .chars()
        .find(|&c| crate::security::text::is_invisible_or_reordering(c))
    {
        return Err(format!(
            "Names cannot contain invisible or text-reordering characters (U+{:04X})",
            bad as u32
        ));
    }
    if trimmed.ends_with('.') || trimmed.ends_with(' ') {
        return Err("Names cannot end with a dot or space".into());
    }
    if is_reserved_name(trimmed) {
        return Err(format!("“{trimmed}” is a reserved Windows device name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        assert!(validate_name("report.txt").is_ok());
        assert!(validate_name("New folder").is_ok());
        assert!(validate_name(".gitignore").is_ok());
    }

    #[test]
    fn rejects_empty_and_dots() {
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
    }

    #[test]
    fn rejects_invalid_and_control_chars() {
        assert!(validate_name("a:b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("tab\there").is_err());
    }

    #[test]
    fn rejects_trailing_dot_or_space_after_trim() {
        assert!(validate_name("name.").is_err());
    }

    #[test]
    fn rejects_reserved_device_names() {
        assert!(validate_name("CON").is_err());
        assert!(validate_name("con.txt").is_err());
        assert!(validate_name("CONIN$").is_err());
    }

    #[test]
    fn rejects_overlong_names() {
        assert!(validate_name(&"x".repeat(256)).is_err());
        assert!(validate_name(&"x".repeat(255)).is_ok());
    }

    /// These all passed before: `char::is_control` is category `Cc`, and every
    /// bidi override and zero-width character is `Cf`.
    #[test]
    fn rejects_invisible_and_reordering_characters() {
        for bad in [
            "invoice\u{202E}gpj.exe", // the extension spoof
            "a\u{200B}b",             // zero-width space
            "a\u{200D}b",             // zero-width joiner
            "a\u{2066}b",             // isolate
            "a\u{FEFF}b",             // BOM
            "a\u{00AD}b",             // soft hyphen
            "a\u{2028}b",             // line separator
        ] {
            assert!(
                validate_name(bad).is_err(),
                "accepted a spoofing name: {bad:?}"
            );
        }
    }

    /// The new check must not reject legitimate non-ASCII names.
    #[test]
    fn still_accepts_ordinary_unicode_names() {
        for good in [
            "Ünicöde.txt",
            "日本語.txt",
            "مرحبا.txt",
            "e\u{0301}clair.txt", // combining acute
            "party 🎉.png",
        ] {
            assert!(
                validate_name(good).is_ok(),
                "rejected a legitimate name: {good:?}"
            );
        }
    }
}
