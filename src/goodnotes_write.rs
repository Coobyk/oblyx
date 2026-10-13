//! `.goodnotes` writer.
//!
//! Serialises the portable [`Page`]/[`Item`] IR back into a GoodNotes zip
//! archive: schema, page/attachment/event indexes, per-page record streams
//! and raw attachment bytes. Strokes are encoded via TPL → stored-bv4 →
//! kind-7 content records; images via kind-1 placements; shapes via kind-9
//! frames; text via kind-21 textboxes. UUIDs and clocks are regenerated
//! (the IR does not carry the originals), so output is semantically
//! equivalent but not byte-identical to the source.
//!
//! The writer targets schema 35 and emits the minimum member set the reader
//! (`Document::load`) requires plus the auxiliary members GoodNotes expects
//! (`document.info.pb`, `index.search.pb`, `search/*` stubs).

use std::io::Write;

use anyhow::{Context, Result};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::bv4::compress_bv4_stored;
use crate::doc::{Document, Item, Page, ShapeKind, Stroke};
use crate::pbw::*;
use crate::tpl;

/// Document id observed in every real sample.
const DOC_ID: u64 = 754_340_535_263_529_922;

/// Fixed wrapper constants observed in samples.
const WRAPPER_14: u64 = 5381;
const WRAPPER_16: u64 = 24;

/// Deterministic UUID + clock generator so writes are reproducible.
struct IdGen {
    counter: u64,
    actor: u64,
}

impl IdGen {
    fn new() -> Self {
        Self {
            counter: 1,
            actor: 0xABCDEF01,
        }
    }

    /// Mint a 36-char uppercase hyphenated UUID from the counter.
    fn uuid(&mut self) -> String {
        let c = self.counter;
        self.counter += 1;
        format!("{c:08X}-0000-4000-8000-{:012X}", self.actor & 0xFFFF_FFFF)
    }

    /// Hybrid clock `{1: counter, 2: actor}` as a protobuf sub-message.
    fn clock_msg(&mut self) -> Vec<u8> {
        let c = self.counter;
        self.counter += 1;
        let mut out = Vec::new();
        write_varint_field(&mut out, 1, c);
        write_varint_field(&mut out, 2, self.actor);
        out
    }

    /// Monotonic LSN for wrapper field 9.
    fn lsn(&mut self) -> u64 {
        let v = self.counter;
        self.counter += 1;
        v
    }
}

/// Encode a wrapper record: `{1: uuid, 2: clock, 8: doc_id, 9: lsn, 14: 5381, 16: 24}`.
fn wrapper_rec(ids: &mut IdGen, obj_uuid: &str) -> Vec<u8> {
    let clock = ids.clock_msg();
    let lsn = ids.lsn();
    let mut w = Vec::new();
    write_bytes_field(&mut w, 1, obj_uuid.as_bytes());
    write_bytes_field(&mut w, 2, &clock);
    write_varint_field(&mut w, 8, DOC_ID);
    write_varint_field(&mut w, 9, lsn);
    write_varint_field(&mut w, 14, WRAPPER_14);
    write_varint_field(&mut w, 16, WRAPPER_16);
    w
}

/// Colour sub-message `{1:R,2:G,3:B,4:A}` with f32-bit channels.
fn color_msg(rgba: &[f32; 4]) -> Vec<u8> {
    let mut c = Vec::new();
    write_f32_field(&mut c, 1, rgba[0].clamp(0.0, 1.0));
    write_f32_field(&mut c, 2, rgba[1].clamp(0.0, 1.0));
    write_f32_field(&mut c, 3, rgba[2].clamp(0.0, 1.0));
    write_f32_field(&mut c, 4, rgba[3].clamp(0.0, 1.0));
    c
}

/// Encode a stroke as a kind-7 content record. Returns the object uuid so
/// the caller can build a matching wrapper.
fn stroke_content(ids: &mut IdGen, obj_uuid: &str, stroke: &Stroke) -> Vec<u8> {
    let tpl_img = tpl::encode_stroke(&stroke.points, stroke.width, stroke.dash);
    let blob = compress_bv4_stored(&tpl_img);
    let color = color_msg(&stroke.rgba);
    let clock = ids.clock_msg();
    let clock2 = ids.clock_msg();

    let mut inner = Vec::new();
    write_bytes_field(&mut inner, 1, obj_uuid.as_bytes());
    write_bytes_field(&mut inner, 2, &blob);
    write_bytes_field(&mut inner, 4, &color);
    write_bytes_field(&mut inner, 7, &clock);
    write_bytes_field(&mut inner, 15, &clock2);

    let mut rec = Vec::new();
    write_bytes_field(&mut rec, 7, &inner);
    rec
}

/// Encode an image placement as a kind-1 content record.
fn image_content(
    ids: &mut IdGen,
    obj_uuid: &str,
    att_uuid: &str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Vec<u8> {
    let mut pt_xy = Vec::new();
    write_f32_field(&mut pt_xy, 1, x);
    write_f32_field(&mut pt_xy, 2, y);
    let mut pt_wh = Vec::new();
    write_f32_field(&mut pt_wh, 1, w);
    write_f32_field(&mut pt_wh, 2, h);
    let mut frame = Vec::new();
    write_bytes_field(&mut frame, 1, &pt_xy);
    write_bytes_field(&mut frame, 2, &pt_wh);

    let clock = ids.clock_msg();
    let mut inner = Vec::new();
    write_bytes_field(&mut inner, 1, obj_uuid.as_bytes());
    write_bytes_field(&mut inner, 2, &frame);
    write_bytes_field(&mut inner, 4, att_uuid.as_bytes());
    write_bytes_field(&mut inner, 15, &clock);
    write_varint_field(&mut inner, 18, WRAPPER_16);

    let mut rec = Vec::new();
    write_bytes_field(&mut rec, 1, &inner);
    rec
}

/// Encode a shape as a kind-9 frame content record (fill + stroke).
#[allow(clippy::too_many_arguments)]
fn shape_content(
    ids: &mut IdGen,
    obj_uuid: &str,
    kind: ShapeKind,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fill: &[f32; 4],
    stroke: &[f32; 4],
    width: f32,
) -> Vec<u8> {
    // IR Ellipse uses centre + radii; Rect uses top-left + full size.
    let (fx, fy) = match kind {
        ShapeKind::Ellipse => (x, y),
        _ => (x + w * 0.5, y + h * 0.5),
    };
    let mut pt_c = Vec::new();
    write_f32_field(&mut pt_c, 1, fx);
    write_f32_field(&mut pt_c, 2, fy);
    let mut pt_s = Vec::new();
    write_f32_field(&mut pt_s, 1, w);
    write_f32_field(&mut pt_s, 2, h);
    let mut frame = Vec::new();
    write_bytes_field(&mut frame, 1, &pt_c);
    write_bytes_field(&mut frame, 2, &pt_s);

    let fill_msg = color_msg(fill);
    let stroke_msg = color_msg(stroke);
    let clock = ids.clock_msg();

    let mut inner = Vec::new();
    write_bytes_field(&mut inner, 1, obj_uuid.as_bytes());
    write_bytes_field(&mut inner, 2, &frame);
    write_bytes_field(&mut inner, 7, &fill_msg);
    write_bytes_field(&mut inner, 5, &stroke_msg);
    write_f32_field(&mut inner, 31, width);
    write_bytes_field(&mut inner, 15, &clock);

    let mut rec = Vec::new();
    write_bytes_field(&mut rec, 9, &inner);
    rec
}

/// Encode a text box as a kind-21 content record.
fn text_content(
    ids: &mut IdGen,
    obj_uuid: &str,
    text: &str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Vec<u8> {
    let clock = ids.clock_msg();

    let mut blocks = Vec::new();
    let mut block = Vec::new();
    let mut inner1 = Vec::new();
    write_bytes_field(&mut inner1, 1, text.as_bytes());
    write_bytes_field(&mut block, 1, &inner1);
    write_bytes_field(&mut blocks, 1, &block);
    let blob = compress_bv4_stored(&blocks);
    let mut text_field = Vec::new();
    write_bytes_field(&mut text_field, 2, &blob);
    write_varint_field(&mut text_field, 4, 1);

    let mut pos = Vec::new();
    write_f32_field(&mut pos, 1, x);
    write_f32_field(&mut pos, 2, y);
    let mut size = Vec::new();
    write_f32_field(&mut size, 1, w);
    write_f32_field(&mut size, 2, h);

    let mut inner = Vec::new();
    write_bytes_field(&mut inner, 1, obj_uuid.as_bytes());
    write_bytes_field(&mut inner, 20, &pos);
    write_bytes_field(&mut inner, 21, &size);
    write_bytes_field(&mut inner, 32, &text_field);
    write_bytes_field(&mut inner, 15, &clock);

    let mut rec = Vec::new();
    write_bytes_field(&mut rec, 21, &inner);
    rec
}

/// Encode a sticky note as a kind-20 content record.
fn sticky_content(
    ids: &mut IdGen,
    obj_uuid: &str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: &[f32; 4],
) -> Vec<u8> {
    let clock = ids.clock_msg();
    let color_m = color_msg(color);

    let mut pos = Vec::new();
    write_f32_field(&mut pos, 1, x);
    write_f32_field(&mut pos, 2, y);
    let mut size = Vec::new();
    write_f32_field(&mut size, 1, w);
    write_f32_field(&mut size, 2, h);

    let mut inner = Vec::new();
    write_bytes_field(&mut inner, 1, obj_uuid.as_bytes());
    write_bytes_field(&mut inner, 20, &pos);
    write_bytes_field(&mut inner, 21, &size);
    write_bytes_field(&mut inner, 30, &color_m);
    write_bytes_field(&mut inner, 15, &clock);

    let mut rec = Vec::new();
    write_bytes_field(&mut rec, 20, &inner);
    rec
}

/// Emit one wrapper+content pair into the page record stream.
fn emit_pair(out: &mut Vec<u8>, ids: &mut IdGen, content: Vec<u8>) {
    // The content record's inner message field 1 holds the object uuid.
    let obj_uuid = extract_uuid(&content).unwrap_or_else(|| ids.uuid());
    let wrapper = wrapper_rec(ids, &obj_uuid);
    write_record(out, &wrapper);
    write_record(out, &content);
}

/// Pull the object uuid (inner field 1) out of an encoded content record.
fn extract_uuid(rec: &[u8]) -> Option<String> {
    use crate::pb::{Wire, bytes_of, parse};
    let fs = parse(rec).ok()?;
    let first = fs.first()?;
    let Wire::Bytes(inner_bytes) = first.wire else {
        return None;
    };
    let inner_fs = parse(inner_bytes).ok()?;
    let uuid_raw = bytes_of(&inner_fs, 1)?;
    std::str::from_utf8(uuid_raw).ok().map(String::from)
}

/// Encode one page's items into a record stream (wrapper + content pairs).
fn encode_page(ids: &mut IdGen, page: &Page) -> Vec<u8> {
    let mut out = Vec::new();
    for item in &page.items {
        match item {
            Item::Stroke(s) => {
                if s.points.len() < 2 {
                    continue;
                }
                let uuid = ids.uuid();
                let content = stroke_content(ids, &uuid, s);
                emit_pair(&mut out, ids, content);
            }
            Item::Image(img) => {
                let uuid = ids.uuid();
                let content =
                    image_content(ids, &uuid, &img.attachment, img.x, img.y, img.w, img.h);
                emit_pair(&mut out, ids, content);
            }
            Item::Shape(sh) => {
                if sh.kind == ShapeKind::Polygon && sh.points.len() >= 3 {
                    // Closed polygon → stroke.
                    let mut pts = sh.points.clone();
                    if let Some(first) = pts.first().copied() {
                        pts.push(first);
                    }
                    let stroke = Stroke {
                        points: pts,
                        width: sh.width,
                        rgba: sh.stroke,
                        dash: sh.dash,
                        parented: false,
                    };
                    let uuid = ids.uuid();
                    let content = stroke_content(ids, &uuid, &stroke);
                    emit_pair(&mut out, ids, content);
                } else {
                    let uuid = ids.uuid();
                    let content = shape_content(
                        ids, &uuid, sh.kind, sh.x, sh.y, sh.w, sh.h, &sh.fill, &sh.stroke, sh.width,
                    );
                    emit_pair(&mut out, ids, content);
                }
            }
            Item::Path(p) => {
                if p.points.len() < 2 {
                    continue;
                }
                let stroke = Stroke {
                    points: p.points.clone(),
                    width: p.width,
                    rgba: p.rgba,
                    dash: None,
                    parented: false,
                };
                let uuid = ids.uuid();
                let content = stroke_content(ids, &uuid, &stroke);
                emit_pair(&mut out, ids, content);
            }
            Item::FillPath(fp) => {
                for contour in &fp.contours {
                    if contour.len() < 2 {
                        continue;
                    }
                    let stroke = Stroke {
                        points: contour.clone(),
                        width: 1.0,
                        rgba: fp.rgba,
                        dash: None,
                        parented: false,
                    };
                    let uuid = ids.uuid();
                    let content = stroke_content(ids, &uuid, &stroke);
                    emit_pair(&mut out, ids, content);
                }
            }
            Item::Connector(c) => {
                let stroke = Stroke {
                    points: vec![[c.anchor[0], c.anchor[1]], [c.anchor[0], c.anchor[1]]],
                    width: c.width,
                    rgba: c.rgba,
                    dash: None,
                    parented: false,
                };
                let uuid = ids.uuid();
                let content = stroke_content(ids, &uuid, &stroke);
                emit_pair(&mut out, ids, content);
            }
            Item::Sticky(st) => {
                let uuid = ids.uuid();
                let rgba = [st.rgb[0], st.rgb[1], st.rgb[2], 1.0];
                let content = sticky_content(ids, &uuid, st.x, st.y, st.w, st.h, &rgba);
                emit_pair(&mut out, ids, content);
            }
            Item::Text(t) => {
                let uuid = ids.uuid();
                let content =
                    text_content(ids, &uuid, &t.text, t.x, t.y, t.size * 10.0, t.size * 1.5);
                emit_pair(&mut out, ids, content);
            }
            // Backgrounds and PDF backgrounds are handled via events, not page records.
            Item::Background(_) | Item::PdfBackground(_) => {}
        }
    }
    out
}

/// Collect attachment keys actually referenced by image/PDF-background items.
fn used_attachments(pages: &[Page]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for page in pages {
        for item in &page.items {
            let key = match item {
                Item::Image(img) => Some(&img.attachment),
                Item::PdfBackground(bg) => Some(&bg.attachment),
                _ => None,
            };
            if let Some(key) = key
                && !seen.contains(key)
            {
                seen.insert(key.clone());
                out.push(key.clone());
            }
        }
    }
    out
}

/// Build `index.events.pb`: config records + type-54 page bindings for every
/// page that has a background attachment.
fn build_events(pages: &[Page], doc: &Document, ids: &mut IdGen) -> Vec<u8> {
    let mut out = Vec::new();
    for page in pages {
        let Some(cfg) = doc.page_cfg().get(&page.uuid) else {
            continue;
        };
        let cfg_uuid = ids.uuid();
        let page_prev = decrement_last_byte(&page.uuid);

        // Config event: field 1 = cfg uuid, field 2 = {4: attachment, 5: pdf_page}.
        let mut sub = Vec::new();
        write_bytes_field(&mut sub, 4, cfg.attachment.as_bytes());
        if let Some(n) = cfg.pdf_page {
            write_varint_field(&mut sub, 5, n);
        }
        let mut config_ev = Vec::new();
        write_bytes_field(&mut config_ev, 1, cfg_uuid.as_bytes());
        write_bytes_field(&mut config_ev, 2, &sub);
        write_record(&mut out, &config_ev);

        // Type-54 binding: field 1 = ev uuid, field 54 = {2: page_uuid, 3: {1: cfg_uuid}}.
        let mut binding_sub = Vec::new();
        write_bytes_field(&mut binding_sub, 2, page_prev.as_bytes());
        let mut cfg_ref = Vec::new();
        write_bytes_field(&mut cfg_ref, 1, cfg_uuid.as_bytes());
        write_bytes_field(&mut binding_sub, 3, &cfg_ref);
        let mut binding = Vec::new();
        write_bytes_field(&mut binding, 1, ids.uuid().as_bytes());
        write_bytes_field(&mut binding, 54, &binding_sub);
        write_record(&mut out, &binding);
    }
    out
}

/// Decrement the last two hex digits (one byte) of a UUID string, matching
/// GoodNotes' `uuid_prev` quirk (`…-5E00 → …-5EFF`).
fn decrement_last_byte(uuid: &str) -> String {
    let (head, last) = match uuid.rsplit_once('-') {
        Some(v) => v,
        None => return uuid.to_string(),
    };
    if last.len() < 2 {
        return uuid.to_string();
    }
    let (prefix, byte) = last.split_at(last.len() - 2);
    let b = u8::from_str_radix(byte, 16)
        .map(|v| v.wrapping_sub(1))
        .unwrap_or(0);
    format!("{head}-{prefix}{b:02X}")
}

/// Write a `.goodnotes` archive to `w`.
pub fn write_goodnotes(pages: &[Page], doc: &Document, mut w: impl Write) -> Result<()> {
    let mut ids = IdGen::new();

    let page_uuids: Vec<String> = pages.iter().map(|_| ids.uuid()).collect();
    let page_blobs: Vec<Vec<u8>> = pages.iter().map(|p| encode_page(&mut ids, p)).collect();

    let mut schema = Vec::new();
    write_varint_field(&mut schema, 1, 35);

    let mut index_notes = Vec::new();
    for uuid in &page_uuids {
        let mut rec = Vec::new();
        write_bytes_field(&mut rec, 1, uuid.as_bytes());
        write_bytes_field(&mut rec, 2, format!("notes/{uuid}").as_bytes());
        write_record(&mut index_notes, &rec);
    }

    let att_keys = used_attachments(pages);
    let mut index_atts = Vec::new();
    let mut att_uuids: Vec<String> = Vec::new();
    let mut att_file_map: Vec<(String, String)> = Vec::new(); // (key, uuid_filename)
    for key in &att_keys {
        let att_uuid = ids.uuid();
        let mut rec = Vec::new();
        write_bytes_field(&mut rec, 1, att_uuid.as_bytes());
        write_bytes_field(&mut rec, 2, format!("attachments/{att_uuid}").as_bytes());
        write_record(&mut index_atts, &rec);
        att_uuids.push(att_uuid.clone());
        att_file_map.push((key.clone(), att_uuid));
    }

    let events = build_events(pages, doc, &mut ids);

    let mut index_search = Vec::new();
    let mut search_blobs: Vec<(String, Vec<u8>)> = Vec::new();
    for uuid in &page_uuids {
        let search_uuid = ids.uuid();
        let mut rec = Vec::new();
        write_bytes_field(&mut rec, 1, uuid.as_bytes());
        write_bytes_field(&mut rec, 2, format!("search/{search_uuid}").as_bytes());
        write_record(&mut index_search, &rec);
        let mut stub = Vec::new();
        write_varint_field(&mut stub, 2, 1);
        write_bytes_field(&mut stub, 3, b"");
        let mut framed = Vec::new();
        write_record(&mut framed, &stub);
        search_blobs.push((search_uuid, framed));
    }

    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));

    zip.start_file("schema.pb", opts)?;
    zip.write_all(&schema)?;

    zip.start_file("document.info.pb", opts)?;

    zip.start_file("index.notes.pb", opts)?;
    zip.write_all(&index_notes)?;

    zip.start_file("index.attachments.pb", opts)?;
    zip.write_all(&index_atts)?;

    zip.start_file("index.events.pb", opts)?;
    zip.write_all(&events)?;

    zip.start_file("index.search.pb", opts)?;
    zip.write_all(&index_search)?;

    for (uuid, blob) in &search_blobs {
        zip.start_file(format!("search/{uuid}"), opts)?;
        zip.write_all(blob)?;
    }

    for (uuid, blob) in page_uuids.iter().zip(&page_blobs) {
        zip.start_file(format!("notes/{uuid}"), opts)?;
        zip.write_all(blob)?;
    }

    for (key, att_uuid) in &att_file_map {
        if let Some(bytes) = doc.attachments.get(key) {
            zip.start_file(format!("attachments/{att_uuid}"), opts)?;
            zip.write_all(bytes)?;
        }
    }

    zip.start_file("thumbnail.jpg", opts)?;
    zip.write_all(&tiny_jpg())?;

    let cursor = zip.finish().context("finish goodnotes zip")?;
    w.write_all(&cursor.into_inner())?;
    Ok(())
}

/// Convenience wrapper: write a `.goodnotes` archive and return the bytes.
pub fn write_goodnotes_to_vec(pages: &[Page], doc: &Document) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_goodnotes(pages, doc, &mut out)?;
    Ok(out)
}

/// A minimal 1×1 white JPEG used as a placeholder thumbnail.
fn tiny_jpg() -> Vec<u8> {
    // Minimal valid JPEG (1x1 white pixel).
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, //
        0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, //
        0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, //
        0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, //
        0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20, //
        0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, //
        0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32, //
        0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, //
        0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x03, 0xFF, 0xC4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, //
        0x3F, 0x00, 0x7F, 0xFF, 0xD9,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Item, Page, Stroke};

    #[test]
    fn uuid_format_is_valid() {
        let mut g = IdGen::new();
        let u = g.uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.chars().filter(|c| *c == '-').count(), 4);
        assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn decrement_last_byte_wraps() {
        assert_eq!(decrement_last_byte("AAAA-0001"), "AAAA-0000");
        assert_eq!(decrement_last_byte("AAAA-0000"), "AAAA-00FF");
        assert_eq!(decrement_last_byte("AAAA-5E00"), "AAAA-5EFF");
        assert_eq!(decrement_last_byte("AAAA-01"), "AAAA-00");
        // Single-char last segment: unchanged (matches uuid_prev returning None).
        assert_eq!(decrement_last_byte("AAAA-0"), "AAAA-0");
    }

    #[test]
    fn empty_document_writes() {
        let doc = Document::empty();
        let pages = vec![Page {
            uuid: "test".into(),
            items: vec![],
            width: 612.0,
            height: 792.0,
        }];
        let mut out = Vec::new();
        write_goodnotes(&pages, &doc, &mut out).unwrap();
        assert!(out.len() > 100);
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(out)).unwrap();
        assert!(z.by_name("schema.pb").is_ok());
        assert!(z.by_name("index.notes.pb").is_ok());
    }

    #[test]
    fn stroke_round_trips_through_reader() {
        let doc = Document::empty();
        let stroke = Stroke {
            points: vec![[10.0, 20.0], [30.0, 40.0], [50.0, 60.0]],
            width: 2.0,
            rgba: [1.0, 0.0, 0.0, 1.0],
            dash: None,
            parented: false,
        };
        let pages = vec![Page {
            uuid: "p0".into(),
            items: vec![Item::Stroke(stroke)],
            width: 612.0,
            height: 792.0,
        }];
        let mut out = Vec::new();
        write_goodnotes(&pages, &doc, &mut out).unwrap();

        let reopened = Document::from_bytes(out).unwrap();
        assert_eq!(reopened.pages.len(), 1);
        let decoded = reopened.decode_page(&reopened.pages[0], false).unwrap();
        let strokes: Vec<_> = decoded
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Stroke(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(strokes.len(), 1);
        assert!((strokes[0].width - 2.0).abs() < 0.01);
        assert_eq!(strokes[0].points.len(), 3);
        assert!((strokes[0].points[0][0] - 10.0).abs() < 0.01);
        assert!((strokes[0].points[2][1] - 60.0).abs() < 0.01);
    }
}
