//! PDF preview via pdfium, loaded dynamically at runtime.
//!
//! Hardening: the pdfium library is a C parser on untrusted input. We cap the
//! document size before handing bytes over ([`PDF_MAX_BYTES`]), cap the page
//! count ([`PDF_MAX_PAGES`]), render only a bounded subset of pages, run
//! everything on a worker thread, and wrap the parse/render in `catch_unwind`.
//! pdfium's V8/JavaScript feature is disabled (not enabled in `Cargo.toml`) and
//! forms are never initialized, so no embedded document script ever executes.
//! If the pdfium library is not present the preview degrades to a friendly note
//! rather than failing.
//!
//! Pdfium is documented as *not* thread-safe; `pdfium-render` serializes every
//! call behind a mutex. Rendering therefore gains nothing from parallelism and
//! is deliberately left sequential.

use pdfium_render::prelude::*;

use crate::backend::error::PreviewError;
use crate::backend::protocol::Cancel;
use crate::backend::services::preview::content::{PreviewPayload, RawImage};
use crate::backend::services::preview::{
    LoadCtx, PDF_MAX_BYTES, PDF_MAX_PAGES, PreviewKind, PreviewProvider, read,
};

/// Pages rasterized by the initial load.
///
/// One. The viewer shows a single page at a time and fetches the rest on demand
/// through [`render_single_page`], so rendering more here is work thrown away:
/// twelve pages cost eleven rasterizations nothing displayed and ~60 MB of the
/// preview cache's 96 MB byte budget for one document, which evicted almost
/// everything else on a single selection. It also capped the document at twelve
/// pages, because the viewer could only show what the loader had rendered.
const PDF_PREVIEW_PAGES: usize = 1;
/// Target raster width per page, in pixels.
const PDF_PAGE_WIDTH: Pixels = 1000;
/// Clamp any single page's rasterized height.
const PDF_PAGE_MAX_HEIGHT: Pixels = 4000;

enum PdfError {
    /// The pdfium library could not be loaded.
    Unavailable,
    /// The document could not be parsed/rendered.
    Failed,
    /// The request was superseded mid-render.
    Cancelled,
}

pub struct Pdf;

impl PreviewProvider for Pdf {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Pdf
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;

        // Cap the size before the C parser ever sees the bytes — and before the
        // *allocator* does. `read_head` would read up to the cap first and let
        // the refusal come after, which for a 256 MiB cap means reading and
        // holding 256 MiB of a document we are about to decline.
        let bytes = match read::read_all_bounded(ctx.path, PDF_MAX_BYTES)? {
            read::BoundedRead::All(bytes) => bytes,
            read::BoundedRead::TooLarge { size } => {
                return Ok(PreviewPayload::TooLarge { size });
            }
        };

        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_pages(&bytes, ctx.cancel)
        })) {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(PdfError::Unavailable)) => Ok(PreviewPayload::Pdf {
                pages: Vec::new(),
                total_pages: 0,
                note: Some("PDF rendering is unavailable (pdfium library not found).".into()),
            }),
            Ok(Err(PdfError::Cancelled)) => Err(PreviewError::Cancelled),
            _ => Err(PreviewError::Undecodable(
                "Could not render this PDF.".into(),
            )),
        }
    }
}

fn render_pages(bytes: &[u8], cancel: &Cancel) -> Result<PreviewPayload, PdfError> {
    let pdfium = bind_pdfium().ok_or(PdfError::Unavailable)?;
    let document = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|_| PdfError::Failed)?;

    let total_pages = document.pages().len() as usize;
    if total_pages == 0 {
        return Err(PdfError::Failed);
    }
    if total_pages > PDF_MAX_PAGES {
        return Ok(PreviewPayload::Pdf {
            pages: Vec::new(),
            total_pages,
            note: Some("Document has too many pages to preview.".into()),
        });
    }

    let render_count = total_pages.min(PDF_PREVIEW_PAGES);
    let config = PdfRenderConfig::new()
        .set_target_width(PDF_PAGE_WIDTH)
        .set_maximum_height(PDF_PAGE_MAX_HEIGHT);

    let mut pages = Vec::with_capacity(render_count);
    for index in 0..render_count as u16 {
        // A page is a full rasterization at up to 1000×4000, so a superseded
        // selection should not pay for one it will never show.
        if cancel.is_cancelled() {
            return Err(PdfError::Cancelled);
        }
        let Ok(page) = document.pages().get(index) else {
            break;
        };
        let Ok(bitmap) = page.render_with_config(&config) else {
            continue;
        };
        pages.push(RawImage::from_rgba(bitmap.as_image().into_rgba8()));
    }
    if pages.is_empty() {
        return Err(PdfError::Failed);
    }

    // No "showing the first N of M" note. The viewer fetches page N on demand
    // through `render_single_page`, so every page is reachable and a note
    // saying otherwise would understate what the preview can do. The total
    // belongs in the toolbar's page counter, which has it.
    Ok(PreviewPayload::Pdf {
        pages,
        total_pages,
        note: None,
    })
}

/// Rasterize exactly one page, turned `quarter_turns` × 90° clockwise.
///
/// This is what makes a document longer than [`PDF_PREVIEW_PAGES`] reachable.
/// It re-reads and re-parses the document, which is the price of not holding a
/// pdfium `PdfDocument` across requests: the document borrows its `Pdfium`
/// binding, pdfium is documented as not thread-safe, and every call in
/// `pdfium-render` is already serialized behind a mutex — so a cached document
/// would be a lifetime and threading problem in exchange for parse time on a
/// file the OS has in page cache.
///
/// Every limit the initial load applies applies here too: the size cap before
/// the allocator, the page cap, `catch_unwind` around the C parser, no V8, and
/// forms never initialized.
pub fn render_single_page(
    path: &std::path::Path,
    index: usize,
    quarter_turns: u8,
    cancel: &Cancel,
) -> Result<RawImage, PreviewError> {
    cancel.check()?;
    let bytes = match read::read_all_bounded(path, PDF_MAX_BYTES)? {
        read::BoundedRead::All(bytes) => bytes,
        // The initial load renders the size refusal as its own state; a derived
        // frame has no such state, and it is only ever asked for after that load
        // succeeded — so reaching here means the file grew underneath us.
        read::BoundedRead::TooLarge { .. } => {
            return Err(PreviewError::Undecodable(
                "This document is now too large to render.".into(),
            ));
        }
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render_one(&bytes, index, quarter_turns, cancel)
    })) {
        Ok(Ok(raw)) => Ok(raw),
        Ok(Err(PdfError::Cancelled)) => Err(PreviewError::Cancelled),
        Ok(Err(PdfError::Unavailable)) => Err(PreviewError::Undecodable(
            "PDF rendering is unavailable (pdfium library not found).".into(),
        )),
        _ => Err(PreviewError::Undecodable(
            "Could not render that page.".into(),
        )),
    }
}

fn render_one(
    bytes: &[u8],
    index: usize,
    quarter_turns: u8,
    cancel: &Cancel,
) -> Result<RawImage, PdfError> {
    let pdfium = bind_pdfium().ok_or(PdfError::Unavailable)?;
    let document = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|_| PdfError::Failed)?;

    let total_pages = document.pages().len() as usize;
    if total_pages == 0 || total_pages > PDF_MAX_PAGES || index >= total_pages {
        return Err(PdfError::Failed);
    }
    // `PdfPages::get` indexes with a `u16`. `PDF_MAX_PAGES` is well inside that,
    // but the conversion is done as a conversion rather than an `as` cast so a
    // future cap raise fails here instead of wrapping to page 0.
    let index = u16::try_from(index).map_err(|_| PdfError::Failed)?;

    if cancel.is_cancelled() {
        return Err(PdfError::Cancelled);
    }
    let config = PdfRenderConfig::new()
        .set_target_width(PDF_PAGE_WIDTH)
        .set_maximum_height(PDF_PAGE_MAX_HEIGHT)
        // `true`: the width/height constraints above describe the *result*, so
        // a quarter-turned page must be measured after turning, not before.
        .rotate(render_rotation(quarter_turns), true);

    let page = document.pages().get(index).map_err(|_| PdfError::Failed)?;
    let bitmap = page
        .render_with_config(&config)
        .map_err(|_| PdfError::Failed)?;
    Ok(RawImage::from_rgba(bitmap.as_image().into_rgba8()))
}

/// Quarter turns as pdfium's clockwise rotation enum.
fn render_rotation(quarter_turns: u8) -> PdfPageRenderRotation {
    match quarter_turns % 4 {
        1 => PdfPageRenderRotation::Degrees90,
        2 => PdfPageRenderRotation::Degrees180,
        3 => PdfPageRenderRotation::Degrees270,
        _ => PdfPageRenderRotation::None,
    }
}

/// Bind to a pdfium library shipped next to the executable, else a system one.
fn bind_pdfium() -> Option<Pdfium> {
    let bindings = exe_dir()
        .and_then(|dir| {
            Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir)).ok()
        })
        .or_else(|| Pdfium::bind_to_system_library().ok())?;
    Some(Pdfium::new(bindings))
}

fn exe_dir() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal one-page PDF (blank Letter page). Enough to exercise the full
    /// pdfium bind → parse → render path.
    const MINIMAL_PDF: &[u8] = b"%PDF-1.1\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n\
xref\n\
0 4\n\
0000000000 65535 f \n\
0000000009 00000 n \n\
0000000052 00000 n \n\
0000000101 00000 n \n\
trailer<</Size 4/Root 1 0 R>>\n\
startxref\n\
164\n\
%%EOF";

    fn pdfium_staged() -> bool {
        exe_dir()
            .map(|dir| Pdfium::pdfium_platform_library_name_at_path(&dir).exists())
            .unwrap_or(false)
    }

    #[test]
    fn renders_when_pdfium_present() {
        // Only runs where the vendored pdfium library is staged next to the
        // test binary (build.rs copies it there); otherwise it is a no-op so
        // CI without the DLL still passes.
        if !pdfium_staged() {
            eprintln!("pdfium library not staged next to test binary — skipping");
            return;
        }

        match render_pages(MINIMAL_PDF, &Cancel::never()) {
            Ok(PreviewPayload::Pdf {
                pages, total_pages, ..
            }) => {
                assert_eq!(total_pages, 1, "expected a single-page document");
                assert_eq!(pages.len(), 1, "expected one rendered page image");
            }
            Ok(other) => panic!(
                "unexpected preview payload: {:?}",
                std::mem::discriminant(&other)
            ),
            Err(_) => panic!("pdfium is present but rendering the minimal PDF failed"),
        }
    }

    /// A cancelled request must not rasterize a single page.
    #[test]
    fn an_already_cancelled_render_produces_no_pages() {
        if !pdfium_staged() {
            eprintln!("pdfium library not staged next to test binary — skipping");
            return;
        }
        assert!(matches!(
            render_pages(MINIMAL_PDF, &Cancel::already()),
            Err(PdfError::Cancelled)
        ));
    }
}
