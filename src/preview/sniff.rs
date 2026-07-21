//! Magic-byte sniffing over a file's head. Deliberately hand-rolled — the
//! table below covers every signature the preview engine cares about, and a
//! table we own is a table we can audit.

/// Best-effort signature detection from head bytes. Returns a short
/// human-readable format name, or `None` when nothing matches.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    // Longest / most specific prefixes first.
    const TABLE: &[(&[u8], &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "PNG image"),
        (b"GIF87a", "GIF image"),
        (b"GIF89a", "GIF image"),
        (b"\xff\xd8\xff", "JPEG image"),
        (b"BM", "BMP image"),
        (b"%PDF-", "PDF document"),
        (b"PK\x03\x04", "ZIP archive"),
        (b"PK\x05\x06", "ZIP archive (empty)"),
        (b"Rar!\x1a\x07", "RAR archive"),
        (b"7z\xbc\xaf\x27\x1c", "7-Zip archive"),
        (b"\x1f\x8b", "GZIP archive"),
        (b"BZh", "BZIP2 archive"),
        (b"\xfd7zXZ\x00", "XZ archive"),
        (b"\x28\xb5\x2f\xfd", "Zstandard archive"),
        (b"MZ", "Windows executable (PE)"),
        (b"\x7fELF", "ELF executable"),
        (b"OggS", "OGG media"),
        (b"fLaC", "FLAC audio"),
        (b"ID3", "MP3 audio (ID3)"),
        (b"\xff\xfb", "MP3 audio"),
        (b"\x1a\x45\xdf\xa3", "Matroska/WebM media"),
        (b"SQLite format 3\x00", "SQLite database"),
        (b"\xef\xbb\xbf", "UTF-8 text (BOM)"),
        (b"\xff\xfe", "UTF-16 LE text"),
        (b"\xfe\xff", "UTF-16 BE text"),
    ];

    for (magic, name) in TABLE {
        if head.starts_with(magic) {
            return Some(name);
        }
    }

    // Container formats with the magic at an offset.
    if head.len() >= 12 {
        if &head[0..4] == b"RIFF" {
            return Some(match &head[8..12] {
                b"WEBP" => "WebP image",
                b"WAVE" => "WAV audio",
                b"AVI " => "AVI video",
                _ => "RIFF container",
            });
        }
        if &head[4..8] == b"ftyp" {
            return Some("MP4/QuickTime media");
        }
    }
    None
}

/// True when the head looks like binary data rather than text: any NUL byte
/// in the sample is a strong signal (UTF-8/ANSI text never contains one).
pub fn looks_binary(head: &[u8]) -> bool {
    head.iter().take(8 * 1024).any(|&b| b == 0)
}

/// The subset of sniffed formats gpui's `img()` can render from a path —
/// used to upgrade extensionless files to an image preview.
pub fn sniffed_renderable_image(head: &[u8]) -> bool {
    matches!(
        sniff(head),
        Some("PNG image" | "GIF image" | "JPEG image" | "BMP image" | "WebP image")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("PNG image"));
        assert_eq!(sniff(b"%PDF-1.7"), Some("PDF document"));
        assert_eq!(sniff(b"PK\x03\x04rest"), Some("ZIP archive"));
        assert_eq!(sniff(b"MZ\x90\x00"), Some("Windows executable (PE)"));
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("WebP image"));
        assert_eq!(sniff(b"\x00\x00\x00\x20ftypisom"), Some("MP4/QuickTime media"));
        assert_eq!(sniff(b"plain text here"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn binary_detection() {
        assert!(looks_binary(b"ab\x00cd"));
        assert!(!looks_binary(b"hello world\nplain text"));
        assert!(!looks_binary(b""));
    }
}
