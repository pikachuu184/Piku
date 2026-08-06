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
                    has_prefix = true;
                    match prefix.kind() {
                        Prefix::Disk(_) => normalized.push(component.as_os_str()),
                        // `fs::canonicalize` returns extended-length verbatim
                        // paths on Windows (`\\?\C:\…`). Collapse the prefix
                        // to the plain disk form so root containment, display,
                        // and downstream consumers all see one spelling.
                        // (Verbatim *UNC* prefixes are rejected below — they
                        // cannot be safely simplified.)
                        Prefix::VerbatimDisk(letter) => {
                            normalized.push(format!("{}:", letter as char));
                        }
                        _ => return Err(PathGuardError::UnauthorizedPrefix),
                    }
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

    /// A guard rooted at the platform's own notion of a drive root.
    ///
    /// These tests used to build a `C:\` guard and feed it backslash paths
    /// unconditionally, which only ever meant anything on Windows: on Unix
    /// `Path::components` treats `C:\Users\demo` as a single relative
    /// component, so every case degenerated to `NotAbsolute`. The
    /// platform-specific cases now live in the `windows` and `unix` modules
    /// below, and only genuinely portable assertions live out here.
    fn guard() -> PathGuard {
        PathGuard {
            roots: vec![PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" })],
        }
    }

    /// An absolute path under the guard's root, spelled for this platform.
    fn under_root(rel: &str) -> PathBuf {
        let mut p = PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" });
        p.extend(rel.split('/'));
        p
    }

    #[test]
    fn accepts_normal_absolute_path() {
        let g = guard();
        assert!(g.sanitize(&under_root("Users/demo/file.txt")).is_ok());
    }

    #[test]
    fn normalizes_inner_dotdot() {
        let g = guard();
        let p = g.sanitize(&under_root("a/b/../c")).unwrap();
        assert_eq!(p, under_root("a/c"));
    }

    #[test]
    fn rejects_device_names() {
        let g = guard();
        assert!(matches!(
            g.sanitize(&under_root("folder/CON.txt")),
            Err(PathGuardError::ReservedName(_))
        ));
    }

    #[test]
    fn rejects_console_device_names() {
        let g = guard();
        assert!(matches!(
            g.sanitize(&under_root("folder/CONIN$")),
            Err(PathGuardError::ReservedName(_))
        ));
        assert!(matches!(
            g.sanitize(&under_root("folder/conout$.txt")),
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
    fn rejects_relative() {
        let g = guard();
        assert!(matches!(
            g.sanitize(Path::new("relative/path")),
            Err(PathGuardError::NotAbsolute)
        ));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            guard().sanitize(Path::new("")),
            Err(PathGuardError::Empty)
        ));
    }

    #[cfg(windows)]
    mod windows {
        use super::*;

        #[test]
        fn rejects_traversal_above_the_drive_root() {
            let g = guard();
            assert!(matches!(
                g.sanitize(Path::new("C:\\..\\..\\secret")),
                Err(PathGuardError::Traversal)
            ));
        }

        #[test]
        fn normalizes_verbatim_disk_prefix() {
            let g = guard();
            // What `fs::canonicalize` hands back on Windows must both pass
            // the root check and come out in plain-disk spelling.
            let p = g
                .sanitize(Path::new("\\\\?\\C:\\Users\\demo\\file.txt"))
                .unwrap();
            assert_eq!(p, PathBuf::from("C:\\Users\\demo\\file.txt"));
        }

        #[test]
        fn rejects_verbatim_unc() {
            let g = guard();
            assert!(matches!(
                g.sanitize(Path::new("\\\\?\\UNC\\server\\share\\x")),
                Err(PathGuardError::UnauthorizedPrefix)
            ));
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
        fn rejects_a_path_on_an_unauthorized_drive() {
            let g = guard();
            assert!(matches!(
                g.sanitize(Path::new("D:\\elsewhere\\file.txt")),
                Err(PathGuardError::OutsideRoots)
            ));
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;

        #[test]
        fn rejects_traversal_above_the_root() {
            let g = guard();
            assert!(matches!(
                g.sanitize(Path::new("/../../secret")),
                Err(PathGuardError::Traversal)
            ));
        }

        /// `with_system_roots` authorizes `/` on POSIX, which makes
        /// `OutsideRoots` unreachable there. Containment only becomes
        /// meaningful against a narrower root — this pins that the check
        /// itself works, so a per-operation scope can rely on it.
        #[test]
        fn containment_is_enforced_against_a_narrow_root() {
            let g = PathGuard {
                roots: vec![PathBuf::from("/home/demo")],
            };
            assert!(g.sanitize(Path::new("/home/demo/notes/a.txt")).is_ok());
            assert!(matches!(
                g.sanitize(Path::new("/etc/shadow")),
                Err(PathGuardError::OutsideRoots)
            ));
            // A sibling that merely shares a textual prefix is not inside.
            assert!(matches!(
                g.sanitize(Path::new("/home/demo-other/a.txt")),
                Err(PathGuardError::OutsideRoots)
            ));
        }

        /// `..` may normalize away inside the path but must never be able to
        /// climb out of the authorized root.
        #[test]
        fn dotdot_cannot_escape_a_narrow_root() {
            let g = PathGuard {
                roots: vec![PathBuf::from("/home/demo")],
            };
            assert_eq!(
                g.sanitize(Path::new("/home/demo/a/../b")).unwrap(),
                PathBuf::from("/home/demo/b")
            );
            assert!(matches!(
                g.sanitize(Path::new("/home/demo/../../etc/shadow")),
                Err(PathGuardError::OutsideRoots)
            ));
        }

        #[test]
        fn system_roots_authorize_everything_absolute() {
            // Documents today's POSIX posture explicitly: the root is `/`, so
            // the guard's value here is normalization and traversal
            // rejection, not containment.
            let g = PathGuard::with_system_roots();
            assert_eq!(g.roots(), [PathBuf::from("/")]);
            assert!(g.sanitize(Path::new("/etc/hosts")).is_ok());
        }

        #[test]
        fn rejects_null_bytes() {
            let g = guard();
            assert!(matches!(
                g.sanitize(Path::new("/home/demo/a\0b")),
                Err(PathGuardError::NullByte)
            ));
        }
    }
}
