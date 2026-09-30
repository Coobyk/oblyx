use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use zip::ZipArchive;

use crate::bv4::decompress_bv4;
use crate::pb::*;
use crate::tpl;

pub const PAGE_W: f32 = 1091.35;
pub const PAGE_H: f32 = 1543.5;

pub struct PageSource {
    pub uuid: String,
    pub data: Vec<u8>,
}

pub struct Document {
    pub schema: u64,
    pub pages: Vec<PageSource>,
    pub attachments: HashMap<String, Arc<Vec<u8>>>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Stroke(Stroke),
    Image(ImageEl),
    Sticky(Sticky),
    Text(TextBox),
}

#[derive(Debug, Clone)]
pub struct Stroke {
    pub points: Vec<[f32; 2]>,
    pub width: f32,
    pub rgba: [f32; 4],
}

#[derive(Debug, Clone)]
pub struct ImageEl {
    pub attachment: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub from_frame: bool,
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
}

pub struct Page {
    pub uuid: String,
    pub items: Vec<Item>,
}

type Zip = ZipArchive<BufReader<File>>;

fn read_member(z: &mut Zip, name: &str) -> Result<Option<Vec<u8>>> {
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

impl Document {
    pub fn open(path: &Path) -> Result<Document> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut z = Zip::new(BufReader::new(file))
            .with_context(|| format!("{} is not a readable zip archive", path.display()))?;

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
        if let Some(idx) = read_member(&mut z, "index.attachments.pb")? {
            for (uuid, member) in index_entries(&idx)? {
                if let Some(data) = read_member(&mut z, &member)? {
                    if !data.is_empty() {
                        attachments.insert(uuid, Arc::new(data));
                    }
                }
            }
        }

        Ok(Document {
            schema,
            pages,
            attachments,
        })
    }

    pub fn decode_page(&self, src: &PageSource, include_deleted: bool) -> Result<Page> {
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
                )?;
                i += 2;
            } else {
                decode_content(&keys, recs[i], &mut items, include_deleted)?;
                i += 1;
            }
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

        Ok(Page {
            uuid: src.uuid.clone(),
            items,
        })
    }
}

fn is_wrapper(keys: &[u32]) -> bool {
    keys.contains(&8)
        && keys
            .iter()
            .all(|k| matches!(k, 1 | 2 | 3 | 4 | 8 | 9 | 14 | 16 | 20))
}

fn decode_content(
    keys: &[u32],
    rec: &[u8],
    items: &mut Vec<Item>,
    include_deleted: bool,
) -> Result<()> {
    if keys == [7] {
        decode_stroke(rec, items);
        return Ok(());
    }
    if keys.len() == 1 {
        decode_element(keys[0], rec, items, include_deleted);
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

fn decode_stroke(rec: &[u8], items: &mut Vec<Item>) {
    let Ok(fs) = parse(rec) else { return };
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
    }

    let Some(blob) = bytes_of(&stroke, 2) else {
        return;
    };
    if blob.len() < 4 || (blob[..4] != *b"bv41" && blob[..4] != *b"bv4-") {
        return;
    }
    let Ok(decomp) = decompress_bv4(blob) else {
        return;
    };
    let Ok(Some(geom)) = tpl::stroke_geometry(&decomp) else {
        return;
    };
    let mut pts = tpl::polyline(&geom.starts, &geom.segments);
    if pts.len() < 2 {
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
    }));
}

fn point_of(msg: &[u8]) -> Option<(f32, f32)> {
    let f = parse(msg).ok()?;
    let x = num_as_u32(&f, 1).map(f32_bits)?;
    let y = num_as_u32(&f, 2).map(f32_bits)?;
    if x.is_finite() && y.is_finite() {
        Some((x, y))
    } else {
        None
    }
}

fn rect_xy(msg: &[u8]) -> Option<(f32, f32)> {
    let f = parse(msg).ok()?;
    point_of(bytes_of(&f, 1)?)
}

fn rect_wh(msg: &[u8]) -> Option<(f32, f32)> {
    let f = parse(msg).ok()?;
    point_of(bytes_of(&f, 2)?)
}

fn decode_element(kind: u32, rec: &[u8], items: &mut Vec<Item>, include_deleted: bool) {
    let Ok(fs) = parse(rec) else { return };
    let Some(payload) = bytes_of(&fs, kind) else {
        return;
    };
    let Ok(f) = parse(payload) else {
        return;
    };
    match kind {
        1 => decode_placement(&f, items, include_deleted),
        9 => decode_frame(&f, items),
        11 => decode_equation(&f, items),
        20 => decode_sticky(&f, items),
        21 => decode_textbox(&f, items),
        _ => {}
    }
}

fn decode_placement(f: &[Field<'_>], items: &mut Vec<Item>, include_deleted: bool) {
    if varint_of(f, 6) == Some(1) && !include_deleted {
        return;
    }
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
    }));
}

fn decode_frame(f: &[Field<'_>], items: &mut Vec<Item>) {
    let Some(att) = bytes_of(f, 5).and_then(as_uuid) else {
        return;
    };
    let Some(rect) = bytes_of(f, 2) else {
        return;
    };
    let Some(rf) = parse(rect).ok() else {
        return;
    };
    let (p1, p2) = match (
        num_as_u32(&rf, 1),
        num_as_u32(&rf, 2),
        num_as_u32(&rf, 3),
        num_as_u32(&rf, 4),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => (
            Some((f32_bits(a), f32_bits(b))),
            Some((f32_bits(c), f32_bits(d))),
        ),
        _ => (
            bytes_of(&rf, 1).and_then(point_of),
            bytes_of(&rf, 2).and_then(point_of),
        ),
    };
    let (Some((x, y)), Some((w, h))) = (p1, p2) else {
        return;
    };
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return;
    }
    items.push(Item::Image(ImageEl {
        attachment: att.to_string(),
        x,
        y,
        w,
        h,
        from_frame: true,
    }));
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

    let mut size = 24.0f32;
    let mut font: Option<String> = None;
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

    if !text.trim().is_empty() {
        items.push(Item::Text(TextBox {
            x,
            y,
            size,
            text,
            font,
        }));
    }
}
