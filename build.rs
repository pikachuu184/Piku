//! Build script: embeds the Windows executable resources (taskbar/app icon)
//! and, best-effort, bundles a standalone ffmpeg binary so video posters and
//! thumbnails work out of the box.

fn main() {
    #[cfg(windows)]
    {
        // Compiles `app.rc`, which references `assets/branding/piku.ico`, into
        // the executable so the OS shell/taskbar shows the PIKU mark.
        let _ = embed_resource::compile("app.rc", embed_resource::NONE);
        println!("cargo:rerun-if-changed=app.rc");
        println!("cargo:rerun-if-changed=assets/branding/piku.ico");
    }

    bundle_ffmpeg();
    bundle_pdfium();
}

/// Copy the vendored pdfium library next to the app executable (and the test
/// binaries) so PDF previews work with no user setup. Best-effort.
fn bundle_pdfium() {
    #[cfg(windows)]
    const LIB: &str = "vendor/pdfium/pdfium.dll";
    #[cfg(not(windows))]
    const LIB: &str = "vendor/pdfium/libpdfium.so";

    println!("cargo:rerun-if-changed={LIB}");
    let src = std::path::Path::new(LIB);
    if !src.exists() {
        return;
    }
    let Some(name) = src.file_name() else {
        return;
    };
    if let Some(dir) = target_profile_dir() {
        let _ = std::fs::copy(src, dir.join(name));
        let _ = std::fs::copy(src, dir.join("deps").join(name));
    }
}

/// Download a standalone ffmpeg during the build and copy it next to the final
/// executable so video previews work with no user setup. Best-effort: an
/// offline build (or an explicit `PIKU_SKIP_FFMPEG_DOWNLOAD=1`) simply ships
/// without video support instead of failing. The binary is an isolated
/// subprocess at runtime — never linked into the app.
fn bundle_ffmpeg() {
    println!("cargo:rerun-if-env-changed=PIKU_SKIP_FFMPEG_DOWNLOAD");
    if std::env::var_os("PIKU_SKIP_FFMPEG_DOWNLOAD").is_some() {
        return;
    }

    // Fetch ffmpeg only (no ffplay/ffprobe).
    unsafe {
        std::env::set_var("KEEP_ONLY_FFMPEG", "1");
    }
    if let Err(error) = ffmpeg_sidecar::download::auto_download() {
        println!("cargo:warning=PIKU: ffmpeg bundle skipped ({error})");
        return;
    }

    // Copy the downloaded binary next to the app executable (target/<profile>/),
    // where the runtime resolver (ffmpeg_sidecar::paths::ffmpeg_path) looks.
    let src = ffmpeg_sidecar::paths::ffmpeg_path();
    if let (Some(dir), Some(name)) = (target_profile_dir(), src.file_name()) {
        let _ = std::fs::copy(&src, dir.join(name));
    }
}

/// `target/<profile>/` derived from `OUT_DIR` (= `.../target/<profile>/build/<pkg>-<hash>/out`).
fn target_profile_dir() -> Option<std::path::PathBuf> {
    let out = std::env::var_os("OUT_DIR")?;
    std::path::Path::new(&out)
        .ancestors()
        .nth(3)
        .map(std::path::Path::to_path_buf)
}
