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

use std::io::Write;

use anyhow::{Context, Result};
use sha2::{Digest, Sha512};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::doc::{Document, Item, Page, ShapeKind, Stroke};

// PayloadType constants (matching the reader in `ntb.rs`).
const PT_CREATE_PAGE: u8 = 3;
const PT_CREATE_INK: u8 = 15;
const PT_CREATE_SHAPE: u8 = 18;
const PT_DOCUMENT_SETUP: u8 = 1;

// Root table field indices (verified against real samples).
const ROOT_F0_NOTE_GUID: usize = 0; // 16-byte inline GUID
const ROOT_F3_DEVICE_UUID_UPPER: usize = 3; // uoffset to string
const ROOT_F4_TIMESTAMP: usize = 4; // u64 ms epoch
const ROOT_F5_DEVICE_UUID_LOWER: usize = 5; // uoffset to string
const ROOT_F6_OPS: usize = 6; // uoffset to ops vector
const ROOT_F7_VERSION: usize = 7; // u32 version

/// Version observed in real samples: 0x800012.
const NTB_VERSION: u32 = 0x800012;

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

    /// Write a FlatBuffer table with the given fields. Returns the table
    /// position. The vtable is written immediately after the table with a
    /// negative soffset (vtable after table).
    fn write_table(&mut self, fields: &[(usize, FieldVal)]) -> usize {
        let mut sorted: Vec<(usize, FieldVal)> = fields.to_vec();
        sorted.sort_by_key(|(i, _)| *i);

        // Compute field byte offsets within the table.
        // Table layout: [i32 soffset] [field_data ...]
        let mut field_offs: Vec<(usize, usize)> = Vec::new();
        let mut data_off = 4usize; // after soffset
        for &(idx, ref val) in &sorted {
            let size = match val {
                FieldVal::Inline(b) => b.len(),
            };
            let align = match size {
                1 => 1,
                2 => 2,
                _ => 4,
            };
            data_off = data_off.next_multiple_of(align);
            field_offs.push((idx, data_off));
            data_off += size;
        }
        let table_size = data_off;

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
    let mut elements: Vec<(u8, Vec<u8>)> = Vec::new();

    // First element: moveto at (0, 0) relative to anchor.
    {
        let mut b = Vec::new();
        b.extend_from_slice(&0f32.to_le_bytes());
        b.extend_from_slice(&0f32.to_le_bytes());
        elements.push((PATH_MOVETO, b));
    }
    // Remaining: line-to each point (relative to anchor).
    for p in &points[1..] {
        let mut b = Vec::new();
        b.extend_from_slice(&(p[0] - anchor[0]).to_le_bytes());
        b.extend_from_slice(&(p[1] - anchor[1]).to_le_bytes());
        elements.push((PATH_LINE, b));
    }

    let mut stream = Vec::new();
    stream.push(1); // version 0 << 3 | precision 1 (f32)
    stream.extend_from_slice(&(elements.len() as u16).to_le_bytes());
    for (typ, body) in &elements {
        stream.push(*typ);
        stream.extend_from_slice(body);
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

/// Entity id counter: (site, timestamp) pairs.
struct EntityGen {
    site: u16,
    ts: u32,
}

impl EntityGen {
    fn new() -> Self {
        // Real samples start at site=0, ts=0.
        Self { site: 0, ts: 0 }
    }

    fn next(&mut self) -> (u16, u32) {
        let id = (self.site, self.ts);
        self.ts += 1;
        id
    }
}

/// Write one CreateInk op into the buffer. Returns the op table position.
fn write_create_ink(
    fb: &mut Fb,
    entity: (u16, u32),
    page: (u16, u32, u32),
    stroke: &Stroke,
) -> usize {
    // Colour struct (inline Gf, 4 bytes).
    let color = gf_color(&stroke.rgba);
    let origin = stroke.points.first().copied().unwrap_or([0.0, 0.0]);
    let width = if stroke.width.is_finite() && stroke.width > 0.0 {
        stroke.width
    } else {
        1.0
    };

    // Write the op table FIRST, then the payload, then the leaf blob, so every
    // uoffset points forward (required by real FlatBuffers readers).
    // Op table: f0 = entity id (sf 8B inline), f4 = payloadType (u8), f5 = payload uoffset.
    let op = fb.write_table(&[
        (0, FieldVal::Inline(sf_inline(entity.0, entity.1))),
        (4, FieldVal::Inline(vec![PT_CREATE_INK])),
        (5, FieldVal::Inline(vec![0; 4])), // placeholder
    ]);

    // CreateInk payload: f0 page (df inline), f1 origin (Sf inline),
    // f4 tool, f5 style, f7 colour (gf inline), f8 width, f9 path blob.
    let payload = fb.write_table(&[
        (0, FieldVal::Inline(df_inline(page.0, page.1, page.2))),
        (1, FieldVal::Inline(sf_xy(origin[0], origin[1]))),
        (4, FieldVal::Inline(vec![0])), // tool = Pen
        (5, FieldVal::Inline(vec![0])), // style = VariableWidth
        (7, FieldVal::Inline(color)),
        (8, FieldVal::Inline(width.to_le_bytes().to_vec())),
        (9, FieldVal::Inline(vec![0; 4])), // placeholder
    ]);

    let path_pos = fb.write_byte_vector(&build_path_stream(&stroke.points));
    fb.patch_uoffset(payload, 9, path_pos);
    fb.patch_uoffset(op, 5, payload);
    op
}

/// Write one CreateShape op (box/ellipse). Returns the op table position.
#[allow(clippy::too_many_arguments)]
fn write_create_shape(
    fb: &mut Fb,
    entity: (u16, u32),
    page: (u16, u32, u32),
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    ellipse: bool,
    fill: &[f32; 4],
    stroke: &[f32; 4],
    border_width: f32,
) -> usize {
    let color = gf_color(stroke);
    let fill_color = gf_color(fill);

    // Op table first, then payload, then definition table (forward uoffsets).
    let op = fb.write_table(&[
        (0, FieldVal::Inline(sf_inline(entity.0, entity.1))),
        (4, FieldVal::Inline(vec![PT_CREATE_SHAPE])),
        (5, FieldVal::Inline(vec![0; 4])), // placeholder
    ]);

    // CreateShape payload: f0 page, f1 origin, f2 rotation, f4 defType,
    // f5 def (uoffset), f9 colour, f10 border, f11 fill.
    let payload = fb.write_table(&[
        (0, FieldVal::Inline(df_inline(page.0, page.1, page.2))),
        (1, FieldVal::Inline(sf_xy(x, y))),
        (2, FieldVal::Inline(0f32.to_le_bytes().to_vec())), // rotation = 0
        (4, FieldVal::Inline(vec![3])),                     // DEF_NORMAL
        (5, FieldVal::Inline(vec![0; 4])),                  // placeholder
        (9, FieldVal::Inline(color)),
        (10, FieldVal::Inline(border_width.to_le_bytes().to_vec())),
        (11, FieldVal::Inline(fill_color)),
    ]);

    // NormalShape definition: f0 type (0=Ellipse, 1=Rect), f1 size (Sf inline).
    let def_type: u8 = if ellipse { 0 } else { 1 };
    let def = fb.write_table(&[
        (0, FieldVal::Inline(vec![def_type])),
        (1, FieldVal::Inline(sf_xy(w, h))),
    ]);

    fb.patch_uoffset(payload, 5, def);
    fb.patch_uoffset(op, 5, payload);
    op
}

/// Write a string field (uoffset to u32-len + bytes).
fn write_string(fb: &mut Fb, s: &str) -> usize {
    fb.write_byte_vector(s.as_bytes())
}

/// Write a DocumentSetup op (pt=1). This is the first op in real files and
/// configures the notebook metadata (title, locale, default settings).
fn write_document_setup(fb: &mut Fb, entity: (u16, u32), title: &str, locale: &str) -> usize {
    // Op → payload → sub-tables → strings, all written referrer-first so
    // their uoffsets point forward.
    let op = fb.write_table(&[
        (0, FieldVal::Inline(sf_inline(entity.0, entity.1))),
        (4, FieldVal::Inline(vec![PT_DOCUMENT_SETUP])),
        (5, FieldVal::Inline(vec![0; 4])), // placeholder
    ]);

    // Payload: f0=title, f1=subject, f2=locale, f3=flags, f5=default_font_size.
    let payload = fb.write_table(&[
        (0, FieldVal::Inline(vec![0; 4])), // placeholder
        (1, FieldVal::Inline(vec![0; 4])), // placeholder
        (2, FieldVal::Inline(vec![0; 4])), // placeholder
        (3, FieldVal::Inline(6145u32.to_le_bytes().to_vec())), // flags
        (5, FieldVal::Inline(14.0f32.to_le_bytes().to_vec())), // default font size
    ]);

    let title_tbl = fb.write_table(&[(0, FieldVal::Inline(vec![0; 4]))]);
    let title_str = write_string(fb, title);
    fb.patch_uoffset(title_tbl, 0, title_str);

    let locale_tbl = fb.write_table(&[(0, FieldVal::Inline(vec![0; 4]))]);
    let locale_str = write_string(fb, locale);
    fb.patch_uoffset(locale_tbl, 0, locale_str);

    // Subject name placeholder (binary-ish data observed in samples).
    let subject_tbl = fb.write_table(&[(0, FieldVal::Inline(vec![0x20; 16]))]);

    fb.patch_uoffset(payload, 0, title_tbl);
    fb.patch_uoffset(payload, 1, subject_tbl);
    fb.patch_uoffset(payload, 2, locale_tbl);
    fb.patch_uoffset(op, 5, payload);
    op
}

/// Write one CreatePage op. Returns the op table position.
///
/// When `background_hash` is `Some`, the page gets a `PageBackground` table
/// with the 64-byte asset hash and page size. `pdf_page` is the 1-based
/// index within the asset (stored implicitly by page_count / ordering).
fn write_create_page(
    fb: &mut Fb,
    entity: (u16, u32),
    width: f32,
    height: f32,
    background_hash: Option<&[u8; 64]>,
    background_size: u32,
    page_count: u32,
) -> usize {
    let op = fb.write_table(&[
        (0, FieldVal::Inline(sf_inline(entity.0, entity.1))),
        (4, FieldVal::Inline(vec![PT_CREATE_PAGE])),
        (5, FieldVal::Inline(vec![0; 4])), // placeholder
    ]);

    let payload = match background_hash {
        Some(hash) => {
            // Referrer-first order: payload → bg → nested → inner.
            let payload = fb.write_table(&[
                (1, FieldVal::Inline(vec![0; 4])), // placeholder
                (2, FieldVal::Inline(page_count.to_le_bytes().to_vec())),
            ]);
            // PageBackground: f1 = uoffset to nested, f3 = size (Sf inline).
            let bg = fb.write_table(&[
                (1, FieldVal::Inline(vec![0; 4])), // placeholder
                (3, FieldVal::Inline(sf_xy(width, height))),
            ]);
            // Nested table is the PDFAsset: f0 metadata, f2 totalPageCount,
            // f3 pagesConsumed, f4 pageOffset, f5 cropBoxes (vector of 8-byte
            // width/height structs). Notability requires one crop box per
            // page consumed when it copies a page background.
            let nested = fb.write_table(&[
                (0, FieldVal::Inline(vec![0; 4])),  // metadata placeholder
                (2, FieldVal::Inline(1u32.to_le_bytes().to_vec())), // totalPageCount
                (3, FieldVal::Inline(1u32.to_le_bytes().to_vec())), // pagesConsumed
                (4, FieldVal::Inline(0u32.to_le_bytes().to_vec())), // pageOffset
                (5, FieldVal::Inline(vec![0; 4])),  // cropBoxes placeholder
            ]);
            // AssetMetadata: f0 = 64-byte hash (inline struct), f1 = fileName,
            // f2 = mimeType. Notability requires all three when it copies the
            // page background; omitting f1/f2 makes import fail with
            // "FlatBuffers: field 6 must be set".
            let inner = fb.write_table(&[
                (0, FieldVal::Inline(hash.to_vec())),
                (1, FieldVal::Inline(vec![0; 4])), // fileName placeholder
                (2, FieldVal::Inline(vec![0; 4])), // mimeType placeholder
                (3, FieldVal::Inline(background_size.to_le_bytes().to_vec())),
            ]);
            let file_name: String = hash.iter().map(|b| format!("{b:02x}")).collect();
            let file_name = format!("{file_name}.pdf");
            let name_pos = fb.write_byte_vector(file_name.as_bytes());
            let mime_pos = fb.write_byte_vector(b"application/pdf");
            fb.patch_uoffset(inner, 1, name_pos);
            fb.patch_uoffset(inner, 2, mime_pos);
            let mut crop = Vec::with_capacity(8);
            crop.extend_from_slice(&width.to_le_bytes());
            crop.extend_from_slice(&height.to_le_bytes());
            let crop_vec = fb.write_struct_vector(&crop, 8);
            fb.patch_uoffset(nested, 5, crop_vec);
            fb.patch_uoffset(nested, 0, inner);
            fb.patch_uoffset(bg, 1, nested);
            fb.patch_uoffset(payload, 1, bg);
            payload
        }
        None => fb.write_table(&[(2, FieldVal::Inline(page_count.to_le_bytes().to_vec()))]),
    };

    fb.patch_uoffset(op, 5, payload);
    op
}

/// Build the `noteBundle` FlatBuffer from pages and document.
/// Deferred op description: ops are collected first, then written after the
/// root table and ops vector so every uoffset points forward.
enum OpSpec {
    Setup {
        entity: (u16, u32),
        title: String,
        locale: String,
    },
    Page {
        entity: (u16, u32),
        width: f32,
        height: f32,
        background: Option<[u8; 64]>,
        background_size: u32,
        page_count: u32,
    },
    Ink {
        entity: (u16, u32),
        page: (u16, u32, u32),
        stroke: Stroke,
    },
    Shape {
        entity: (u16, u32),
        page: (u16, u32, u32),
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        ellipse: bool,
        fill: [f32; 4],
        stroke: [f32; 4],
        border_width: f32,
    },
}

fn build_note_bundle(pages: &[Page], doc: &Document) -> Vec<u8> {
    let mut ids = EntityGen::new();
    let mut specs: Vec<OpSpec> = Vec::new();

    // First op: DocumentSetup (pt=1). Required by Notability.
    let setup_entity = ids.next();
    specs.push(OpSpec::Setup {
        entity: setup_entity,
        title: format!("Note {}", chrono_like_timestamp()),
        locale: "en_US".to_string(),
    });

    for (_page_idx, page) in pages.iter().enumerate() {
        let page_entity = ids.next();
        let page_id = (page_entity.0, page_entity.1, 0u32);

        // Background hash: look up via page_cfg if the page has a PDF background.
        let bg = doc
            .page_cfg()
            .get(&page.uuid)
            .and_then(|cfg| doc.attachments.get(&cfg.attachment))
            .map(|bytes| (compute_hash(bytes), bytes.len() as u32));

        specs.push(OpSpec::Page {
            entity: page_entity,
            width: page.width,
            height: page.height,
            background: bg.as_ref().map(|(h, _)| *h),
            background_size: bg.map(|(_, size)| size).unwrap_or(0),
            page_count: 1,
        });

        for item in &page.items {
            match item {
                Item::Stroke(s) => {
                    if s.points.len() < 2 {
                        continue;
                    }
                    specs.push(OpSpec::Ink {
                        entity: ids.next(),
                        page: page_id,
                        stroke: s.clone(),
                    });
                }
                Item::Shape(sh) => {
                    let (sx, sy, sw, sh2, ellipse) = match sh.kind {
                        ShapeKind::Ellipse => {
                            (sh.x - sh.w, sh.y - sh.h, sh.w * 2.0, sh.h * 2.0, true)
                        }
                        ShapeKind::Rect => (sh.x, sh.y, sh.w, sh.h, false),
                        _ => {
                            // Polygon → emit as a stroke (closed polyline).
                            if sh.points.len() >= 2 {
                                let mut pts = sh.points.clone();
                                if let Some(f) = pts.first().copied() {
                                    pts.push(f);
                                }
                                let stroke = Stroke {
                                    points: pts,
                                    width: sh.width,
                                    rgba: sh.stroke,
                                    dash: sh.dash,
                                    parented: false,
                                };
                                specs.push(OpSpec::Ink {
                                    entity: ids.next(),
                                    page: page_id,
                                    stroke,
                                });
                            }
                            continue;
                        }
                    };
                    specs.push(OpSpec::Shape {
                        entity: ids.next(),
                        page: page_id,
                        x: sx,
                        y: sy,
                        w: sw,
                        h: sh2,
                        ellipse,
                        fill: sh.fill,
                        stroke: sh.stroke,
                        border_width: sh.width,
                    });
                }
                Item::Path(p) => {
                    if p.points.len() < 2 {
                        continue;
                    }
                    specs.push(OpSpec::Ink {
                        entity: ids.next(),
                        page: page_id,
                        stroke: Stroke {
                            points: p.points.clone(),
                            width: p.width,
                            rgba: p.rgba,
                            dash: None,
                            parented: false,
                        },
                    });
                }
                Item::FillPath(fp) => {
                    for contour in &fp.contours {
                        if contour.len() < 2 {
                            continue;
                        }
                        specs.push(OpSpec::Ink {
                            entity: ids.next(),
                            page: page_id,
                            stroke: Stroke {
                                points: contour.clone(),
                                width: 1.0,
                                rgba: fp.rgba,
                                dash: None,
                                parented: false,
                            },
                        });
                    }
                }
                Item::Connector(c) => {
                    specs.push(OpSpec::Ink {
                        entity: ids.next(),
                        page: page_id,
                        stroke: Stroke {
                            points: vec![
                                [c.anchor[0], c.anchor[1]],
                                [c.anchor[0], c.anchor[1]],
                            ],
                            width: c.width,
                            rgba: c.rgba,
                            dash: None,
                            parented: false,
                        },
                    });
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

    // Root table first, with placeholder uoffsets for fields 3/5/6.
    let mut fb = Fb::new();
    let note_guid: [u8; 16] = [
        0xb7, 0xaf, 0xff, 0x54, 0xb0, 0xce, 0xb0, 0x85, 0x10, 0x41, 0x94, 0xc5, 0x81, 0xb7, 0x1b,
        0x23,
    ];
    let timestamp_ms: u64 = 1789453933593; // Matches sample files.
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
    let device_uuid_upper = write_string(&mut fb, "798E2A39-80A9-4BBE-B95C-B205F461CDB1");
    let device_uuid_lower = write_string(&mut fb, "798e2a39-80a9-4bbe-b95c-b205f461cdb1");
    fb.patch_uoffset(root, ROOT_F3_DEVICE_UUID_UPPER, device_uuid_upper);
    fb.patch_uoffset(root, ROOT_F5_DEVICE_UUID_LOWER, device_uuid_lower);

    // Ops vector is written before the op tables it references.
    let ops_vec = fb.write_uoffset_slots(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        let op = match spec {
            OpSpec::Setup {
                entity,
                title,
                locale,
            } => write_document_setup(&mut fb, *entity, title, locale),
            OpSpec::Page {
                entity,
                width,
                height,
                background,
                background_size,
                page_count,
            } => write_create_page(
                &mut fb,
                *entity,
                *width,
                *height,
                background.as_ref(),
                *background_size,
                *page_count,
            ),
            OpSpec::Ink {
                entity,
                page,
                stroke,
            } => write_create_ink(&mut fb, *entity, *page, stroke),
            OpSpec::Shape {
                entity,
                page,
                x,
                y,
                w,
                h,
                ellipse,
                fill,
                stroke,
                border_width,
            } => write_create_shape(
                &mut fb, *entity, *page, *x, *y, *w, *h, *ellipse, fill, stroke, *border_width,
            ),
        };
        fb.patch_uoffset_slot(ops_vec, i, op);
    }
    fb.patch_uoffset(root, ROOT_F6_OPS, ops_vec);

    fb.finish(root)
}

/// Generate a simple timestamp string for note titles.
fn chrono_like_timestamp() -> String {
    // Use a fixed timestamp matching the sample files for reproducibility.
    "Sep 15, 2026".to_string()
}

/// Compute a 64-byte content hash (SHA-512).
///
/// Notability uses SHA-512 of the asset content as the filename.
fn compute_hash(data: &[u8]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Write a `.ntb` archive to `w`.
pub fn write_ntb(pages: &[Page], doc: &Document, mut w: impl Write) -> Result<()> {
    let bundle = build_note_bundle(pages, doc);

    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));

    // Version file: Notability's NTB parser decodes this as a UTF-8 string and
    // requires it to equal "1" (ASCII 0x31), not a single byte 0x01.
    zip.start_file("version", opts)?;
    zip.write_all(b"1")?;

    zip.start_file("noteBundle", opts)?;
    zip.write_all(&bundle)?;

    // Manifest with required fields (subjectId, subjectName, appVersion).
    let manifest = r#"{
  "subjectId" : "f662d1e9-a300-45b4-affe-bb36e2a825c3",
  "subjectName" : "Imported",
  "appVersion" : "16.12.5"
}"#;
    zip.start_file("manifest.json", opts)?;
    zip.write_all(manifest.as_bytes())?;

    // HandwritingIndex.fb — required by Notability import.
    // Structure from real samples: 4-field table with version/flags.
    zip.start_file("index/ios/HandwritingIndex.fb", opts)?;
    zip.write_all(&build_handwriting_index())?;

    // Thumbnail — generate a minimal valid PNG (64x64 white).
    zip.start_file("thumbnail.png", opts)?;
    zip.write_all(&generate_thumbnail(pages, doc))?;

    // Assets: copy any PDF attachments referenced by page configs.
    let mut written_assets = std::collections::HashSet::new();
    for page in pages {
        if let Some(cfg) = doc.page_cfg().get(&page.uuid)
            && let Some(bytes) = doc.attachments.get(&cfg.attachment)
            && written_assets.insert(cfg.attachment.clone())
        {
            let hash = compute_hash(bytes);
            let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
            zip.start_file(format!("assets/{hex}.pdf"), opts)?;
            zip.write_all(bytes)?;
        }
    }

    let cursor = zip.finish().context("finish ntb zip")?;
    w.write_all(&cursor.into_inner())?;
    Ok(())
}

/// Convenience wrapper: write a `.ntb` archive and return the bytes.
pub fn write_ntb_to_vec(pages: &[Page], doc: &Document) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_ntb(pages, doc, &mut out)?;
    Ok(out)
}

/// Build HandwritingIndex.fb — a minimal FlatBuffer required by Notability.
/// Structure reverse-engineered from real samples: 4-field table.
fn build_handwriting_index() -> Vec<u8> {
    let mut fb = Fb::new();
    // Simple table with version and count fields.
    let root = fb.write_table(&[
        (0, FieldVal::Inline(vec![0x04, 0x00, 0x00, 0x00])), // version-ish
        (1, FieldVal::Inline(vec![0x00, 0x00, 0x00, 0x00])), // count = 0
        (2, FieldVal::Inline(vec![0x00, 0x00, 0x00, 0x00])), // offset
        (3, FieldVal::Inline(vec![0x00, 0x00, 0x00, 0x00])), // flags
    ]);
    fb.finish(root)
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
