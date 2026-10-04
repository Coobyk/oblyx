//! Raw-WASM bridge for the browser converter. `worker.js` allocates input
//! buffers with `oblyx_alloc`, calls into these exports, and reads results
//! from the OUT buffer via `oblyx_out_ptr`/`oblyx_out_len`.
//!
//! Two flows share these exports:
//! - single notebook: `oblyx_open` → `oblyx_convert` → OUT holds the result.
//! - zip archive: `oblyx_open_archive` → per entry (`oblyx_open_entry`,
//!   `oblyx_convert` writes straight into an in-memory zip) → `oblyx_pack_zip`
//!   → OUT holds the packed archive.

use std::cell::RefCell;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use oblyx::convert::{Sink, ZipSink, zip_member_layout};
use oblyx::doc::{Document, Page};
use oblyx::render::pdf::document_to_pdf_with;
use oblyx::render::svg::page_to_svg;

/// Matches the native CLI default; pdf.js renders at `scale = DPI / 72`.
const DPI: f32 = 144.0;

const PHASE_DECODE: u32 = 0;
const PHASE_RENDER: u32 = 1;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "oblyx")]
unsafe extern "C" {
    fn progress(phase: u32, done: u32, total: u32);
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn progress(_: u32, _: u32, _: u32) {}

fn report(phase: u32, done: u32, total: u32) {
    // SAFETY: the worker provides the import; the host stub is a no-op.
    unsafe { progress(phase, done, total) };
}

/// One `.goodnotes` member of the opened zip archive.
#[derive(Clone)]
struct ArchEntry {
    member: String,
    sub: Option<PathBuf>,
    stem: String,
}

thread_local! {
    static DOC: RefCell<Option<Document>> = const { RefCell::new(None) };
    static JOBS: RefCell<Vec<(String, u64)>> = const { RefCell::new(Vec::new()) };
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static ARCHIVE: RefCell<Option<zip::ZipArchive<Cursor<Vec<u8>>>>> =
        const { RefCell::new(None) };
    static ENTRIES: RefCell<Vec<ArchEntry>> = const { RefCell::new(Vec::new()) };
    static CURRENT: RefCell<Option<ArchEntry>> = const { RefCell::new(None) };
    static SINK: RefCell<Option<ZipSink<Cursor<Vec<u8>>>>> = const { RefCell::new(None) };
}

fn set_out(bytes: Vec<u8>) -> i32 {
    OUT.with(|o| *o.borrow_mut() = bytes);
    0
}

fn fail(msg: String) -> i32 {
    set_out(msg.into_bytes());
    -1
}

/// Rebuild the boxed slice handed out by `oblyx_alloc` (exact length/capacity).
fn take_input(ptr: *mut u8, len: usize) -> Vec<u8> {
    unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) }.into_vec()
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn clear_archive() {
    ARCHIVE.with(|a| *a.borrow_mut() = None);
    ENTRIES.with(|e| *e.borrow_mut() = Vec::new());
    CURRENT.with(|c| *c.borrow_mut() = None);
    SINK.with(|s| *s.borrow_mut() = None);
}

#[unsafe(no_mangle)]
pub extern "C" fn oblyx_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8
}

#[unsafe(no_mangle)]
pub extern "C" fn oblyx_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
    }
}

/// Parse the archive. OUT becomes `{"pages":N,"jobs":[["att-uuid",page],…]}`.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_open(ptr: *mut u8, len: usize) -> i32 {
    clear_archive();
    open_doc(take_input(ptr, len))
}

fn open_doc(data: Vec<u8>) -> i32 {
    match Document::from_bytes(data) {
        Ok(doc) => {
            let jobs = doc.pdf_raster_jobs();
            let mut json = format!("{{\"pages\":{},\"jobs\":[", doc.pages.len());
            for (i, (att, page)) in jobs.iter().enumerate() {
                if i > 0 {
                    json.push(',');
                }
                json.push_str(&format!("[\"{att}\",{page}]"));
            }
            json.push_str("]}");
            JOBS.with(|j| *j.borrow_mut() = jobs);
            DOC.with(|d| *d.borrow_mut() = Some(doc));
            set_out(json.into_bytes())
        }
        Err(e) => fail(format!("could not open notebook: {e:#}")),
    }
}

/// Parse a `.zip` full of notebooks and start a fresh output archive.
/// OUT becomes `{"entries":[{"path":…,"sub":…,"stem":…},…]}` (junk such as
/// `__MACOSX` and hidden files is skipped; structure is preserved).
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_open_archive(ptr: *mut u8, len: usize) -> i32 {
    let data = take_input(ptr, len);
    let mut archive = match zip::ZipArchive::new(Cursor::new(data)) {
        Ok(archive) => archive,
        Err(e) => return fail(format!("could not open zip archive: {e}")),
    };
    let mut entries: Vec<ArchEntry> = Vec::new();
    for i in 0..archive.len() {
        let name = match archive.by_index(i) {
            Ok(entry) => entry.name().to_string(),
            Err(e) => return fail(format!("read zip entry {i}: {e}")),
        };
        if let Some((sub, stem)) = zip_member_layout(&name) {
            entries.push(ArchEntry {
                member: name,
                sub,
                stem,
            });
        }
    }
    entries.sort_by(|a, b| a.member.cmp(&b.member));
    let mut json = String::from("{\"entries\":[");
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        let sub = entry
            .sub
            .as_ref()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        json.push_str(&format!(
            "{{\"path\":{},\"sub\":{},\"stem\":{}}}",
            json_string(&entry.member),
            json_string(&sub),
            json_string(&entry.stem)
        ));
    }
    json.push_str("]}");
    DOC.with(|d| *d.borrow_mut() = None);
    JOBS.with(|j| *j.borrow_mut() = Vec::new());
    CURRENT.with(|c| *c.borrow_mut() = None);
    SINK.with(|s| *s.borrow_mut() = None);
    ARCHIVE.with(|a| *a.borrow_mut() = Some(archive));
    ENTRIES.with(|e| *e.borrow_mut() = entries);
    set_out(json.into_bytes())
}

/// Start collecting converted output into a new in-memory zip. Call after
/// `oblyx_open_archive` (and before looping entries) for archive output;
/// without it each `oblyx_convert` returns its result directly.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_begin_archive() -> i32 {
    SINK.with(|s| *s.borrow_mut() = Some(ZipSink::new(Cursor::new(Vec::new()))));
    set_out(Vec::new())
}

/// Name the notebook about to be opened with `oblyx_open_member`.
/// The bytes are a UTF-8 zip member path (`math/week 3/note.goodnotes`).
/// Does not touch the output zip started by `oblyx_begin_archive`.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_set_member(ptr: *mut u8, len: usize) -> i32 {
    let path = String::from_utf8(take_input(ptr, len)).unwrap_or_default();
    let Some((sub, stem)) = zip_member_layout(&path) else {
        return fail(format!("not a notebook: {path}"));
    };
    CURRENT.with(|c| {
        *c.borrow_mut() = Some(ArchEntry {
            member: path,
            sub,
            stem,
        });
    });
    set_out(Vec::new())
}

/// Open one notebook's bytes without dropping an in-progress output archive.
/// OUT becomes the same jobs JSON as `oblyx_open`.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_open_member(ptr: *mut u8, len: usize) -> i32 {
    open_doc(take_input(ptr, len))
}

/// Load archive entry `index` (order from `oblyx_open_archive`). OUT becomes
/// the same jobs JSON as `oblyx_open`.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_open_entry(index: u32) -> i32 {
    let entry = ENTRIES.with(|e| e.borrow().get(index as usize).cloned());
    let Some(entry) = entry else {
        return fail(format!("no archive entry {index}"));
    };
    let data = ARCHIVE.with(|a| -> Result<Vec<u8>, String> {
        let mut guard = a.borrow_mut();
        let archive = guard
            .as_mut()
            .ok_or_else(|| "no archive open".to_string())?;
        let mut file = archive
            .by_name(&entry.member)
            .map_err(|e| format!("{}: {e}", entry.member))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| format!("read {}: {e}", entry.member))?;
        Ok(buf)
    });
    match data {
        Ok(data) => {
            CURRENT.with(|c| *c.borrow_mut() = Some(entry));
            open_doc(data)
        }
        Err(e) => fail(e),
    }
}

/// OUT becomes the raw bytes of a PDF attachment (fed to pdf.js).
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_attachment(ptr: *mut u8, len: usize) -> i32 {
    let uuid = String::from_utf8_lossy(&take_input(ptr, len)).into_owned();
    let bytes = DOC.with(|d| {
        d.borrow()
            .as_ref()
            .and_then(|doc| doc.attachments.get(&uuid).map(|b| (**b).clone()))
    });
    match bytes {
        Some(b) => set_out(b),
        None => fail(format!("unknown attachment {uuid}")),
    }
}

/// Store a pdf.js-rendered JPEG for job `index` (order from `oblyx_open`).
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_set_raster(index: u32, ptr: *mut u8, len: usize) -> i32 {
    let jpeg = take_input(ptr, len);
    let Some(job) = JOBS.with(|j| j.borrow().get(index as usize).cloned()) else {
        return fail(format!("no raster job {index}"));
    };
    DOC.with(|d| match d.borrow().as_ref() {
        Some(doc) => match doc.set_page_raster(&job.0, job.1, DPI, jpeg) {
            Ok(()) => set_out(Vec::new()),
            Err(e) => fail(format!("store raster: {e:#}")),
        },
        None => fail("no document".into()),
    })
}

/// Decode every page, then render. For a single notebook OUT becomes the PDF
/// (fmt 0) or SVG (fmt 1, zip if the notebook has more than one page). Inside
/// an archive run the bytes are written into the pending zip instead and OUT
/// comes back empty (`oblyx_pack_zip` collects everything at the end).
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_convert(fmt: u32) -> i32 {
    DOC.with(|d| {
        let doc_ref = d.borrow();
        let Some(doc) = doc_ref.as_ref() else {
            return fail("no document".into());
        };
        let total = doc.pages.len() as u32;
        // Sequential on purpose: rayon's thread pool cannot run on wasm.
        let mut pages = Vec::with_capacity(doc.pages.len());
        for (i, src) in doc.pages.iter().enumerate() {
            match doc.decode_page(src, false, DPI) {
                Ok(page) => pages.push(page),
                Err(e) => return fail(format!("decode page {}: {e:#}", src.uuid)),
            }
            report(PHASE_DECODE, i as u32 + 1, total);
        }
        let archived = SINK.with(|s| s.borrow().is_some());
        match fmt {
            0 => {
                let bytes = document_to_pdf_with(&pages, doc, |done| {
                    report(PHASE_RENDER, done as u32, total);
                });
                if archived {
                    let Some(entry) = CURRENT.with(|c| c.borrow().clone()) else {
                        return fail("no archive entry".into());
                    };
                    let target = archive_pdf_path(&entry);
                    if let Err(e) = write_archive(&target, bytes) {
                        return fail(e);
                    }
                    set_out(Vec::new())
                } else {
                    set_out(bytes)
                }
            }
            1 => {
                if archived {
                    let Some(entry) = CURRENT.with(|c| c.borrow().clone()) else {
                        return fail("no archive entry".into());
                    };
                    for (i, page) in pages.iter().enumerate() {
                        let target = archive_svg_path(&entry, &page.uuid);
                        let svg = page_to_svg(page, doc);
                        if let Err(e) = write_archive(&target, svg.into_bytes()) {
                            return fail(e);
                        }
                        report(PHASE_RENDER, i as u32 + 1, total);
                    }
                    set_out(Vec::new())
                } else {
                    match render_svg(&pages, doc, total) {
                        Ok(bytes) => set_out(bytes),
                        Err(e) => fail(e),
                    }
                }
            }
            _ => fail(format!("unknown format {fmt}")),
        }
    })
}

/// `sub/stem.pdf` inside the pending archive (structure preserved).
fn archive_pdf_path(entry: &ArchEntry) -> PathBuf {
    let mut path = entry.sub.clone().unwrap_or_default();
    path.push(format!("{}.pdf", entry.stem));
    path
}

/// `sub/stem/<uuid>.svg` inside the pending archive — mirrors the CLI's
/// per-page SVG folders.
fn archive_svg_path(entry: &ArchEntry, uuid: &str) -> PathBuf {
    let mut path = entry.sub.clone().unwrap_or_default();
    path.push(&entry.stem);
    path.push(format!("{uuid}.svg"));
    path
}

fn write_archive(target: &Path, bytes: Vec<u8>) -> Result<(), String> {
    SINK.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(sink) = guard.as_mut() else {
            return Err("no archive in progress".into());
        };
        sink.write(Path::new(""), target, bytes)
            .map_err(|e| format!("write {}: {e:#}", target.display()))
    })
}

/// Finish the archive opened by `oblyx_open_archive`. OUT becomes the zip.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_pack_zip() -> i32 {
    let sink = SINK.with(|s| s.borrow_mut().take());
    let Some(sink) = sink else {
        return fail("no archive in progress".into());
    };
    match sink.finish() {
        Ok(cursor) => {
            ARCHIVE.with(|a| *a.borrow_mut() = None);
            ENTRIES.with(|e| *e.borrow_mut() = Vec::new());
            CURRENT.with(|c| *c.borrow_mut() = None);
            set_out(cursor.into_inner())
        }
        Err(e) => fail(format!("finish archive: {e:#}")),
    }
}

fn render_svg(pages: &[Page], doc: &Document, total: u32) -> Result<Vec<u8>, String> {
    if let [page] = pages {
        return Ok(page_to_svg(page, doc).into_bytes());
    }
    let mut buf = Cursor::new(Vec::new());
    let mut zipw = zip::ZipWriter::new(&mut buf);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (i, page) in pages.iter().enumerate() {
        zipw.start_file(format!("page-{:03}.svg", i + 1), opts)
            .map_err(|e| format!("zip: {e}"))?;
        zipw.write_all(page_to_svg(page, doc).as_bytes())
            .map_err(|e| format!("zip: {e}"))?;
        report(PHASE_RENDER, i as u32 + 1, total);
    }
    zipw.finish().map_err(|e| format!("zip: {e}"))?;
    Ok(buf.into_inner())
}

#[unsafe(no_mangle)]
pub extern "C" fn oblyx_out_ptr() -> *const u8 {
    OUT.with(|o| o.borrow().as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn oblyx_out_len() -> usize {
    OUT.with(|o| o.borrow().len())
}

#[unsafe(no_mangle)]
pub extern "C" fn oblyx_out_clear() {
    OUT.with(|o| *o.borrow_mut() = Vec::new());
}
