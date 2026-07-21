//! Solid per-category colors for the drive capacity bars — the one deliberate
//! exception to the monochrome palette. Muted, flat, no gradients; free space
//! stays neutral (`theme().border`).

use gpui::Hsla;

use crate::core::file_type::FileCategory;

/// A drive at or beyond this used fraction renders one solid red bar.
pub const NEAR_FULL_FRACTION: f32 = 0.9;

pub fn near_full_color() -> Hsla {
    gpui::rgb(0xE05252).into()
}

pub fn category_color(category: FileCategory) -> Hsla {
    let rgb: u32 = match category {
        FileCategory::Image => 0x6FB3E0,
        FileCategory::Video => 0xB48BD6,
        FileCategory::Audio => 0x7FC8A9,
        FileCategory::Archive => 0xD6B36B,
        FileCategory::Document => 0xE0A87F,
        FileCategory::Code => 0x8FA8E0,
        FileCategory::Executable => 0xD98C8C,
        // Folders/symlinks never carry scanned bytes; everything uncategorized
        // (including unscanned system space) reads as neutral gray.
        _ => 0x8C8C8C,
    };
    gpui::rgb(rgb).into()
}
