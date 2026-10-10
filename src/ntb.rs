//! `.ntb` (Notability cloud) reader.
//!
//! Parses the ZIP container, walks the `noteBundle` FlatBuffer, and extracts
//! handwriting strokes (anchor position, width, and decoded path geometry).
//! Intra-stroke paths are cubic Bézier records: each 19-byte record holds
//! three IEEE-754 half-precision points `(c1, c2, end)` relative to the
//! stroke anchor (y top-down); record 0 is a moveto whose control points are
//! clobbered by the stream header, so the path starts at the anchor itself.
//! See `docs/NOTABILITY_FORMAT.md` §4.4.
//!
//! No public prior art exists for this format; the layout below was reverse
//! engineered from 9 sample notebooks (Notability 16.12.5) and cross-checked
//! against the notability.com web app's stroke codec.

use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use zip::ZipArchive;

use crate::doc::{Item, Page, ShapeEl, ShapeKind, Stroke};

// ---------------------------------------------------------------------------
// FlatBuffer walker (minimal, schema-less)
// ---------------------------------------------------------------------------

struct Buf<'a> {
    d: &'a [u8],
}

impl<'a> Buf<'a> {
    fn u32(&self, off: usize) -> Result<u32> {
        self.d
            .get(off..off + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .context(format!("u32 @ {off}"))
    }

    fn i32(&self, off: usize) -> Result<i32> {
        Ok(self.u32(off)? as i32)
    }

    fn u16(&self, off: usize) -> Result<u16> {
        self.d
            .get(off..off + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
            .context(format!("u16 @ {off}"))
    }

    fn f32(&self, off: usize) -> Result<f32> {
        self.d
            .get(off..off + 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .context(format!("f32 @ {off}"))
    }

    /// Read a vtable at `vp`: returns `(vtable_size, table_size, field_offsets)`.
    fn read_vtable(&self, vp: usize) -> Result<(usize, usize, Vec<u16>)> {
        let vsize = self.u16(vp)? as usize;
        let tsize = self.u16(vp + 2)? as usize;
        let mut fields = Vec::new();
        let mut off = vp + 4;
        while off + 2 <= vp + vsize {
            fields.push(self.u16(off)?);
            off += 2;
        }
        Ok((vsize, tsize, fields))
    }

    /// Read the table at `tp`: returns `(vtable_pos, table_size, field_offsets)`.
    fn table(&self, tp: usize) -> Result<(usize, usize, Vec<u16>)> {
        let soff = self.i32(tp)?;
        let vp = (tp as i32 - soff) as usize;
        self.read_vtable(vp)
    }

    /// Follow a uoffset at `pos` to the target position.
    fn follow(&self, pos: usize) -> Result<usize> {
        let rel = self.u32(pos)? as usize;
        Ok(pos + rel)
    }
}

// ---------------------------------------------------------------------------
// noteBundle schema (verified, see docs/NOTABILITY_FORMAT.md §4)
// ---------------------------------------------------------------------------

/// Root table field indices.
const ROOT_F4_TIMESTAMP: usize = 4;
const ROOT_F6_ELEMENTS: usize = 6;

/// Stroke table field indices (same for Type A and Type B).
const STROKE_F1_ANCHOR: usize = 1; // struct 2×f32: (X, Y)
const STROKE_F8_WIDTH: usize = 8; // f32
const STROKE_F9_POINTS: usize = 9; // uoffset to point blob

/// Constant 5-byte trailer ending each 19-byte point record.
const POINT_TRAILER: [u8; 5] = [0x00, 0x3c, 0xff, 0x00, 0x00];
/// Bytes per point record.
const POINT_STRIDE: usize = 19;
/// Bézier samples emitted per cubic record when decoding paths to polylines.
const BEZIER_SAMPLES: usize = 12;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A decoded handwriting stroke. `points` holds the sampled absolute page
/// path (anchor + relative Bézier geometry); `anchor` is the verified
/// absolute page position of the path origin.
#[derive(Debug, Clone)]
pub struct NtbStroke {
    /// Absolute page X (left-right), points.
    pub anchor_x: f32,
    /// Absolute page Y (top-down), points.
    pub anchor_y: f32,
    /// Stroke width in points.
    pub width: f32,
    /// Number of 19-byte point records in the blob (the raw path length).
    pub point_count: usize,
    /// Decoded absolute page path, sampled from the cubic Bézier records.
    /// Empty when the blob carries no trailer-verified records.
    pub points: Vec<[f32; 2]>,
    /// Raw colour u32 (`0xAABBGGRR`, i.e. LE bytes are R,G,B,A) when the
    /// stroke table carries one (fields f5/f7); `None` → default black.
    pub color: Option<u32>,
}

/// A page definition (inner nf=2). Dimensions are in points.
#[derive(Debug, Clone, Copy)]
pub struct NtbPageDef {
    pub width: f32,
    pub height: f32,
}

/// A text style record (inner nf=8 with font metadata). In all 9 samples the
/// only such element is a document-level style record at element index 0
/// (font `Inter`, size 14, locale `en_US`); no positioned text boxes with
/// content were found, so no position/text is decoded here. Other nf=8
/// elements without font metadata are pen-style records and are filtered out.
#[derive(Debug, Clone)]
pub struct NtbText {
    pub font: String,
    pub size: f32,
}

/// A shape (inner nf=11/12/15 with an f5 geometry vector).
///
/// Geometry comes from the f5 vector's first values: for vectors of length 8
/// or 12 they are `(v0, v1)`; for length 14 the first slot is a header
/// (`0x01000000`) and the dims are `(v1, v2)`.
///
/// Classification (best-effort, see `docs/NOTABILITY_FORMAT.md` §4.3):
/// - length 8 with near-equal dims → ellipse (dims are the bbox size);
/// - length 8 with unequal dims → rectangle;
/// - length 12 or 14 → straight line from the anchor by `(dx, dy)`.
#[derive(Debug, Clone)]
pub struct NtbShape {
    /// Absolute page X (left-right), points.
    pub anchor_x: f32,
    /// Absolute page Y (top-down), points.
    pub anchor_y: f32,
    /// X extent of the geometry (line delta or bbox width), points.
    pub dx: f32,
    /// Y extent of the geometry (line delta or bbox height), points.
    pub dy: f32,
    /// Number of f32 slots in the f5 vector (8, 12 or 14).
    pub vector_len: usize,
    /// Raw colour u32 (`0xAABBGGRR`); `None` → default black.
    pub color: Option<u32>,
    /// Outline width in points; 0 when the field was missing.
    pub width: f32,
    /// Shape subtype (inner f4, u8): 1 and 3 observed in samples.
    pub subtype: u8,
}

/// Metadata extracted from the noteBundle root table.
#[derive(Debug, Clone, Default)]
pub struct NtbMeta {
    /// Raw 16-byte note GUID from root f0.
    pub note_guid: [u8; 16],
    /// Uppercase device UUID string (root f3).
    pub device_uuid_upper: String,
    /// Lowercase device UUID string (root f5).
    pub device_uuid_lower: String,
    /// Note timestamp, ms since Unix epoch (root f4).
    pub timestamp_ms: u64,
    /// Version-like u32 (root f7).
    pub version: u32,
    /// Number of elements in root f6.
    pub element_count: usize,
}

/// A fully parsed `.ntb` notebook.
#[derive(Debug)]
pub struct NtbDocument {
    pub meta: NtbMeta,
    /// Handwriting strokes, in file order.
    pub strokes: Vec<NtbStroke>,
    /// Page definitions (inner nf=2). Empty for continuous-scroll notebooks.
    pub pages: Vec<NtbPageDef>,
    /// Text style records (inner nf=8 with font metadata).
    pub texts: Vec<NtbText>,
    /// Shapes (inner nf=11/12/15 with an f5 geometry vector).
    pub shapes: Vec<NtbShape>,
    /// Raw `manifest.json` bytes.
    pub manifest: Vec<u8>,
    /// Raw `thumbnail.png` bytes, if present.
    pub thumbnail: Option<Vec<u8>>,
    /// Content-addressed assets: `(name, bytes)`.
    pub assets: Vec<(String, Vec<u8>)>,
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Open a `.ntb` file from disk.
pub fn open(path: &Path) -> Result<NtbDocument> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut zip = ZipArchive::new(Cursor::new(read_all(file)?))
        .with_context(|| format!("zip open {}", path.display()))?;
    parse_zip(&mut zip)
}

fn read_all(mut r: impl Read) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    Ok(buf)
}

fn parse_zip<R: std::io::Read + std::io::Seek>(
    z: &mut ZipArchive<R>,
) -> Result<NtbDocument> {
    let manifest = read_member(z, "manifest.json")?.unwrap_or_default();
    let thumbnail = read_member(z, "thumbnail.png")?;
    let bundle = read_member(z, "noteBundle")?.context("noteBundle missing")?;

    // Collect assets (content-addressed files under assets/).
    let mut assets = Vec::new();
    for i in 0..z.len() {
        let entry = z.by_index(i)?;
        let name = entry.name().to_string();
        if name.starts_with("assets/") {
            let mut buf = Vec::new();
            let mut entry = entry;
            entry.read_to_end(&mut buf)?;
            assets.push((name, buf));
        }
    }

    let (meta, strokes, pages, texts, shapes) = parse_bundle(&bundle)?;
    Ok(NtbDocument {
        meta,
        strokes,
        pages,
        texts,
        shapes,
        manifest,
        thumbnail,
        assets,
    })
}

fn read_member<R: std::io::Read + std::io::Seek>(
    z: &mut ZipArchive<R>,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    let mut file = match z.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("zip open {name}")),
    };
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(Some(buf))
}

// ---------------------------------------------------------------------------
// noteBundle parsing
// ---------------------------------------------------------------------------

/// Parsed contents of a noteBundle: metadata, strokes, pages, texts.
/// `(meta, strokes, pages, texts, shapes)` — the decoded contents of a
/// `noteBundle`, for callers that parse from memory.
pub type BundleParts = (
    NtbMeta,
    Vec<NtbStroke>,
    Vec<NtbPageDef>,
    Vec<NtbText>,
    Vec<NtbShape>,
);

fn parse_bundle(d: &[u8]) -> Result<BundleParts> {
    let b = Buf { d };
    if d.len() < 8 {
        bail!("noteBundle too small ({} bytes)", d.len());
    }

    // Root: u32 uoffset at byte 0 → root table.
    let root_pos = b.u32(0)? as usize;
    let (_rp, _rts, rf) = b.table(root_pos)?;
    let field = |i: usize| -> usize { *rf.get(i).unwrap_or(&0) as usize };

    let mut meta = NtbMeta::default();

    // f0: 16-byte inline note GUID at offset 56 (when present).
    if field(0) != 0 && d.len() >= root_pos + field(0) + 16 {
        let off = root_pos + field(0);
        meta.note_guid.copy_from_slice(&d[off..off + 16]);
    }

    // f4: u64 timestamp at offset 40.
    if field(ROOT_F4_TIMESTAMP) != 0 {
        let off = root_pos + field(ROOT_F4_TIMESTAMP);
        if d.len() >= off + 8 {
            meta.timestamp_ms = u64::from_le_bytes([
                d[off], d[off + 1], d[off + 2], d[off + 3],
                d[off + 4], d[off + 5], d[off + 6], d[off + 7],
            ]);
        }
    }

    // f3 / f5: device UUID strings.
    if let Some(s) = read_string(&b, root_pos, field(3))? {
        meta.device_uuid_upper = s;
    }
    if let Some(s) = read_string(&b, root_pos, field(5))? {
        meta.device_uuid_lower = s;
    }

    // f7: version-like u32 at offset 30.
    if field(7) != 0 {
        let off = root_pos + field(7);
        if d.len() >= off + 4 {
            meta.version = b.u32(off)?;
        }
    }

    // f6: vector of element tables.
    let mut strokes = Vec::new();
    let mut pages = Vec::new();
    let mut texts = Vec::new();
    let mut shapes = Vec::new();
    if field(ROOT_F6_ELEMENTS) != 0 {
        let vec_pos = b.follow(root_pos + field(ROOT_F6_ELEMENTS))?;
        let n = b.u32(vec_pos)? as usize;
        meta.element_count = n;
        for i in 0..n {
            let ep = vec_pos + 4 + 4 * i;
            let Ok(el_pos) = b.follow(ep) else { continue };
            let Ok(Some(elem)) = parse_element(&b, el_pos) else {
                continue;
            };
            match elem {
                ParsedElement::Stroke(s) => strokes.push(s),
                ParsedElement::PageDef(p) => pages.push(p),
                ParsedElement::Text(t) => texts.push(t),
                ParsedElement::Shape(s) => shapes.push(s),
            }
        }
    }

    Ok((meta, strokes, pages, texts, shapes))
}

fn read_string(b: &Buf, table: usize, field_off: usize) -> Result<Option<String>> {
    if field_off == 0 {
        return Ok(None);
    }
    let s_pos = b.follow(table + field_off)?;
    let len = b.u32(s_pos)? as usize;
    let start = s_pos + 4;
    if b.d.len() < start + len {
        return Ok(None);
    }
    let bytes = &b.d[start..start + len];
    Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
}

/// A decoded element from the root f6 vector, dispatched by inner-table nf.
enum ParsedElement {
    Stroke(NtbStroke),
    PageDef(NtbPageDef),
    Text(NtbText),
    Shape(NtbShape),
}

/// Parse one element: wrapper (nf=6) → inner table. Dispatches on the inner
/// table's field count (nf) to the appropriate decoder.
fn parse_element(b: &Buf, el_pos: usize) -> Result<Option<ParsedElement>> {
    let (_ws, _wt, wf) = b.table(el_pos)?;
    if wf.len() != 6 {
        return Ok(None); // wrapper always has 6 vtable fields
    }
    let f5 = *wf.get(5).unwrap_or(&0) as usize;
    if f5 == 0 {
        return Ok(None);
    }
    let inner = b.follow(el_pos + f5)?;
    let (_is, _it, inf) = b.table(inner)?;
    match inf.len() {
        16 => {
            let stroke = parse_stroke(b, inner, &inf)?;
            // Only emit strokes with trailer-verified points; nf=16 elements
            // with an empty or unrecognised blob are not handwriting.
            if stroke.point_count == 0 {
                return Ok(None);
            }
            Ok(Some(ParsedElement::Stroke(stroke)))
        }
        2 => Ok(parse_page_def(b, inner, &inf)?.map(ParsedElement::PageDef)),
        8 => Ok(parse_text(b, inner, &inf)?.map(ParsedElement::Text)),
        11 | 12 | 15 => Ok(parse_shape(b, inner, &inf)?.map(ParsedElement::Shape)),
        _ => Ok(None), // other types undecoded
    }
}

/// Parse a page definition (inner nf=2). f1 is a vector whose first two f32s
/// are the page width and height in points.
fn parse_page_def(b: &Buf, tp: usize, fields: &[u16]) -> Result<Option<NtbPageDef>> {
    let f1 = *fields.get(1).unwrap_or(&0) as usize;
    if f1 == 0 {
        return Ok(None);
    }
    let vec_pos = b.follow(tp + f1)?;
    let n = b.u32(vec_pos)? as usize;
    if n < 2 {
        return Ok(None);
    }
    let width = b.f32(vec_pos + 4)?;
    let height = b.f32(vec_pos + 8)?;
    // Validate: reasonable page dimensions (reject garbage from non-page nf=2).
    if !width.is_finite() || !height.is_finite() || width < 50.0 || height < 50.0
        || width > 10_000.0 || height > 10_000.0
    {
        return Ok(None);
    }
    Ok(Some(NtbPageDef { width, height }))
}

/// Parse a text style record (inner nf=8). f4 is a string (font family), f5
/// is an f32 font size. The actual text characters are stored elsewhere.
fn parse_text(b: &Buf, tp: usize, fields: &[u16]) -> Result<Option<NtbText>> {
    let f4 = *fields.get(4).unwrap_or(&0) as usize;
    let f5 = *fields.get(5).unwrap_or(&0) as usize;
    let font = if f4 != 0 {
        read_string(b, tp, f4)?.unwrap_or_default()
    } else {
        String::new()
    };
    let size = if f5 != 0 {
        b.f32(tp + f5).unwrap_or(0.0)
    } else {
        0.0
    };
    if font.is_empty() && size <= 0.0 {
        return Ok(None);
    }
    Ok(Some(NtbText { font, size }))
}

/// Read a colour u32 at `tp + off`. The alpha byte is `0xFF` for opaque
/// ink/highlighter and `0x6B` (~42%) for highlighter strokes — both are
/// PDF-confirmed (e.g. `0x6bff6f00` → `#006fff`). The raw u32 reads as
/// `0xAABBGGRR` because the on-disk byte order is R,G,B,A; use
/// [`rgba_from_u32`] to convert.
fn read_color(b: &Buf, tp: usize, off: usize) -> Option<u32> {
    if off == 0 {
        return None;
    }
    let v = b.u32(tp + off).ok()?;
    matches!(v >> 24, 0xFF | 0x6B).then_some(v)
}

/// Convert a raw colour u32 (`0xAABBGGRR`; LE bytes R,G,B,A) to
/// `[r, g, b, a]` floats in 0..1.
pub fn rgba_from_u32(v: u32) -> [f32; 4] {
    [
        (v & 0xFF) as f32 / 255.0,
        ((v >> 8) & 0xFF) as f32 / 255.0,
        ((v >> 16) & 0xFF) as f32 / 255.0,
        ((v >> 24) & 0xFF) as f32 / 255.0,
    ]
}

/// Parse a shape (inner nf=11/12/15 with an f5 geometry vector).
///
/// - f1: struct 2×f32 `(X, Y)` — absolute anchor, same layout as strokes.
/// - f5: vector of f32s. Length 8/12 → dims are `(v0, v1)`; length 14 →
///   first slot is a header and dims are `(v1, v2)`.
/// - colour: layout-driven at f7/f9/f10 depending on nf (see below);
///   width: first plausible f32 among f8/f9/f10 that is not the colour.
/// - f4: u8 subtype (1 and 3 observed).
fn parse_shape(b: &Buf, tp: usize, fields: &[u16]) -> Result<Option<NtbShape>> {
    let field = |i: usize| -> usize { *fields.get(i).unwrap_or(&0) as usize };
    let f1 = field(1);
    let f5 = field(5);
    if f1 == 0 || f5 == 0 {
        return Ok(None);
    }

    // Anchor (X, Y): first f32 is horizontal, second is vertical (top-down).
    // Thumbnail-overlay verified: dots at (f32[0], f32[1) track the handwriting
    // exactly; the opposite order scatters over empty margin/grid.
    let anchor_x = b.f32(tp + f1).unwrap_or(f32::NAN);
    let anchor_y = b.f32(tp + f1 + 4).unwrap_or(f32::NAN);
    if !anchor_x.is_finite() || !anchor_y.is_finite() {
        return Ok(None);
    }

    // Geometry vector.
    let vec_pos = b.follow(tp + f5)?;
    let n = b.u32(vec_pos)? as usize;
    let (dx, dy) = match n {
        8 | 12 => (b.f32(vec_pos + 4)?, b.f32(vec_pos + 8)?),
        14 => (b.f32(vec_pos + 8)?, b.f32(vec_pos + 12)?),
        _ => return Ok(None),
    };
    if !dx.is_finite() || !dy.is_finite() || dx.abs() > 10_000.0 || dy.abs() > 10_000.0 {
        return Ok(None);
    }
    if dx == 0.0 && dy == 0.0 {
        return Ok(None);
    }

    // Colour: layout-driven, never a scan — scanning false-positives on
    // noise fields (f0/f4 hold values like `0xfffedc01` whose low byte is
    // `0x01`, mirroring the stroke f5 noise). Observed: nf=15 → f7;
    // nf=11 → f9 or f10; nf=12 → f7 or f9 (f11 may be a fill colour and
    // is left unused — fill stays transparent).
    let mut color = None;
    let mut color_idx = 0;
    for i in [7usize, 9, 10] {
        let o = field(i);
        if o == 0 {
            continue;
        }
        if let Some(c) = read_color(b, tp, o) {
            color = Some(c);
            color_idx = i;
            break;
        }
    }

    // Width: an f32 in 0.01..=100 among the neighbouring fields
    // (f8/f9/f10 in samples); whichever of them is not the colour.
    let mut width = 0.0;
    for i in [8usize, 9, 10] {
        if i == color_idx {
            continue;
        }
        let o = field(i);
        if o == 0 {
            continue;
        }
        if let Ok(v) = b.f32(tp + o)
            && v.is_finite()
            && (0.01..=100.0).contains(&v)
        {
            width = v;
            break;
        }
    }

    let subtype = if field(4) != 0 {
        b.d.get(tp + field(4)).copied().unwrap_or(0)
    } else {
        0
    };

    Ok(Some(NtbShape {
        anchor_x,
        anchor_y,
        dx,
        dy,
        vector_len: n,
        color,
        width,
        subtype,
    }))
}

fn parse_stroke(b: &Buf, tp: usize, fields: &[u16]) -> Result<NtbStroke> {
    let field = |i: usize| -> usize { *fields.get(i).unwrap_or(&0) as usize };

    // f1: struct 2×f32 `(X, Y)` — absolute page anchor, same layout as strokes.
    let (anchor_x, anchor_y) = if field(STROKE_F1_ANCHOR) != 0 {
        let off = tp + field(STROKE_F1_ANCHOR);
        (b.f32(off).unwrap_or(0.0), b.f32(off + 4).unwrap_or(0.0))
    } else {
        (0.0, 0.0)
    };

    // f8: f32 width.
    let width = if field(STROKE_F8_WIDTH) != 0 {
        b.f32(tp + field(STROKE_F8_WIDTH)).unwrap_or(0.0)
    } else {
        0.0
    };

    // f9: uoffset to the point blob. Follow the uoffset from the field slot —
    // the target is NOT always at tp+tsize (shifted Type B layouts store a
    // larger table with f9 pointing past it).
    let (point_count, points) = if field(STROKE_F9_POINTS) != 0 {
        match b.follow(tp + field(STROKE_F9_POINTS)) {
            Ok(blob) => decode_stroke_points(b.d, blob, anchor_x, anchor_y),
            Err(_) => (0, Vec::new()),
        }
    } else {
        (0, Vec::new())
    };

    // Colour is always at f7 (PDF-verified: `0xff1515b5` ↔ `#b51515`).
    // f5 looks similar but is noise — its low byte is always `0x01`.
    let color = fields
        .get(7)
        .copied()
        .filter(|&o| o != 0)
        .and_then(|o| read_color(b, tp, o as usize));

    Ok(NtbStroke {
        anchor_x,
        anchor_y,
        width,
        point_count,
        points,
        color,
    })
}

/// Decode the point blob at `blob` into `(record_count, absolute_points)`.
///
/// Layout (validated against 9 notebooks + the web-app stroke codec):
/// the blob is a stream of 19-byte records. Record 0's first 8 bytes are
/// clobbered by the stream header (`u32` byte-length ≈ 19n−5, then the record
/// count); its last point is `(0, 0)` — the path origin. Records 1..n are
/// cubic Béziers: three IEEE-754 half-precision points `(c1, c2, end)`,
/// anchor-relative with y top-down, followed by a `u16` width hint and the
/// constant 5-byte trailer. The record count is found by scanning for the
/// trailer at 19-byte stride (the header length field is unreliable).
///
/// Points are returned in absolute page coordinates (anchor + relative),
/// sampled at [`BEZIER_SAMPLES`] steps per cubic.
fn decode_stroke_points(d: &[u8], blob: usize, anchor_x: f32, anchor_y: f32) -> (usize, Vec<[f32; 2]>) {
    // Count records via trailer scan (ground truth; the header length admits
    // false positives on non-stroke blobs).
    let mut count = 0usize;
    let mut p = blob;
    while p + POINT_STRIDE <= d.len() && d[p + 14..p + 19] == POINT_TRAILER {
        count += 1;
        p += POINT_STRIDE;
    }
    if count == 0 {
        return (0, Vec::new());
    }
    // rec0 = moveto at the anchor; rec1..count−1 = cubics.
    let mut points = Vec::with_capacity(count.saturating_mul(BEZIER_SAMPLES));
    points.push([anchor_x, anchor_y]);
    let mut cur = [0.0f32, 0.0f32]; // relative
    for k in 1..count {
        let off = blob + k * POINT_STRIDE;
        let c1 = read_f16_point(d, off);
        let c2 = read_f16_point(d, off + 4);
        let end = read_f16_point(d, off + 8);
        match (c1, c2, end) {
            (Some(c1), Some(c2), Some(end)) => {
                for i in 1..=BEZIER_SAMPLES {
                    let t = i as f32 / BEZIER_SAMPLES as f32;
                    let u = 1.0 - t;
                    let (ur, ut) = (u * u, t * t);
                    let x = u * ur * cur[0] + 3.0 * ur * t * c1[0] + 3.0 * u * ut * c2[0] + t * ut * end[0];
                    let y = u * ur * cur[1] + 3.0 * ur * t * c1[1] + 3.0 * u * ut * c2[1] + t * ut * end[1];
                    points.push([anchor_x + x, anchor_y + y]);
                }
                cur = end;
            }
            _ => {
                // Non-finite half somewhere in the record — fall through as a
                // straight segment to the end point when only that is finite.
                if let Some(end) = end {
                    points.push([anchor_x + end[0], anchor_y + end[1]]);
                    cur = end;
                }
            }
        }
    }
    (count, points)
}

/// Read an IEEE-754 half-precision point `(x, y)` at `off`. Returns `None`
/// when out of bounds or when either component is non-finite.
fn read_f16_point(d: &[u8], off: usize) -> Option<[f32; 2]> {
    let xb = d.get(off..off + 2)?;
    let yb = d.get(off + 2..off + 4)?;
    let x = f16_to_f32(u16::from_le_bytes([xb[0], xb[1]]));
    let y = f16_to_f32(u16::from_le_bytes([yb[0], yb[1]]));
    if x.is_finite() && y.is_finite() { Some([x, y]) } else { None }
}

/// Convert an IEEE-754 binary16 bit pattern to `f32`.
fn f16_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = ((bits >> 10) & 0x1F) as i32;
    let frac = (bits & 0x3FF) as f32;
    match exp {
        0 => sign * frac * (2.0f32).powi(-24),
        31 => {
            if frac == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + frac / 1024.0) * (2.0f32).powi(exp - 15),
    }
}

// ---------------------------------------------------------------------------
// Conversion to renderable pages
// ---------------------------------------------------------------------------

/// One renderable page. `.ntb` notebooks are continuous-scroll, so strokes
/// and shapes are assigned to pages by their Y coordinate using the given
/// page height.
#[derive(Debug)]
pub struct NtbPage {
    pub strokes: Vec<NtbStroke>,
    pub shapes: Vec<NtbShape>,
}

/// Split strokes and shapes into pages of `page_h` points each (top-down Y).
///
/// Notability's cloud notebooks are continuous-scroll; the matching PDF export
/// paginates at A4 height (841.89 pt) or a multiple thereof. Callers pick the
/// height that matches the target output.
pub fn paginate(doc: &NtbDocument, page_h: f32) -> Vec<NtbPage> {
    let empty = || NtbPage {
        strokes: Vec::new(),
        shapes: Vec::new(),
    };
    if page_h <= 0.0 || (doc.strokes.is_empty() && doc.shapes.is_empty()) {
        return vec![NtbPage {
            strokes: doc.strokes.clone(),
            shapes: doc.shapes.clone(),
        }];
    }
    let max_y = doc
        .strokes
        .iter()
        .map(|s| s.anchor_y)
        .chain(doc.shapes.iter().map(|s| s.anchor_y))
        .fold(0.0f32, f32::max);
    let n_pages = ((max_y / page_h).ceil() as usize).max(1);
    let mut pages: Vec<NtbPage> = (0..n_pages).map(|_| empty()).collect();
    for s in &doc.strokes {
        let idx = ((s.anchor_y / page_h).floor() as usize).min(n_pages - 1);
        pages[idx].strokes.push(s.clone());
    }
    for s in &doc.shapes {
        let idx = ((s.anchor_y / page_h).floor() as usize).min(n_pages - 1);
        pages[idx].shapes.push(s.clone());
    }
    pages
}

// ---------------------------------------------------------------------------
// Public helpers for external callers (e.g. the convert pipeline)
// ---------------------------------------------------------------------------

/// Default page width/height used when the notebook has no page definitions:
/// A4 portrait at 72 dpi (matches the PDF exports of all 9 samples).
pub const NTB_PAGE_W: f32 = 595.28;
pub const NTB_PAGE_H: f32 = 841.89;

/// Parse a `noteBundle` blob from memory. Public wrapper around the private
/// [`parse_bundle`], for callers that already hold the bytes (e.g. zip members).
pub fn parse_bundle_public(bundle: &[u8]) -> Result<BundleParts> {
    parse_bundle(bundle)
}

// ---------------------------------------------------------------------------
// Conversion into the portable Page / Document model
// ---------------------------------------------------------------------------

/// Convert an `.ntb` document into a list of portable [`Page`] objects,
/// one per page slice.
///
/// When the notebook contains page definitions (nf=2), the first page's height
/// is used for pagination; otherwise `page_h` is the fallback.
///
/// Strokes are emitted with their decoded cubic-Bézier paths (sampled to
/// polylines). A 1-record stroke (moveto only) degenerates to a zero-length
/// mark at its anchor. Stroke colours come from the element colour field
/// (`0xAABBGGRR`) and default to opaque black.
///
/// Shapes are emitted as [`ShapeEl`] rectangles/ellipses sized from the f5
/// geometry vector, or as 2-point [`Item::Stroke`] lines when the vector
/// encodes a line delta (see [`NtbShape`]). Shape outlines are stroked, not
/// filled, because no fill colour was found in the samples.
pub fn to_pages(doc: &NtbDocument, page_h: f32) -> Vec<Page> {
    let effective_h = doc.pages.first().map(|p| p.height).unwrap_or(page_h);
    let (page_w, page_h) = doc
        .pages
        .first()
        .map(|p| (p.width, p.height))
        .unwrap_or((NTB_PAGE_W, page_h));
    paginate(doc, effective_h)
        .into_iter()
        .enumerate()
        .map(|(i, np)| {
            let mut items: Vec<Item> = np
                .strokes
                .iter()
                .map(|s| {
                    let w = if s.width.is_finite() && s.width > 0.0 {
                        s.width
                    } else {
                        1.0
                    };
                    // Real path when decoded; a lone moveto becomes a
                    // zero-length mark so the position is still visible.
                    let points = if s.points.len() >= 2 {
                        s.points.clone()
                    } else {
                        vec![[s.anchor_x, s.anchor_y], [s.anchor_x, s.anchor_y]]
                    };
                    Item::Stroke(Stroke {
                        points,
                        width: w,
                        rgba: s.color.map(rgba_from_u32).unwrap_or(BLACK),
                        dash: None,
                        parented: false,
                    })
                })
                .collect();
            items.extend(np.shapes.iter().map(shape_to_item));
            Page {
                uuid: format!("ntb-p{i:04}"),
                items,
                width: page_w,
                height: page_h,
            }
        })
        .collect()
}

const BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Map an [`NtbShape`] onto the portable IR. Lines become a 2-point stroke
/// (same dash convention as handwriting); closed shapes become a [`ShapeEl`]
/// whose bbox starts at the anchor with the vector's dims as w/h.
fn shape_to_item(s: &NtbShape) -> Item {
    let rgba = s.color.map(rgba_from_u32).unwrap_or(BLACK);
    let width = if s.width.is_finite() && s.width > 0.0 {
        s.width
    } else {
        1.0
    };
    if s.vector_len == 12 || s.vector_len == 14 {
        return Item::Stroke(Stroke {
            points: vec![[s.anchor_x, s.anchor_y], [s.anchor_x + s.dx, s.anchor_y + s.dy]],
            width,
            rgba,
            dash: None,
            parented: false,
        });
    }
    // n = 8: ellipse when the two dims are near-equal, otherwise rectangle.
    let kind = if (s.dx - s.dy).abs() <= 0.05 * s.dx.abs().max(s.dy.abs()).max(1.0) {
        ShapeKind::Ellipse
    } else {
        ShapeKind::Rect
    };
    Item::Shape(ShapeEl {
        kind,
        x: s.anchor_x,
        y: s.anchor_y,
        w: s.dx.abs(),
        h: s.dy.abs(),
        points: Vec::new(),
        rotation: 0.0,
        fill: [0.0, 0.0, 0.0, 0.0],
        stroke: rgba,
        width,
        dash: None,
        radius: 0.0,
        uuid: None,
        from_f9: false,
    })
}
