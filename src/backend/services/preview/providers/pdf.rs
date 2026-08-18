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

/// Pages rasterized for the (scrollable) preview; large documents show the
/// first N with a note.
const PDF_PREVIEW_PAGES: usize = 12;
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

        // Cap the size before the C parser ever sees the bytes.
        let (bytes, total) = read::read_head(ctx.path, PDF_MAX_BYTES as usize)?;
        if total > PDF_MAX_BYTES {
            return Ok(PreviewPayload::TooLarge { size: total });
        }

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
        // Each page is a full rasterization at up to 1000x4000; a twelve-page
        // document is real work, and a superseded selection should not pay for
        // the remaining pages.
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

    let note = (total_pages > render_count).then(|| {
        format!("Showing the first {render_count} of {total_pages} pages.")
            .as_str()
            .into()
    });
    Ok(PreviewPayload::Pdf {
        pages,
        total_pages,
        note,
    })
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
