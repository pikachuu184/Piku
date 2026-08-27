//! Sanitizer for any text PIKU did not author.
//!
//! Filenames, archive entry names, audio tags, commit messages, branch names,
//! remote URLs, and OS error strings are all attacker-controlled. Rendered
//! raw, a single `U+202E` can reverse everything after it — which is how a
//! file called `invoice<RLO>gpj.exe` displays as `invoice.exe.jpg` in a file
//! list, and, worse, in the dialog that asks whether to run it.
//!
//! This started as a git-only sanitizer (`git_text`), which is why the git
//! layer still calls it under the old name. It now covers every untrusted
//! string that reaches a renderable type.
//!
//! # What is stripped
//!
//! * **C0 controls** (including `ESC`, so ANSI sequences lose their trigger),
//!   **C1 controls**, and **DEL** — via `char::is_control`, which is Unicode
//!   category `Cc`.
//! * **Every `Cf` format character.** This is the important widening:
//!   `is_control` is `Cc` only, and every bidi override and zero-width
//!   character is `Cf`, so they all passed the old check untouched. The set
//!   named by [UTS #39] and the Trojan Source paper — `U+202A..=202E`,
//!   `U+2066..=2069`, `U+200B..=200F`, `U+061C`, `U+2060..=2064`, `U+FEFF`,
//!   `U+180E` — is entirely inside `Cf`, so matching the category covers it
//!   plus the language tags at `U+E0001`/`U+E0020..=E007F`.
//! * **Variation selectors** (`U+FE00..=FE0F`, `U+E0100..=E01EF`), which are
//!   `Mn` rather than `Cf` but are equally invisible.
//! * **Line and paragraph separators** (`U+2028`, `U+2029`), which are `Zl`/`Zp`
//!   and can break out of a single-line label.
//!
//! # What is deliberately *not* stripped
//!
//! Combining marks and confusable/homoglyph scripts. Stripping those would
//! mangle legitimate text in most of the world's languages, and they cannot
//! reorder surrounding content the way the classes above can. Confusables are
//! a rendering-similarity problem, not a text-integrity one.
//!
//! [UTS #39]: https://www.unicode.org/reports/tr39/

/// Characters that are invisible or can reorder the text around them.
///
/// Kept as an explicit predicate rather than only a category test so the
/// intent is greppable and the ranges are documented at the point of use.
pub fn is_invisible_or_reordering(c: char) -> bool {
    // `Cf`, the format category. Covers every bidi control (LRE/RLE/PDF/LRO/RLO,
    // the isolates, ALM, LRM/RLM), the zero-width space/joiners, the word
    // joiner and invisible operators, the BOM, and the Unicode tag characters.
    if matches!(c,
        '\u{00AD}'                  // soft hyphen
        | '\u{0600}'..='\u{0605}'
        | '\u{061C}'                // Arabic letter mark
        | '\u{06DD}' | '\u{070F}' | '\u{0890}' | '\u{0891}' | '\u{08E2}'
        | '\u{180E}'                // Mongolian vowel separator
        | '\u{200B}'..='\u{200F}'   // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | '\u{202A}'..='\u{202E}'   // LRE, RLE, PDF, LRO, RLO
        | '\u{2060}'..='\u{2064}'   // word joiner, invisible operators
        | '\u{2066}'..='\u{2069}'   // LRI, RLI, FSI, PDI
        | '\u{FEFF}'                // BOM / zero-width no-break space
        | '\u{FFF9}'..='\u{FFFB}'   // interlinear annotation
        | '\u{110BD}' | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0001}'
        | '\u{E0020}'..='\u{E007F}' // tag characters
    ) {
        return true;
    }
    // Variation selectors: invisible, and not in `Cf`.
    if matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}') {
        return true;
    }
    // Line/paragraph separators: can escape a single-line label.
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// Strip control characters, ANSI escape triggers, and invisible/reordering
/// characters, then cap the result at `max_chars` (appending `…` if cut).
///
/// - `\r` is always removed.
/// - `\n` and `\t` survive only when `keep_newlines` is true (commit message
///   bodies); single-line fields collapse them to a space.
/// - The cap counts `char`s, not bytes, so multi-byte text truncates cleanly.
pub fn sanitize_display(raw: &str, max_chars: usize, keep_newlines: bool) -> String {
    let mut out = String::with_capacity(raw.len().min(max_chars.saturating_mul(4)));
    let mut count = 0usize;
    let mut truncated = false;

    for c in raw.chars() {
        let mapped = match c {
            '\r' => continue,
            '\n' | '\t' if keep_newlines => c,
            '\n' | '\t' => ' ',
            c if c.is_control() => continue, // C0 (incl. ESC), C1, DEL
            c if is_invisible_or_reordering(c) => continue,
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

/// Cap for a single-line label built from untrusted text (a filename, an
/// archive entry, a tag value). Long enough for any real name, short enough
/// that a pathological one cannot blow out a row.
pub const LABEL_CAP: usize = 512;

/// Sanitize a single-line label. The common case for filenames and tags.
pub fn sanitize_label(raw: &str) -> String {
    sanitize_display(raw, LABEL_CAP, false)
}

/// Cap for a rendered filesystem path. Longer than [`LABEL_CAP`] because a
/// path is many names joined. Matches `audit::MAX_PATH_CHARS`, so a path is
/// bounded the same way whether it is being logged or drawn.
pub const PATH_CAP: usize = 4096;

/// Sanitize a filesystem path for display.
///
/// [`crate::core::entry::FsEntry::name`] is cleaned once at construction, but
/// the path deliberately is not — it keeps the real bytes, because it is what
/// gets opened. So every place that *renders* a path has to clean it here
/// instead, and a path is the more exposed of the two: it carries every
/// ancestor directory's name as well, which means one hostile component
/// upstream reorders the row for every file beneath it.
pub fn sanitize_path(path: &std::path::Path) -> String {
    sanitize_display(&path.to_string_lossy(), PATH_CAP, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The characters from the Trojan Source paper and UTS #39 that motivated
    /// widening this beyond `char::is_control`.
    const HOSTILE: &[char] = &[
        '\u{202A}',
        '\u{202B}',
        '\u{202C}',
        '\u{202D}',
        '\u{202E}', // bidi overrides
        '\u{2066}',
        '\u{2067}',
        '\u{2068}',
        '\u{2069}', // isolates
        '\u{200B}',
        '\u{200C}',
        '\u{200D}', // zero-width
        '\u{200E}',
        '\u{200F}',
        '\u{061C}', // marks
        '\u{2060}',
        '\u{2061}',
        '\u{2062}',
        '\u{2063}',
        '\u{2064}', // invisible ops
        '\u{FEFF}',
        '\u{180E}',
        '\u{00AD}', // BOM, separator, soft hyphen
        '\u{FE0F}', // variation selector
        '\u{2028}',
        '\u{2029}',  // line/paragraph separators
        '\u{E0041}', // tag character
    ];

    #[test]
    fn every_hostile_character_is_recognized() {
        for &c in HOSTILE {
            assert!(
                is_invisible_or_reordering(c),
                "U+{:04X} not recognized as invisible/reordering",
                c as u32
            );
        }
    }

    #[test]
    fn every_hostile_character_is_stripped_from_display_text() {
        for &c in HOSTILE {
            let raw = format!("a{c}b");
            let clean = sanitize_display(&raw, 100, false);
            assert_eq!(clean, "ab", "U+{:04X} survived sanitizing", c as u32);
        }
    }

    /// The concrete attack: a filename that renders as `invoice.exe.jpg` while
    /// actually ending in `.exe`.
    #[test]
    fn the_extension_spoof_is_defused() {
        let spoof = "invoice\u{202E}gpj.exe";
        let clean = sanitize_label(spoof);
        assert_eq!(clean, "invoicegpj.exe");
        assert!(!clean.contains('\u{202E}'));
    }

    #[test]
    fn ordinary_text_including_non_ascii_is_untouched() {
        for good in [
            "report.pdf",
            "Ünicöde Näme.txt",
            "日本語のファイル.txt",
            "مرحبا.txt",    // Arabic, no explicit marks
            "emoji 🎉.png", // emoji without a variation selector
            "a-b_c.1234",
        ] {
            assert_eq!(sanitize_label(good), good, "mangled legitimate text");
        }
    }

    #[test]
    fn combining_marks_survive() {
        // Stripping these would break legitimate text in many languages, and
        // they cannot reorder surrounding content.
        let combining = "e\u{0301}"; // e + combining acute
        assert_eq!(sanitize_label(combining), combining);
    }

    #[test]
    fn ansi_escapes_lose_their_trigger() {
        let ansi = "\u{1B}[31mred\u{1B}[0m";
        assert_eq!(sanitize_display(ansi, 100, false), "[31mred[0m");
    }

    #[test]
    fn newlines_are_collapsed_unless_kept() {
        assert_eq!(sanitize_display("a\nb\tc", 100, false), "a b c");
        assert_eq!(sanitize_display("a\nb", 100, true), "a\nb");
        assert_eq!(sanitize_display("a\r\nb", 100, true), "a\nb");
    }

    #[test]
    fn the_cap_counts_chars_and_marks_truncation() {
        let long = "é".repeat(50);
        let clean = sanitize_display(&long, 10, false);
        assert_eq!(clean.chars().count(), 11, "10 chars plus the ellipsis");
        assert!(clean.ends_with('…'));
    }

    /// A hostile *directory* is worse than a hostile file: it reorders the
    /// rendered path of every file beneath it, and the inspector's Location
    /// row is where a full path is most prominent.
    #[test]
    fn a_hostile_path_component_is_neutralized() {
        let path = std::path::Path::new("/home/u/inv\u{202E}gpj.exe/report.pdf");
        let clean = sanitize_path(path);
        assert_eq!(clean, "/home/u/invgpj.exe/report.pdf");
        assert!(!clean.contains('\u{202E}'));
    }

    #[test]
    fn an_ordinary_path_is_untouched() {
        let path = std::path::Path::new("/home/u/Documents/報告書.pdf");
        assert_eq!(sanitize_path(path), "/home/u/Documents/報告書.pdf");
    }

    #[test]
    fn a_string_that_is_entirely_hostile_becomes_empty() {
        let all_bad: String = HOSTILE.iter().collect();
        assert!(sanitize_label(&all_bad).is_empty());
    }
}
