//! Rasterize pages of PDF-imported notebooks (scanned books) via poppler's
//! `pdftoppm`. Pages in these notebooks reference a shared PDF attachment plus
//! a page index instead of storing strokes or images directly.

#[cfg(not(target_arch = "wasm32"))]
use anyhow::Result;
use std::io::Cursor;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Command, Stdio};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Condvar, LazyLock, Mutex};

#[cfg(not(target_arch = "wasm32"))]
use crate::vlog;

#[cfg(not(target_arch = "wasm32"))]
const MAX_RASTERIZERS: usize = 6;
#[cfg(not(target_arch = "wasm32"))]
static TOOL_WARNED: AtomicBool = AtomicBool::new(false);

#[cfg(not(target_arch = "wasm32"))]
struct Slots {
    free: Mutex<usize>,
    ready: Condvar,
}

#[cfg(not(target_arch = "wasm32"))]
static SLOTS: LazyLock<Slots> = LazyLock::new(|| Slots {
    free: Mutex::new(MAX_RASTERIZERS),
    ready: Condvar::new(),
});

#[cfg(not(target_arch = "wasm32"))]
struct SlotGuard;

#[cfg(not(target_arch = "wasm32"))]
impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut free = SLOTS.free.lock().unwrap_or_else(|e| e.into_inner());
        *free += 1;
        SLOTS.ready.notify_one();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn acquire_slot() -> SlotGuard {
    let mut free = SLOTS.free.lock().unwrap_or_else(|e| e.into_inner());
    while *free == 0 {
        free = SLOTS.ready.wait(free).unwrap_or_else(|e| e.into_inner());
    }
    *free -= 1;
    SlotGuard
}

pub(crate) fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    dec.read_info().ok()?;
    let info = dec.info()?;
    Some((u32::from(info.width), u32::from(info.height)))
}

/// Render one PDF page to JPEG bytes. Returns `None` (with a one-time hint if
/// `pdftoppm` is missing) so callers can fall back to an empty page.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn rasterize_pdf_page(pdf_path: &Path, page: u64, dpi: f32) -> Option<Vec<u8>> {
    let _slot = acquire_slot();
    let dpi = dpi.round().clamp(36.0, 600.0) as u32;
    let mut out_base = pdf_path.as_os_str().to_os_string();
    out_base.push(format!("-p{page}-d{dpi}"));
    let out_base = PathBuf::from(out_base);

    let page_s = page.to_string();
    let dpi_s = dpi.to_string();
    let res = Command::new("pdftoppm")
        .args([
            "-f",
            &page_s,
            "-l",
            &page_s,
            "-r",
            &dpi_s,
            "-singlefile",
            "-jpeg",
            "-jpegopt",
            "quality=85",
        ])
        .arg(pdf_path)
        .arg(&out_base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    match res {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !TOOL_WARNED.swap(true, Ordering::SeqCst) {
                eprintln!(
                    "oblyx: pdftoppm not found; PDF-based notebooks need poppler \
                     (apt install poppler-utils / brew install poppler)"
                );
            }
            None
        }
        Err(e) => {
            vlog!(1, "pdftoppm spawn failed: {e}");
            None
        }
        Ok(out) if out.status.success() => {
            // pdftoppm appends ".jpg" to the out-base (which itself contains
            // dots) — do not use Path::with_extension, it would mangle it.
            let mut jpg_os = out_base.clone().into_os_string();
            jpg_os.push(".jpg");
            let jpg = PathBuf::from(jpg_os);
            let bytes = std::fs::read(&jpg).ok();
            let _ = std::fs::remove_file(&jpg);
            match &bytes {
                Some(b) => vlog!(
                    1,
                    "pdf page {page}: rasterized {}x{} at {dpi} dpi, {} bytes",
                    jpeg_size(b).map_or(0, |s| s.0),
                    jpeg_size(b).map_or(0, |s| s.1),
                    b.len()
                ),
                None => vlog!(1, "pdf page {page}: pdftoppm produced no image"),
            }
            bytes
        }
        Ok(out) => {
            vlog!(
                1,
                "pdf page {page}: pdftoppm failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            None
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pdf_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    base.join("oblyx").join("pdf")
}

/// Best-effort removal of temp PDFs left behind by crashed runs (>7 days old).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn sweep_stale(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with("oblyx-") && name.ends_with(".pdf")) {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() > 7 * 24 * 3600);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_temp_pdf(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}
