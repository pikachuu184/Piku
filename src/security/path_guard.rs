//! Central path authorization: every path that reaches the storage layer is
//! normalized here first. Rejects traversal escapes, null bytes, Windows
//! device names, UNC shares, and anything outside the authorized local roots.

use std::path::{Component, Path, PathBuf, Prefix};

#[derive(Debug, thiserror::Error)]
pub enum PathGuardError {
    #[error("path is empty")]
    Empty,
    #[error("path contains a null byte")]
    NullByte,
    #[error("path uses the reserved device name `{0}`")]
    ReservedName(String),
    #[error("path escapes its root through `..`")]
    Traversal,
    #[error("path must be absolute")]
    NotAbsolute,
    #[error("network (UNC) and device paths are not authorized")]
    UnauthorizedPrefix,
    #[error("path is outside the authorized storage roots")]
    OutsideRoots,
}

#[derive(Debug, Clone)]
pub struct PathGuard {
    roots: Vec<PathBuf>,
}

const RESERVED: [&str; 24] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    "CONOUT$",
];

/// Whether a single path component collides with a reserved Windows device
/// name (the check applies to the stem before the first `.`, per Win32 rules,
/// so `CON.txt` is reserved too).
pub fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    RESERVED.contains(&stem.as_str())
}

impl PathGuard {
    /// Authorize all currently mounted local drive roots.
    pub fn with_system_roots() -> Self {
        let mut roots = Vec::new();
        #[cfg(windows)]
        {
            for letter in b'A'..=b'Z' {
                let root = PathBuf::from(format!("{}:\\", letter as char));
                if root.exists() {
                    roots.push(root);
                }
            }
        }
        #[cfg(not(windows))]
        {
            roots.push(PathBuf::from("/"));
        }
        Self { roots }
    }

    /// The authorized roots (used by future storage providers and tests).
    #[allow(dead_code)]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Normalize and authorize a path. Returns the lexically-normalized
    /// absolute path on success.
    pub fn sanitize(&self, raw: &Path) -> Result<PathBuf, PathGuardError> {
        let text = raw.as_os_str();
        if text.is_empty() {
            return Err(PathGuardError::Empty);
        }
        if raw.to_string_lossy().contains('\0') {
            return Err(PathGuardError::NullByte);
        }

        let mut normalized = PathBuf::new();
        let mut depth: i32 = 0;
        let mut has_prefix = false;

        for component in raw.components() {
            match component {
                Component::Prefix(prefix) => {
                    match prefix.kind() {
                        Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {}
                        _ => return Err(PathGuardError::UnauthorizedPrefix),
                    }
                    has_prefix = true;
                    normalized.push(component.as_os_str());
                }
                Component::RootDir => normalized.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if depth == 0 {
                        return Err(PathGuardError::Traversal);
                    }
                    normalized.pop();
                    depth -= 1;
                }
                Component::Normal(part) => {
                    let part_str = part.to_string_lossy();
                    if is_reserved_name(&part_str) {
                        return Err(PathGuardError::ReservedName(part_str.into_owned()));
                    }
                    normalized.push(part);
                    depth += 1;
                }
            }
        }

        #[cfg(windows)]
        if !has_prefix {
            return Err(PathGuardError::NotAbsolute);
        }
        #[cfg(not(windows))]
        if !raw.is_absolute() {
            let _ = has_prefix;
            return Err(PathGuardError::NotAbsolute);
        }

        if !self.is_within_roots(&normalized) {
            return Err(PathGuardError::OutsideRoots);
        }

        Ok(normalized)
    }

    fn is_within_roots(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| starts_with_ci(path, root))
    }
}

/// Case-insensitive, component-wise prefix check (Windows paths are
/// case-insensitive).
fn starts_with_ci(path: &Path, prefix: &Path) -> bool {
    let mut path_parts = path.components();
    for prefix_part in prefix.components() {
        match path_parts.next() {
            Some(part) => {
                let a = part.as_os_str().to_string_lossy().to_lowercase();
                let b = prefix_part.as_os_str().to_string_lossy().to_lowercase();
                if a != b {
                    return false;
                }
            }
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> PathGuard {
        PathGuard {
            roots: vec![PathBuf::from("C:\\")],
        }
    }

    #[test]
    fn accepts_normal_absolute_path() {
        let g = guard();
        assert!(g.sanitize(Path::new("C:\\Users\\demo\\file.txt")).is_ok());
    }

    #[test]
    fn rejects_traversal() {
        let g = guard();
        assert!(matches!(
            g.sanitize(Path::new("C:\\..\\..\\secret")),
            Err(PathGuardError::Traversal)
        ));
    }

    #[test]
    fn normalizes_inner_dotdot() {
        let g = guard();
        let p = g.sanitize(Path::new("C:\\a\\b\\..\\c")).unwrap();
        assert_eq!(p, PathBuf::from("C:\\a\\c"));
    }

    #[test]
    fn rejects_device_names() {
        let g = guard();
        assert!(matches!(
            g.sanitize(Path::new("C:\\folder\\CON.txt")),
            Err(PathGuardError::ReservedName(_))
        ));
    }

    #[test]
    fn rejects_console_device_names() {
        let g = guard();
        assert!(matches!(
            g.sanitize(Path::new("C:\\folder\\CONIN$")),
            Err(PathGuardError::ReservedName(_))
        ));
        assert!(matches!(
            g.sanitize(Path::new("C:\\folder\\conout$.txt")),
            Err(PathGuardError::ReservedName(_))
        ));
    }

    #[test]
    fn reserved_name_helper() {
        assert!(is_reserved_name("CON"));
        assert!(is_reserved_name("con.txt"));
        assert!(is_reserved_name("Conin$"));
        assert!(!is_reserved_name("console"));
        assert!(!is_reserved_name("com10"));
    }

    #[test]
    fn rejects_unc() {
        let g = guard();
        assert!(matches!(
            g.sanitize(Path::new("\\\\server\\share\\x")),
            Err(PathGuardError::UnauthorizedPrefix)
        ));
    }

    #[test]
    fn rejects_relative() {
        let g = guard();
        assert!(g.sanitize(Path::new("relative\\path")).is_err());
    }
}
