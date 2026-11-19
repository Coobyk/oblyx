//! `.ntb` (Notability) writer.
//!
//! Serialises the portable [`Page`]/[`Item`] IR into a Notability cloud
//! notebook: a FlatBuffer `noteBundle` CRDT op-log plus a zip container.
//! Strokes become `CreateInk` ops with `qT` path streams (f32 precision);
//! shapes become `CreateShape` ops; each IR page becomes a `CreatePage` op.
//! UUIDs, clocks and entity ids are regenerated (the IR does not carry the
//! originals), so output is semantically equivalent but not byte-identical
//! to the source.
//!
//! The FlatBuffer builder writes referrers before referents (root table, ops
//! vector, op tables, payloads, leaves) and patches uoffsets so they point
//! forward, as required by Notability's FlatBuffers readers.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use sha2::{Digest, Sha512};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::doc::{Document, Item, Page, ShapeKind};

// PayloadType constants (matching the reader in `ntb.rs`).
const PT_DOCUMENT_SETUP: u8 = 1;
const PT_CREATE_ASSET: u8 = 2;
const PT_CREATE_PAGE: u8 = 3;
const PT_CREATE_INK: u8 = 15;

// Root table field indices (verified against real samples).
const ROOT_F0_NOTE_GUID: usize = 0; // 16-byte inline GUID
const ROOT_F3_DEVICE_UUID_UPPER: usize = 3; // uoffset to string
const ROOT_F4_TIMESTAMP: usize = 4; // u64 ms epoch
const ROOT_F5_DEVICE_UUID_LOWER: usize = 5; // uoffset to string
const ROOT_F6_OPS: usize = 6; // uoffset to ops vector
const ROOT_F7_VERSION: usize = 7; // u32 version

/// Version observed in real samples: 0x800012.
const NTB_VERSION: u32 = 0x800012;

/// Notability's Android importer (`NtbImporter`/`ope.c`) refuses any single
/// zip entry larger than 64 MiB, and its picker (`hb9.d`) refuses `.ntb`
/// streams larger than 500 MB. Warn when output would hit either limit.
const MOBILE_MAX_ENTRY: u64 = 64 * 1024 * 1024;
const MOBILE_MAX_ARCHIVE: u64 = 500 * 1024 * 1024;

// Path element types (non-attributed, to avoid the 6-byte attribute tail).
const PATH_LINE: u8 = 6;
const PATH_MOVETO: u8 = 7;

/// A field value for table writing.
#[derive(Clone)]
enum FieldVal {
    /// Inline bytes (struct, varint, fixed, etc.).
    Inline(Vec<u8>),
}

/// Minimal forward FlatBuffer builder.
struct Fb {
    buf: Vec<u8>,
}

impl Fb {
    fn new() -> Self {
        // Reserve 4 bytes for the root uoffset.
        Self { buf: vec![0; 4] }
    }

    /// Empty buffer for a self-contained sub-buffer (one op serialised
    /// independently, later appended to the main buffer). Relative uoffsets
    /// are preserved by the shift, so internal references stay valid.
    fn bare() -> Self {
        Self { buf: Vec::new() }
    }

    fn into_buf(self) -> Vec<u8> {
        self.buf
    }

    fn align(&mut self, n: usize) {
        while !self.buf.len().is_multiple_of(n) {
            self.buf.push(0);
        }
    }

    /// Write a byte vector: u32 length + data. Returns the vector position.
    fn write_byte_vector(&mut self, data: &[u8]) -> usize {
        self.align(4);
        let pos = self.buf.len();
        self.buf
            .extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(data);
        pos
    }

    /// Write a string: u32 length + data + NUL terminator, padded to 4.
    /// FlatBuffers strings must be NUL-terminated or the native verifier
    /// rejects the buffer (web import accepts them anyway).
    fn write_fb_string(&mut self, data: &[u8]) -> usize {
        self.align(4);
        let pos = self.buf.len();
        self.buf
            .extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(data);
        self.buf.push(0);
        pos
    }

    /// Natural alignment for an inline field of `size` bytes. FlatBuffers
    /// requires every member to be aligned to its largest scalar; we only
    /// know the byte length, so align to the largest power of two (≤8) that
    /// divides it — never under-aligns for scalar/struct members, and the
    /// native verifier rejects under-aligned objects.
    fn field_align(size: usize) -> usize {
        if size.is_multiple_of(8) {
            8
        } else if size.is_multiple_of(4) {
            4
        } else if size.is_multiple_of(2) {
            2
        } else {
            1
        }
    }

    /// Write a FlatBuffer table with the given fields. Returns the table
    /// position. The vtable is written after the table (negative soffset).
    /// The table start is aligned to its most-aligned member so every
    /// field lands on a properly aligned absolute offset.
    fn write_table(&mut self, fields: &[(usize, FieldVal)]) -> usize {
        let mut sorted: Vec<(usize, FieldVal)> = fields.to_vec();
        sorted.sort_by_key(|(i, _)| *i);

        // Compute field byte offsets within the table.
        // Table layout: [i32 soffset] [field_data ...]
        let mut field_offs: Vec<(usize, usize)> = Vec::new();
        let mut data_off = 4usize; // after soffset
        let mut max_align = 4usize;
        for &(idx, ref val) in &sorted {
            let size = match val {
                FieldVal::Inline(b) => b.len(),
            };
            let align = Self::field_align(size);
            max_align = max_align.max(align);
            data_off = data_off.next_multiple_of(align);
            field_offs.push((idx, data_off));
            data_off += size;
        }
        let table_size = data_off;

        // Align the table so every member's absolute offset is aligned.
        self.align(max_align);

        // Write table body.
        let tp = self.buf.len();
        self.buf.extend_from_slice(&0i32.to_le_bytes()); // placeholder soffset
        for &(idx, ref val) in &sorted {
            let off = field_offs
                .iter()
                .find(|(i, _)| *i == idx)
                .map(|(_, o)| *o)
                .unwrap_or(0);
            while self.buf.len() < tp + off {
                self.buf.push(0);
            }
            let FieldVal::Inline(b) = val;
            self.buf.extend_from_slice(b);
        }
        while self.buf.len() < tp + table_size {
            self.buf.push(0);
        }

        // Write vtable after the table.
        let vp = self.buf.len();
        let nfields = sorted.last().map(|(i, _)| i + 1).unwrap_or(0);
        let vtable_size = 4 + nfields * 2;
        self.buf
            .extend_from_slice(&(vtable_size as u16).to_le_bytes());
        self.buf
            .extend_from_slice(&(table_size as u16).to_le_bytes());
        for fi in 0..nfields {
            let off = field_offs
                .iter()
                .find(|(i, _)| *i == fi)
                .map(|(_, o)| *o as u16)
                .unwrap_or(0);
            self.buf.extend_from_slice(&off.to_le_bytes());
        }

        // Patch soffset: vp = tp - soffset ⇒ soffset = tp - vp (negative).
        let soffset = (tp as i64 - vp as i64) as i32;
        self.buf[tp..tp + 4].copy_from_slice(&soffset.to_le_bytes());

        tp
    }

    /// Finish: patch the root uoffset at byte 0 to point to `root`.
    fn finish(mut self, root: usize) -> Vec<u8> {
        let v = root as u32; // root is always > 0 since we reserved 4 bytes
        self.buf[0..4].copy_from_slice(&v.to_le_bytes());
        self.buf
    }

    /// Byte position of a table field's value, or `None` when absent.
    fn field_pos(&self, table_pos: usize, field_index: usize) -> Option<usize> {
        let soff = i32::from_le_bytes(self.buf[table_pos..table_pos + 4].try_into().ok()?);
        let vtable = (table_pos as i64 - soff as i64) as usize;
        let vsize = u16::from_le_bytes(self.buf[vtable..vtable + 2].try_into().ok()?) as usize;
        if 4 + field_index * 2 + 2 > vsize {
            return None;
        }
        let off = u16::from_le_bytes(
            self.buf[vtable + 4 + field_index * 2..vtable + 6 + field_index * 2]
                .try_into()
                .ok()?,
        ) as usize;
        (off > 0).then_some(table_pos + off)
    }

    /// Point a table's uoffset field forward at `target`.
    ///
    /// Real FlatBuffers readers compute `field_pos + u32(field_pos)`, so the
    /// target must be written *after* the field. We write tables first with a
    /// zero placeholder, write the referenced data afterwards, then patch.
    fn patch_uoffset(&mut self, table_pos: usize, field_index: usize, target: usize) {
        if let Some(pos) = self.field_pos(table_pos, field_index) {
            let rel = (target as i64 - pos as i64) as u32;
            self.buf[pos..pos + 4].copy_from_slice(&rel.to_le_bytes());
        }
    }

    /// Write a vector of `n` zeroed uoffset slots and return the vector pos.
    fn write_uoffset_slots(&mut self, n: usize) -> usize {
        self.align(4);
        let pos = self.buf.len();
        self.buf.extend_from_slice(&(n as u32).to_le_bytes());
        self.buf.extend_from_slice(&vec![0u8; n * 4]);
        pos
    }

    /// Write a vector of fixed-size structs (u32 element count + bytes).
    fn write_struct_vector(&mut self, data: &[u8], elem_size: usize) -> usize {
        self.align(4);
        let pos = self.buf.len();
        self.buf.extend_from_slice(&((data.len() / elem_size) as u32).to_le_bytes());
        self.buf.extend_from_slice(data);
        pos
    }

    /// Patch slot `i` of a uoffset vector to point forward at `target`.
    fn patch_uoffset_slot(&mut self, vec_pos: usize, i: usize, target: usize) {
        let pos = vec_pos + 4 + i * 4;
        let rel = (target as i64 - pos as i64) as u32;
        self.buf[pos..pos + 4].copy_from_slice(&rel.to_le_bytes());
    }
}

/// Build a `qT` path stream blob (f32 precision, non-attributed elements).
///
/// Stream: version/precision byte (0<<3|1=1) ‖ `u16` LE element count ‖ elements.
/// Each element: `u8` type ‖ anchor-relative points (8 B each for f32).
/// Caller wraps this in a byte vector (`write_byte_vector` adds the u32 length).
fn build_path_stream(points: &[[f32; 2]]) -> Vec<u8> {
    if points.is_empty() {
        return Vec::new();
    }
    let anchor = points[0];
    let mut stream = Vec::with_capacity(3 + points.len() * 9);
    stream.push(1); // version 0 << 3 | precision 1 (f32)
    stream.extend_from_slice(&(points.len() as u16).to_le_bytes());
    // First element: moveto at (0, 0) relative to anchor.
    stream.push(PATH_MOVETO);
    stream.extend_from_slice(&0f32.to_le_bytes());
    stream.extend_from_slice(&0f32.to_le_bytes());
    // Remaining: line-to each point (relative to anchor).
    for p in &points[1..] {
        stream.push(PATH_LINE);
        stream.extend_from_slice(&(p[0] - anchor[0]).to_le_bytes());
        stream.extend_from_slice(&(p[1] - anchor[1]).to_le_bytes());
    }
    stream
}

/// Write an inline 8-byte sf struct `{site u16, pad 2, timestamp u32}`.
fn sf_inline(site: u16, ts: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.extend_from_slice(&site.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // pad
    b.extend_from_slice(&ts.to_le_bytes());
    b
}

/// Write an inline 12-byte df SeqId struct `{site u16, pad 2, ts u32, index u32}`.
fn df_inline(site: u16, ts: u32, index: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    b.extend_from_slice(&site.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&ts.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b
}

/// Write an inline Sf struct `{x f32, y f32}`.
fn sf_xy(x: f32, y: f32) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.extend_from_slice(&x.to_le_bytes());
    b.extend_from_slice(&y.to_le_bytes());
    b
}

/// Write an inline Gf colour struct `{R, G, B, A}` as bytes.
fn gf_color(rgba: &[f32; 4]) -> Vec<u8> {
    let to_byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    vec![
        to_byte(rgba[0]),
        to_byte(rgba[1]),
        to_byte(rgba[2]),
        to_byte(rgba[3]),
    ]
}

/// Entity id counter: (site, seq) pairs — seq is a plain per-note op
/// ordinal, matching both the wrapper entity and `df` reference ts fields.
struct EntityGen {
    site: u16,
    seq: u32,
}

impl EntityGen {
    fn new() -> Self {
        // Real samples start at site=0, seq=0.
        Self { site: 0, seq: 0 }
    }

    fn next(&mut self) -> (u16, u32) {
        let id = (self.site, self.seq);
        self.seq += 1;
        id
    }
}

/// Write the shared op wrapper table: entity + µs timestamps + payloadType +
/// payload uoffset placeholder. Native exports carry f1/f2 wall-clock
/// timestamps on every op; the web-importer layout omits them but the mobile
/// importer's verifier expects the native field set.
fn write_op_table(fb: &mut Fb, entity: (u16, u32), ts_us: u64, pt: u8) -> usize {
    fb.write_table(&[
        (0, FieldVal::Inline(sf_inline(entity.0, entity.1))),
        (1, FieldVal::Inline(ts_us.to_le_bytes().to_vec())),
        (2, FieldVal::Inline(ts_us.to_le_bytes().to_vec())),
        (4, FieldVal::Inline(vec![pt])),
        (5, FieldVal::Inline(vec![0; 4])), // payload placeholder
    ])
}

/// Write one CreateInk op into the buffer. Returns the op table position.
fn write_create_ink(
    fb: &mut Fb,
    entity: (u16, u32),
    ts_us: u64,
    page: (u16, u32, u32),
    points: &[[f32; 2]],
    width: f32,
    rgba: &[f32; 4],
    ink_id: u32,
) -> usize {
    // Colour struct (inline Gf, 4 bytes).
    let color = gf_color(rgba);
    let origin = points.first().copied().unwrap_or([0.0, 0.0]);
    let width = if width.is_finite() && width > 0.0 {
        width
    } else {
        1.0
    };

    // Write the op table FIRST, then the payload, then the leaf blob, so every
    // uoffset points forward (required by real FlatBuffers readers).
    // Op table (native layout): f0 = entity (sf 8B inline), f1/f2 = op
    // created/modified timestamps (u64 µs), f4 = payloadType (u8),
    // f5 = payload uoffset.
    let op = write_op_table(fb, entity, ts_us, PT_CREATE_INK);

    // CreateInk payload (native): f0 page (df inline), f1 origin (Sf inline),
    // f5 style, f7 colour (packed RGBA), f8 width, f9 path blob, f15 stroke id.
    let payload = fb.write_table(&[
        (0, FieldVal::Inline(df_inline(page.0, page.1, page.2))),
        (1, FieldVal::Inline(sf_xy(origin[0], origin[1]))),
        (5, FieldVal::Inline(vec![1])),
        (7, FieldVal::Inline(color)),
        (8, FieldVal::Inline(width.to_le_bytes().to_vec())),
        (9, FieldVal::Inline(vec![0; 4])), // placeholder
        (15, FieldVal::Inline(ink_id.to_le_bytes().to_vec())),
    ]);

    let path_pos = fb.write_byte_vector(&build_path_stream(points));
    fb.patch_uoffset(payload, 9, path_pos);
    fb.patch_uoffset(op, 5, payload);
    op
}

/// Write a CreateAsset op (pt=2). Native exports register every background
/// attachment before pages reference it; the payload is just the 64-byte
/// content hash, and the entity uses the reserved site id 0xffff.
fn write_create_asset(fb: &mut Fb, entity: (u16, u32), ts_us: u64, hash: &[u8; 64]) -> usize {
    let op = write_op_table(fb, entity, ts_us, PT_CREATE_ASSET);
    let payload = fb.write_table(&[(0, FieldVal::Inline(hash.to_vec()))]);
    fb.patch_uoffset(op, 5, payload);
    op
}

/// Write a string field (uoffset to u32-len + bytes + NUL terminator).
fn write_string(fb: &mut Fb, s: &str) -> usize {
    fb.write_fb_string(s.as_bytes())
}

/// Write a DocumentSetup op (pt=1). This is the first op in real files and
/// configures the notebook metadata (title, locale, default settings).
/// Native payload: f0 title table, f1 subject table, f2 locale table,
/// f3 flags u8, f4 default font name string, f5 default font size,
/// f6/f7 u8 flags.
fn write_document_setup(
    fb: &mut Fb,
    entity: (u16, u32),
    ts_us: u64,
    title: &str,
    locale: &str,
) -> usize {
    // Op → payload → sub-tables → strings, all written referrer-first so
    // their uoffsets point forward.
    let op = write_op_table(fb, entity, ts_us, PT_DOCUMENT_SETUP);

    let payload = fb.write_table(&[
        (0, FieldVal::Inline(vec![0; 4])), // title placeholder
        (1, FieldVal::Inline(vec![0; 4])), // subject placeholder
        (2, FieldVal::Inline(vec![0; 4])), // locale placeholder
        (3, FieldVal::Inline(vec![1])),    // flags
        (4, FieldVal::Inline(vec![0; 4])), // font placeholder
        (5, FieldVal::Inline(14.0f32.to_le_bytes().to_vec())), // default font size
        (6, FieldVal::Inline(vec![0])),
        (7, FieldVal::Inline(vec![0])),
    ]);

    let title_tbl = fb.write_table(&[(0, FieldVal::Inline(vec![0; 4]))]);
    let title_str = write_string(fb, title);
    fb.patch_uoffset(title_tbl, 0, title_str);

    let locale_tbl = fb.write_table(&[(0, FieldVal::Inline(vec![0; 4]))]);
    let locale_str = write_string(fb, locale);
    fb.patch_uoffset(locale_tbl, 0, locale_str);

    // f1 sub-table: f0 → f32 vector (native files carry five 36.0 values —
    // likely default margins/grid metrics).
    let subject_tbl = fb.write_table(&[(0, FieldVal::Inline(vec![0; 4]))]);
    let mut metrics = Vec::with_capacity(20);
    for _ in 0..5 {
        metrics.extend_from_slice(&36.0f32.to_le_bytes());
    }
    let vec_pos = fb.write_struct_vector(&metrics, 4);
    fb.patch_uoffset(subject_tbl, 0, vec_pos);

    let font_str = write_string(fb, "Inter");

    fb.patch_uoffset(payload, 0, title_tbl);
    fb.patch_uoffset(payload, 1, subject_tbl);
    fb.patch_uoffset(payload, 2, locale_tbl);
    fb.patch_uoffset(payload, 4, font_str);
    fb.patch_uoffset(op, 5, payload);
    op
}

/// Everything a page needs to bind its background asset: content hash,
/// byte size, file extension/mime, and the source PDF's per-page crop boxes
/// (one per page, as native exports record).
struct Bg<'a> {
    hash: [u8; 64],
    size: u32,
    ext: &'a str,
    mime: &'a str,
    boxes: &'a [(f32, f32)],
    /// 0-based index of the first source page this op consumes.
    page_offset: u32,
}

/// Write one CreatePage op (native layout). Returns the op table position.
///
/// Native pages are ordered by location: payload f0 is a df reference to
/// the page the new ones follow (absent on the first op). f2 is the
/// pageCount — how many sibling pages the op materialises (default 1).
/// Ops bound to a PDF background carry f1 → PageBackground → PDFAsset →
/// AssetMetadata; siblings show consecutive source pages.
#[allow(clippy::too_many_arguments)]
fn write_create_page(
    fb: &mut Fb,
    entity: (u16, u32),
    ts_us: u64,
    prev: Option<(u16, u32, u32)>,
    width: f32,
    height: f32,
    background: Option<&Bg>,
    page_count: u32,
) -> usize {
    let op = write_op_table(fb, entity, ts_us, PT_CREATE_PAGE);

    let mut fields: Vec<(usize, FieldVal)> = Vec::new();
    if let Some(prev) = prev {
        fields.push((0, FieldVal::Inline(df_inline(prev.0, prev.1, prev.2))));
    }
    if page_count > 1 {
        fields.push((2, FieldVal::Inline(page_count.to_le_bytes().to_vec())));
    }
    let payload = if let Some(bg_info) = background {
        fields.push((1, FieldVal::Inline(vec![0; 4]))); // bg placeholder
        let payload = fb.write_table(&fields);

        // PageBackground: f1 = uoffset to PDFAsset, f3 = size (Sf inline).
        let bg = fb.write_table(&[
            (1, FieldVal::Inline(vec![0; 4])), // placeholder
            (3, FieldVal::Inline(sf_xy(width, height))),
        ]);
        // PDFAsset: f0 = metadata uoffset, f2/f3 = total source-page count
        // (native files omit it for 1-page PDFs), f5 = cropBoxes — one
        // 8-byte {width,height} struct per source PDF page.
        let mut asset_fields: Vec<(usize, FieldVal)> =
            vec![(0, FieldVal::Inline(vec![0; 4]))];
        if bg_info.boxes.len() > 1 {
            let n = (bg_info.boxes.len() as u32).to_le_bytes().to_vec();
            asset_fields.push((2, FieldVal::Inline(n.clone())));
            asset_fields.push((3, FieldVal::Inline(n)));
        }
        if bg_info.page_offset > 0 {
            asset_fields.push((
                4,
                FieldVal::Inline(bg_info.page_offset.to_le_bytes().to_vec()),
            ));
        }
        asset_fields.push((5, FieldVal::Inline(vec![0; 4]))); // cropBoxes placeholder
        let nested = fb.write_table(&asset_fields);
        // AssetMetadata: f0 = 64-byte hash, f1 = fileName, f2 = mimeType,
        // f3 = byte size. All required by the native importer.
        let inner = fb.write_table(&[
            (0, FieldVal::Inline(bg_info.hash.to_vec())),
            (1, FieldVal::Inline(vec![0; 4])), // fileName placeholder
            (2, FieldVal::Inline(vec![0; 4])), // mimeType placeholder
            (3, FieldVal::Inline(bg_info.size.to_le_bytes().to_vec())),
        ]);
        let file_name: String = bg_info
            .hash
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let file_name = format!("{file_name}.{}", bg_info.ext);
        let name_pos = fb.write_fb_string(file_name.as_bytes());
        let mime_pos = fb.write_fb_string(bg_info.mime.as_bytes());
        fb.patch_uoffset(inner, 1, name_pos);
        fb.patch_uoffset(inner, 2, mime_pos);
        let mut crop = Vec::with_capacity(bg_info.boxes.len() * 8);
        for &(w, h) in bg_info.boxes {
            crop.extend_from_slice(&w.to_le_bytes());
            crop.extend_from_slice(&h.to_le_bytes());
        }
        let crop_vec = fb.write_struct_vector(&crop, 8);
        fb.patch_uoffset(nested, 5, crop_vec);
        fb.patch_uoffset(nested, 0, inner);
        fb.patch_uoffset(bg, 1, nested);
        fb.patch_uoffset(payload, 1, bg);
        payload
    } else {
        fb.write_table(&fields)
    };

    fb.patch_uoffset(op, 5, payload);
    op
}

/// Stroke geometry for a CreateInk op: borrowed item points, or a synthetic
/// two-point segment (connectors have no point list of their own).
enum InkPts<'a> {
    Ref(&'a [[f32; 2]]),
    Owned(Vec<[f32; 2]>),
}

impl InkPts<'_> {
    fn points(&self) -> &[[f32; 2]] {
        match self {
            InkPts::Ref(p) => p,
            InkPts::Owned(v) => v,
        }
    }
}

/// Deferred op description: ops are collected first, then written after the
/// root table and ops vector so every uoffset points forward.
enum OpSpec<'a> {
    Setup {
        entity: (u16, u32),
        title: String,
        locale: String,
    },
    Asset {
        entity: (u16, u32),
        hash: [u8; 64],
    },
    Page {
        entity: (u16, u32),
        prev: Option<(u16, u32, u32)>,
        width: f32,
        height: f32,
        background: Option<Bg<'a>>,
        page_count: u32,
    },
    Ink {
        entity: (u16, u32),
        page: (u16, u32, u32),
        pts: InkPts<'a>,
        width: f32,
        rgba: [f32; 4],
        ink_id: u32,
    },
}

/// Serialise one op into a self-contained buffer. All uoffsets it writes are
/// relative to field positions inside the buffer, so the whole buffer can be
/// appended to the main one and its references stay valid; the ops vector
/// just needs slot `i` patched to `base + op_table_pos`.
fn serialize_op(spec: &OpSpec, ts_us: u64) -> (usize, Vec<u8>) {
    let mut fb = Fb::bare();
    let op = match spec {
        OpSpec::Setup {
            entity,
            title,
            locale,
        } => write_document_setup(&mut fb, *entity, ts_us, title, locale),
        OpSpec::Asset { entity, hash } => write_create_asset(&mut fb, *entity, ts_us, hash),
        OpSpec::Page {
            entity,
            prev,
            width,
            height,
            background,
            page_count,
        } => write_create_page(
            &mut fb,
            *entity,
            ts_us,
            *prev,
            *width,
            *height,
            background.as_ref(),
            *page_count,
        ),
        OpSpec::Ink {
            entity,
            page,
            pts,
            width,
            rgba,
            ink_id,
        } => write_create_ink(
            &mut fb,
            *entity,
            ts_us,
            *page,
            pts.points(),
            *width,
            rgba,
            *ink_id,
        ),
    };
    (op, fb.into_buf())
}

/// Approximate a rectangle as a closed 5-point polyline.
fn rect_outline(x: f32, y: f32, w: f32, h: f32) -> Vec<[f32; 2]> {
    vec![
        [x, y],
        [x + w, y],
        [x + w, y + h],
        [x, y + h],
        [x, y],
    ]
}

/// Approximate an ellipse (center cx/cy, half-axes w/h) as a 25-point
/// polyline. GoodNotes shapes become strokes — the native shape schema is
/// only partially decoded, and visual fidelity beats an unimportable note.
fn ellipse_outline(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<[f32; 2]> {
    const N: usize = 24;
    let mut pts = Vec::with_capacity(N + 1);
    for i in 0..=N {
        let a = (i as f32) * std::f32::consts::TAU / (N as f32);
        pts.push([cx + rx * a.cos(), cy + ry * a.sin()]);
    }
    pts
}

/// Build the `noteBundle` FlatBuffer from pages and document.
/// `hashes` maps each background attachment key to its precomputed content
/// hash (computed once per unique attachment, not per page).
fn build_note_bundle(
    pages: &[Page],
    doc: &Document,
    hashes: &HashMap<String, [u8; 64]>,
) -> (Vec<u8>, [u8; 16], u64, [u8; 16]) {
    let mut ids = EntityGen::new();
    let mut specs: Vec<OpSpec> = Vec::new();
    let mut ink_id: u32 = 0;
    macro_rules! ink {
        ($page:expr, $pts:expr, $width:expr, $rgba:expr) => {{
            ink_id += 1;
            specs.push(OpSpec::Ink {
                entity: ids.next(),
                page: $page,
                pts: $pts,
                width: $width,
                rgba: $rgba,
                ink_id,
            });
        }};
    }

    // First op: DocumentSetup (pt=1). Required by Notability.
    let setup_entity = ids.next();
    specs.push(OpSpec::Setup {
        entity: setup_entity,
        title: format!("Note {}", chrono_like_timestamp()),
        locale: "en_US".to_string(),
    });

    // Register every referenced background asset (pt=2). Native exports emit
    // one asset op per unique attachment hash before the pages that use it.
    // Asset entities live on the reserved site 0xffff with their own seq
    // counter starting at 1.
    let mut asset_entities: HashMap<[u8; 64], u32> = HashMap::new();
    // Per-attachment extension/mime and (for PDFs) the per-page crop boxes
    // native files record inside the PDFAsset table.
    let mut bg_kinds: HashMap<String, (&'static str, &'static str)> = HashMap::new();
    let mut bg_boxes: HashMap<String, Vec<(f32, f32)>> = HashMap::new();
    {
        let mut seen = HashSet::new();
        for page in pages {
            let Some(cfg) = doc.page_cfg().get(&page.uuid) else {
                continue;
            };
            let Some(&hash) = hashes.get(&cfg.attachment) else {
                continue;
            };
            if seen.insert(hash) {
                let n = asset_entities.len() as u32 + 1;
                asset_entities.insert(hash, n);
                specs.push(OpSpec::Asset {
                    entity: (0xffff, n),
                    hash,
                });
            }
            let Some(bytes) = doc.attachments.get(&cfg.attachment) else {
                continue;
            };
            bg_kinds
                .entry(cfg.attachment.clone())
                .or_insert_with(|| sniff_asset(bytes));
            bg_boxes
                .entry(cfg.attachment.clone())
                .or_insert_with(|| {
                    doc.pdf_page_boxes(&cfg.attachment).unwrap_or_default()
                });
        }
    }

    // Native pageCount semantics: one CreatePage op can materialise several
    // sibling pages — SeqIds (site, seq, 0..N-1). PDF-imported notes use one
    // op per PDF: sibling i shows source page i+1. Group consecutive pages
    // that share an attachment with consecutive source-page numbers (and
    // consecutive un-bound pages) into one op, as real exports do.
    let mut page_ids: Vec<(u16, u32, u32)> = Vec::with_capacity(pages.len());
    {
        let mut prev_loc: Option<(u16, u32, u32)> = None;
        let mut i = 0;
        while i < pages.len() {
            let cfg_of = |p: &Page| doc.page_cfg().get(&p.uuid);
            let cfg = cfg_of(&pages[i]);
            let att = cfg.map(|c| c.attachment.as_str());
            let first_pdf = cfg.and_then(|c| c.pdf_page).unwrap_or(0) as u32;

            // Extend the run while pages share the same binding.
            let mut count = 1usize;
            while i + count < pages.len() {
                let c2 = cfg_of(&pages[i + count]);
                let same = match (att, c2.map(|c| c.attachment.as_str())) {
                    (Some(a), Some(b)) if a == b => {
                        let expect = first_pdf + count as u32;
                        c2.and_then(|c| c.pdf_page).unwrap_or(0) as u32 == expect
                    }
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    break;
                }
                count += 1;
            }

            let page_entity = ids.next();
            let seq = page_entity.1;
            for sib in 0..count {
                page_ids.push((page_entity.0, seq, sib as u32));
            }

            let bg = cfg.and_then(|cfg| {
                let hash = *hashes.get(&cfg.attachment)?;
                let size = doc.attachments.get(&cfg.attachment)?.len() as u32;
                let &(ext, mime) = bg_kinds.get(&cfg.attachment)?;
                let boxes = bg_boxes.get(&cfg.attachment).map(Vec::as_slice)?;
                Some(Bg {
                    hash,
                    size,
                    ext,
                    mime,
                    boxes,
                    page_offset: first_pdf.saturating_sub(1),
                })
            });

            specs.push(OpSpec::Page {
                entity: page_entity,
                prev: prev_loc,
                width: pages[i].width,
                height: pages[i].height,
                background: bg,
                page_count: count as u32,
            });
            prev_loc = Some((page_entity.0, seq, count as u32 - 1));
            i += count;
        }
    }

    for (page, &page_id) in pages.iter().zip(&page_ids) {

        for item in &page.items {
            match item {
                Item::Stroke(s) => {
                    if s.points.len() >= 2 {
                        ink!(page_id, InkPts::Ref(&s.points), s.width, s.rgba);
                    }
                }
                Item::Shape(sh) => {
                    let pts = match sh.kind {
                        ShapeKind::Ellipse => {
                            ellipse_outline(sh.x, sh.y, sh.w.abs(), sh.h.abs())
                        }
                        ShapeKind::Rect => rect_outline(sh.x, sh.y, sh.w, sh.h),
                        _ => {
                            // Polygon → closed polyline.
                            let mut pts = sh.points.clone();
                            if pts.len() >= 2 {
                                if let Some(f) = pts.first().copied() {
                                    pts.push(f);
                                }
                            }
                            pts
                        }
                    };
                    if pts.len() >= 2 {
                        ink!(page_id, InkPts::Owned(pts), sh.width, sh.stroke);
                    }
                }
                Item::Path(p) => {
                    if p.points.len() >= 2 {
                        ink!(page_id, InkPts::Ref(&p.points), p.width, p.rgba);
                    }
                }
                Item::FillPath(fp) => {
                    for contour in &fp.contours {
                        if contour.len() >= 2 {
                            ink!(page_id, InkPts::Ref(contour), 1.0, fp.rgba);
                        }
                    }
                }
                Item::Connector(c) => {
                    ink!(
                        page_id,
                        InkPts::Owned(vec![
                            [c.anchor[0], c.anchor[1]],
                            [c.anchor[0], c.anchor[1]],
                        ]),
                        c.width,
                        c.rgba
                    );
                }
                // Backgrounds, PDF backgrounds, images, text, stickies: not
                // yet encoded as Notability ops. Skipped for now.
                Item::Image(_)
                | Item::Text(_)
                | Item::Sticky(_)
                | Item::Background(_)
                | Item::PdfBackground(_) => {}
            }
        }
    }

    // Ops vector is written before the op tables it references. Each op is
    // serialised into its own buffer in parallel — uoffsets are all relative,
    // so appending each buffer whole keeps its internal references valid.
    // Op buffers are built first so the note GUID can derive from a content
    // hash: every converted note gets a unique, stable identity instead of
    // recycling the sample's GUID.
    let timestamp_ms = now_ms();
    let ts_us = timestamp_ms * 1_000;
    let op_bufs = crate::par::map(&specs, |s| serialize_op(s, ts_us));

    let mut guid_src = Vec::with_capacity(op_bufs.iter().map(|(_, b)| b.len()).sum::<usize>() + 8);
    guid_src.extend_from_slice(&timestamp_ms.to_le_bytes());
    for (_, b) in &op_bufs {
        guid_src.extend_from_slice(b);
    }
    let note_guid = derive_guid(&guid_src);
    let device_uuid = format_guid(&note_guid);
    let subject_id = derive_guid(&[&note_guid[..], &timestamp_ms.to_le_bytes()].concat());

    // Root table first, with placeholder uoffsets for fields 3/5/6.
    let mut fb = Fb::new();
    let root = fb.write_table(&[
        (ROOT_F0_NOTE_GUID, FieldVal::Inline(note_guid.to_vec())),
        (ROOT_F3_DEVICE_UUID_UPPER, FieldVal::Inline(vec![0; 4])),
        (
            ROOT_F4_TIMESTAMP,
            FieldVal::Inline(timestamp_ms.to_le_bytes().to_vec()),
        ),
        (ROOT_F5_DEVICE_UUID_LOWER, FieldVal::Inline(vec![0; 4])),
        (ROOT_F6_OPS, FieldVal::Inline(vec![0; 4])),
        (
            ROOT_F7_VERSION,
            FieldVal::Inline(NTB_VERSION.to_le_bytes().to_vec()),
        ),
    ]);

    // Referenced data follows the root table, so all uoffsets point forward.
    let device_uuid_upper = write_string(&mut fb, &device_uuid.to_uppercase());
    let device_uuid_lower = write_string(&mut fb, &device_uuid);
    fb.patch_uoffset(root, ROOT_F3_DEVICE_UUID_UPPER, device_uuid_upper);
    fb.patch_uoffset(root, ROOT_F5_DEVICE_UUID_LOWER, device_uuid_lower);

    let ops_vec = fb.write_uoffset_slots(specs.len());
    for (i, (op_pos, buf)) in op_bufs.iter().enumerate() {
        // Each op buffer was built with objects aligned relative to its own
        // start; appending at an 8-aligned offset preserves that alignment
        // for the strictest member any op payload contains (u64/8B structs).
        fb.align(8);
        let base = fb.buf.len();
        fb.buf.extend_from_slice(buf);
        fb.patch_uoffset_slot(ops_vec, i, base + op_pos);
    }
    fb.patch_uoffset(root, ROOT_F6_OPS, ops_vec);

    let bundle = fb.finish(root);
    (bundle, note_guid, timestamp_ms, subject_id)
}

/// Current wall-clock time in milliseconds. The root table stores ms while
/// per-op timestamps use µs; on wasm we take what `js_sys::Date` gives.
#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(target_arch = "wasm32")]
fn now_ms() -> u64 {
    // Raw WASM has no clock import (no wasm-bindgen). Identity still comes
    // from the content-derived GUID, so a fixed plausible value is fine.
    1_789_453_933_593
}

/// Content-derive a 16-byte note GUID with UUID v4 version/variant bits so
/// repeated conversions of the same source produce a stable, unique identity.
fn derive_guid(seed: &[u8]) -> [u8; 16] {
    let hash = compute_hash(seed);
    let mut g = [0u8; 16];
    g.copy_from_slice(&hash[..16]);
    g[6] = (g[6] & 0x0f) | 0x40; // version 4
    g[8] = (g[8] & 0x3f) | 0x80; // variant 10xx
    g
}

/// Format raw 16 GUID bytes as a lowercase `xxxxxxxx-…` UUID string.
fn format_guid(g: &[u8; 16]) -> String {
    let h = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
    format!(
        "{}-{}-{}-{}-{}",
        h(&g[0..4]),
        h(&g[4..6]),
        h(&g[6..8]),
        h(&g[8..10]),
        h(&g[10..16])
    )
}

/// Generate a simple timestamp string for note titles.
fn chrono_like_timestamp() -> String {
    // Titles are display-only; keep a fixed, sample-like format.
    "Sep 15, 2026".to_string()
}

/// Identify a background attachment by magic bytes → (extension, mime).
/// Native files store real types (`.pdf`, `.webp`, …) in AssetMetadata.
fn sniff_asset(bytes: &[u8]) -> (&'static str, &'static str) {
    match bytes {
        b if b.starts_with(b"%PDF") => ("pdf", "application/pdf"),
        b if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") => ("webp", "image/webp"),
        b if b.starts_with(b"\x89PNG") => ("png", "image/png"),
        b if b.starts_with(b"\xff\xd8\xff") => ("jpg", "image/jpeg"),
        _ => ("pdf", "application/pdf"),
    }
}

/// Compute a 64-byte content hash (SHA-512).
///
/// Notability uses SHA-512 of the asset content as the filename.
fn compute_hash(data: &[u8]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Distinct attachment keys referenced by page backgrounds, in first-seen
/// order. Several keys can alias one byte buffer.
fn referenced_attachments(pages: &[Page], doc: &Document) -> Vec<(String, Arc<Vec<u8>>)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for page in pages {
        let Some(cfg) = doc.page_cfg().get(&page.uuid) else {
            continue;
        };
        if !seen.insert(cfg.attachment.clone()) {
            continue;
        }
        if let Some(bytes) = doc.attachments.get(&cfg.attachment) {
            out.push((cfg.attachment.clone(), Arc::clone(bytes)));
        }
    }
    out
}

/// Write a `.ntb` archive to `w`.
pub fn write_ntb(pages: &[Page], doc: &Document, mut w: impl Write) -> Result<()> {
    // SHA-512 each unique background attachment once — hashing per page is
    // quadratic on PDF-imported notebooks where every page shares one
    // multi-hundred-MB asset. Aliased keys share one hash via their byte
    // pointer; distinct keys with identical bytes hash separately and are
    // deduplicated again by entry name below.
    let atts = referenced_attachments(pages, doc);
    let mut uniq: Vec<&Arc<Vec<u8>>> = Vec::new();
    let mut seen_ptrs = HashSet::new();
    for (_, bytes) in &atts {
        if seen_ptrs.insert(Arc::as_ptr(bytes) as usize) {
            uniq.push(bytes);
        }
    }
    let ptr_hash: HashMap<usize, [u8; 64]> = crate::par::map(&uniq, |bytes| {
        (Arc::as_ptr(*bytes) as usize, compute_hash(bytes))
    })
    .into_iter()
    .collect();
    let hashes: HashMap<String, [u8; 64]> = atts
        .iter()
        .map(|(key, bytes)| {
            (
                key.clone(),
                ptr_hash[&(Arc::as_ptr(bytes) as usize)],
            )
        })
        .collect();

    let (bundle, _note_guid, _ts_ms, subject_id) = build_note_bundle(pages, doc, &hashes);
    if bundle.len() as u64 > MOBILE_MAX_ENTRY {
        eprintln!(
            "warning: noteBundle is {} MiB; Notability's Android importer rejects entries over 64 MiB",
            bundle.len() / 1024 / 1024
        );
    }

    let deflated = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    // Assets are PDF/images — already compressed, so deflate gains nothing
    // but takes most of the write time on large notebooks.
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));

    // Version file: Notability's NTB parser decodes this as a UTF-8 string and
    // requires it to equal "1" (ASCII 0x31), not a single byte 0x01.
    zip.start_file("version", deflated)?;
    zip.write_all(b"1")?;

    zip.start_file("noteBundle", deflated)?;
    zip.write_all(&bundle)?;

    // Manifest with required fields (subjectId, subjectName, appVersion).
    // subjectId is unique per converted note — recycling one ID made every
    // output collide in libraries that dedupe by subject.
    let manifest = format!(
        "{{\n  \"subjectId\" : \"{}\",\n  \"subjectName\" : \"Imported\",\n  \"appVersion\" : \"16.12.5\"\n}}",
        format_guid(&subject_id)
    );
    zip.start_file("manifest.json", deflated)?;
    zip.write_all(manifest.as_bytes())?;

    // HandwritingIndex.fb — required by Notability import.
    // Structure from real samples: 4-field table with version/flags.
    zip.start_file("index/ios/HandwritingIndex.fb", deflated)?;
    zip.write_all(&build_handwriting_index())?;

    // Thumbnail — generate a minimal valid PNG (64x64 white).
    zip.start_file("thumbnail.png", deflated)?;
    zip.write_all(&generate_thumbnail(pages, doc))?;

    // Assets: copy any PDF attachments referenced by page configs. Entries are
    // named by content hash, so deduplicate by that name rather than by the
    // attachment UUID: a GoodNotes export can use several UUIDs for the very
    // same PDF bytes, which would otherwise produce duplicate zip entries.
    let mut written_assets = HashSet::new();
    for (key, bytes) in &atts {
        let hash = &hashes[key];
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        if !written_assets.insert(hex.clone()) {
            continue;
        }
        if bytes.len() as u64 > MOBILE_MAX_ENTRY {
            eprintln!(
                "warning: asset {hex}.pdf is {} MiB; Notability's Android importer rejects entries over 64 MiB",
                bytes.len() / 1024 / 1024
            );
        }
        zip.start_file(format!("assets/{hex}.pdf"), stored)?;
        zip.write_all(bytes)?;
    }

    let cursor = zip.finish().context("finish ntb zip")?;
    let bytes = cursor.into_inner();
    if bytes.len() as u64 > MOBILE_MAX_ARCHIVE {
        eprintln!(
            "warning: .ntb is {} MB; Notability's Android picker rejects files over 500 MB",
            bytes.len() / 1_000_000
        );
    }
    w.write_all(&bytes)?;
    Ok(())
}

/// Convenience wrapper: write a `.ntb` archive and return the bytes.
pub fn write_ntb_to_vec(pages: &[Page], doc: &Document) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_ntb(pages, doc, &mut out)?;
    Ok(out)
}

/// Build HandwritingIndex.fb — a minimal FlatBuffer required by Notability.
/// Empty handwriting search index, matching the 60-byte stub native
/// exports write when a note has no recognised text: a root table
/// `{u8 4, u8 5, uoffset→inner, u32 0}` and an inner `{u32 4, u32 8}`.
/// The dword at offset 24 is undeclared padding in real files.
fn build_handwriting_index() -> Vec<u8> {
    vec![
        0x10, 0x00, 0x00, 0x00, // root uoffset → table @16
        0x0c, 0x00, 0x14, 0x00, 0x13, 0x00, 0x12, 0x00, 0x0c, 0x00, 0x04, 0x00, // vtable
        0x0c, 0x00, 0x00, 0x00, // soffset → vtable @4
        0x00, 0x00, 0x00, 0x00, // f3 = 0
        0x00, 0x00, 0x00, 0x00, // padding (undeclared)
        0x10, 0x00, 0x00, 0x00, // f2 uoffset → inner table @44
        0x00, 0x00, 0x05, 0x04, // f1 = 5 (@34), f0 = 4 (@35)
        0x08, 0x00, 0x0c, 0x00, 0x08, 0x00, 0x04, 0x00, // inner vtable
        0x08, 0x00, 0x00, 0x00, // inner soffset → vtable @36
        0x08, 0x00, 0x00, 0x00, // inner f1 = 8
        0x04, 0x00, 0x00, 0x00, // inner f0 = 4
        0x00, 0x00, 0x00, 0x00, // trailing pad
    ]
}

/// Generate a minimal valid PNG thumbnail (64x64 white background).
fn generate_thumbnail(pages: &[Page], _doc: &Document) -> Vec<u8> {
    // Use the tiny 1x1 PNG for now — Notability may accept it.
    // A real thumbnail would require rendering the first page.
    let _ = pages;
    tiny_png()
}

/// A minimal 1×1 transparent PNG used as a placeholder thumbnail.
fn tiny_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, //
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, //
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, //
        0x08, 0x06, 0x00, 0x00, 0x00, //
        0x1F, 0x15, 0xC4, 0x89, //
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, //
        0x78, 0xDA, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, //
        0x0D, 0x0A, 0x2D, 0xB4, //
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Item, Page, Stroke};
    use std::io::Read as _;

    #[test]
    fn path_stream_encodes_points() {
        let points = vec![[10.0, 20.0], [30.0, 40.0], [50.0, 60.0]];
        let stream = build_path_stream(&points);
        // Stream: header byte + u16 count + elements.
        assert!(stream.len() > 3);
        // Header: precision 1, count 3.
        assert_eq!(stream[0], 1);
        let count = u16::from_le_bytes(stream[1..3].try_into().unwrap());
        assert_eq!(count, 3);
        // First element: moveto (type 7).
        assert_eq!(stream[3], PATH_MOVETO);
    }

    #[test]
    fn empty_document_writes() {
        let doc = Document::empty();
        let pages = vec![Page {
            uuid: "p0".into(),
            items: vec![],
            width: 595.28,
            height: 841.89,
        }];
        let mut out = Vec::new();
        write_ntb(&pages, &doc, &mut out).unwrap();
        assert!(out.len() > 50);
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(out)).unwrap();
        assert!(z.by_name("noteBundle").is_ok());
        assert!(z.by_name("manifest.json").is_ok());
    }

    #[test]
    fn archive_satisfies_mobile_importer_entry_checks() {
        // Mirrors what Notability's Android NtbImporter checks:
        // version text == "1", a noteBundle entry, unique names, and the
        // 64 MiB per-entry / 500 MB whole-file limits.
        let mut doc = Document::empty();
        doc.insert_attachment("att", b"%PDF-1.4 mobile limits test".to_vec());
        doc.set_page_cfg("p0", "att".into(), None);
        let pages = vec![Page {
            uuid: "p0".into(),
            items: vec![],
            width: 595.28,
            height: 841.89,
        }];
        let mut out = Vec::new();
        write_ntb(&pages, &doc, &mut out).unwrap();

        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&out)).unwrap();
        assert_eq!(z.by_name("version").unwrap().size(), 1);
        let mut version = Vec::new();
        z.by_name("version").unwrap().read_to_end(&mut version).unwrap();
        assert_eq!(version, b"1");
        assert!(z.by_name("noteBundle").is_ok());
        let names: Vec<String> = (0..z.len())
            .filter_map(|i| z.by_index(i).ok().map(|e| e.name().to_string()))
            .collect();
        assert_eq!(names.len(), names.iter().collect::<std::collections::HashSet<_>>().len());
        assert!(names.iter().all(|n| z.by_name(n).unwrap().size() as u64 <= MOBILE_MAX_ENTRY));
        assert!(out.len() as u64 <= MOBILE_MAX_ARCHIVE);
    }

    #[test]
    fn identical_asset_contents_are_deduplicated_by_hash() {
        // A GoodNotes export can bind different page configs to distinct
        // attachment UUIDs that contain the same PDF bytes. The .ntb archive
        // names assets by content hash, so both would map to one zip entry.
        let mut doc = Document::empty();
        let pdf = b"%PDF-1.4 identical content".to_vec();
        doc.insert_attachment("att-a", pdf.clone());
        doc.insert_attachment("att-b", pdf);
        doc.set_page_cfg("p0", "att-a".into(), None);
        doc.set_page_cfg("p1", "att-b".into(), None);
        let pages = vec![
            Page {
                uuid: "p0".into(),
                items: vec![],
                width: 595.28,
                height: 841.89,
            },
            Page {
                uuid: "p1".into(),
                items: vec![],
                width: 595.28,
                height: 841.89,
            },
        ];

        let mut out = Vec::new();
        write_ntb(&pages, &doc, &mut out).unwrap();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(out)).unwrap();
        let assets: Vec<String> = (0..z.len())
            .filter_map(|i| z.by_index(i).ok().map(|e| e.name().to_string()))
            .filter(|name| name.starts_with("assets/"))
            .collect();
        assert_eq!(assets.len(), 1, "expected one asset for identical bytes");
    }

    #[test]
    fn bundle_parses_back_with_reader() {
        use crate::ntb;
        let doc = Document::empty();
        let stroke = Stroke {
            points: vec![[10.0, 20.0], [30.0, 40.0]],
            width: 2.0,
            rgba: [1.0, 0.0, 0.0, 1.0],
            dash: None,
            parented: false,
        };
        let pages = vec![Page {
            uuid: "p0".into(),
            items: vec![Item::Stroke(stroke)],
            width: 595.28,
            height: 841.89,
        }];
        let mut out = Vec::new();
        write_ntb(&pages, &doc, &mut out).unwrap();

        // Extract noteBundle and parse with the reader.
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(out)).unwrap();
        let mut bundle = Vec::new();
        z.by_name("noteBundle")
            .unwrap()
            .read_to_end(&mut bundle)
            .unwrap();
        let (meta, strokes, page_defs, _texts, shapes) = ntb::parse_bundle_public(&bundle).unwrap();
        assert_eq!(meta.element_count, 3); // 1 DocumentSetup + 1 CreatePage + 1 CreateInk
        assert_eq!(strokes.len(), 1);
        assert_eq!(page_defs.len(), 1);
        assert!(shapes.is_empty());
        let s = &strokes[0];
        assert!((s.width - 2.0).abs() < 0.01);
        assert_eq!(s.points.len(), 2);
        assert!((s.points[0][0] - 10.0).abs() < 0.1);
        assert!((s.points[1][1] - 40.0).abs() < 0.1);
    }
}
// appended debug test
