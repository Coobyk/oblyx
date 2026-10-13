//! Raw-WASM bridge for the browser converter. `worker.js` allocates input
//! buffers with `oblyx_alloc`, calls into these exports, and reads results
//! from the OUT buffer via `oblyx_out_ptr`/`oblyx_out_len`.
//!
//! Two flows share these exports:
//! - single notebook: `oblyx_open` → `oblyx_convert` → OUT holds the result.
//! - zip archive: `oblyx_open_archive` → per entry (`oblyx_open_entry`,
//!   `oblyx_convert` writes straight into an in-memory zip) → `oblyx_pack_zip`
//!   → OUT holds the packed archive.

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // lopdf only needs entropy for encrypted PDFs; GoodNotes attachments are
    // not encrypted. A deterministic fill keeps us on raw WASM (no wbg).
    let buf = unsafe { std::slice::from_raw_parts_mut(dest, len) };
    let mut x = 0xA5u8;
    for b in buf {
        x = x.wrapping_mul(37).wrapping_add(17);
        *b = x;
    }
    Ok(())
}

use std::cell::RefCell;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use oblyx::convert::{Sink, ZipSink, zip_member_layout};
use oblyx::doc::{Document, Page};
use oblyx::goodnotes_write::write_goodnotes_to_vec;
use oblyx::ntb_write::write_ntb_to_vec;
use oblyx::render::pdf::document_to_pdf_with;
use oblyx::render::png::page_to_jpeg;
use oblyx::render::svg::page_to_svg;

/// Matches the native CLI default (used when decoding pages).
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

/// One `.goodnotes` or `.ntb` member of the opened zip archive.
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
    /// Pre-built pages from a `.ntb` (None for GoodNotes, which uses `decode_page`).
    static NTB_PAGES: RefCell<Option<Vec<Page>>> = const { RefCell::new(None) };
    /// Decoded + background-injected pages, cached after first use so
    /// `oblyx_render_page` and `oblyx_convert` share one decode pass.
    static DECODED_PAGES: RefCell<Option<Vec<Page>>> = const { RefCell::new(None) };
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
    DECODED_PAGES.with(|p| *p.borrow_mut() = None);
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

/// Parse the archive. OUT becomes `{"pages":N}`.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_open(ptr: *mut u8, len: usize) -> i32 {
    clear_archive();
    open_doc(take_input(ptr, len))
}

fn open_doc(data: Vec<u8>) -> i32 {
    // Detect .ntb: a zip containing noteBundle.
    if is_ntb_bytes(&data) {
        return open_ntb(data);
    }
    NTB_PAGES.with(|p| *p.borrow_mut() = None);
    DECODED_PAGES.with(|p| *p.borrow_mut() = None);
    match Document::from_bytes(data) {
        Ok(doc) => {
            // Background pages that SVG/PNG must rasterize through pdf.js.
            // PDF output embeds them as Form XObjects and ignores this list.
            let jobs = doc.pdf_raster_jobs(doc.pages.iter().map(|p| p.uuid.as_str()));
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

/// A `.ntb` is a zip whose `noteBundle` member holds the CRDT op log.
fn is_ntb_bytes(data: &[u8]) -> bool {
    let mut cursor = Cursor::new(data);
    zip::ZipArchive::new(&mut cursor)
        .map(|mut z| z.by_name("noteBundle").is_ok())
        .unwrap_or(false)
}

/// Parse a `.ntb` from raw bytes and store the resulting document + pages.
fn open_ntb(data: Vec<u8>) -> i32 {
    match parse_ntb(data) {
        Ok((doc, pages)) => {
            let raster_jobs = doc.pdf_raster_jobs(pages.iter().map(|p| p.uuid.as_str()));
            let mut json = format!("{{\"pages\":{},\"jobs\":[", pages.len());
            for (i, (att, page)) in raster_jobs.iter().enumerate() {
                if i > 0 {
                    json.push(',');
                }
                json.push_str(&format!("[\"{att}\",{page}]"));
            }
            json.push_str("]}");
            JOBS.with(|j| *j.borrow_mut() = raster_jobs);
            DOC.with(|d| *d.borrow_mut() = Some(doc));
            NTB_PAGES.with(|p| *p.borrow_mut() = Some(pages));
            DECODED_PAGES.with(|p| *p.borrow_mut() = None);
            set_out(json.into_bytes())
        }
        Err(e) => fail(format!("could not open Notability notebook: {e}")),
    }
}

/// Parse `.ntb` bytes into a `Document` + pre-built `Page`s, binding PDF
/// backgrounds. Mirrors the native `load_ntb_pages` path.
fn parse_ntb(bytes: Vec<u8>) -> Result<(Document, Vec<Page>), String> {
    use std::io::Read as _;
    let mut z = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("ntb zip: {e}"))?;
    let mut bundle = Vec::new();
    z.by_name("noteBundle")
        .and_then(|mut f| f.read_to_end(&mut bundle).map_err(|e| e.into()))
        .map_err(|e| format!("noteBundle: {e}"))?;
    let mut assets = Vec::new();
    for i in 0..z.len() {
        let entry = z.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let name = entry.name().to_string();
        if name.starts_with("assets/") {
            let mut buf = Vec::new();
            let mut entry = entry;
            entry
                .read_to_end(&mut buf)
                .map_err(|e| format!("read {name}: {e}"))?;
            assets.push((name, buf));
        }
    }
    let (meta, strokes, pages, texts, shapes) =
        oblyx::ntb::parse_bundle_public(&bundle).map_err(|e| format!("parse bundle: {e}"))?;
    let ntb_doc = oblyx::ntb::NtbDocument {
        meta,
        strokes,
        pages,
        texts,
        shapes,
        manifest: Vec::new(),
        thumbnail: None,
        assets,
    };
    let pages = oblyx::ntb::to_pages(&ntb_doc, oblyx::ntb::NTB_PAGE_H);
    let mut doc = Document::empty();
    let mut hash_to_key: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (name, bytes) in &ntb_doc.assets {
        let key = name.rsplit('/').next().unwrap_or(name).to_string();
        if let Some(hex) = name
            .rsplit('/')
            .next()
            .and_then(|f| f.split('.').next())
            .filter(|h| h.len() == 128 && h.chars().all(|c| c.is_ascii_hexdigit()))
        {
            hash_to_key.insert(hex.to_ascii_lowercase(), key.clone());
        }
        doc.insert_attachment(key, bytes.clone());
    }
    for page in &pages {
        let bg = page.uuid.split_once('#').and_then(|(_, rest)| {
            let (h, p) = rest.split_once(':')?;
            let n: u64 = p.parse().ok()?;
            Some((h.to_string(), n))
        });
        if let Some((hash_hex, pdf_page)) = bg
            && let Some(key) = hash_to_key.get(&hash_hex)
        {
            doc.set_page_cfg(&page.uuid, key.clone(), Some(pdf_page));
        }
    }
    // Backgrounds are injected later in oblyx_convert once JS has rendered
    // the PDF pages via pdf.js and stored them with oblyx_set_raster.
    Ok((doc, pages))
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
/// OUT becomes the same JSON as `oblyx_open`.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_open_member(ptr: *mut u8, len: usize) -> i32 {
    open_doc(take_input(ptr, len))
}

/// Load archive entry `index` (order from `oblyx_open_archive`). OUT becomes
/// the same JSON as `oblyx_open`.
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
/// SVG/PNG output rasterizes PDF backgrounds; PDF output embeds them as Form
/// XObjects and never asks for these.
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

/// Decode every page (once) and inject PDF backgrounds from JS-rendered
/// rasters. Returns the cached page list. Both `oblyx_render_page` and
/// `oblyx_convert` go through here so a single decode pass serves thumbnails
/// and the final output.
fn ensure_pages(doc: &Document) -> Result<Vec<Page>, String> {
    let cached = DECODED_PAGES.with(|p| p.borrow().clone());
    if let Some(pages) = cached {
        return Ok(pages);
    }
    let ntb_pages = NTB_PAGES.with(|p| p.borrow().clone());
    let total = if let Some(ref np) = ntb_pages {
        np.len() as u32
    } else {
        doc.pages.len() as u32
    };
    let mut pages = if let Some(np) = ntb_pages {
        np
    } else {
        let mut pages = Vec::with_capacity(doc.pages.len());
        for (i, src) in doc.pages.iter().enumerate() {
            match doc.decode_page(src, false) {
                Ok(page) => pages.push(page),
                Err(e) => return Err(format!("decode page {}: {e:#}", src.uuid)),
            }
            report(PHASE_DECODE, i as u32 + 1, total);
        }
        pages
    };
    doc.inject_pdf_backgrounds(&mut pages);
    DECODED_PAGES.with(|p| *p.borrow_mut() = Some(pages.clone()));
    Ok(pages)
}

/// Render page `index` (0-based) as a JPEG thumbnail at scale 2.0 (144 dpi,
/// matching the background raster cache). OUT becomes the JPEG bytes.
/// Call after `oblyx_set_raster` has filled the background cache.
#[unsafe(no_mangle)]
pub extern "C" fn oblyx_render_page(index: u32) -> i32 {
    DOC.with(|d| {
        let doc_ref = d.borrow();
        let Some(doc) = doc_ref.as_ref() else {
            return fail("no document".into());
        };
        let pages = match ensure_pages(doc) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        let Some(page) = pages.get(index as usize) else {
            return fail(format!("no page {index}"));
        };
        match page_to_jpeg(page, doc, 2.0, 82) {
            Ok(bytes) => set_out(bytes),
            Err(e) => fail(format!("render page {index}: {e:#}")),
        }
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
        let pages = match ensure_pages(doc) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        let total = pages.len() as u32;
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
                        let svg = page_to_svg(page, doc, DPI);
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
            2 => {
                // GoodNotes format.
                let bytes = match write_goodnotes_to_vec(&pages, doc) {
                    Ok(b) => b,
                    Err(e) => return fail(format!("write goodnotes: {e:#}")),
                };
                report(PHASE_RENDER, total, total);
                if archived {
                    let Some(entry) = CURRENT.with(|c| c.borrow().clone()) else {
                        return fail("no archive entry".into());
                    };
                    let target = archive_notebook_path(&entry, "goodnotes");
                    if let Err(e) = write_archive(&target, bytes) {
                        return fail(e);
                    }
                    set_out(Vec::new())
                } else {
                    set_out(bytes)
                }
            }
            3 => {
                // Notability (.ntb) format.
                let bytes = match write_ntb_to_vec(&pages, doc) {
                    Ok(b) => b,
                    Err(e) => return fail(format!("write ntb: {e:#}")),
                };
                report(PHASE_RENDER, total, total);
                if archived {
                    let Some(entry) = CURRENT.with(|c| c.borrow().clone()) else {
                        return fail("no archive entry".into());
                    };
                    let target = archive_notebook_path(&entry, "ntb");
                    if let Err(e) = write_archive(&target, bytes) {
                        return fail(e);
                    }
                    set_out(Vec::new())
                } else {
                    set_out(bytes)
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

/// `sub/stem.goodnotes` or `sub/stem.ntb` inside the pending archive.
fn archive_notebook_path(entry: &ArchEntry, ext: &str) -> PathBuf {
    let mut path = entry.sub.clone().unwrap_or_default();
    path.push(format!("{}.{}", entry.stem, ext));
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
        return Ok(page_to_svg(page, doc, DPI).into_bytes());
    }
    let mut buf = Cursor::new(Vec::new());
    let mut zipw = zip::ZipWriter::new(&mut buf);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (i, page) in pages.iter().enumerate() {
        zipw.start_file(format!("page-{:03}.svg", i + 1), opts)
            .map_err(|e| format!("zip: {e}"))?;
        zipw.write_all(page_to_svg(page, doc, DPI).as_bytes())
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
