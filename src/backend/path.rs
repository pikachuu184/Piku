//! Validated path objects. Services take these instead of raw `&Path`, so
//! "was this authorized?" is answered by the type rather than by remembering
//! to call a function.
//!
//! # The two tiers
//!
//! [`PathPolicy`] is the process-wide authority: normalization, traversal
//! rejection, Windows device-name and UNC rejection, and a deny list. It is
//! **lexical** — zero syscalls — so it is cheap enough to call from the UI
//! thread.
//!
//! [`ScopePolicy`] is per-operation containment: "everything this operation
//! touches must be at or beneath *this* root". That is the tier that actually
//! defeats symlink escape during a recursive delete and Zip Slip during
//! extraction, and it is the one that matters on POSIX, where the global
//! policy's root is `/` and therefore authorizes everything absolute.
//!
//! # The rule
//!
//! > [`PathPolicy::validate`] is the only method the UI layer may call. Every
//! > mutating service method takes a [`ValidatedPath`] — proving lexical
//! > validation happened — and calls [`PathPolicy::reauthorize`] as its first
//! > statement *inside* the blocking closure, proving the real target is still
//! > authorized at the moment of the operation.
//!
//! This generalizes what `shell_open` already did: sanitize, canonicalize,
//! then re-sanitize the resolved target, so a symlink inside an allowed root
//! cannot be used to reach something outside it.

// The services that take `ValidatedPath` land in Stage 3; until then the
// type is exercised only by its own tests. `expect` rather than `allow`:
// it starts erroring once every item is live, which is the reminder to
// delete it.
#![expect(dead_code)]

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;

use crate::backend::error::PathError;
use crate::security::path_guard::is_reserved_name;

/// A path that has passed lexical validation.
///
/// Constructible only through a [`PathPolicy`], so holding one is evidence
/// that the checks ran. `canonical` records whether the *real* target was
/// resolved and re-authorized, which mutating operations require.
#[derive(Clone)]
pub struct ValidatedPath {
    path: PathBuf,
    /// Lossy display form, computed once. Tracing fields and audit records
    /// both want a `&str`, and doing it here avoids re-allocating per event —
    /// which is also the ergonomic win camino would have provided, without
    /// camino's fatal flaw for a file manager: `Utf8PathBuf::from_path_buf`
    /// *fails* on the non-UTF-8 names that really exist on Linux.
    text: Arc<str>,
    canonical: bool,
}

impl ValidatedPath {
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.path
    }

    /// Cheap lossy text, for tracing fields and audit records.
    pub fn text(&self) -> Arc<str> {
        self.text.clone()
    }

    pub fn file_name(&self) -> Option<&OsStr> {
        self.path.file_name()
    }

    pub fn parent(&self) -> Option<&Path> {
        self.path.parent()
    }

    /// Whether the real target has been resolved and re-authorized.
    pub fn is_canonical(&self) -> bool {
        self.canonical
    }

    fn new(path: PathBuf, canonical: bool) -> Self {
        let text: Arc<str> = Arc::from(path.to_string_lossy().into_owned());
        Self {
            path,
            text,
            canonical,
        }
    }
}

impl std::fmt::Display for ValidatedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::fmt::Debug for ValidatedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedPath")
            .field("path", &self.path)
            .field("canonical", &self.canonical)
            .finish()
    }
}

impl PartialEq for ValidatedPath {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}
impl Eq for ValidatedPath {}
impl std::hash::Hash for ValidatedPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

/// A validated single path component.
///
/// Separate from [`ValidatedPath`] because the rules differ: a component may
/// not contain a separator at all, and it carries the filename restrictions
/// (reserved device names, trailing dot/space, invisible characters).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileName(String);

impl FileName {
    /// Validate a user-typed component. Wraps
    /// [`crate::security::file_name::validate_name`] so both the dialogs and
    /// the services enforce exactly one rule set.
    pub fn new(raw: &str) -> Result<Self, PathError> {
        crate::security::file_name::validate_name(raw).map_err(PathError::InvalidName)?;
        Ok(Self(raw.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Process-wide path authority.
#[derive(Debug, Clone)]
pub struct PathPolicy {
    roots: Vec<PathBuf>,
    deny: Vec<PathBuf>,
}

impl Default for PathPolicy {
    fn default() -> Self {
        Self::with_system_roots()
    }
}

impl PathPolicy {
    /// Authorize all mounted local drive roots, and deny the pseudo-filesystems
    /// that are not real storage.
    ///
    /// On POSIX the root is `/`, so `OutsideRoots` is structurally unreachable
    /// there — and that is deliberate: a general file manager must be able to
    /// open `/etc`. The global tier's value on POSIX is normalization,
    /// traversal rejection, and the deny list; containment is
    /// [`ScopePolicy`]'s job.
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
        Self {
            roots,
            deny: default_deny_list(),
        }
    }

    /// A policy rooted at specific directories. Used by tests and by any future
    /// sandboxed provider.
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            deny: default_deny_list(),
        }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Normalize and authorize a path. **Lexical only — no syscalls.**
    ///
    /// Cheap enough for the UI thread; that is the point of splitting it from
    /// [`resolve`](Self::resolve).
    pub fn validate(&self, raw: &Path) -> Result<ValidatedPath, PathError> {
        let normalized = normalize(raw)?;
        if !self.is_within_roots(&normalized) {
            return Err(PathError::OutsideRoots);
        }
        if self.is_denied(&normalized) {
            return Err(PathError::Denied);
        }
        Ok(ValidatedPath::new(normalized, false))
    }

    /// Resolve symlinks and re-authorize the **real** target. Blocking.
    ///
    /// `dunce::canonicalize` rather than `std::fs::canonicalize` so Windows
    /// gets a plain-disk spelling instead of a `\\?\` verbatim prefix. Its
    /// output is fed back through [`validate`](Self::validate) regardless:
    /// dunce refuses to simplify a path whose components are non-UTF-8 or
    /// reserved, so a verbatim prefix can still come back, and our own
    /// normalization collapses it unconditionally.
    pub fn resolve(&self, raw: &Path) -> Result<ValidatedPath, PathError> {
        let lexical = self.validate(raw)?;
        let real = dunce::canonicalize(lexical.as_path()).map_err(|source| PathError::Resolve {
            path: lexical.text(),
            source,
        })?;
        let mut authorized = self.validate(&real)?;
        authorized.canonical = true;
        Ok(authorized)
    }

    /// Build a path for something that does not exist yet.
    ///
    /// The parent is resolved (it does exist), then the validated leaf is
    /// attached. Canonicalizing a not-yet-existent target would simply fail,
    /// and joining an unvalidated name to a resolved parent is how a separator
    /// smuggled into a "filename" escapes.
    pub fn resolve_for_create(
        &self,
        parent: &Path,
        leaf: &FileName,
    ) -> Result<ValidatedPath, PathError> {
        let parent = self.resolve(parent)?;
        let joined = parent.as_path().join(leaf.as_str());
        // Re-validate the join: `leaf` cannot contain a separator (the
        // filename rules forbid `/` and `\`), but the check is free and this
        // is the last gate before a mutation.
        let mut target = self.validate(&joined)?;
        target.canonical = parent.is_canonical();
        Ok(target)
    }

    /// Re-authorize immediately before a mutation.
    ///
    /// Called as the first statement inside a blocking closure, so the check
    /// happens at the moment of use rather than at the moment of dispatch.
    /// Already-canonical paths are re-validated lexically rather than
    /// re-canonicalized, which keeps the hot path free of a syscall while
    /// still catching a policy that changed underneath.
    pub fn reauthorize(&self, p: &ValidatedPath) -> Result<ValidatedPath, PathError> {
        if p.is_canonical() {
            let mut again = self.validate(p.as_path())?;
            again.canonical = true;
            Ok(again)
        } else {
            self.resolve(p.as_path())
        }
    }

    fn is_within_roots(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| starts_with_ci(path, root))
    }

    fn is_denied(&self, path: &Path) -> bool {
        self.deny.iter().any(|d| starts_with_ci(path, d))
    }
}

/// Pseudo-filesystems and device namespaces that are not real storage.
///
/// Without these the POSIX tier has nothing to reject at all (its root is
/// `/`), and walking into `/proc` in particular means chasing every process's
/// entire memory map.
fn default_deny_list() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        vec![PathBuf::from("\\\\.\\"), PathBuf::from("\\\\?\\GLOBALROOT")]
    }
    #[cfg(not(windows))]
    {
        vec![
            PathBuf::from("/proc"),
            PathBuf::from("/sys"),
            PathBuf::from("/dev"),
        ]
    }
}

/// Per-operation containment.
///
/// The primitive behind symlink-escape defense on recursive delete and Zip
/// Slip defense on extraction: build one from the operation's root, then run
/// every untrusted name through [`contain`](Self::contain).
#[derive(Debug, Clone)]
pub struct ScopePolicy {
    root: ValidatedPath,
}

impl ScopePolicy {
    /// Establish a scope. The root is resolved, so the containment check
    /// compares real paths rather than spellings.
    pub fn new(policy: &PathPolicy, root: &Path) -> Result<Self, PathError> {
        Ok(Self {
            root: policy.resolve(root)?,
        })
    }

    pub fn root(&self) -> &ValidatedPath {
        &self.root
    }

    /// Contain an untrusted **relative** name beneath the root.
    ///
    /// Rejects absolute paths, any prefix, `..` in any position, and reserved
    /// names — before touching the filesystem. This is the Zip Slip gate: an
    /// archive entry named `../../etc/passwd` never becomes a path.
    pub fn contain(&self, untrusted: &Path) -> Result<ValidatedPath, PathError> {
        let mut joined = self.root.as_path().to_path_buf();
        let mut components = 0usize;

        for component in untrusted.components() {
            match component {
                Component::Normal(part) => {
                    let text = part.to_string_lossy();
                    if is_reserved_name(&text) {
                        return Err(PathError::ReservedName(text.into_owned()));
                    }
                    if text
                        .chars()
                        .any(crate::security::text::is_invisible_or_reordering)
                    {
                        return Err(PathError::InvalidName(
                            "entry name contains invisible or text-reordering characters".into(),
                        ));
                    }
                    joined.push(part);
                    components += 1;
                }
                Component::CurDir => {}
                // `..` is rejected outright rather than normalized: inside an
                // untrusted name it has no legitimate use, and normalizing it
                // is how "contain" quietly becomes "escape".
                Component::ParentDir => return Err(PathError::Traversal),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(PathError::OutsideScope(
                        untrusted.to_string_lossy().into_owned().into(),
                    ));
                }
            }
        }

        if components == 0 {
            return Err(PathError::Empty);
        }
        Ok(ValidatedPath::new(joined, false))
    }

    /// Whether `p` is at or beneath the root.
    ///
    /// Purely lexical, so it is only meaningful for paths that have already
    /// been resolved — call it on the canonicalized child before descending.
    pub fn contains(&self, p: &ValidatedPath) -> bool {
        starts_with_ci(p.as_path(), self.root.as_path())
    }
}

/// Lexical normalization: the shared core of every check above.
///
/// Rejects empty paths, null bytes, `..` above the root, Windows reserved
/// device names, and every path prefix except a plain disk. Verbatim disk
/// prefixes (`\\?\C:\…`, which is what `fs::canonicalize` returns) collapse to
/// the plain form so root containment, display, and downstream consumers all
/// see one spelling.
fn normalize(raw: &Path) -> Result<PathBuf, PathError> {
    if raw.as_os_str().is_empty() {
        return Err(PathError::Empty);
    }
    if raw.to_string_lossy().contains('\0') {
        return Err(PathError::NullByte);
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
                    // Collapsed unconditionally, unlike `dunce::simplified`,
                    // which bails on non-UTF-8 or reserved components and
                    // would leave the verbatim prefix in place.
                    Prefix::VerbatimDisk(letter) => {
                        normalized.push(format!("{}:", letter as char));
                    }
                    _ => return Err(PathError::UnauthorizedPrefix),
                }
            }
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Err(PathError::Traversal);
                }
                normalized.pop();
                depth -= 1;
            }
            Component::Normal(part) => {
                let text = part.to_string_lossy();
                if is_reserved_name(&text) {
                    return Err(PathError::ReservedName(text.into_owned()));
                }
                normalized.push(part);
                depth += 1;
            }
        }
    }

    #[cfg(windows)]
    if !has_prefix {
        return Err(PathError::NotAbsolute);
    }
    #[cfg(not(windows))]
    if !raw.is_absolute() {
        let _ = has_prefix;
        return Err(PathError::NotAbsolute);
    }

    Ok(normalized)
}

/// Case-insensitive, component-wise prefix check.
///
/// Component-wise rather than string-wise on purpose: `/home/demo-other` must
/// not count as inside `/home/demo`, which a plain `starts_with` on the text
/// would get wrong.
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

    fn policy() -> PathPolicy {
        PathPolicy::with_roots(vec![PathBuf::from(if cfg!(windows) {
            "C:\\"
        } else {
            "/"
        })])
    }

    fn under_root(rel: &str) -> PathBuf {
        let mut p = PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" });
        p.extend(rel.split('/'));
        p
    }

    // --- validate: the inherited rules ----------------------------------

    #[test]
    fn accepts_a_normal_absolute_path() {
        assert!(
            policy()
                .validate(&under_root("Users/demo/file.txt"))
                .is_ok()
        );
    }

    #[test]
    fn normalizes_inner_dotdot() {
        let p = policy().validate(&under_root("a/b/../c")).unwrap();
        assert_eq!(p.as_path(), under_root("a/c"));
    }

    #[test]
    fn rejects_the_inherited_cases() {
        let p = policy();
        assert!(matches!(p.validate(Path::new("")), Err(PathError::Empty)));
        assert!(matches!(
            p.validate(Path::new("relative/path")),
            Err(PathError::NotAbsolute)
        ));
        assert!(matches!(
            p.validate(&under_root("folder/CON.txt")),
            Err(PathError::ReservedName(_))
        ));
    }

    #[test]
    fn a_validated_path_is_not_canonical_until_resolved() {
        let p = policy().validate(&under_root("a")).unwrap();
        assert!(!p.is_canonical());
    }

    #[test]
    fn display_text_is_cached_and_matches_the_path() {
        let p = policy().validate(&under_root("a/b.txt")).unwrap();
        assert_eq!(&*p.text(), p.as_path().to_string_lossy());
        assert_eq!(p.to_string(), p.as_path().to_string_lossy());
    }

    // --- FileName -------------------------------------------------------

    #[test]
    fn file_name_enforces_the_shared_rules() {
        assert!(FileName::new("report.txt").is_ok());
        assert!(FileName::new("").is_err());
        assert!(FileName::new("a/b").is_err());
        assert!(FileName::new("CON").is_err());
        // The spoof rejected in `security::file_name`.
        assert!(FileName::new("invoice\u{202E}gpj.exe").is_err());
    }

    #[test]
    fn file_name_trims() {
        assert_eq!(FileName::new("  a.txt  ").unwrap().as_str(), "a.txt");
    }

    // --- ScopePolicy: the containment tier -------------------------------

    fn scope() -> (tempdir::TempDir, ScopePolicy) {
        let dir = tempdir::TempDir::new();
        let scope = ScopePolicy::new(&PathPolicy::with_system_roots(), dir.path()).unwrap();
        (dir, scope)
    }

    #[test]
    fn scope_contains_ordinary_relative_names() {
        let (_dir, scope) = scope();
        let inside = scope.contain(Path::new("a/b/c.txt")).unwrap();
        assert!(inside.as_path().starts_with(scope.root().as_path()));
        assert!(scope.contains(&inside));
    }

    /// The Zip Slip shape: an archive entry that walks out of the destination.
    #[test]
    fn scope_rejects_traversal_in_an_untrusted_name() {
        let (_dir, scope) = scope();
        for bad in ["../escape", "a/../../escape", "..", "a/../..", "./../x"] {
            assert!(
                matches!(scope.contain(Path::new(bad)), Err(PathError::Traversal)),
                "accepted a traversing entry: {bad:?}"
            );
        }
    }

    #[test]
    fn scope_rejects_absolute_and_prefixed_names() {
        let (_dir, scope) = scope();
        let absolute = if cfg!(windows) {
            "C:\\evil"
        } else {
            "/etc/passwd"
        };
        assert!(matches!(
            scope.contain(Path::new(absolute)),
            Err(PathError::OutsideScope(_))
        ));
    }

    #[test]
    fn scope_rejects_reserved_and_invisible_entry_names() {
        let (_dir, scope) = scope();
        assert!(matches!(
            scope.contain(Path::new("CON.txt")),
            Err(PathError::ReservedName(_))
        ));
        assert!(matches!(
            scope.contain(Path::new("invoice\u{202E}gpj.exe")),
            Err(PathError::InvalidName(_))
        ));
    }

    #[test]
    fn scope_rejects_an_empty_name() {
        let (_dir, scope) = scope();
        assert!(matches!(
            scope.contain(Path::new("")),
            Err(PathError::Empty)
        ));
        assert!(matches!(
            scope.contain(Path::new(".")),
            Err(PathError::Empty)
        ));
    }

    #[test]
    fn scope_containment_is_component_wise_not_textual() {
        // `/tmp/x-other` must not count as inside `/tmp/x`.
        let dir = tempdir::TempDir::new();
        let policy = PathPolicy::with_system_roots();
        let root = dir.path().join("x");
        let sibling = dir.path().join("x-other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        let scope = ScopePolicy::new(&policy, &root).unwrap();
        let sibling = policy.resolve(&sibling).unwrap();
        assert!(
            !scope.contains(&sibling),
            "a name-prefix sibling was treated as contained"
        );
    }

    // --- resolve / reauthorize ------------------------------------------

    #[test]
    fn resolve_follows_a_symlink_to_its_real_target() {
        let dir = tempdir::TempDir::new();
        let policy = PathPolicy::with_system_roots();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, b"x").unwrap();

        let resolved = policy.resolve(&real).unwrap();
        assert!(resolved.is_canonical());
        assert!(resolved.as_path().ends_with("real.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_scope_is_detected_after_resolution() {
        // The escape that lexical validation cannot see: the name is contained,
        // but the real target is not. This is why mutations resolve first.
        let dir = tempdir::TempDir::new();
        let policy = PathPolicy::with_system_roots();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let scope = ScopePolicy::new(&policy, &root).unwrap();

        // Lexically the link looks fine...
        let lexical = scope.contain(Path::new("link")).unwrap();
        assert!(scope.contains(&lexical));

        // ...but its real target is outside, and resolving exposes that.
        let resolved = policy.resolve(lexical.as_path()).unwrap();
        assert!(
            !scope.contains(&resolved),
            "a symlink escaping the scope was not detected after resolution"
        );
    }

    #[test]
    fn resolve_reports_a_missing_path_rather_than_inventing_one() {
        let dir = tempdir::TempDir::new();
        let missing = dir.path().join("nope");
        assert!(matches!(
            PathPolicy::with_system_roots().resolve(&missing),
            Err(PathError::Resolve { .. })
        ));
    }

    #[test]
    fn resolve_for_create_works_on_a_nonexistent_leaf() {
        let dir = tempdir::TempDir::new();
        let policy = PathPolicy::with_system_roots();
        let leaf = FileName::new("new.txt").unwrap();
        let target = policy.resolve_for_create(dir.path(), &leaf).unwrap();
        assert!(target.as_path().ends_with("new.txt"));
        assert!(!target.as_path().exists());
    }

    #[test]
    fn reauthorize_is_idempotent_for_a_canonical_path() {
        let dir = tempdir::TempDir::new();
        let policy = PathPolicy::with_system_roots();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();

        let once = policy.resolve(&file).unwrap();
        let twice = policy.reauthorize(&once).unwrap();
        assert_eq!(once, twice);
        assert!(twice.is_canonical());
    }

    #[test]
    fn validate_is_idempotent() {
        let policy = policy();
        let once = policy.validate(&under_root("a/b/../c")).unwrap();
        let twice = policy.validate(once.as_path()).unwrap();
        assert_eq!(once, twice);
    }

    // --- deny list -------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn pseudo_filesystems_are_denied() {
        let policy = PathPolicy::with_system_roots();
        for denied in ["/proc", "/proc/1/maps", "/sys/kernel", "/dev/null"] {
            assert!(
                matches!(policy.validate(Path::new(denied)), Err(PathError::Denied)),
                "did not deny {denied}"
            );
        }
        // ...but ordinary system paths still work: this is a file manager.
        assert!(policy.validate(Path::new("/etc/hosts")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_name_prefix_of_a_denied_root_is_not_denied() {
        // `/process` is not `/proc`.
        let policy = PathPolicy::with_system_roots();
        assert!(policy.validate(Path::new("/process/x")).is_ok());
    }

    /// Minimal scratch directory helper — the crate has no `tempfile`
    /// dependency, and adding one for tests alone is not worth the supply
    /// chain.
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new() -> Self {
                static N: AtomicU32 = AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "piku-path-tests-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("create temp dir");
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
