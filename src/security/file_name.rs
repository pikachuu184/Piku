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
}
