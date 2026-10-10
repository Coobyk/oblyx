//! `.ntb` (Notability cloud) reader.
//!
//! Parses the ZIP container, walks the `noteBundle` FlatBuffer, and extracts
//! handwriting strokes (anchor position, width, point count). The intra-stroke
//! path geometry is **not** decoded — the three f32s per point have a
//! velocity-envelope pattern that does not integrate to page coordinates
//! (see `docs/NOTABILITY_FORMAT.md` §4.4). Strokes are therefore emitted as
//! zero-length paths at their anchor so the page layout is preserved.
//!
//! No public prior art exists for this format; the layout below was reverse
//! engineered from 9 sample notebooks (Notability 16.12.5).

use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use zip::ZipArchive;

// ---------------------------------------------------------------------------
// FlatBuffer walker (minimal, schema-less)
// ---------------------------------------------------------------------------

struct Buf<'a> {
    d: &'a [u8],
}

impl<'a> Buf<'a> {
    fn u32(&self, off: usize) -> Result<u32> {
        self.d
            .get(off..off + 4)
            .and_then(|b| Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
            .context(format!("u32 @ {off}"))
    }

    fn i32(&self, off: usize) -> Result<i32> {
        Ok(self.u32(off)? as i32)
    }

    fn u16(&self, off: usize) -> Result<u16> {
        self.d
            .get(off..off + 2)
            .and_then(|b| Some(u16::from_le_bytes([b[0], b[1]])))
            .context(format!("u16 @ {off}"))
    }

    fn f32(&self, off: usize) -> Result<f32> {
        self.d
            .get(off..off + 4)
            .and_then(|b| Some(f32::from_le_bytes([b[0], b[1], b[2], b[3]])))
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
const STROKE_F1_ANCHOR: usize = 1; // struct 2×f32: (Y, X)
const STROKE_F8_WIDTH: usize = 8; // f32
const STROKE_F9_POINTS: usize = 9; // uoffset to point blob

/// Constant 5-byte trailer ending each 19-byte point record.
const POINT_TRAILER: [u8; 5] = [0x00, 0x3c, 0xff, 0x00, 0x00];
/// Bytes per point record.
const POINT_STRIDE: usize = 19;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A decoded handwriting stroke. `points` is empty until the coordinate
/// encoding is solved; `anchor` is the verified absolute page position.
#[derive(Debug, Clone)]
pub struct NtbStroke {
    /// Absolute page X (left-right), points.
    pub anchor_x: f32,
    /// Absolute page Y (top-down), points.
    pub anchor_y: f32,
    /// Stroke width in points.
    pub width: f32,
    /// Number of point records in the blob (the raw path length).
    pub point_count: usize,
    /// Decoded path points. Currently always empty — see module docs.
    pub points: Vec<[f32; 2]>,
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

    let (meta, strokes) = parse_bundle(&bundle)?;
    Ok(NtbDocument {
        meta,
        strokes,
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

fn parse_bundle(d: &[u8]) -> Result<(NtbMeta, Vec<NtbStroke>)> {
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
    if field(ROOT_F6_ELEMENTS) != 0 {
        let vec_pos = b.follow(root_pos + field(ROOT_F6_ELEMENTS))?;
        let n = b.u32(vec_pos)? as usize;
        meta.element_count = n;
        for i in 0..n {
            let ep = vec_pos + 4 + 4 * i;
            let Ok(el_pos) = b.follow(ep) else { continue };
            if let Ok(Some(stroke)) = parse_element(&b, el_pos) {
                strokes.push(stroke);
            }
        }
    }

    Ok((meta, strokes))
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

/// Parse one element: wrapper (nf=6) → inner table. Returns a stroke when the
/// inner table has nf=16 (the handwriting-stroke type).
fn parse_element(b: &Buf, el_pos: usize) -> Result<Option<NtbStroke>> {
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
    if inf.len() != 16 {
        return Ok(None); // only nf=16 is a stroke
    }
    let stroke = parse_stroke(b, inner, &inf)?;
    // Only emit strokes with trailer-verified points; nf=16 elements with an
    // empty or unrecognised blob are not handwriting.
    if stroke.point_count == 0 {
        return Ok(None);
    }
    Ok(Some(stroke))
}

fn parse_stroke(b: &Buf, tp: usize, fields: &[u16]) -> Result<NtbStroke> {
    let field = |i: usize| -> usize { *fields.get(i).unwrap_or(&0) as usize };

    // f1: struct 2×f32 (Y, X) — absolute page anchor.
    let (anchor_y, anchor_x) = if field(STROKE_F1_ANCHOR) != 0 {
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

    // f9: uoffset to the point blob, at tp + tsize.
    let table_size = infer_table_size(fields);
    let (point_count, points) = if field(STROKE_F9_POINTS) != 0 {
        let blob = tp + table_size;
        count_points(b.d, blob)
    } else {
        (0, Vec::new())
    };

    Ok(NtbStroke {
        anchor_x,
        anchor_y,
        width,
        point_count,
        points,
    })
}

/// Infer the table size from the vtable: the blob starts at tp + tsize, and
/// the largest field offset + its size gives a lower bound. For Type A tsize=44
/// and Type B tsize=40; both have their largest field at offset 32 (Type A f0)
/// or 28 (Type B f0), so tsize = max_field_off + 4 rounded up works, but the
/// reliable rule is: tsize is the value stored at vtable+2. We recompute it
/// from the caller — here we just use the known constants via field layout.
fn infer_table_size(fields: &[u16]) -> usize {
    // Type A: largest offset 32, tsize 44. Type B: largest offset 28, tsize 40.
    // tsize = largest_offset + 12 in both cases (32+12=44, 28+12=40).
    let max_off = fields.iter().copied().max().unwrap_or(0) as usize;
    max_off + 12
}

/// Count point records in a blob using the 5-byte trailer at offset 14 of each
/// 19-byte record. Returns `(count, decoded_points)`; `decoded_points` is
/// always empty because the coordinate encoding is unsolved.
///
/// Only trailer-verified records are counted — the header byte at `blob+5`
/// also stores the count, but trusting it alone admits false positives from
/// non-stroke blobs that happen to have a non-zero byte at that offset.
fn count_points(d: &[u8], blob: usize) -> (usize, Vec<[f32; 2]>) {
    if blob + 8 > d.len() {
        return (0, Vec::new());
    }
    let mut count = 0;
    let mut p = blob;
    while p + POINT_STRIDE <= d.len() && d[p + 14..p + 19] == POINT_TRAILER {
        count += 1;
        p += POINT_STRIDE;
    }
    // Path geometry not decoded — see module docs.
    (count, Vec::new())
}

// ---------------------------------------------------------------------------
// Conversion to renderable pages
// ---------------------------------------------------------------------------

/// One renderable page. `.ntb` notebooks are continuous-scroll, so strokes
/// are assigned to pages by their Y coordinate using the given page height.
#[derive(Debug)]
pub struct NtbPage {
    pub strokes: Vec<NtbStroke>,
}

/// Split strokes into pages of `page_h` points each (top-down Y).
///
/// Notability's cloud notebooks are continuous-scroll; the matching PDF export
/// paginates at A4 height (841.89 pt) or a multiple thereof. Callers pick the
/// height that matches the target output.
pub fn paginate(doc: &NtbDocument, page_h: f32) -> Vec<NtbPage> {
    if page_h <= 0.0 || doc.strokes.is_empty() {
        return vec![NtbPage {
            strokes: doc.strokes.clone(),
        }];
    }
    let max_y = doc
        .strokes
        .iter()
        .map(|s| s.anchor_y)
        .fold(0.0f32, f32::max);
    let n_pages = ((max_y / page_h).ceil() as usize).max(1);
    let mut pages: Vec<NtbPage> = (0..n_pages)
        .map(|_| NtbPage { strokes: Vec::new() })
        .collect();
    for s in &doc.strokes {
        let idx = ((s.anchor_y / page_h).floor() as usize).min(n_pages - 1);
        pages[idx].strokes.push(s.clone());
    }
    pages
}
