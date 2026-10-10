//! `.ntb` (Notability cloud) reader.
//!
//! Parses the ZIP container, walks the `noteBundle` FlatBuffer, and folds its
//! CRDT op log into the final document state. The root f6 vector holds `Op`
//! tables; each op carries an entity id (f0), a `payloadType` (f4) and a
//! payload union (f5). Creates insert entities, later modifies patch them
//! (origin, colour, width, path), and `DeleteEntities` ops remove them — the
//! fold applies ops in log order and emits only live entities. See
//! `docs/NOTABILITY_FORMAT.md` §4.2 and §5.
//!
//! Handwriting strokes (CreateInk) decode anchor position, width, colour and
//! cubic-Bézier path geometry: each 19-byte point record holds three IEEE-754
//! half-precision points `(c1, c2, end)` relative to the stroke anchor (y
//! top-down); record 0 is a moveto whose control points are clobbered by the
//! stream header, so the path starts at the anchor itself. See §4.4.
//!
//! Shapes (CreateShape) decode definition type (Line / Polygon / NormalShape),
//! a top-left origin, stroke colour, border width and optional fill colour.
//! See §4.3.
//!
//! No public prior art exists for this format; the layout below was reverse
//! engineered from 9 sample notebooks (Notability 16.12.5) and cross-checked
//! against the notability.com web app's FlatBuffer schema and stroke codec.

use std::collections::{HashMap, HashSet};
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

    fn u8(&self, off: usize) -> Result<u8> {
        self.d.get(off).copied().context(format!("u8 @ {off}"))
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

    /// Field byte offset within a table, or 0 when the field is absent.
    fn field_off(fields: &[u16], i: usize) -> usize {
        *fields.get(i).unwrap_or(&0) as usize
    }
}

// ---------------------------------------------------------------------------
// noteBundle schema (verified, see docs/NOTABILITY_FORMAT.md §4–§5)
// ---------------------------------------------------------------------------

/// Root table field indices.
const ROOT_F6_OPS: usize = 6;

// Op table field indices.
const OP_F0_ID: usize = 0; // sf struct (site u16, timestamp u32), inline 8 bytes
const OP_F4_PAYLOAD_TYPE: usize = 4; // u8
const OP_F5_PAYLOAD: usize = 5; // uoffset to payload union table

// PayloadType enum (from the web-app schema; see §4.2).
const PT_CREATE_PAGE: u8 = 3;
const PT_CREATE_INK: u8 = 15;
const PT_MODIFY_INK: u8 = 17;
const PT_CREATE_SHAPE: u8 = 18;
const PT_MODIFY_SHAPE: u8 = 19;
const PT_MODIFY_POSITIONS: u8 = 24;
const PT_DELETE_ENTITIES: u8 = 25;

// CreateInk payload field indices (nf=16 in archived tables).
const INK_F0_PAGE: usize = 0; // df SeqId (site u16, timestamp u32, index u32), 12B inline
const INK_F1_ORIGIN: usize = 1; // Sf struct 2×f32, inline
const INK_F4_TOOL: usize = 4; // i8
const INK_F5_STYLE: usize = 5; // i8
const INK_F7_COLOR: usize = 7; // gf struct (R,G,B,A bytes)
const INK_F8_WIDTH: usize = 8; // f32
const INK_F9_PATH: usize = 9; // uoffset to point blob
const INK_F15_AUDIO_DURATION: usize = 15; // i32 (ms)

// CreateShape payload field indices.
const SHAPE_F0_PAGE: usize = 0; // df SeqId, 12B inline
const SHAPE_F1_ORIGIN: usize = 1; // Sf struct 2×f32 (top-left), inline
const SHAPE_F2_ROTATION: usize = 2; // f32
const SHAPE_F4_DEF_TYPE: usize = 4; // u8 (1=Line, 2=Polygon, 3=NormalShape)
const SHAPE_F5_DEF: usize = 5; // uoffset to definition table
const SHAPE_F9_COLOR: usize = 9; // gf struct
const SHAPE_F10_BORDER: usize = 10; // f32
const SHAPE_F11_FILL: usize = 11; // gf struct (present when nf >= 12)

// CreateShape definitionType values.
const DEF_LINE: u8 = 1;
const DEF_POLYGON: u8 = 2;
const DEF_NORMAL: u8 = 3;

// ModifyInk payload field indices.
const MINK_F0_TARGETS: usize = 0; // vector of sf structs (8 bytes each)
const MINK_F2_ORIGIN: usize = 2; // Sf struct
const MINK_F6_COLOR: usize = 6; // gf struct
const MINK_F7_WIDTH: usize = 7; // f32
const MINK_F8_PATH: usize = 8; // uoffset to point blob

// ModifyShape payload field indices (shifted +1 vs CreateShape: f0 = targets).
const MSHAPE_F0_TARGETS: usize = 0;
const MSHAPE_F2_ORIGIN: usize = 2;
const MSHAPE_F10_COLOR: usize = 10;
const MSHAPE_F11_BORDER: usize = 11;
const MSHAPE_F12_FILL: usize = 12;

// ModifyPositions payload field indices.
const MPOS_F0_MODS: usize = 0; // vector of uoffsets to ModifyPosition tables

// ModifyPosition table field indices.
const MPOS_TARGET: usize = 0; // sf struct
const MPOS_PAGE: usize = 1; // df SeqId (moves the target to this page)
const MPOS_ORIGIN: usize = 2; // Sf struct

// DeleteEntities payload field indices.
const DEL_F0_DELETES: usize = 0; // vector of sf structs
const DEL_F1_UNDELETES: usize = 1; // vector of sf structs

// CreatePage payload field indices.
const PAGE_F0_LOCATION: usize = 0; // df SeqId (matches ink/shape f0 page ids)
const PAGE_F1_BACKGROUND: usize = 1; // uoffset to PageBackground table

// PageBackground table field indices.
const BG_F1_NESTED: usize = 1; // uoffset to nested asset-ref table
const BG_F3_SIZE: usize = 3; // vf struct 2×f32 (width, height), inline

// Nested asset-ref table field indices (inside PageBackground.f1).
const NESTED_F0_INNER: usize = 0; // uoffset to inner table holding the hash

// CreatePage payload field indices (beyond f0 location / f1 background).
const PAGE_F2_PDF_PAGE: usize = 2; // u32, 1-based page index within the PDF asset

/// Constant 5-byte trailer ending each 19-byte point record.
const POINT_TRAILER: [u8; 5] = [0x00, 0x3c, 0xff, 0x00, 0x00];
/// Bytes per point record.
const POINT_STRIDE: usize = 19;
/// Bézier samples emitted per cubic record when decoding paths to polylines.
const BEZIER_SAMPLES: usize = 12;

/// Entity id: `(site, timestamp)` from an op's f0 sf struct.
type EntityId = (u16, u32);

/// Page id: `(site, timestamp, index)` from a df SeqId struct (12 bytes,
/// aligned: `site u16 @0`, `pad 2 @2`, `timestamp u32 @4`, `index u32 @8`).
/// Matches CreateInk/CreateShape f0 and CreatePage f0 (location).
type PageId = (u16, u32, u32);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A decoded handwriting stroke. `points` holds the sampled absolute page
/// path (anchor + relative Bézier geometry); `anchor` is the verified
/// absolute page position of the path origin.
#[derive(Debug, Clone)]
pub struct NtbStroke {
    /// Page id (CreateInk f0 df SeqId); `None` when the field is absent.
    pub page: Option<PageId>,
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
    /// stroke table carries one (field f7); `None` → default black.
    pub color: Option<u32>,
    /// Tool enum (CreateInk f4): Pen=0, Pencil=1, Highlighter=2, Tape=3 …
    pub tool: Option<i8>,
    /// Style enum (CreateInk f5): VariableWidth=0, FixedWidth=1, Dash=2,
    /// Dots=3. Controls whether the per-record u16 width hint varies.
    pub style: Option<i8>,
    /// Audio-sync duration in milliseconds (CreateInk f15) when the stroke
    /// was drawn during a recording; `None` otherwise.
    pub audio_duration_ms: Option<i32>,
}

/// A page definition from a CreatePage op. `id` is the CreatePage f0 location
/// SeqId matching ink/shape page ids. `width`/`height` are the background size
/// in points when present; `0×0` means the page was created bare (no
/// background recorded) and pagination falls back to A4. `asset_hash` is the
/// 64-byte content hash of the background PDF/image (from the PageBackground
/// nested table) when a background is present; `pdf_page` is the 1-based page
/// index within that asset (CreatePage payload f2, defaulting to 1).
#[derive(Debug, Clone)]
pub struct NtbPageDef {
    pub id: PageId,
    pub width: f32,
    pub height: f32,
    /// 64-byte content hash of the background asset, when present.
    pub asset_hash: Option<[u8; 64]>,
    /// 1-based page index within the background asset PDF.
    pub pdf_page: u64,
}

impl Default for NtbPageDef {
    fn default() -> Self {
        Self {
            id: (0, 0, 0),
            width: 0.0,
            height: 0.0,
            asset_hash: None,
            pdf_page: 1,
        }
    }
}

/// A text style record (best-effort). In all 9 samples the only such element
/// is a document-level style record (font `Inter`, size 14, locale `en_US`);
/// no positioned text boxes with content were found, so no position/text is
/// decoded here.
#[derive(Debug, Clone)]
pub struct NtbText {
    pub font: String,
    pub size: f32,
}

/// Shape geometry, decoded from a CreateShape definition union.
/// Origins are top-left (web-schema verified); Line/Polygon points are
/// origin-relative and converted to absolute here.
#[derive(Debug, Clone)]
pub enum NtbShapeGeom {
    /// A (possibly cubic) line. `c1`/`c2` are optional cubic control points;
    /// when absent the line is straight from start to end.
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        c1: Option<[f32; 2]>,
        c2: Option<[f32; 2]>,
    },
    /// An axis-aligned box with top-left `(x, y)` and size `(w, h)`.
    /// `ellipse` is true for NormalShape type Ellipse (the only type in the
    /// web schema); other types render as rectangles.
    Box {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        ellipse: bool,
    },
    /// A closed polygon through absolute points (at least 3).
    Polygon { points: Vec<[f32; 2]> },
}

/// A decoded shape (CreateShape, payloadType 18).
#[derive(Debug, Clone)]
pub struct NtbShape {
    /// Page id (CreateShape f0 df SeqId); `None` when the field is absent.
    pub page: Option<PageId>,
    /// Shape geometry in absolute page coordinates.
    pub geom: NtbShapeGeom,
    /// Rotation in degrees (payload f2); 0 when absent.
    pub rotation: f32,
    /// Stroke colour u32 (`0xAABBGGRR`); `None` → default black.
    pub color: Option<u32>,
    /// Fill colour u32 (`0xAABBGGRR`); `None` → transparent.
    pub fill: Option<u32>,
    /// Outline width in points (payload f10); 1.0 when absent.
    pub border_width: f32,
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
    /// Number of ops in root f6.
    pub element_count: usize,
}

/// A fully parsed `.ntb` notebook. Strokes and shapes hold the folded final
/// state: creates patched by later modifies, with deleted entities removed.
#[derive(Debug)]
pub struct NtbDocument {
    pub meta: NtbMeta,
    /// Live handwriting strokes, in creation order.
    pub strokes: Vec<NtbStroke>,
    /// Page sizes from CreatePage backgrounds (informational; pagination
    /// still uses Y-slicing because op-log page ids do not reliably group
    /// strokes — see §9).
    pub pages: Vec<NtbPageDef>,
    /// Text style records (best-effort).
    pub texts: Vec<NtbText>,
    /// Live shapes, in creation order.
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
// noteBundle parsing: CRDT op-log fold
// ---------------------------------------------------------------------------

/// Parsed contents of a noteBundle: metadata, live strokes, pages, texts,
/// live shapes. `(meta, strokes, pages, texts, shapes)`.
pub type BundleParts = (
    NtbMeta,
    Vec<NtbStroke>,
    Vec<NtbPageDef>,
    Vec<NtbText>,
    Vec<NtbShape>,
);

/// Mutable fold state while walking the op log.
struct Fold {
    strokes: HashMap<EntityId, NtbStroke>,
    shapes: HashMap<EntityId, NtbShape>,
    stroke_order: Vec<EntityId>,
    shape_order: Vec<EntityId>,
    deleted: HashSet<EntityId>,
}

fn parse_bundle(d: &[u8]) -> Result<BundleParts> {
    let b = Buf { d };
    if d.len() < 8 {
        bail!("noteBundle too small ({} bytes)", d.len());
    }

    // Root: u32 uoffset at byte 0 → root table.
    let root_pos = b.u32(0)? as usize;
    let (_rp, _rts, rf) = b.table(root_pos)?;
    let field = |i: usize| -> usize { Buf::field_off(&rf, i) };

    let mut meta = NtbMeta::default();

    // f0: 16-byte inline note GUID at offset 56 (when present).
    if field(0) != 0 && d.len() >= root_pos + field(0) + 16 {
        let off = root_pos + field(0);
        meta.note_guid.copy_from_slice(&d[off..off + 16]);
    }

    // f4: u64 timestamp at offset 40.
    if field(4) != 0 {
        let off = root_pos + field(4);
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

    // f6: vector of Op tables — the CRDT log.
    let mut fold = Fold {
        strokes: HashMap::new(),
        shapes: HashMap::new(),
        stroke_order: Vec::new(),
        shape_order: Vec::new(),
        deleted: HashSet::new(),
    };
    let mut pages = Vec::new();
    let mut texts = Vec::new();

    if field(ROOT_F6_OPS) != 0 {
        let vec_pos = b.follow(root_pos + field(ROOT_F6_OPS))?;
        let n = b.u32(vec_pos)? as usize;
        meta.element_count = n;
        for i in 0..n {
            let ep = vec_pos + 4 + 4 * i;
            let Ok(op_pos) = b.follow(ep) else { continue };
            let _ = fold_op(&b, op_pos, &mut fold, &mut pages, &mut texts);
        }
    }

    // Emit live entities in creation order.
    let mut strokes = Vec::with_capacity(fold.stroke_order.len());
    for id in &fold.stroke_order {
        if !fold.deleted.contains(id)
            && let Some(s) = fold.strokes.get(id)
        {
            strokes.push(s.clone());
        }
    }
    let mut shapes = Vec::with_capacity(fold.shape_order.len());
    for id in &fold.shape_order {
        if !fold.deleted.contains(id)
            && let Some(s) = fold.shapes.get(id)
        {
            shapes.push(s.clone());
        }
    }

    Ok((meta, strokes, pages, texts, shapes))
}

/// Apply one op to the fold state. Errors on a single op are non-fatal
/// (the op is skipped).
fn fold_op(
    b: &Buf,
    op_pos: usize,
    fold: &mut Fold,
    pages: &mut Vec<NtbPageDef>,
    texts: &mut Vec<NtbText>,
) -> Result<()> {
    let (_ops, _ot, of) = b.table(op_pos)?;
    let id = read_entity_id(b, op_pos, Buf::field_off(&of, OP_F0_ID));
    let payload_type = if Buf::field_off(&of, OP_F4_PAYLOAD_TYPE) != 0 {
        b.u8(op_pos + Buf::field_off(&of, OP_F4_PAYLOAD_TYPE))?
    } else {
        0
    };
    let payload_pos = if Buf::field_off(&of, OP_F5_PAYLOAD) != 0 {
        b.follow(op_pos + Buf::field_off(&of, OP_F5_PAYLOAD))?
    } else {
        return Ok(());
    };
    let (_ps, _pt, pf) = b.table(payload_pos)?;

    match payload_type {
        PT_CREATE_INK => {
            if let Some(id) = id {
                let stroke = parse_stroke(b, payload_pos, &pf)?;
                if stroke.point_count > 0 {
                    if !fold.strokes.contains_key(&id) {
                        fold.stroke_order.push(id);
                    }
                    fold.strokes.insert(id, stroke);
                }
            }
        }
        PT_CREATE_SHAPE => {
            if let Some(id) = id
                && let Ok(Some(shape)) = parse_shape(b, payload_pos, &pf)
            {
                if !fold.shapes.contains_key(&id) {
                    fold.shape_order.push(id);
                }
                fold.shapes.insert(id, shape);
            }
        }
        PT_MODIFY_INK => {
            apply_ink_patch(b, payload_pos, &pf, fold)?;
        }
        PT_MODIFY_SHAPE => {
            apply_shape_patch(b, payload_pos, &pf, fold)?;
        }
        PT_MODIFY_POSITIONS => {
            apply_positions(b, payload_pos, &pf, fold)?;
        }
        PT_DELETE_ENTITIES => {
            apply_deletes(b, payload_pos, &pf, fold)?;
        }
        PT_CREATE_PAGE => {
            if let Some(pd) = parse_create_page(b, payload_pos, &pf)? {
                upsert_page(pages, pd);
            }
        }
        _ => {
            // Best-effort text-style extraction from unhandled payloads
            // (f4 string + f5 f32 that looks like a font size).
            if let Some(t) = parse_text(b, payload_pos, &pf)? {
                texts.push(t);
            }
        }
    }
    Ok(())
}

/// Read an entity id from an op's f0 sf struct: `(site u16, timestamp u32)`.
fn read_entity_id(b: &Buf, tp: usize, off: usize) -> Option<EntityId> {
    if off == 0 {
        return None;
    }
    let site = b.u16(tp + off).ok()?;
    let ts = b.u32(tp + off + 4).ok()?;
    Some((site, ts))
}

/// Read a vector of sf entity ids (8 bytes each) at `tp + field_off`.
/// Each element is an inline sf struct: site u16 @0, timestamp u32 @4.
fn read_id_vector(b: &Buf, tp: usize, field_off: usize) -> Vec<EntityId> {
    let mut out = Vec::new();
    if field_off == 0 {
        return out;
    }
    let Ok(vec_pos) = b.follow(tp + field_off) else {
        return out;
    };
    let Ok(n) = b.u32(vec_pos) else { return out };
    for k in 0..n as usize {
        let s = vec_pos + 4 + 8 * k;
        if let (Ok(site), Ok(ts)) = (b.u16(s), b.u32(s + 4)) {
            out.push((site, ts));
        }
    }
    out
}

/// Apply a ModifyInk op: patch origin (f2), colour (f6), width (f7) and
/// path (f8) on each target entity.
fn apply_ink_patch(b: &Buf, tp: usize, fields: &[u16], fold: &mut Fold) -> Result<()> {
    let targets = read_id_vector(b, tp, Buf::field_off(fields, MINK_F0_TARGETS));
    let origin = read_sf(b, tp, Buf::field_off(fields, MINK_F2_ORIGIN));
    let color = read_color(b, tp, Buf::field_off(fields, MINK_F6_COLOR));
    let width = read_f32_field(b, tp, Buf::field_off(fields, MINK_F7_WIDTH));
    let path_pos = if Buf::field_off(fields, MINK_F8_PATH) != 0 {
        b.follow(tp + Buf::field_off(fields, MINK_F8_PATH)).ok()
    } else {
        None
    };

    for tid in &targets {
        let Some(s) = fold.strokes.get_mut(tid) else {
            continue;
        };
        if let Some([nx, ny]) = origin {
            let dx = nx - s.anchor_x;
            let dy = ny - s.anchor_y;
            s.anchor_x = nx;
            s.anchor_y = ny;
            for p in &mut s.points {
                p[0] += dx;
                p[1] += dy;
            }
        }
        if color.is_some() {
            s.color = color;
        }
        if let Some(w) = width
            && w.is_finite()
            && w > 0.0
        {
            s.width = w;
        }
        if let Some(blob) = path_pos {
            let (count, points) = decode_stroke_points(b.d, blob, s.anchor_x, s.anchor_y);
            if count > 0 {
                s.point_count = count;
                s.points = points;
            }
        }
    }
    Ok(())
}

/// Apply a ModifyShape op: patch origin (f2), colour (f10), border (f11)
/// and fill (f12) on each target entity.
fn apply_shape_patch(b: &Buf, tp: usize, fields: &[u16], fold: &mut Fold) -> Result<()> {
    let targets = read_id_vector(b, tp, Buf::field_off(fields, MSHAPE_F0_TARGETS));
    let origin = read_sf(b, tp, Buf::field_off(fields, MSHAPE_F2_ORIGIN));
    let color = read_color(b, tp, Buf::field_off(fields, MSHAPE_F10_COLOR));
    let border = read_f32_field(b, tp, Buf::field_off(fields, MSHAPE_F11_BORDER));
    let fill = read_color(b, tp, Buf::field_off(fields, MSHAPE_F12_FILL));

    for tid in &targets {
        let Some(s) = fold.shapes.get_mut(tid) else {
            continue;
        };
        if let Some([nx, ny]) = origin {
            shift_shape_origin(&mut s.geom, nx, ny);
        }
        if color.is_some() {
            s.color = color;
        }
        if let Some(w) = border
            && w.is_finite()
            && w > 0.0
        {
            s.border_width = w;
        }
        if fill.is_some() {
            s.fill = fill;
        }
    }
    Ok(())
}

/// Shift a shape geometry so its origin moves to `(nx, ny)`. For Box the
/// origin is the top-left; for Line/Polygon the origin is implicit in the
/// absolute points, so we translate by the delta from the current origin.
fn shift_shape_origin(geom: &mut NtbShapeGeom, nx: f32, ny: f32) {
    match geom {
        NtbShapeGeom::Line { x1, y1, x2, y2, c1, c2 } => {
            let dx = nx - *x1;
            let dy = ny - *y1;
            *x1 = nx;
            *y1 = ny;
            *x2 += dx;
            *y2 += dy;
            if let Some(c) = c1.as_mut() {
                c[0] += dx;
                c[1] += dy;
            }
            if let Some(c) = c2.as_mut() {
                c[0] += dx;
                c[1] += dy;
            }
        }
        NtbShapeGeom::Box { x, y, .. } => {
            *x = nx;
            *y = ny;
        }
        NtbShapeGeom::Polygon { points } => {
            if let Some(first) = points.first().copied() {
                let dx = nx - first[0];
                let dy = ny - first[1];
                for p in points.iter_mut() {
                    p[0] += dx;
                    p[1] += dy;
                }
            }
        }
    }
}

/// Apply a ModifyPositions op: for each ModifyPosition entry, patch the
/// Apply a ModifyPositions op: for each ModifyPosition entry, patch the
/// target's page (f1) and origin (f2).
fn apply_positions(b: &Buf, tp: usize, fields: &[u16], fold: &mut Fold) -> Result<()> {
    let f0 = Buf::field_off(fields, MPOS_F0_MODS);
    if f0 == 0 {
        return Ok(());
    }
    let vec_pos = b.follow(tp + f0)?;
    let n = b.u32(vec_pos)? as usize;
    for k in 0..n {
        let slot = vec_pos + 4 + 4 * k;
        let Ok(mp) = b.follow(slot) else { continue };
        let (_ms, _mt, mf) = b.table(mp).unwrap_or((0, 0, Vec::new()));
        let target = read_entity_id(b, mp, Buf::field_off(&mf, MPOS_TARGET));
        let page = read_page_id(b, mp, Buf::field_off(&mf, MPOS_PAGE));
        let origin = read_sf(b, mp, Buf::field_off(&mf, MPOS_ORIGIN));
        let Some(tid) = target else { continue };
        if let Some(s) = fold.strokes.get_mut(&tid) {
            if let Some(p) = page {
                s.page = Some(p);
            }
            if let Some([nx, ny]) = origin {
                let dx = nx - s.anchor_x;
                let dy = ny - s.anchor_y;
                s.anchor_x = nx;
                s.anchor_y = ny;
                for pt in &mut s.points {
                    pt[0] += dx;
                    pt[1] += dy;
                }
            }
        } else if let Some(s) = fold.shapes.get_mut(&tid) {
            if let Some(p) = page {
                s.page = Some(p);
            }
            if let Some([nx, ny]) = origin {
                shift_shape_origin(&mut s.geom, nx, ny);
            }
        }
    }
    Ok(())
}

/// Apply a DeleteEntities op: add f0 deletes to the dead set, remove f1
/// undeletes from it (order matters: a later undelete revives, a later
/// delete re-kills).
fn apply_deletes(b: &Buf, tp: usize, fields: &[u16], fold: &mut Fold) -> Result<()> {
    for id in read_id_vector(b, tp, Buf::field_off(fields, DEL_F0_DELETES)) {
        fold.deleted.insert(id);
    }
    for id in read_id_vector(b, tp, Buf::field_off(fields, DEL_F1_UNDELETES)) {
        fold.deleted.remove(&id);
    }
    Ok(())
}

/// Parse a CreatePage op: f0 location SeqId + optional f1 → PageBackground →
/// nested asset-ref (64-byte hash) + f3 size. A bare CreatePage (f0 present,
/// f1/size absent) registers the page with unknown dims (`0×0`); a later
/// CreatePage with a background updates the dims (see [`upsert_page`]).
/// Payload f2, when present, is the 1-based PDF page index within the asset.
fn parse_create_page(b: &Buf, tp: usize, fields: &[u16]) -> Result<Option<NtbPageDef>> {
    let f0 = Buf::field_off(fields, PAGE_F0_LOCATION);
    let Some(id) = read_page_id(b, tp, f0) else {
        return Ok(None);
    };
    let unknown = NtbPageDef {
        id,
        width: 0.0,
        height: 0.0,
        asset_hash: None,
        pdf_page: 1,
    };
    let f2 = Buf::field_off(fields, PAGE_F2_PDF_PAGE);
    let pdf_page = if f2 != 0 {
        let v = b.u32(tp + f2).unwrap_or(1);
        if (1..=100_000).contains(&v) { v as u64 } else { 1 }
    } else {
        1
    };
    let f1 = Buf::field_off(fields, PAGE_F1_BACKGROUND);
    if f1 == 0 {
        return Ok(Some(unknown));
    }
    let bg = b.follow(tp + f1)?;
    let (_bs, _bt, bf) = b.table(bg)?;
    let f3 = Buf::field_off(&bf, BG_F3_SIZE);
    if f3 == 0 {
        return Ok(Some(unknown));
    }
    let width = b.f32(bg + f3)?;
    let height = b.f32(bg + f3 + 4)?;
    if !width.is_finite() || !height.is_finite() || width < 50.0 || height < 50.0
        || width > 10_000.0 || height > 10_000.0
    {
        return Ok(Some(unknown));
    }
    // Asset hash: PageBackground.f1 → nested table.f0 → inner table with the
    // 64-byte content hash stored inline at the field's absolute position.
    let asset_hash = read_asset_hash(b, bg, &bf);
    Ok(Some(NtbPageDef {
        id,
        width,
        height,
        asset_hash,
        pdf_page,
    }))
}

/// Read the 64-byte background asset hash from a PageBackground table.
///
/// Layout: `PageBackground.f1` → nested table; nested `.f0` → inner table;
/// the inner table's first present field's absolute position holds the hash
/// inline (not a vector). Verified against E1/B1/D1/F1/G1/P1/Eth1 samples.
fn read_asset_hash(b: &Buf, bg: usize, bf: &[u16]) -> Option<[u8; 64]> {
    let f1 = Buf::field_off(bf, BG_F1_NESTED);
    if f1 == 0 {
        return None;
    }
    let nested = b.follow(bg + f1).ok()?;
    let (_ns, _nt, nf) = b.table(nested).ok()?;
    let f0 = Buf::field_off(&nf, NESTED_F0_INNER);
    if f0 == 0 {
        return None;
    }
    let inner = b.follow(nested + f0).ok()?;
    let (_is, _it, inf) = b.table(inner).ok()?;
    // The hash is inline at the position of the inner table's first present
    // field that is large enough (≥64 bytes of following data). In all
    // samples the first present field (index 0) points at the hash start.
    for &fo in inf.iter() {
        if fo == 0 {
            continue;
        }
        let abs = inner + fo as usize;
        if abs + 64 <= b.d.len() {
            let mut hash = [0u8; 64];
            hash.copy_from_slice(&b.d[abs..abs + 64]);
            return Some(hash);
        }
    }
    None
}

/// Insert or merge a page def: one entry per id, first-appearance order.
/// A later def with valid dims (`>0`) updates the existing entry's size and
/// asset hash; a bare def (`0×0`) never clears dims or an asset already
/// recorded.
fn upsert_page(pages: &mut Vec<NtbPageDef>, def: NtbPageDef) {
    if let Some(existing) = pages.iter_mut().find(|p| p.id == def.id) {
        if def.width > 0.0 && def.height > 0.0 {
            existing.width = def.width;
            existing.height = def.height;
        }
        if def.asset_hash.is_some() {
            existing.asset_hash = def.asset_hash;
            existing.pdf_page = def.pdf_page;
        }
    } else {
        pages.push(def);
    }
}

/// Read a df SeqId struct (12 bytes, aligned) at `tp + off`:
/// `site u16 @0`, `pad 2 @2`, `timestamp u32 @4`, `index u32 @8`.
fn read_page_id(b: &Buf, tp: usize, off: usize) -> Option<PageId> {
    if off == 0 {
        return None;
    }
    let site = b.u16(tp + off).ok()?;
    let ts = b.u32(tp + off + 4).ok()?;
    let idx = b.u32(tp + off + 8).ok()?;
    Some((site, ts, idx))
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

/// Read an Sf struct (2×f32) inline at `tp + off`. `None` when off == 0.
fn read_sf(b: &Buf, tp: usize, off: usize) -> Option<[f32; 2]> {
    if off == 0 {
        return None;
    }
    let x = b.f32(tp + off).ok()?;
    let y = b.f32(tp + off + 4).ok()?;
    if x.is_finite() && y.is_finite() { Some([x, y]) } else { None }
}

/// Read an f32 field at `tp + off`. `None` when off == 0 or non-finite.
fn read_f32_field(b: &Buf, tp: usize, off: usize) -> Option<f32> {
    if off == 0 {
        return None;
    }
    let v = b.f32(tp + off).ok()?;
    v.is_finite().then_some(v)
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

/// Parse a shape (CreateShape payload). Field indices from the web schema;
/// origin is top-left. Returns `None` on unparseable geometry.
fn parse_shape(b: &Buf, tp: usize, fields: &[u16]) -> Result<Option<NtbShape>> {
    let field = |i: usize| -> usize { Buf::field_off(fields, i) };

    // f0: page SeqId (df, 12B aligned).
    let page = read_page_id(b, tp, field(SHAPE_F0_PAGE));

    // f1: origin (Sf, top-left of the bounding box).
    let Some([origin_x, origin_y]) = read_sf(b, tp, field(SHAPE_F1_ORIGIN)) else {
        return Ok(None);
    };

    // f2: rotation (degrees).
    let rotation = read_f32_field(b, tp, field(SHAPE_F2_ROTATION)).unwrap_or(0.0);

    // f4: definitionType u8.
    let def_type = if field(SHAPE_F4_DEF_TYPE) != 0 {
        b.u8(tp + field(SHAPE_F4_DEF_TYPE)).unwrap_or(0)
    } else {
        0
    };

    // f5: definition union.
    let Some(geom) = parse_shape_def(b, tp, field(SHAPE_F5_DEF), def_type, origin_x, origin_y)?
    else {
        return Ok(None);
    };

    let color = read_color(b, tp, field(SHAPE_F9_COLOR));
    let fill = read_color(b, tp, field(SHAPE_F11_FILL));
    let border_width = read_f32_field(b, tp, field(SHAPE_F10_BORDER)).unwrap_or(1.0);

    Ok(Some(NtbShape {
        page,
        geom,
        rotation,
        color,
        fill,
        border_width,
    }))
}

/// Parse a shape definition union at `tp + def_off`, dispatched on
/// `def_type`. Points are origin-relative and converted to absolute.
fn parse_shape_def(
    b: &Buf,
    tp: usize,
    def_off: usize,
    def_type: u8,
    origin_x: f32,
    origin_y: f32,
) -> Result<Option<NtbShapeGeom>> {
    if def_off == 0 {
        return Ok(None);
    }
    let dpos = b.follow(tp + def_off)?;
    let (_ds, _dt, df) = b.table(dpos)?;
    let dfield = |i: usize| -> usize { Buf::field_off(&df, i) };

    match def_type {
        DEF_LINE => {
            // start (f0), cp1 (f1), cp2 (f2), end (f3) — inline Sf structs.
            let Some([sx, sy]) = read_sf(b, dpos, dfield(0)) else {
                return Ok(None);
            };
            let Some([ex, ey]) = read_sf(b, dpos, dfield(3)) else {
                return Ok(None);
            };
            let c1 = read_sf(b, dpos, dfield(1));
            let c2 = read_sf(b, dpos, dfield(2));
            Ok(Some(NtbShapeGeom::Line {
                x1: origin_x + sx,
                y1: origin_y + sy,
                x2: origin_x + ex,
                y2: origin_y + ey,
                c1: c1.map(|[x, y]| [origin_x + x, origin_y + y]),
                c2: c2.map(|[x, y]| [origin_x + x, origin_y + y]),
            }))
        }
        DEF_NORMAL => {
            // type (f0, i8, default Ellipse=0), size (f1, vf 2×f32 inline).
            let ntype = if dfield(0) != 0 {
                b.u8(dpos + dfield(0)).unwrap_or(0)
            } else {
                0
            };
            let Some([w, h]) = read_sf(b, dpos, dfield(1)) else {
                return Ok(None);
            };
            if !w.is_finite() || !h.is_finite() || w == 0.0 && h == 0.0 {
                return Ok(None);
            }
            Ok(Some(NtbShapeGeom::Box {
                x: origin_x,
                y: origin_y,
                w,
                h,
                ellipse: ntype == 0,
            }))
        }
        DEF_POLYGON => {
            // points (f0): vector of inline Sf structs.
            let f0 = dfield(0);
            if f0 == 0 {
                return Ok(None);
            }
            let vec_pos = b.follow(dpos + f0)?;
            let n = b.u32(vec_pos)? as usize;
            if n < 2 {
                return Ok(None);
            }
            let mut points = Vec::with_capacity(n);
            for k in 0..n {
                let off = vec_pos + 4 + 8 * k;
                let Ok(x) = b.f32(off) else { break };
                let Ok(y) = b.f32(off + 4) else { break };
                points.push([origin_x + x, origin_y + y]);
            }
            if points.len() < 2 {
                return Ok(None);
            }
            Ok(Some(NtbShapeGeom::Polygon { points }))
        }
        _ => Ok(None),
    }
}

/// Parse a text style record (best-effort from any payload: f4 string +
/// f5 f32). Used only for unhandled payload types.
fn parse_text(b: &Buf, tp: usize, fields: &[u16]) -> Result<Option<NtbText>> {
    let f4 = Buf::field_off(fields, 4);
    let f5 = Buf::field_off(fields, 5);
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

/// Parse a CreateInk payload into a stroke.
fn parse_stroke(b: &Buf, tp: usize, fields: &[u16]) -> Result<NtbStroke> {
    let field = |i: usize| -> usize { Buf::field_off(fields, i) };

    // f0: page SeqId (df, 12B aligned).
    let page = read_page_id(b, tp, field(INK_F0_PAGE));

    // f1: origin (Sf 2×f32) — absolute page anchor.
    let (anchor_x, anchor_y) = if let Some([x, y]) = read_sf(b, tp, field(INK_F1_ORIGIN)) {
        (x, y)
    } else {
        (0.0, 0.0)
    };

    // f8: f32 width.
    let width = read_f32_field(b, tp, field(INK_F8_WIDTH)).unwrap_or(0.0);

    // f9: uoffset to the point blob.
    let (point_count, points) = if field(INK_F9_PATH) != 0 {
        match b.follow(tp + field(INK_F9_PATH)) {
            Ok(blob) => decode_stroke_points(b.d, blob, anchor_x, anchor_y),
            Err(_) => (0, Vec::new()),
        }
    } else {
        (0, Vec::new())
    };

    // f7: colour (gf). PDF-verified: `0xff1515b5` ↔ `#b515b5`.
    let color = read_color(b, tp, field(INK_F7_COLOR));

    // f4/f5: tool and style enums (i8).
    let tool = if field(INK_F4_TOOL) != 0 {
        b.u8(tp + field(INK_F4_TOOL)).ok().map(|v| v as i8)
    } else {
        None
    };
    let style = if field(INK_F5_STYLE) != 0 {
        b.u8(tp + field(INK_F5_STYLE)).ok().map(|v| v as i8)
    } else {
        None
    };

    // f15: audio duration (i32 ms).
    let audio_duration_ms = if field(INK_F15_AUDIO_DURATION) != 0 {
        b.i32(tp + field(INK_F15_AUDIO_DURATION)).ok()
    } else {
        None
    };

    Ok(NtbStroke {
        page,
        anchor_x,
        anchor_y,
        width,
        point_count,
        points,
        color,
        tool,
        style,
        audio_duration_ms,
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
/// A few strokes (2 in G1) carry a shifted header; when the aligned scan
/// finds nothing, the decoder retries at byte offsets 1..19 to recover
/// alignment (see §4.4).
///
/// Points are returned in absolute page coordinates (anchor + relative),
/// sampled at [`BEZIER_SAMPLES`] steps per cubic.
fn decode_stroke_points(d: &[u8], blob: usize, anchor_x: f32, anchor_y: f32) -> (usize, Vec<[f32; 2]>) {
    // Count records via trailer scan (ground truth; the header length admits
    // false positives on non-stroke blobs).
    let mut count = scan_trailers(d, blob);
    let mut base = blob;
    if count == 0 {
        // Fallback: shifted-header strokes (2 known in G1). Try each
        // alignment offset until trailers line up.
        for shift in 1..POINT_STRIDE {
            let c = scan_trailers(d, blob + shift);
            if c > 0 {
                count = c;
                base = blob + shift;
                break;
            }
        }
    }
    if count == 0 {
        return (0, Vec::new());
    }
    // rec0 = moveto at the anchor; rec1..count−1 = cubics.
    let mut points = Vec::with_capacity(count.saturating_mul(BEZIER_SAMPLES));
    points.push([anchor_x, anchor_y]);
    let mut cur = [0.0f32, 0.0f32]; // relative
    for k in 1..count {
        let off = base + k * POINT_STRIDE;
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

/// Count consecutive 19-byte records starting at `start` whose last 5 bytes
/// match the constant trailer.
fn scan_trailers(d: &[u8], start: usize) -> usize {
    let mut count = 0usize;
    let mut p = start;
    while p + POINT_STRIDE <= d.len() && d[p + 14..p + 19] == POINT_TRAILER {
        count += 1;
        p += POINT_STRIDE;
    }
    count
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

/// One renderable page with its dimensions.
#[derive(Debug)]
pub struct NtbPage {
    pub strokes: Vec<NtbStroke>,
    pub shapes: Vec<NtbShape>,
    pub width: f32,
    pub height: f32,
    /// 64-byte content hash of the background asset, when the page def
    /// recorded one. Used by [`to_pages`] callers to wire PDF backgrounds.
    pub asset_hash: Option<[u8; 64]>,
    /// 1-based page index within the background asset PDF.
    pub pdf_page: u64,
}

/// Split strokes and shapes into pages.
///
/// Primary strategy: group by the page SeqId carried on each entity's Create
/// op (CreateInk/CreateShape f0). Page dimensions come from the matching
/// CreatePage background (by location SeqId), defaulting to A4. Entities
/// without a page id fall through to the secondary Y-slice strategy.
///
/// Secondary strategy (no page ids at all): split by Y coordinate at
/// `page_h` points — the continuous-scroll approximation.
pub fn paginate(doc: &NtbDocument, page_h: f32) -> Vec<NtbPage> {
    let a4 = || NtbPage {
        strokes: Vec::new(),
        shapes: Vec::new(),
        width: NTB_PAGE_W,
        height: if page_h > 0.0 { page_h } else { NTB_PAGE_H },
        asset_hash: None,
        pdf_page: 1,
    };
    if doc.strokes.is_empty() && doc.shapes.is_empty() {
        return vec![NtbPage {
            strokes: Vec::new(),
            shapes: Vec::new(),
            width: NTB_PAGE_W,
            height: if page_h > 0.0 { page_h } else { NTB_PAGE_H },
            asset_hash: None,
            pdf_page: 1,
        }];
    }

    // Page-id grouping when every entity carries one.
    let all_have_ids = doc.strokes.iter().all(|s| s.page.is_some())
        && doc.shapes.iter().all(|s| s.page.is_some());
    if all_have_ids {
        return paginate_by_page_id(doc, page_h);
    }

    // Y-slice fallback.
    if page_h <= 0.0 {
        return vec![NtbPage {
            strokes: doc.strokes.clone(),
            shapes: doc.shapes.clone(),
            width: NTB_PAGE_W,
            height: NTB_PAGE_H,
            asset_hash: None,
            pdf_page: 1,
        }];
    }
    let max_y = doc
        .strokes
        .iter()
        .map(|s| s.anchor_y)
        .chain(doc.shapes.iter().map(shape_anchor_y))
        .fold(0.0f32, f32::max);
    let n_pages = ((max_y / page_h).ceil() as usize).max(1);
    let mut pages: Vec<NtbPage> = (0..n_pages).map(|_| a4()).collect();
    for s in &doc.strokes {
        let idx = ((s.anchor_y / page_h).floor() as usize).min(n_pages - 1);
        pages[idx].strokes.push(s.clone());
    }
    for s in &doc.shapes {
        let idx = ((shape_anchor_y(s) / page_h).floor() as usize).min(n_pages - 1);
        pages[idx].shapes.push(s.clone());
    }
    pages
}

/// Group by page SeqId. Page sequence follows the CreatePage defs
/// (`doc.pages`, one entry per id in first-appearance order; `0×0` dims mean
/// unknown → A4 fallback). Only pages that carry live ink (strokes or shapes)
/// are emitted — bare CreatePage ops often mark scratch/deleted pages whose
/// content was removed later. Entities whose page id has no CreatePage def are
/// appended after the def pages (A4 fallback).
fn paginate_by_page_id(doc: &NtbDocument, fallback_h: f32) -> Vec<NtbPage> {
    use std::collections::HashMap as Map;

    let default_h = if fallback_h > 0.0 { fallback_h } else { NTB_PAGE_H };
    let a4_dims = (NTB_PAGE_W, default_h);

    let mut order: Vec<PageId> = Vec::new();
    let mut groups: Map<PageId, NtbPage> = Map::new();
    for def in &doc.pages {
        if groups.contains_key(&def.id) {
            continue;
        }
        order.push(def.id);
        let (w, h) = if def.width > 0.0 && def.height > 0.0 {
            (def.width, def.height)
        } else {
            a4_dims
        };
        groups.insert(
            def.id,
            NtbPage {
                strokes: Vec::new(),
                shapes: Vec::new(),
                width: w,
                height: h,
                asset_hash: def.asset_hash,
                pdf_page: def.pdf_page,
            },
        );
    }

    for s in &doc.strokes {
        let Some(id) = s.page else { continue };
        groups.entry(id).or_insert_with(|| {
            order.push(id);
            NtbPage {
                strokes: Vec::new(),
                shapes: Vec::new(),
                width: a4_dims.0,
                height: a4_dims.1,
                asset_hash: None,
                pdf_page: 1,
            }
        });
        if let Some(p) = groups.get_mut(&id) {
            p.strokes.push(s.clone());
        }
    }
    for s in &doc.shapes {
        let Some(id) = s.page else { continue };
        groups.entry(id).or_insert_with(|| {
            order.push(id);
            NtbPage {
                strokes: Vec::new(),
                shapes: Vec::new(),
                width: a4_dims.0,
                height: a4_dims.1,
                asset_hash: None,
                pdf_page: 1,
            }
        });
        if let Some(p) = groups.get_mut(&id) {
            p.shapes.push(s.clone());
        }
    }
    let mut out: Vec<NtbPage> = order
        .into_iter()
        .filter_map(|id| groups.remove(&id))
        .filter(|p| !p.strokes.is_empty() || !p.shapes.is_empty())
        .collect();
    if out.is_empty() {
        out.push(NtbPage {
            strokes: Vec::new(),
            shapes: Vec::new(),
            width: a4_dims.0,
            height: a4_dims.1,
            asset_hash: None,
            pdf_page: 1,
        });
    }
    out
}

/// Y anchor of a shape for pagination (top of the bounding box).
fn shape_anchor_y(s: &NtbShape) -> f32 {
    match &s.geom {
        NtbShapeGeom::Line { y1, .. } => *y1,
        NtbShapeGeom::Box { y, .. } => *y,
        NtbShapeGeom::Polygon { points } => points.first().map(|p| p[1]).unwrap_or(0.0),
    }
}

// ---------------------------------------------------------------------------
// Public helpers for external callers (e.g. the convert pipeline)
// ---------------------------------------------------------------------------

/// Default page width/height used for pagination: A4 portrait at 72 dpi
/// (matches the PDF exports of all 9 samples).
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

/// Convert an `.ntb` document into a list of portable [`Page`] objects.
///
/// Pagination groups entities by their Create-op page SeqId (falling back to
/// Y-slicing when page ids are absent); each page uses its CreatePage
/// dimensions (or A4). Strokes are emitted with their decoded cubic-Bézier
/// paths (sampled to polylines). A 1-record stroke (moveto only) degenerates
/// to a zero-length mark at its anchor. Stroke colours come from the element
/// colour field (`0xAABBGGRR`) and default to opaque black.
///
/// Shapes map their geometry onto the portable IR: Lines become 2-point
/// strokes (or sampled cubics when control points are present); NormalShape
/// boxes become ellipses or rectangles at the top-left origin; Polygons
/// become polygon shapes. Fill colours (when present) are applied.
pub fn to_pages(doc: &NtbDocument, page_h: f32) -> Vec<Page> {
    let page_h = if page_h > 0.0 { page_h } else { NTB_PAGE_H };
    paginate(doc, page_h)
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
            let mut page = Page {
                uuid: format!("ntb-p{i:04}"),
                items,
                width: np.width,
                height: np.height,
            };
            // Tag the page UUID with the asset hash so `load_ntb_pages` can
            // wire `page_cfg` without re-paginating. Format:
            // `ntb-p{i:04}` normally; when a background PDF is present,
            // `ntb-p{i:04}#<hexhash>:<pdf_page>`.
            if let Some(hash) = np.asset_hash {
                let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
                page.uuid = format!("ntb-p{i:04}#{hex}:{}", np.pdf_page);
            }
            page
        })
        .collect()
}

const BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
const TRANSPARENT: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

/// Map an [`NtbShape`] onto the portable IR.
fn shape_to_item(s: &NtbShape) -> Item {
    let stroke_rgba = s.color.map(rgba_from_u32).unwrap_or(BLACK);
    let fill_rgba = s.fill.map(rgba_from_u32).unwrap_or(TRANSPARENT);
    let width = if s.border_width.is_finite() && s.border_width > 0.0 {
        s.border_width
    } else {
        1.0
    };
    match &s.geom {
        NtbShapeGeom::Line { x1, y1, x2, y2, c1, c2 } => {
            // Cubic when both controls present; straight otherwise.
            let points = match (c1, c2) {
                (Some(c1), Some(c2)) => sample_cubic([*x1, *y1], *c1, *c2, [*x2, *y2]),
                _ => vec![[*x1, *y1], [*x2, *y2]],
            };
            Item::Stroke(Stroke {
                points,
                width,
                rgba: stroke_rgba,
                dash: None,
                parented: false,
            })
        }
        NtbShapeGeom::Box { x, y, w, h, ellipse } => {
            let (w, h) = (w.abs(), h.abs());
            // IR Ellipse convention is centre + radii (matches GoodNotes
            // decoder); Box geometry is top-left + full size.
            let (sx, sy, sw, sh) = if *ellipse {
                (x + w * 0.5, y + h * 0.5, w * 0.5, h * 0.5)
            } else {
                (*x, *y, w, h)
            };
            Item::Shape(ShapeEl {
                kind: if *ellipse { ShapeKind::Ellipse } else { ShapeKind::Rect },
                x: sx,
                y: sy,
                w: sw,
                h: sh,
                points: Vec::new(),
                rotation: s.rotation,
                fill: fill_rgba,
                stroke: stroke_rgba,
                width,
                dash: None,
                radius: 0.0,
                uuid: None,
                from_f9: false,
            })
        }
        NtbShapeGeom::Polygon { points } => Item::Shape(ShapeEl {
            kind: ShapeKind::Polygon,
            x: points.first().map(|p| p[0]).unwrap_or(0.0),
            y: points.first().map(|p| p[1]).unwrap_or(0.0),
            w: 0.0,
            h: 0.0,
            points: points.clone(),
            rotation: s.rotation,
            fill: fill_rgba,
            stroke: stroke_rgba,
            width,
            dash: None,
            radius: 0.0,
            uuid: None,
            from_f9: false,
        }),
    }
}

/// Sample a cubic Bézier into a polyline (same step count as stroke paths).
fn sample_cubic(p0: [f32; 2], c1: [f32; 2], c2: [f32; 2], p3: [f32; 2]) -> Vec<[f32; 2]> {
    let mut pts = Vec::with_capacity(BEZIER_SAMPLES + 1);
    pts.push(p0);
    for i in 1..=BEZIER_SAMPLES {
        let t = i as f32 / BEZIER_SAMPLES as f32;
        let u = 1.0 - t;
        let (ur, ut) = (u * u, t * t);
        let x = u * ur * p0[0] + 3.0 * ur * t * c1[0] + 3.0 * u * ut * c2[0] + t * ut * p3[0];
        let y = u * ur * p0[1] + 3.0 * ur * t * c1[1] + 3.0 * u * ut * c2[1] + t * ut * p3[1];
        pts.push([x, y]);
    }
    pts
}

// ---------------------------------------------------------------------------
// Tests (pure helpers + synthetic fold fixtures)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_roundtrip_simple() {
        // 1.0 = 0x3C00
        assert!((f16_to_f32(0x3C00) - 1.0).abs() < 1e-6);
        // 0.0
        assert_eq!(f16_to_f32(0x0000), 0.0);
        // -2.0 = 0xC000
        assert!((f16_to_f32(0xC000) + 2.0).abs() < 1e-6);
        // infinity
        assert!(f16_to_f32(0x7C00).is_infinite());
        // NaN
        assert!(f16_to_f32(0x7E00).is_nan());
    }

    #[test]
    fn rgba_layout() {
        // 0xAABBGGRR with LE bytes R,G,B,A.
        let [r, g, b, a] = rgba_from_u32(0xFF1515B5);
        assert_eq!((r * 255.0) as u8, 0xB5);
        assert_eq!((g * 255.0) as u8, 0x15);
        assert_eq!((b * 255.0) as u8, 0x15);
        assert_eq!((a * 255.0) as u8, 0xFF);
    }

    #[test]
    fn trailer_scan_counts_records() {
        // Two 19-byte records with valid trailers.
        let mut blob = vec![0u8; 38];
        blob[14..19].copy_from_slice(&POINT_TRAILER);
        blob[33..38].copy_from_slice(&POINT_TRAILER);
        assert_eq!(scan_trailers(&blob, 0), 2);
        // Broken trailer at record 1 → only 1.
        blob[33] = 1;
        assert_eq!(scan_trailers(&blob, 0), 1);
        // Shifted header: valid records start at offset 3.
        let mut shifted = vec![0u8; 3 + 19];
        shifted[3 + 14..3 + 19].copy_from_slice(&POINT_TRAILER);
        assert_eq!(scan_trailers(&shifted, 0), 0);
        assert_eq!(scan_trailers(&shifted, 3), 1);
    }

    #[test]
    fn decode_recovers_shifted_header() {
        // One record with a 3-byte shifted header: the aligned scan fails,
        // the fallback finds it.
        let mut blob = vec![0xAAu8; 3 + 19];
        // Trailer at the shifted record's +14.
        blob[3 + 14..3 + 19].copy_from_slice(&POINT_TRAILER);
        // End point (record 0 bytes 8..12) = (0, 0) halves.
        // (already zero)
        let (count, points) = decode_stroke_points(&blob, 0, 10.0, 20.0);
        assert_eq!(count, 1);
        assert_eq!(points, vec![[10.0, 20.0]]);
    }

    #[test]
    fn shape_line_absolute_points() {
        // Synthetic Line definition: origin (10, 20), start (0, 0), end (5, 5).
        // Build a minimal table by hand is complex; instead test the geometry
        // mapping indirectly via shape_to_item on a constructed NtbShape.
        let s = NtbShape {
            page: None,
            geom: NtbShapeGeom::Line {
                x1: 10.0,
                y1: 20.0,
                x2: 15.0,
                y2: 25.0,
                c1: None,
                c2: None,
            },
            rotation: 0.0,
            color: Some(0xFF000000),
            fill: None,
            border_width: 1.0,
        };
        let item = shape_to_item(&s);
        match item {
            Item::Stroke(st) => {
                assert_eq!(st.points, vec![[10.0, 20.0], [15.0, 25.0]]);
            }
            other => panic!("expected stroke, got {other:?}"),
        }
    }

    #[test]
    fn shape_box_becomes_ellipse() {
        let s = NtbShape {
            page: None,
            geom: NtbShapeGeom::Box {
                x: 10.0,
                y: 20.0,
                w: 30.0,
                h: 40.0,
                ellipse: true,
            },
            rotation: 0.0,
            color: Some(0xFF000000),
            fill: Some(0xFFFFFFFF),
            border_width: 2.0,
        };
        let item = shape_to_item(&s);
        match item {
            Item::Shape(sh) => {
                assert_eq!(sh.kind, ShapeKind::Ellipse);
                // IR Ellipse is centre + radii: box (10,20,30x40) → c=(25,40) r=(15,20).
                assert_eq!(sh.x, 25.0);
                assert_eq!(sh.y, 40.0);
                assert_eq!(sh.w, 15.0);
                assert_eq!(sh.h, 20.0);
                let [r, _g, _b, a] = sh.fill;
                assert_eq!((r * 255.0) as u8, 0xFF);
                assert_eq!((a * 255.0) as u8, 0xFF);
            }
            other => panic!("expected shape, got {other:?}"),
        }
    }

    #[test]
    fn shift_box_origin() {
        let mut geom = NtbShapeGeom::Box { x: 1.0, y: 2.0, w: 3.0, h: 4.0, ellipse: true };
        shift_shape_origin(&mut geom, 10.0, 20.0);
        match geom {
            NtbShapeGeom::Box { x, y, .. } => {
                assert_eq!(x, 10.0);
                assert_eq!(y, 20.0);
            }
            other => panic!("expected box, got {other:?}"),
        }
    }

    #[test]
    fn paginate_splits_by_y() {
        let mk = |y: f32| NtbStroke {
            page: None,
            anchor_x: 0.0,
            anchor_y: y,
            width: 1.0,
            point_count: 1,
            points: vec![],
            color: None,
            tool: None,
            style: None,
            audio_duration_ms: None,
        };
        let doc = NtbDocument {
            meta: NtbMeta::default(),
            strokes: vec![mk(100.0), mk(900.0), mk(1700.0)],
            pages: vec![],
            texts: vec![],
            shapes: vec![],
            manifest: vec![],
            thumbnail: None,
            assets: vec![],
        };
        let pages = paginate(&doc, 841.89);
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].strokes.len(), 1);
        assert_eq!(pages[1].strokes.len(), 1);
        assert_eq!(pages[2].strokes.len(), 1);
    }

    #[test]
    fn paginate_groups_by_page_id() {
        let mk = |page: PageId, y: f32| NtbStroke {
            page: Some(page),
            anchor_x: 0.0,
            anchor_y: y,
            width: 1.0,
            point_count: 1,
            points: vec![],
            color: None,
            tool: None,
            style: None,
            audio_duration_ms: None,
        };
        // Two pages; strokes interleaved in creation order.
        let p0: PageId = (0, 1, 0);
        let p1: PageId = (0, 2, 0);
        let doc = NtbDocument {
            meta: NtbMeta::default(),
            strokes: vec![mk(p0, 10.0), mk(p1, 20.0), mk(p0, 30.0)],
            pages: vec![
                NtbPageDef { id: p0, width: 595.28, height: 841.89, ..Default::default() },
                NtbPageDef { id: p1, width: 100.0, height: 200.0, ..Default::default() },
            ],
            texts: vec![],
            shapes: vec![],
            manifest: vec![],
            thumbnail: None,
            assets: vec![],
        };
        let pages = paginate(&doc, 841.89);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].strokes.len(), 2);
        assert_eq!(pages[1].strokes.len(), 1);
        // Dimensions from CreatePage defs.
        assert_eq!(pages[0].width, 595.28);
        assert_eq!(pages[0].height, 841.89);
        assert_eq!(pages[1].width, 100.0);
        assert_eq!(pages[1].height, 200.0);
    }

    #[test]
    fn upsert_page_merges_bare_and_dims() {
        let mut pages = Vec::new();
        let p0: PageId = (0, 1, 0);
        let p1: PageId = (0, 2, 0);
        // Bare create first, then dims update; another page stays bare.
        upsert_page(&mut pages, NtbPageDef { id: p0, ..Default::default() });
        upsert_page(&mut pages, NtbPageDef { id: p1, ..Default::default() });
        upsert_page(&mut pages, NtbPageDef { id: p0, width: 842.0, height: 595.0, ..Default::default() });
        // A later bare re-create must not clear recorded dims.
        upsert_page(&mut pages, NtbPageDef { id: p0, ..Default::default() });
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].id, p0);
        assert_eq!(pages[0].width, 842.0);
        assert_eq!(pages[0].height, 595.0);
        assert_eq!(pages[1].id, p1);
        assert_eq!(pages[1].width, 0.0);
    }

    #[test]
    fn paginate_filters_empty_pages_keeps_ink_order() {
        let mk = |page: PageId| NtbStroke {
            page: Some(page),
            anchor_x: 0.0,
            anchor_y: 10.0,
            width: 1.0,
            point_count: 1,
            points: vec![],
            color: None,
            tool: None,
            style: None,
            audio_duration_ms: None,
        };
        let p0: PageId = (0, 1, 0);
        let p1: PageId = (0, 2, 0); // bare page, no ink — filtered out
        let p2: PageId = (0, 3, 0);
        let doc = NtbDocument {
            meta: NtbMeta::default(),
            strokes: vec![mk(p0), mk(p2)],
            pages: vec![
                NtbPageDef { id: p0, width: 595.28, height: 841.89, ..Default::default() },
                NtbPageDef { id: p1, ..Default::default() },
                NtbPageDef { id: p2, width: 842.0, height: 595.0, ..Default::default() },
            ],
            texts: vec![],
            shapes: vec![],
            manifest: vec![],
            thumbnail: None,
            assets: vec![],
        };
        let pages = paginate(&doc, 841.89);
        // Empty p1 filtered; CreatePage order preserved for ink pages.
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].strokes.len(), 1);
        assert_eq!(pages[0].width, 595.28);
        assert_eq!(pages[1].strokes.len(), 1);
        assert_eq!(pages[1].width, 842.0);
        assert_eq!(pages[1].height, 595.0);
    }
}
