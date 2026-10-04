use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use zip::ZipArchive;

use crate::bv4::decompress_bv4;
use crate::paper::paper_rects;
use crate::pb::*;
use crate::pdfinput;
use crate::tpl;

pub const PAGE_W: f32 = 1091.35;
pub const PAGE_H: f32 = 1543.5;

const KIND22_WIDTH: f32 = 1.5591;
const FRAME_WIDTH: f32 = 1.5591;
const LINE_R: f32 = 11.0;
const CONN_R: f32 = 5.6;
const ARROW_LEN: f32 = 9.0;
const ARROW_HALF: f32 = 4.5;

pub struct PageSource {
    pub uuid: String,
    pub data: Vec<u8>,
}

/// Per-page configuration resolved from the events index: which background
/// attachment the page uses and (for PDF-imported notebooks) which page of
/// that PDF to show.
#[derive(Debug, Clone)]
pub struct PageCfg {
    pub attachment: String,
    /// 1-based page number inside the PDF attachment.
    pub pdf_page: Option<u64>,
}

pub struct Document {
    pub schema: u64,
    pub pages: Vec<PageSource>,
    pub attachments: HashMap<String, Arc<Vec<u8>>>,
    page_cfg: HashMap<String, PageCfg>,
    /// Paper rectangles per background attachment, parsed once and shared
    /// across every page (backgrounds can be a 300 MB PDF).
    paper_cache: Mutex<HashMap<usize, Arc<PaperRects>>>,
    /// Temp copies of PDF attachments for pdftoppm, keyed by attachment
    /// pointer so aliased attachment uuids share one file.
    pdf_temps: Mutex<HashMap<usize, PathBuf>>,
    /// Single-flight raster results keyed by (attachment ptr, pdf page, dpi).
    /// Several notebook pages can share one PDF page (copied pages); without
    /// this they race on the same pdftoppm output file and corrupt/lose it.
    raster_cache: Mutex<HashMap<(usize, u64, u32), RasterCell>>,
    /// Per-document nonce so temp names never collide across processes.
    /// Unused on wasm, where `temp_pdf`/pdftoppm do not exist.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    temp_tag: u64,
}

static DOC_TAG: AtomicU64 = AtomicU64::new(0);

impl Drop for Document {
    fn drop(&mut self) {
        if let Ok(temps) = self.pdf_temps.lock() {
            for path in temps.values() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    Stroke(Stroke),
    Image(ImageEl),
    Sticky(Sticky),
    Text(TextBox),
    Shape(ShapeEl),
    Path(PathEl),
    FillPath(FillPathEl),
    Connector(ConnectorEl),
    Background(Background),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShapeKind {
    Rect,
    Ellipse,
    Polygon,
    Triangle,
    Diamond,
}

#[derive(Debug, Clone)]
pub struct ShapeEl {
    pub kind: ShapeKind,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub points: Vec<[f32; 2]>,
    pub rotation: f32,
    pub fill: [f32; 4],
    pub stroke: [f32; 4],
    pub width: f32,
    pub dash: Option<[f32; 2]>,
    pub radius: f32,
    pub uuid: Option<String>,
    pub from_f9: bool,
}

#[derive(Debug, Clone)]
pub struct PathEl {
    pub points: Vec<[f32; 2]>,
    pub head: Option<[[f32; 2]; 3]>,
    pub width: f32,
    pub rgba: [f32; 4],
}

#[derive(Debug, Clone)]
pub struct FillPathEl {
    pub contours: Vec<Vec<[f32; 2]>>,
    pub rgba: [f32; 4],
}

#[derive(Debug, Clone)]
pub struct ConnectorEl {
    pub a: String,
    pub b: String,
    pub anchor: [f32; 2],
    pub width: f32,
    pub rgba: [f32; 4],
}

#[derive(Debug, Clone)]
pub struct Background {
    pub rects: Vec<(f32, f32, f32, f32, [f32; 3])>,
}

#[derive(Debug, Clone)]
pub struct Stroke {
    pub points: Vec<[f32; 2]>,
    pub width: f32,
    pub rgba: [f32; 4],
    pub dash: Option<[f32; 2]>,
    pub parented: bool,
}

#[derive(Debug, Clone)]
pub struct ImageEl {
    pub attachment: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub from_frame: bool,
    /// Inline image data (rasterized PDF page) when the bytes are not part of
    /// the shared document attachment map.
    pub bytes: Option<Arc<Vec<u8>>>,
}

#[derive(Debug, Clone)]
pub struct Sticky {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub rgb: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct TextBox {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub text: String,
    pub font: Option<String>,
    pub rgb: [f32; 3],
    pub bold: bool,
}

pub struct Page {
    pub uuid: String,
    pub items: Vec<Item>,
}

type Zip = ZipArchive<BufReader<File>>;
/// `(x, y, w, h, rgb)` per paper rectangle.
type PaperRects = Vec<(f32, f32, f32, f32, [f32; 3])>;
/// Single-flight result of rasterizing one PDF page.
type RasterCell = Arc<Mutex<Option<Arc<Vec<u8>>>>>;
/// `(configs, page_cfgs)` as returned by [`parse_events`].
type EventsIndex = (
    HashMap<String, (String, Option<u64>)>,
    HashMap<String, String>,
);

fn read_member<R: Read + Seek>(z: &mut ZipArchive<R>, name: &str) -> Result<Option<Vec<u8>>> {
    let mut file = match z.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("zip open {name}")),
    };
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(Some(buf))
}

fn index_entries(raw: &[u8]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for rec in records(raw)? {
        let f = parse(rec)?;
        if let (Some(a), Some(b)) = (bytes_of(&f, 1), bytes_of(&f, 2)) {
            if let (Ok(u), Ok(p)) = (std::str::from_utf8(a), std::str::from_utf8(b)) {
                out.push((u.to_string(), p.to_string()));
            }
        }
    }
    Ok(out)
}

fn uuid_prev(u: &str) -> Option<String> {
    let (head, last) = u.rsplit_once('-')?;
    if last.len() < 2 {
        return None;
    }
    // GoodNotes decrements only the final byte with wraparound
    // (…-5E00 → …-5EFF), not the whole 48-bit tail (…-5DFF).
    let (prefix, byte) = last.split_at(last.len() - 2);
    let b = u8::from_str_radix(byte, 16).ok()?.wrapping_sub(1);
    Some(format!("{head}-{prefix}{b:02X}"))
}

/// Returns `(configs, page_cfgs)`:
/// - `configs`: config uuid -> (attachment uuid, 1-based page index in that
///   attachment for PDF-imported notebooks)
/// - `page_cfgs`: page uuid (minus one) -> config uuid
fn parse_events(raw: &[u8]) -> Result<EventsIndex> {
    let mut configs: HashMap<String, (String, Option<u64>)> = HashMap::new();
    let mut page_cfgs: HashMap<String, String> = HashMap::new();
    for rec in records(raw)? {
        let f = parse(rec)?;
        if let Some(p2) = bytes_of(&f, 2) {
            if let Ok(pf) = parse(p2) {
                if let (Some(ev), Some(att)) = (
                    bytes_of(&f, 1).and_then(as_uuid),
                    bytes_of(&pf, 4).and_then(as_uuid),
                ) {
                    configs.insert(ev.to_string(), (att.to_string(), varint_of(&pf, 5)));
                }
            }
        }
        if let Some(p54) = bytes_of(&f, 54) {
            if let Ok(pf) = parse(p54) {
                let page = bytes_of(&pf, 2).and_then(as_uuid);
                let cfg = bytes_of(&pf, 3)
                    .and_then(|m| parse(m).ok())
                    .and_then(|m| bytes_of(&m, 1).and_then(as_uuid));
                if let (Some(p), Some(c)) = (page, cfg) {
                    page_cfgs.insert(p.to_string(), c.to_string());
                }
            }
        }
    }
    Ok((configs, page_cfgs))
}

impl Document {
    pub fn open(path: &Path) -> Result<Document> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let z = Zip::new(BufReader::new(file))
            .with_context(|| format!("{} is not a readable zip archive", path.display()))?;
        Self::load(z)
    }

    /// Open a notebook from an in-memory archive (the WASM/web entry point).
    pub fn from_bytes(data: Vec<u8>) -> Result<Document> {
        let z = ZipArchive::new(Cursor::new(data)).context("not a readable zip archive")?;
        Self::load(z)
    }

    fn load<R: Read + Seek>(mut z: ZipArchive<R>) -> Result<Document> {
        let schema = match read_member(&mut z, "schema.pb")? {
            Some(raw) => parse(&raw).ok().and_then(|f| varint_of(&f, 1)).unwrap_or(0),
            None => 0,
        };

        let mut pages = Vec::new();
        if let Some(idx) = read_member(&mut z, "index.notes.pb")? {
            for (uuid, member) in index_entries(&idx)? {
                let data = read_member(&mut z, &member)?
                    .filter(|d| !d.is_empty())
                    .unwrap_or_default();
                pages.push(PageSource { uuid, data });
            }
        }

        let mut attachments = HashMap::new();
        let mut member_cache: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
        if let Some(idx) = read_member(&mut z, "index.attachments.pb")? {
            for (uuid, member) in index_entries(&idx)? {
                let data = match member_cache.get(&member) {
                    Some(cached) => Arc::clone(cached),
                    None => {
                        let Some(data) = read_member(&mut z, &member)? else {
                            continue;
                        };
                        if data.is_empty() {
                            continue;
                        }
                        crate::vlog!(1, "asset {uuid}: {} bytes loaded", data.len());
                        let data = Arc::new(data);
                        member_cache.insert(member, Arc::clone(&data));
                        data
                    }
                };
                attachments.insert(uuid, data);
            }
        }

        let mut page_cfg: HashMap<String, PageCfg> = HashMap::new();
        if let Some(raw) = read_member(&mut z, "index.events.pb")? {
            if let Ok((configs, page_cfgs)) = parse_events(&raw) {
                for page in &pages {
                    let Some(prev) = uuid_prev(&page.uuid) else {
                        continue;
                    };
                    let Some(cfg) = page_cfgs.get(&prev) else {
                        continue;
                    };
                    if let Some((att, pdf_page)) = configs.get(cfg) {
                        page_cfg.insert(
                            page.uuid.clone(),
                            PageCfg {
                                attachment: att.clone(),
                                pdf_page: *pdf_page,
                            },
                        );
                    }
                }
            }
        }

        Ok(Document {
            schema,
            pages,
            attachments,
            page_cfg,
            paper_cache: Mutex::new(HashMap::new()),
            pdf_temps: Mutex::new(HashMap::new()),
            raster_cache: Mutex::new(HashMap::new()),
            temp_tag: DOC_TAG.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Temp copy of a PDF attachment for pdftoppm, created once per document
    /// (aliased attachment uuids share the same bytes and thus one file).
    /// Not available on wasm — rasterizations are injected from JavaScript.
    #[cfg(not(target_arch = "wasm32"))]
    fn temp_pdf(&self, att: &Arc<Vec<u8>>) -> Option<PathBuf> {
        let key = Arc::as_ptr(att) as usize;
        let mut temps = self.pdf_temps.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(path) = temps.get(&key) {
            return Some(path.clone());
        }
        let dir = pdfinput::pdf_cache_dir();
        pdfinput::sweep_stale(&dir);
        let path = dir.join(format!(
            "oblyx-{}-{}-{:x}.pdf",
            std::process::id(),
            self.temp_tag,
            key
        ));
        if let Err(e) = pdfinput::write_temp_pdf(&path, att) {
            crate::vlog!(1, "temp pdf write failed: {e:#}");
            return None;
        }
        temps.insert(key, path.clone());
        Some(path)
    }

    /// Single-flight rasterization of a PDF page: concurrent decode threads
    /// requesting the same (attachment, page, dpi) share one pdftoppm run
    /// instead of racing on its output file.
    fn raster_page(&self, att: &Arc<Vec<u8>>, pdf_page: u64, dpi: f32) -> Option<Arc<Vec<u8>>> {
        let key = (Arc::as_ptr(att) as usize, pdf_page, dpi.to_bits());
        let cell = {
            let mut cache = self.raster_cache.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(cache.entry(key).or_default())
        };
        let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = guard.as_ref() {
            return Some(Arc::clone(hit));
        }
        #[cfg(not(target_arch = "wasm32"))]
        let bytes = self
            .temp_pdf(att)
            .and_then(|temp| pdfinput::rasterize_pdf_page(&temp, pdf_page, dpi).map(Arc::new));
        #[cfg(target_arch = "wasm32")]
        let bytes: Option<Arc<Vec<u8>>> = None;
        let bytes = bytes?;
        *guard = Some(Arc::clone(&bytes));
        Some(bytes)
    }

    /// Unique `(attachment uuid, 1-based PDF page)` pairs whose pages show a
    /// PDF background — the work list a caller must rasterize before decoding
    /// (pdftoppm on native, pdf.js in the browser).
    pub fn pdf_raster_jobs(&self) -> Vec<(String, u64)> {
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        let mut jobs = Vec::new();
        for page in &self.pages {
            let Some(cfg) = self.page_cfg.get(&page.uuid) else {
                continue;
            };
            let Some(att) = self.attachments.get(&cfg.attachment) else {
                continue;
            };
            let (Some(pdf_page), true) = (cfg.pdf_page, att.starts_with(b"%PDF-")) else {
                continue;
            };
            if seen.insert((cfg.attachment.clone(), pdf_page)) {
                jobs.push((cfg.attachment.clone(), pdf_page));
            }
        }
        jobs
    }

    /// Store a caller-rendered JPEG for a PDF background page (keyed like
    /// `raster_page`), so `decode_page` picks it up without pdftoppm.
    pub fn set_page_raster(
        &self,
        attachment: &str,
        pdf_page: u64,
        dpi: f32,
        jpeg: Vec<u8>,
    ) -> Result<()> {
        let att = self
            .attachments
            .get(attachment)
            .with_context(|| format!("unknown attachment {attachment}"))?;
        let key = (Arc::as_ptr(att) as usize, pdf_page, dpi.to_bits());
        let mut cache = self.raster_cache.lock().unwrap_or_else(|e| e.into_inner());
        let cell = RasterCell::default();
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(jpeg));
        cache.insert(key, cell);
        Ok(())
    }

    /// Paper rectangles for a background attachment, computed once per file.
    /// Keyed by the shared bytes pointer so aliased attachment uuids (several
    /// uuids can point at one member) parse the PDF only once.
    fn cached_paper_rects(&self, attachment: &str, att: &Arc<Vec<u8>>) -> Arc<PaperRects> {
        let key = Arc::as_ptr(att) as usize;
        if let Some(hit) = self
            .paper_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            let hit = Arc::clone(hit);
            crate::vlog!(2, "background {attachment}: cached ({} rects)", hit.len());
            return hit;
        }
        let rects = Arc::new(paper_rects(att));
        crate::vlog!(1, "background {attachment}: parsed {} rects", rects.len());
        self.paper_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Arc::clone(&rects));
        rects
    }

    pub fn decode_page(&self, src: &PageSource, include_deleted: bool, dpi: f32) -> Result<Page> {
        let recs = records(&src.data)?;
        let parsed: Vec<Vec<Field<'_>>> = recs.iter().map(|r| parse(r)).collect::<Result<_>>()?;

        let mut items: Vec<Item> = Vec::new();
        let mut i = 0;
        while i < recs.len() {
            let keys = field_nums(&parsed[i]);
            let next_keys = if i + 1 < recs.len() {
                Some(field_nums(&parsed[i + 1]))
            } else {
                None
            };
            let paired = next_keys.as_ref().is_some_and(|nk| !is_wrapper(nk));
            if is_wrapper(&keys) {
                let deleted = varint_of(&parsed[i], 3) == Some(1);
                if (deleted && !include_deleted) || !paired {
                    i += if paired { 2 } else { 1 };
                    continue;
                }
                decode_content(
                    next_keys.as_ref().unwrap(),
                    recs[i + 1],
                    &mut items,
                    include_deleted,
                    Some(&parsed[i]),
                )?;
                i += 2;
            } else {
                decode_content(&keys, recs[i], &mut items, include_deleted, None)?;
                i += 1;
            }
        }

        if let Some(si) = items.iter().position(|it| matches!(it, Item::Sticky(_))) {
            let author_follows = matches!(
                (items.get(si + 1), &items[si]),
                (Some(Item::Text(t)), Item::Sticky(s))
                    if t.x >= s.x
                        && t.x <= s.x + s.w + 8.0
                        && t.y >= s.y - 4.0
                        && t.y <= s.y + s.h
            );
            let author = author_follows.then(|| items.remove(si + 1));
            let sticky = items.remove(si);
            let (mut rest, children): (Vec<Item>, Vec<Item>) = items
                .into_iter()
                .partition(|it| !matches!(it, Item::Stroke(s) if s.parented));
            rest.push(sticky);
            rest.extend(author);
            rest.extend(children);
            items = rest;
        }

        let placement_atts: HashSet<String> = items
            .iter()
            .filter_map(|it| match it {
                Item::Image(im) if !im.from_frame => Some(im.attachment.clone()),
                _ => None,
            })
            .collect();
        items.retain(|it| match it {
            Item::Image(im) if im.from_frame => !placement_atts.contains(&im.attachment),
            _ => true,
        });

        let frame_sigs: HashSet<String> = items
            .iter()
            .filter_map(|it| match it {
                Item::Shape(s) if !s.from_f9 => Some(shape_f9_sig(s)),
                _ => None,
            })
            .collect();
        if !frame_sigs.is_empty() {
            items.retain(|it| match it {
                Item::Shape(s) if s.from_f9 => !frame_sigs.contains(&shape_f9_sig(s)),
                _ => true,
            });
        }

        let mut boxes: HashMap<String, [f32; 4]> = HashMap::new();
        for it in &items {
            if let Item::Shape(s) = it {
                if let Some(u) = &s.uuid {
                    let b = if matches!(s.kind, ShapeKind::Ellipse) {
                        [s.x - s.w, s.y - s.h, s.w * 2.0, s.h * 2.0]
                    } else {
                        [s.x, s.y, s.w, s.h]
                    };
                    boxes.insert(u.clone(), b);
                }
            }
        }
        let mut out: Vec<Item> = Vec::with_capacity(items.len());
        for it in items {
            match it {
                Item::Connector(c) => {
                    if let Some(path) = resolve_connector(&c, &boxes) {
                        out.push(Item::Path(path));
                    }
                }
                it => out.push(it),
            }
        }

        if let Some(cfg) = self.page_cfg.get(&src.uuid)
            && let Some(att) = self.attachments.get(&cfg.attachment)
            && let Some(pdf_page) = cfg.pdf_page
            && att.starts_with(b"%PDF-")
            && let Some(bytes) = self.raster_page(att, pdf_page, dpi)
        {
            match pdfinput::jpeg_size(&bytes) {
                Some((pw, ph)) => {
                    let (pw, ph) = (pw as f32, ph as f32);
                    let s = (PAGE_W / pw).min(PAGE_H / ph);
                    let (w, h) = (pw * s, ph * s);
                    out.insert(
                        0,
                        Item::Image(ImageEl {
                            attachment: format!("pdf:{}:{pdf_page}", cfg.attachment),
                            x: (PAGE_W - w) / 2.0,
                            y: (PAGE_H - h) / 2.0,
                            w,
                            h,
                            from_frame: false,
                            bytes: Some(bytes),
                        }),
                    );
                }
                None => {
                    crate::vlog!(
                        1,
                        "page {}: raster of pdf page {pdf_page} unreadable",
                        src.uuid
                    );
                }
            }
        }
        if let Some(cfg) = self.page_cfg.get(&src.uuid)
            && let Some(att) = self.attachments.get(&cfg.attachment)
        {
            let rects = self.cached_paper_rects(&cfg.attachment, att);
            if !rects.is_empty() {
                out.insert(
                    0,
                    Item::Background(Background {
                        rects: (*rects).clone(),
                    }),
                );
            }
        }

        if crate::verbose::level() >= 1 {
            let (mut strokes, mut images, mut texts, mut shapes) = (0usize, 0usize, 0usize, 0usize);
            for it in &out {
                match it {
                    Item::Stroke(_) => strokes += 1,
                    Item::Image(_) => images += 1,
                    Item::Text(_) => texts += 1,
                    Item::Shape(_) => shapes += 1,
                    _ => {}
                }
            }
            crate::vlog!(
                1,
                "page {}: {strokes} strokes, {images} images, {texts} texts, {shapes} shapes",
                src.uuid
            );
            if crate::verbose::level() >= 2 {
                let mut n = 0;
                for it in &out {
                    if let Item::Stroke(st) = it {
                        n += 1;
                        crate::vlog!(
                            2,
                            "  stroke #{n}: {} points, width {:.2}",
                            st.points.len(),
                            st.width
                        );
                    }
                }
            }
        }

        Ok(Page {
            uuid: src.uuid.clone(),
            items: out,
        })
    }
}

fn edge_mid(b: &[f32; 4], toward: &[f32; 2]) -> [f32; 2] {
    let cx = b[0] + b[2] * 0.5;
    let cy = b[1] + b[3] * 0.5;
    let dx = toward[0] - cx;
    let dy = toward[1] - cy;
    if dx.abs() >= dy.abs() {
        if dx >= 0.0 {
            [b[0] + b[2], cy]
        } else {
            [b[0], cy]
        }
    } else if dy >= 0.0 {
        [cx, b[1] + b[3]]
    } else {
        [cx, b[1]]
    }
}

fn elbow(a: [f32; 2], anchor: [f32; 2], end: [f32; 2]) -> Vec<[f32; 2]> {
    if (end[0] - a[0]).abs() >= (end[1] - a[1]).abs() {
        vec![a, [anchor[0], a[1]], [anchor[0], end[1]], end]
    } else {
        vec![a, [a[0], anchor[1]], [end[0], anchor[1]], end]
    }
}

fn round_corners(pts: &[[f32; 2]], r: f32) -> Vec<[f32; 2]> {
    let mut p: Vec<[f32; 2]> = Vec::with_capacity(pts.len());
    for pt in pts {
        if p.last()
            .is_none_or(|q| (q[0] - pt[0]).abs() > 1e-3 || (q[1] - pt[1]).abs() > 1e-3)
        {
            p.push(*pt);
        }
    }
    if p.len() < 3 || r <= 0.0 {
        return p;
    }
    let mut out = vec![p[0]];
    for i in 1..p.len() - 1 {
        let prev = *out.last().unwrap();
        let v = p[i];
        let next = p[i + 1];
        let d1 = [v[0] - prev[0], v[1] - prev[1]];
        let l1 = (d1[0] * d1[0] + d1[1] * d1[1]).sqrt();
        let d2 = [next[0] - v[0], next[1] - v[1]];
        let l2 = (d2[0] * d2[0] + d2[1] * d2[1]).sqrt();
        if l1 < 1e-4 || l2 < 1e-4 {
            out.push(v);
            continue;
        }
        let cut = r.min(l1 * 0.5).min(l2 * 0.5);
        let p1 = [v[0] - d1[0] / l1 * cut, v[1] - d1[1] / l1 * cut];
        let p2 = [v[0] + d2[0] / l2 * cut, v[1] + d2[1] / l2 * cut];
        out.push(p1);
        let steps = 8;
        for k in 1..steps {
            let t = k as f32 / steps as f32;
            let it = 1.0 - t;
            out.push([
                it * it * p1[0] + 2.0 * it * t * v[0] + t * t * p2[0],
                it * it * p1[1] + 2.0 * it * t * v[1] + t * t * p2[1],
            ]);
        }
        out.push(p2);
    }
    out.push(*p.last().unwrap());
    out
}

fn flatten_smooth(pts: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let mut out: Vec<[f32; 2]> = Vec::with_capacity(pts.len() * 3);
    if pts.is_empty() {
        return out;
    }
    out.push(pts[0]);
    if pts.len() == 2 {
        out.push(pts[1]);
        return out;
    }
    let m = pts.len();
    let mut cur = pts[0];
    for j in 0..m - 1 {
        let p1 = pts[j];
        let p2 = pts[j + 1];
        let prev = if j == 0 { pts[0] } else { pts[j - 1] };
        let next = if j + 2 < m { pts[j + 2] } else { pts[j + 1] };
        let c1 = [
            p1[0] + (p2[0] - prev[0]) / 6.0,
            p1[1] + (p2[1] - prev[1]) / 6.0,
        ];
        let c2 = [
            p2[0] - (next[0] - p1[0]) / 6.0,
            p2[1] - (next[1] - p1[1]) / 6.0,
        ];
        let chord = (p2[0] - cur[0]).hypot(p2[1] - cur[1]);
        let steps = ((chord / 5.0).ceil() as i32).clamp(1, 32) as u32;
        for k in 1..=steps {
            let t = k as f32 / steps as f32;
            let it = 1.0 - t;
            out.push([
                it * it * it * cur[0]
                    + 3.0 * it * it * t * c1[0]
                    + 3.0 * it * t * t * c2[0]
                    + t * t * t * p2[0],
                it * it * it * cur[1]
                    + 3.0 * it * it * t * c1[1]
                    + 3.0 * it * t * t * c2[1]
                    + t * t * t * p2[1],
            ]);
        }
        cur = p2;
    }
    out
}

fn arrowhead(pts: &[[f32; 2]]) -> Option<[[f32; 2]; 3]> {
    if pts.len() < 2 {
        return None;
    }
    let end = *pts.last()?;
    let prev = pts[pts.len() - 2];
    let d = [end[0] - prev[0], end[1] - prev[1]];
    let l = (d[0] * d[0] + d[1] * d[1]).sqrt();
    if l < 1e-4 {
        return None;
    }
    let u = [d[0] / l, d[1] / l];
    let base = [end[0] - u[0] * ARROW_LEN, end[1] - u[1] * ARROW_LEN];
    let perp = [-u[1], u[0]];
    Some([
        end,
        [
            base[0] + perp[0] * ARROW_HALF,
            base[1] + perp[1] * ARROW_HALF,
        ],
        [
            base[0] - perp[0] * ARROW_HALF,
            base[1] - perp[1] * ARROW_HALF,
        ],
    ])
}

fn resolve_connector(c: &ConnectorEl, boxes: &HashMap<String, [f32; 4]>) -> Option<PathEl> {
    let Some(b1) = boxes.get(&c.a) else {
        return None;
    };
    let Some(b2) = boxes.get(&c.b) else {
        return None;
    };
    let c1 = [b1[0] + b1[2] * 0.5, b1[1] + b1[3] * 0.5];
    let c2 = [b2[0] + b2[2] * 0.5, b2[1] + b2[3] * 0.5];
    let a1 = edge_mid(b1, &c2);
    let a2 = edge_mid(b2, &c1);
    let pts = elbow(a1, c.anchor, a2);
    let pts = round_corners(&pts, CONN_R);
    let head = arrowhead(&pts);
    Some(PathEl {
        points: pts,
        head,
        width: c.width,
        rgba: c.rgba,
    })
}

fn is_wrapper(keys: &[u32]) -> bool {
    keys.contains(&8)
        && keys
            .iter()
            .all(|k| matches!(k, 1 | 2 | 3 | 4 | 8 | 9 | 14 | 16 | 20 | 22 | 23))
}

fn decode_content(
    keys: &[u32],
    rec: &[u8],
    items: &mut Vec<Item>,
    include_deleted: bool,
    wrapper: Option<&[Field<'_>]>,
) -> Result<()> {
    if keys == [7] {
        decode_stroke(rec, items);
        return Ok(());
    }
    if keys.len() == 1 {
        decode_element(keys[0], rec, items, include_deleted, wrapper);
    } else {
    }
    Ok(())
}

fn num_as_u32(fs: &[Field<'_>], num: u32) -> Option<u32> {
    fixed32_of(fs, num).or_else(|| varint_of(fs, num).map(|v| v as u32))
}

fn decode_rgba(fs: &[Field<'_>]) -> Option<[f32; 4]> {
    let r = f32_bits(num_as_u32(fs, 1).unwrap_or(0));
    let g = f32_bits(num_as_u32(fs, 2).unwrap_or(0));
    let b = f32_bits(num_as_u32(fs, 3).unwrap_or(0));
    let a = f32_bits(num_as_u32(fs, 4).unwrap_or(0x3f800000));
    Some([
        r.clamp(0.0, 1.0),
        g.clamp(0.0, 1.0),
        b.clamp(0.0, 1.0),
        if a.is_finite() && a > 0.0 && a <= 1.0 {
            a
        } else {
            1.0
        },
    ])
}

fn rgba_opt(fs: &[Field<'_>]) -> Option<[f32; 4]> {
    let present = (1..=3).all(|n| num_as_u32(fs, n).is_some());
    present.then(|| decode_rgba(fs)).flatten()
}

fn decode_stroke(rec: &[u8], items: &mut Vec<Item>) {
    let Ok(fs) = parse(rec) else {
        return;
    };
    let Some(stroke_raw) = bytes_of(&fs, 7) else {
        return;
    };
    let Ok(stroke) = parse(stroke_raw) else {
        return;
    };

    let mut rgba = [0.1176, 0.1059, 0.1059, 1.0];
    if let Some(cm) = bytes_of(&stroke, 4).and_then(|m| parse(m).ok()) {
        if let Some(c) = decode_rgba(&cm) {
            rgba = c;
        }
    }

    let parented = bytes_of(&stroke, 100).is_some();
    let (mut dx, mut dy) = (0.0f32, 0.0f32);
    if let Some(mm) = bytes_of(&stroke, 6).and_then(|m| parse(m).ok()) {
        dx = num_as_u32(&mm, 1).map(f32_bits).unwrap_or(0.0);
        dy = num_as_u32(&mm, 2).map(f32_bits).unwrap_or(0.0);
        if !dx.is_finite() {
            dx = 0.0;
        }
        if !dy.is_finite() {
            dy = 0.0;
        }
        if parented {
            dx = 0.0;
            dy = 0.0;
        }
    }

    let Some(blob) = bytes_of(&stroke, 2) else {
        if decode_shape_f9(&stroke, rgba, dx, dy, items) {
            return;
        }
        return;
    };
    if blob.len() < 4 || (blob[..4] != *b"bv41" && blob[..4] != *b"bv4-") {
        if decode_shape_f9(&stroke, rgba, dx, dy, items) {
            return;
        }
        return;
    }
    let Ok(decomp) = decompress_bv4(blob) else {
        if decode_shape_f9(&stroke, rgba, dx, dy, items) {
            return;
        }
        return;
    };
    if let Ok(Some((contours, colored))) = tpl::vector_fill_geometry(&decomp) {
        let shift = |pts: &[[f32; 2]]| -> Vec<[f32; 2]> {
            let mut out = pts.to_vec();
            for p in &mut out {
                p[0] += dx;
                p[1] += dy;
            }
            out
        };
        if colored {
            let shadow: Vec<Vec<[f32; 2]>> = contours
                .iter()
                .map(|c| shift(c).into_iter().map(|p| [p[0], p[1] + 2.0]).collect())
                .collect();
            push_fill_path(items, shadow, [0.0, 0.0, 0.0, 0.3]);
        }
        let main: Vec<Vec<[f32; 2]>> = contours.iter().map(|c| shift(c)).collect();
        push_fill_path(items, main, rgba);
        return;
    }
    let Ok(Some(geom)) = tpl::stroke_geometry(&decomp) else {
        if decode_shape_f9(&stroke, rgba, dx, dy, items) {
            return;
        }
        return;
    };
    let mut pts = tpl::polyline(&geom.starts, &geom.segments);
    if pts.len() < 2 {
        if decode_shape_f9(&stroke, rgba, dx, dy, items) {
            return;
        }
        return;
    }
    for p in &mut pts {
        p[0] += dx;
        p[1] += dy;
    }
    let mut width = f32_bits(geom.width_bits);
    if !width.is_finite() || width <= 0.0 {
        width = 1.5;
    }
    items.push(Item::Stroke(Stroke {
        points: pts,
        width,
        rgba,
        dash: geom.dash,
        parented,
    }));
}

fn decode_shape_f9(
    stroke: &[Field<'_>],
    rgba: [f32; 4],
    dx: f32,
    dy: f32,
    items: &mut Vec<Item>,
) -> bool {
    let Some(f9raw) = bytes_of(stroke, 9) else {
        return false;
    };
    let Ok(f9) = parse(f9raw) else {
        return false;
    };
    if f9.is_empty() {
        return false;
    }
    let mut width = num_as_u32(&f9, 15).map(f32_bits).unwrap_or(0.0);
    if !width.is_finite() || width <= 0.0 {
        width = FRAME_WIDTH;
    }
    let pts_of = |n: u32| -> Option<Vec<[f32; 2]>> {
        let raw = bytes_of(&f9, n)?;
        let g = parse(raw).ok()?;
        let mut out = Vec::new();
        for f in g.iter() {
            if let Wire::Bytes(pb) = f.wire {
                if let Some(p) = point_of(pb) {
                    out.push([p[0] + dx, p[1] + dy]);
                }
            }
        }
        Some(out)
    };
    if let Some(pts) = pts_of(1).filter(|p| p.len() >= 2) {
        let last = *pts.last().unwrap();
        let closed = (pts[0][0] - last[0]).hypot(pts[0][1] - last[1]) < 0.5;
        if closed && pts.len() >= 4 {
            push_f9_shape(items, ShapeKind::Polygon, pts, rgba, width);
        } else {
            items.push(Item::Path(PathEl {
                points: smooth_open(&pts),
                head: None,
                width,
                rgba,
            }));
        }
        return true;
    }
    if let Some(pts) = pts_of(2).filter(|p| p.len() >= 2) {
        items.push(Item::Path(PathEl {
            points: smooth_open(&pts),
            head: None,
            width,
            rgba,
        }));
        return true;
    }
    if let Some(raw) = bytes_of(&f9, 3) {
        if let Ok(r) = parse(raw) {
            if let (Some(c), Some(s)) = (
                bytes_of(&r, 1).and_then(point_of),
                bytes_of(&r, 2).and_then(point_of),
            ) {
                if s[0] > 0.0 && s[1] > 0.0 {
                    items.push(Item::Shape(ShapeEl {
                        kind: ShapeKind::Rect,
                        x: c[0] + dx - s[0] * 0.5,
                        y: c[1] + dy - s[1] * 0.5,
                        w: s[0],
                        h: s[1],
                        points: Vec::new(),
                        rotation: 0.0,
                        fill: [0.0, 0.0, 0.0, 0.0],
                        stroke: rgba,
                        width,
                        dash: None,
                        radius: 0.0,
                        uuid: None,
                        from_f9: true,
                    }));
                    return true;
                }
            }
        }
    }
    if let Some(raw) = bytes_of(&f9, 4) {
        if let Ok(e) = parse(raw) {
            if let (Some(c), Some(s)) = (
                bytes_of(&e, 1).and_then(point_of),
                bytes_of(&e, 2).and_then(point_of),
            ) {
                if s[0] > 0.0 && s[1] > 0.0 {
                    let mut rot = num_as_u32(&e, 3).map(f32_bits).unwrap_or(0.0);
                    if !rot.is_finite() {
                        rot = 0.0;
                    }
                    items.push(Item::Shape(ShapeEl {
                        kind: ShapeKind::Ellipse,
                        x: c[0] + dx,
                        y: c[1] + dy,
                        w: s[0],
                        h: s[1],
                        points: Vec::new(),
                        rotation: rot,
                        fill: [0.0, 0.0, 0.0, 0.0],
                        stroke: rgba,
                        width,
                        dash: None,
                        radius: 0.0,
                        uuid: None,
                        from_f9: true,
                    }));
                    return true;
                }
            }
        }
    }
    false
}

fn smooth_open(pts: &[[f32; 2]]) -> Vec<[f32; 2]> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut out: Vec<[f32; 2]> = Vec::new();
    let mut i = 0;
    while i + 2 < pts.len() {
        let a = pts[i];
        let c = pts[i + 1];
        let b = pts[i + 2];
        let steps = (((b[0] - a[0]).hypot(b[1] - a[1]) / 5.0).ceil() as usize).clamp(6, 32);
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let u = 1.0 - t;
            let x = u * u * a[0] + 2.0 * u * t * c[0] + t * t * b[0];
            let y = u * u * a[1] + 2.0 * u * t * c[1] + t * t * b[1];
            out.push([x, y]);
        }
        i += 2;
    }
    if i < pts.len() {
        let last = *pts.last().unwrap();
        if out
            .last()
            .is_some_and(|p| (p[0] - last[0]).abs() > 0.01 || (p[1] - last[1]).abs() > 0.01)
        {
            out.push(last);
        }
    }
    out
}

fn signed_area(pts: &[[f32; 2]]) -> f32 {
    let mut a = 0.0f32;
    for i in 0..pts.len() {
        let p = pts[i];
        let q = pts[(i + 1) % pts.len()];
        a += p[0] * q[1] - q[0] * p[1];
    }
    a * 0.5
}

fn push_fill_path(items: &mut Vec<Item>, contours: Vec<Vec<[f32; 2]>>, rgba: [f32; 4]) {
    let mut out: Vec<Vec<[f32; 2]>> = Vec::with_capacity(contours.len());
    for c in contours {
        if c.len() < 3 {
            continue;
        }
        let mut c = c;
        if signed_area(&c) < 0.0 {
            c.reverse();
        }
        out.push(c);
    }
    if out.is_empty() {
        return;
    }
    items.push(Item::FillPath(FillPathEl {
        contours: out,
        rgba,
    }));
}

fn push_f9_shape(
    items: &mut Vec<Item>,
    kind: ShapeKind,
    pts: Vec<[f32; 2]>,
    rgba: [f32; 4],
    width: f32,
) {
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for p in &pts {
        min_x = min_x.min(p[0]);
        min_y = min_y.min(p[1]);
        max_x = max_x.max(p[0]);
        max_y = max_y.max(p[1]);
    }
    items.push(Item::Shape(ShapeEl {
        kind,
        x: min_x,
        y: min_y,
        w: max_x - min_x,
        h: max_y - min_y,
        points: pts,
        rotation: 0.0,
        fill: [0.0, 0.0, 0.0, 0.0],
        stroke: rgba,
        width,
        dash: None,
        radius: 0.0,
        uuid: None,
        from_f9: true,
    }));
}

fn shape_f9_sig(s: &ShapeEl) -> String {
    match s.kind {
        ShapeKind::Ellipse => format!(
            "E{:.2},{:.2},{:.2},{:.2},{:.2}",
            s.x, s.y, s.w, s.h, s.rotation
        ),
        ShapeKind::Polygon => {
            let mut pts = s.points.clone();
            if pts.len() >= 2 {
                let first = pts[0];
                let last = *pts.last().unwrap();
                if (first[0] - last[0]).abs() < 0.5 && (first[1] - last[1]).abs() < 0.5 {
                    pts.pop();
                }
            }
            let body: Vec<String> = pts
                .iter()
                .map(|p| format!("{:.1},{:.1}", p[0], p[1]))
                .collect();
            format!("P{}", body.join(";"))
        }
        _ => format!(
            "R{:.1},{:.1},{:.1},{:.1},{:.2}",
            s.x, s.y, s.w, s.h, s.rotation
        ),
    }
}

fn point_of(msg: &[u8]) -> Option<[f32; 2]> {
    let f = parse(msg).ok()?;
    let x = num_as_u32(&f, 1).map(f32_bits)?;
    let y = num_as_u32(&f, 2).map(f32_bits)?;
    if x.is_finite() && y.is_finite() {
        Some([x, y])
    } else {
        None
    }
}

fn rect_xy(msg: &[u8]) -> Option<(f32, f32)> {
    point_of(bytes_of(&parse(msg).ok()?, 1)?).map(|[x, y]| (x, y))
}

fn rect_wh(msg: &[u8]) -> Option<(f32, f32)> {
    point_of(bytes_of(&parse(msg).ok()?, 2)?).map(|[x, y]| (x, y))
}

fn decode_element(
    kind: u32,
    rec: &[u8],
    items: &mut Vec<Item>,
    include_deleted: bool,
    wrapper: Option<&[Field<'_>]>,
) {
    let Ok(fs) = parse(rec) else { return };
    let Some(payload) = bytes_of(&fs, kind) else {
        return;
    };
    let Ok(f) = parse(payload) else {
        return;
    };
    if !include_deleted && varint_of(&f, 14) == Some(1) {
        return;
    }
    match kind {
        1 => decode_placement(&f, items),
        9 => decode_frame(&f, items),
        11 => decode_equation(&f, items),
        20 => decode_sticky(&f, items),
        21 => decode_textbox(&f, items),
        22 => decode_kind22(&f, items, wrapper),
        _ => {}
    }
}

fn decode_placement(f: &[Field<'_>], items: &mut Vec<Item>) {
    let Some(att) = bytes_of(f, 4).and_then(as_uuid) else {
        return;
    };
    let Some(frame) = bytes_of(f, 2) else {
        return;
    };
    let Some((x, y)) = rect_xy(frame) else {
        return;
    };
    let Some((w, h)) = rect_wh(frame) else {
        return;
    };
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    items.push(Item::Image(ImageEl {
        attachment: att.to_string(),
        x,
        y,
        w,
        h,
        from_frame: false,
        bytes: None,
    }));
}

fn rect_bbox(rf: &[Field<'_>]) -> Option<(f32, f32, f32, f32)> {
    let (p1, p2) = match (
        num_as_u32(rf, 1),
        num_as_u32(rf, 2),
        num_as_u32(rf, 3),
        num_as_u32(rf, 4),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => (
            Some([f32_bits(a), f32_bits(b)]),
            Some([f32_bits(c), f32_bits(d)]),
        ),
        _ => (
            bytes_of(rf, 1).and_then(point_of),
            bytes_of(rf, 2).and_then(point_of),
        ),
    };
    let (Some([x, y]), Some([w, h])) = (p1, p2) else {
        return None;
    };
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((x, y, w, h))
}

fn decode_frame(f: &[Field<'_>], items: &mut Vec<Item>) {
    if let Some(fill) = bytes_of(f, 7)
        .and_then(|m| parse(m).ok())
        .and_then(|m| decode_rgba(&m))
    {
        decode_shape_frame(f, fill, items);
        return;
    }
    let Some(att) = bytes_of(f, 5).and_then(as_uuid) else {
        return;
    };
    let Some(rect) = bytes_of(f, 2) else {
        return;
    };
    let Some(rf) = parse(rect).ok() else {
        return;
    };
    let Some((x, y, w, h)) = rect_bbox(&rf) else {
        return;
    };
    items.push(Item::Image(ImageEl {
        attachment: att.to_string(),
        x,
        y,
        w,
        h,
        from_frame: true,
        bytes: None,
    }));
}

fn decode_shape_frame(f: &[Field<'_>], fill: [f32; 4], items: &mut Vec<Item>) {
    let Some(rect) = bytes_of(f, 2) else {
        return;
    };
    let Some(rf) = parse(rect).ok() else {
        return;
    };
    let Some((x, y, w, h)) = rect_bbox(&rf) else {
        return;
    };
    let type_id = bytes_of(f, 3)
        .and_then(|m| parse(m).ok())
        .and_then(|m| bytes_of(&m, 1).and_then(|m| parse(m).ok()))
        .and_then(|m| varint_of(&m, 1))
        .unwrap_or(69) as u32;
    let stroke = [fill[0], fill[1], fill[2], 1.0];

    let mut points: Vec<[f32; 2]> = Vec::new();
    let mut ellipse: Option<([f32; 2], [f32; 2], f32)> = None;
    if let Some(f4) = bytes_of(f, 4).and_then(|m| parse(m).ok()) {
        for fld in f4.iter().filter(|x| x.num == 1) {
            if let Wire::Bytes(b) = fld.wire {
                let mut pushed = false;
                if let Some(inner) = parse(b).ok() {
                    for pf in inner.iter().filter(|x| x.num == 1) {
                        if let Wire::Bytes(pb) = pf.wire {
                            if let Some(p) = point_of(pb) {
                                points.push(p);
                                pushed = true;
                            }
                        }
                    }
                }
                if !pushed {
                    if let Some(p) = point_of(b) {
                        points.push(p);
                    }
                }
            }
        }
        ellipse = bytes_of(&f4, 4).and_then(|m| parse(m).ok()).and_then(|m| {
            let c = bytes_of(&m, 1).and_then(point_of)?;
            let s = bytes_of(&m, 2).and_then(point_of)?;
            let rot = num_as_u32(&m, 3)
                .map(f32_bits)
                .filter(|v| v.is_finite())
                .unwrap_or(0.0);
            Some((c, s, rot))
        });
    }

    let shape = if points.len() >= 3 {
        ShapeEl {
            kind: ShapeKind::Polygon,
            x,
            y,
            w,
            h,
            points,
            rotation: 0.0,
            fill,
            stroke,
            width: FRAME_WIDTH,
            dash: None,
            radius: 0.0,
            uuid: None,
            from_f9: false,
        }
    } else if let Some((c, s, rot)) = ellipse {
        ShapeEl {
            kind: ShapeKind::Ellipse,
            x: c[0],
            y: c[1],
            w: s[0],
            h: s[1],
            points: Vec::new(),
            rotation: rot,
            fill,
            stroke,
            width: FRAME_WIDTH,
            dash: None,
            radius: 0.0,
            uuid: None,
            from_f9: false,
        }
    } else if matches!(type_id, 67 | 73) {
        ShapeEl {
            kind: ShapeKind::Ellipse,
            x: x + w * 0.5,
            y: y + h * 0.5,
            w: w * 0.5,
            h: h * 0.5,
            points: Vec::new(),
            rotation: 0.0,
            fill,
            stroke,
            width: FRAME_WIDTH,
            dash: None,
            radius: 0.0,
            uuid: None,
            from_f9: false,
        }
    } else {
        ShapeEl {
            kind: ShapeKind::Rect,
            x,
            y,
            w,
            h,
            points: Vec::new(),
            rotation: 0.0,
            fill,
            stroke,
            width: FRAME_WIDTH,
            dash: None,
            radius: 0.0,
            uuid: None,
            from_f9: false,
        }
    };
    items.push(Item::Shape(shape));
}

fn stroke_item(decomp: &[u8], rgba: [f32; 4]) -> Option<Stroke> {
    let geom = tpl::stroke_geometry(decomp).ok()??;
    let pts = tpl::polyline(&geom.starts, &geom.segments);
    if pts.len() < 2 {
        return None;
    }
    let mut width = f32_bits(geom.width_bits);
    if !width.is_finite() || width <= 0.0 {
        width = 1.5;
    }
    Some(Stroke {
        points: pts,
        width,
        rgba,
        dash: geom.dash,
        parented: false,
    })
}

fn decode_equation(f: &[Field<'_>], items: &mut Vec<Item>) {
    for field in f.iter().filter(|x| x.num == 11) {
        let Wire::Bytes(sub_raw) = field.wire else {
            continue;
        };
        let Ok(sub) = parse(sub_raw) else {
            continue;
        };
        let mut rgba = [0.1176, 0.1059, 0.1059, 1.0];
        if let Some(cm) = bytes_of(&sub, 2).and_then(|m| parse(m).ok()) {
            if let Some(c) = decode_rgba(&cm) {
                rgba = c;
            }
        }
        let Some(blob) = bytes_of(&sub, 1) else {
            continue;
        };
        if blob.len() < 4 || (blob[..4] != *b"bv41" && blob[..4] != *b"bv4-") {
            continue;
        }
        let Ok(decomp) = decompress_bv4(blob) else {
            continue;
        };
        if let Some(stroke) = stroke_item(&decomp, rgba) {
            items.push(Item::Stroke(stroke));
        }
    }
}

fn decode_sticky(f: &[Field<'_>], items: &mut Vec<Item>) {
    let Some(pos_raw) = bytes_of(f, 20) else {
        return;
    };
    let Some((x, y)) = rect_xy(pos_raw) else {
        return;
    };
    let size_raw = bytes_of(f, 21);
    let (w, h) = size_raw
        .and_then(rect_wh)
        .or_else(|| size_raw.and_then(rect_xy))
        .unwrap_or((256.0, 256.0));
    let rgb = bytes_of(f, 30)
        .and_then(|m| parse(m).ok())
        .and_then(|c| decode_rgba(&c))
        .map(|c| [c[0], c[1], c[2]])
        .unwrap_or([0.984, 0.906, 0.941]);
    items.push(Item::Sticky(Sticky { x, y, w, h, rgb }));
    if let Some(author) = bytes_of(f, 33)
        .and_then(|b| std::str::from_utf8(b).ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let size = 12.0f32;
        items.push(Item::Text(TextBox {
            x: x + 8.0,
            y: y + h - 0.952 * size,
            size,
            text: author.to_string(),
            font: None,
            rgb: [0.3, 0.15, 0.0],
            bold: false,
        }));
    }
}

fn extract_text(f: &[Field<'_>]) -> Option<String> {
    let t_raw = bytes_of(f, 32)?;
    let t = parse(t_raw).ok()?;
    let seg = bytes_of(&t, 1)?;
    let segf = parse(seg).ok()?;
    let blob = bytes_of(&segf, 2)?;
    if blob.len() < 4 || (blob[..4] != *b"bv41" && blob[..4] != *b"bv4-") {
        return None;
    }
    let decomp = decompress_bv4(blob).ok()?;
    let blocks = parse(&decomp).ok()?;
    let mut out = String::new();
    for b in blocks.iter().filter(|x| x.num == 1) {
        let Wire::Bytes(block) = b.wire else {
            continue;
        };
        if let Ok(bf) = parse(block) {
            for piece in bf.iter().filter(|x| x.num == 1) {
                if let Wire::Bytes(s) = piece.wire {
                    if let Ok(text) = std::str::from_utf8(s) {
                        out.push_str(text);
                    }
                }
            }
        } else if let Ok(text) = std::str::from_utf8(block) {
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn decode_textbox(f: &[Field<'_>], items: &mut Vec<Item>) {
    let Some(pos_raw) = bytes_of(f, 20) else {
        return;
    };
    let Some((x, y)) = rect_xy(pos_raw) else {
        return;
    };
    let text = extract_text(f).unwrap_or_default();

    let mut size = 0.0f32;
    let mut font: Option<String> = None;
    let mut bold = false;
    if let Some(t5) = bytes_of(f, 5).and_then(|m| parse(m).ok()) {
        if let Some(style) = bytes_of(&t5, 1).and_then(|m| parse(m).ok()) {
            if let Some(name) = bytes_of(&style, 30) {
                if let Ok(s) = std::str::from_utf8(name) {
                    if !s.trim().is_empty() {
                        font = Some(s.to_string());
                    }
                }
            }
            if let Some(v) = varint_of(&style, 40) {
                let sz = f32_bits(v as u32);
                if sz.is_finite() && sz > 0.0 && sz < 500.0 {
                    size = sz;
                }
            }
        }
    }
    let mut inset_l = 10.0f32;
    let mut inset_t = 10.0f32;
    if let Some(t32) = bytes_of(f, 32).and_then(|m| parse(m).ok()) {
        if let Some(ins) = bytes_of(&t32, 10).and_then(|m| parse(m).ok()) {
            if let Some(v) = num_as_u32(&ins, 1)
                .map(f32_bits)
                .filter(|v| v.is_finite() && *v >= 0.0 && *v < 500.0)
            {
                inset_l = v;
            }
            if let Some(v) = num_as_u32(&ins, 2)
                .map(f32_bits)
                .filter(|v| v.is_finite() && *v >= 0.0 && *v < 500.0)
            {
                inset_t = v;
            }
        }
        if let Some(pm) = bytes_of(&t32, 5).and_then(|m| parse(m).ok()) {
            if let Some(pa) = bytes_of(&pm, 1).and_then(|m| parse(m).ok()) {
                if font.is_none() {
                    if let Some(name) = bytes_of(&pa, 30) {
                        if let Ok(s) = std::str::from_utf8(name) {
                            if !s.trim().is_empty() {
                                font = Some(s.to_string());
                            }
                        }
                    }
                }
                if size <= 0.0 {
                    if let Some(sz) = num_as_u32(&pa, 40)
                        .map(f32_bits)
                        .filter(|v| v.is_finite() && *v > 0.0 && *v < 500.0)
                    {
                        size = sz;
                    }
                }
            }
        }
        if let Some(segf) = bytes_of(&t32, 1).and_then(|m| parse(m).ok()) {
            if let Some(blob) = bytes_of(&segf, 2) {
                if let Ok(d) = decompress_bv4(blob) {
                    if let Ok(blocks) = parse(&d) {
                        let style = blocks
                            .iter()
                            .find(|b| b.num == 1)
                            .and_then(|b| match b.wire {
                                Wire::Bytes(bl) => parse(bl).ok(),
                                _ => None,
                            })
                            .and_then(|blk| bytes_of(&blk, 2).and_then(|s| parse(s).ok()));
                        if let Some(style) = style {
                            if let Some(sz) = num_as_u32(&style, 40)
                                .map(f32_bits)
                                .filter(|v| v.is_finite() && *v > 0.0 && *v < 500.0)
                            {
                                size = sz;
                            }
                            if let Some(v) = varint_of(&style, 60) {
                                if v as i64 != -404 {
                                    bold = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if size <= 0.0 {
        size = 24.0;
    }

    let rotation = parse(pos_raw)
        .ok()
        .and_then(|pf| num_as_u32(&pf, 2).map(f32_bits))
        .filter(|v| v.is_finite())
        .unwrap_or(0.0);

    let stroke_info = bytes_of(f, 31).and_then(|m| parse(m).ok());
    let stroke_width = stroke_info.as_ref().and_then(|s| {
        num_as_u32(s, 1)
            .map(f32_bits)
            .filter(|v| v.is_finite() && *v > 0.0)
    });
    let size_opt = bytes_of(f, 21).and_then(rect_wh);
    let f22m = bytes_of(f, 22).and_then(|m| parse(m).ok());
    let f7_type = bytes_of(f, 7)
        .and_then(|m| parse(m).ok())
        .and_then(|m| {
            m.iter()
                .rev()
                .find(|x| x.num == 1)
                .and_then(|x| match x.wire {
                    Wire::Bytes(b) => Some(b),
                    _ => None,
                })
        })
        .and_then(|b| parse(b).ok())
        .and_then(|m| num_as_u32(&m, 3));
    let unit_pts: Option<Vec<[f32; 2]>> = f22m
        .as_ref()
        .and_then(|m| {
            bytes_of(m, 3).and_then(|s| parse(s).ok()).and_then(|v3| {
                bytes_of(&v3, 1).and_then(|s| parse(s).ok()).map(|v1| {
                    v1.iter()
                        .filter(|x| x.num == 1)
                        .filter_map(|x| match x.wire {
                            Wire::Bytes(b) => Some(b),
                            _ => None,
                        })
                        .filter_map(|b| parse(b).ok())
                        .map(|item| {
                            let inner = bytes_of(&item, 1).and_then(|s| parse(s).ok());
                            let src = inner.as_deref().unwrap_or(&item);
                            let u = num_as_u32(src, 1).map(f32_bits).unwrap_or(0.0);
                            let v = num_as_u32(src, 2).map(f32_bits).unwrap_or(0.0);
                            [u, v]
                        })
                        .collect()
                })
            })
        })
        .filter(|p: &Vec<[f32; 2]>| p.len() >= 3)
        .or_else(|| match f7_type {
            Some(2) => Some(vec![[0.5, 0.0], [0.0, 1.0], [1.0, 1.0]]),
            Some(3) => Some(vec![[0.5, 0.0], [1.0, 0.5], [0.5, 1.0], [0.0, 0.5]]),
            _ => None,
        });
    let ellipse = f7_type == Some(1) || f22m.as_ref().is_some_and(|m| m.iter().any(|x| x.num == 2));
    let dash = stroke_info
        .as_ref()
        .and_then(|s| bytes_of(s, 2))
        .and_then(|m| parse(m).ok())
        .and_then(|m| bytes_of(&m, 2))
        .and_then(|m| parse(m).ok())
        .and_then(|d| {
            let gap = num_as_u32(&d, 2).map(f32_bits)?;
            let seg = num_as_u32(&d, 1).map(f32_bits).unwrap_or(0.0);
            if seg.is_finite() && gap.is_finite() && seg >= 0.0 && gap > 0.0 {
                let sw = stroke_width.unwrap_or(0.0);
                if sw <= 0.0 {
                    return None;
                }
                let res = if seg * sw < sw {
                    [0.0, gap * sw]
                } else {
                    [seg * sw, gap * sw]
                };
                Some(res)
            } else {
                None
            }
        });

    if size_opt.is_none() || stroke_width.is_none() {}
    if let (Some((bw, bh)), Some(sw_val)) = (size_opt, stroke_width) {
        let stroke = stroke_info
            .as_ref()
            .and_then(|s| bytes_of(s, 3))
            .and_then(|m| parse(m).ok())
            .and_then(|m| bytes_of(&m, 1))
            .and_then(|m| parse(m).ok())
            .and_then(|m| decode_rgba(&m))
            .unwrap_or([0.1176, 0.1059, 0.1059, 1.0]);
        let fill = bytes_of(f, 30)
            .and_then(|m| parse(m).ok())
            .and_then(|m| {
                let one = bytes_of(&m, 1).and_then(|x| parse(x).ok())?;
                bytes_of(&one, 1)
                    .and_then(|x| parse(x).ok())
                    .and_then(|x| rgba_opt(&x))
                    .or_else(|| rgba_opt(&one))
            })
            .unwrap_or([
                stroke[0] * 0.1 + 0.9,
                stroke[1] * 0.1 + 0.9,
                stroke[2] * 0.1 + 0.9,
                1.0,
            ]);
        let raw_r = f22m
            .as_ref()
            .and_then(|m| bytes_of(m, 1))
            .and_then(|s| parse(s).ok())
            .and_then(|m| num_as_u32(&m, 1))
            .map(f32_bits)
            .filter(|v| v.is_finite() && *v > 0.0);
        let radius = raw_r.unwrap_or(0.0).min(bw * 0.5).min(bh * 0.5);
        if radius > 0.0 {}
        let (kind, sx, sy, sw, sh, points, rot) = if let Some(units) = &unit_pts {
            let cs = rotation.cos();
            let sn = rotation.sin();
            let pts = units
                .iter()
                .map(|uv| {
                    let dx = uv[0] * bw;
                    let dy = uv[1] * bh;
                    [x + dx * cs - dy * sn, y + dx * sn + dy * cs]
                })
                .collect();
            (ShapeKind::Polygon, x, y, bw, bh, pts, 0.0)
        } else if ellipse {
            (
                ShapeKind::Ellipse,
                x + bw * 0.5,
                y + bh * 0.5,
                bw * 0.5,
                bh * 0.5,
                Vec::new(),
                rotation,
            )
        } else if f7_type == Some(2) {
            (ShapeKind::Triangle, x, y, bw, bh, Vec::new(), rotation)
        } else if f7_type == Some(3) {
            (ShapeKind::Diamond, x, y, bw, bh, Vec::new(), rotation)
        } else {
            (ShapeKind::Rect, x, y, bw, bh, Vec::new(), rotation)
        };
        items.push(Item::Shape(ShapeEl {
            kind,
            x: sx,
            y: sy,
            w: sw,
            h: sh,
            points,
            rotation: rot,
            fill,
            stroke,
            width: sw_val,
            dash,
            radius,
            uuid: bytes_of(f, 1).and_then(as_uuid).map(|s| s.to_string()),
            from_f9: false,
        }));
    }

    if !text.trim().is_empty() {
        items.push(Item::Text(TextBox {
            x: x + inset_l,
            y: y + inset_t + 0.952 * size,
            size,
            text,
            font,
            rgb: [0.1176, 0.1059, 0.1059],
            bold,
        }));
    }
}

fn decode_kind22(f: &[Field<'_>], items: &mut Vec<Item>, wrapper: Option<&[Field<'_>]>) {
    let mut width = KIND22_WIDTH;
    let mut rgba = [0.1176, 0.1059, 0.1059, 1.0];
    if let Some(style) = bytes_of(f, 32).and_then(|m| parse(m).ok()) {
        if let Some(inner) = bytes_of(&style, 1).and_then(|m| parse(m).ok()) {
            if let Some(w) = num_as_u32(&inner, 1).map(f32_bits) {
                if w.is_finite() && w > 0.0 {
                    width = w;
                }
            }
            if let Some(c) = bytes_of(&inner, 3)
                .and_then(|m| parse(m).ok())
                .and_then(|m| bytes_of(&m, 1))
                .and_then(|m| parse(m).ok())
                .and_then(|m| decode_rgba(&m))
            {
                rgba = c;
            }
        }
    }

    if let Some(g) = bytes_of(f, 20).and_then(|m| parse(m).ok()) {
        let mut pts: Vec<[f32; 2]> = Vec::new();
        if let Some(s) = bytes_of(&g, 1)
            .and_then(|m| parse(m).ok())
            .and_then(|m| bytes_of(&m, 1))
            .and_then(point_of)
        {
            pts.push(s);
        }
        for fld in g.iter().filter(|x| x.num == 2) {
            if let Wire::Bytes(b) = fld.wire {
                if let Some(p) = point_of(b) {
                    pts.push(p);
                }
            }
        }
        if let Some(e) = bytes_of(&g, 3)
            .and_then(|m| parse(m).ok())
            .and_then(|m| bytes_of(&m, 1))
            .and_then(point_of)
        {
            pts.push(e);
        }
        if pts.len() >= 2 {
            let pts = flatten_smooth(&pts);
            let head = arrowhead(&pts);
            items.push(Item::Path(PathEl {
                points: pts,
                head,
                width,
                rgba,
            }));
        }
        return;
    }

    if let Some(g) = bytes_of(f, 21).and_then(|m| parse(m).ok()) {
        let anchor = bytes_of(&g, 2).and_then(point_of);
        let r1 = wrapper
            .and_then(|w| bytes_of(w, 22))
            .and_then(as_uuid)
            .map(|s| s.to_string());
        let r2 = wrapper
            .and_then(|w| bytes_of(w, 23))
            .and_then(as_uuid)
            .map(|s| s.to_string());
        if let (Some(a), Some(b)) = (r1.clone(), r2.clone()) {
            if let Some(an) = anchor {
                items.push(Item::Connector(ConnectorEl {
                    a,
                    b,
                    anchor: an,
                    width,
                    rgba,
                }));
            } else {
            }
            return;
        }
        let start = bytes_of(&g, 1)
            .and_then(|m| parse(m).ok())
            .and_then(|m| bytes_of(&m, 1))
            .and_then(point_of);
        let end = bytes_of(&g, 3)
            .and_then(|m| parse(m).ok())
            .and_then(|m| bytes_of(&m, 1))
            .and_then(point_of);
        if let (Some(s), Some(e)) = (start, end) {
            let an = anchor.unwrap_or(e);
            let pts = elbow(s, an, e);
            let pts = round_corners(&pts, LINE_R);
            let head = arrowhead(&pts);
            items.push(Item::Path(PathEl {
                points: pts,
                head,
                width,
                rgba,
            }));
        } else {
        }
    }
}
