use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::Cursor;

use crate::doc::{Document, Item, PAGE_H, PAGE_W, Page};
use crate::geom;
use crate::render::png::decode_rgba_image;

enum Obj {
    Text(String),
    Stream(String, Vec<u8>),
}

fn gs_key(alpha: f32) -> u16 {
    ((alpha * 1000.0).round() as u16).clamp(1, 949)
}

fn num(out: &mut String, v: f32) {
    if v.is_finite() {
        let _ = write!(out, "{v}");
    } else {
        out.push('0');
    }
}

fn pdf_escape(s: &str, out: &mut String) {
    out.push('(');
    for ch in s.chars() {
        match ch {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            ch if (ch as u32) < 128 => out.push(ch),
            ch if (ch as u32) <= 255 => {
                let _ = write!(out, "\\{:03o}", ch as u32);
            }
            _ => out.push('?'),
        }
    }
    out.push(')');
}

fn encode_jpeg_rgb(w: u32, h: u32, rgb: &[u8]) -> Option<Vec<u8>> {
    let w16 = u16::try_from(w).ok()?;
    let h16 = u16::try_from(h).ok()?;
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 90)
        .encode(rgb, w16, h16, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    Some(out)
}

fn prepare_image(bytes: &[u8]) -> Option<(u32, u32, bool, Vec<u8>)> {
    if bytes.len() > 3 && bytes[..3] == [0xff, 0xd8, 0xff] {
        let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
        dec.read_info().ok()?;
        let info = dec.info()?;
        let (w, h) = (info.width, info.height);
        match info.pixel_format {
            jpeg_decoder::PixelFormat::L8 => Some((w as u32, h as u32, true, bytes.to_vec())),
            jpeg_decoder::PixelFormat::RGB24 => Some((w as u32, h as u32, false, bytes.to_vec())),
            _ => {
                drop(dec);
                let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
                let raw = dec.decode().ok()?;
                let mut rgb = Vec::with_capacity((w * h * 3) as usize);
                for px in raw.chunks_exact(4) {
                    let k = px[3] as u32;
                    rgb.push((px[0] as u32 * k / 255) as u8);
                    rgb.push((px[1] as u32 * k / 255) as u8);
                    rgb.push((px[2] as u32 * k / 255) as u8);
                }
                let data = encode_jpeg_rgb(w as u32, h as u32, &rgb)?;
                Some((w as u32, h as u32, false, data))
            }
        }
    } else if bytes.len() > 4 && &bytes[..4] == b"\x89PNG" {
        let (w, h, rgba) = decode_rgba_image(bytes)?;
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for px in rgba.chunks_exact(4) {
            let a = px[3] as u32;
            let inv = 255 - a;
            rgb.push(((px[0] as u32 * a + 255 * inv) / 255) as u8);
            rgb.push(((px[1] as u32 * a + 255 * inv) / 255) as u8);
            rgb.push(((px[2] as u32 * a + 255 * inv) / 255) as u8);
        }
        let data = encode_jpeg_rgb(w, h, &rgb)?;
        Some((w, h, false, data))
    } else {
        None
    }
}

struct Ctx {
    img_obj: HashMap<String, Option<usize>>,
    gs_obj: HashMap<u16, usize>,
}

fn page_content(page: &Page, ctx: &Ctx) -> String {
    let mut s = String::with_capacity(64 * 1024);
    s.push_str("q\n1 1 1 rg\n0 0 ");
    num(&mut s, PAGE_W);
    s.push(' ');
    num(&mut s, PAGE_H);
    s.push_str(" re\nf\nQ\n");

    for item in &page.items {
        match item {
            Item::Stroke(st) => {
                if st.points.len() < 2 {
                    continue;
                }
                let width = if st.width.is_finite() && st.width > 0.0 {
                    st.width
                } else {
                    1.5
                };
                let alpha = st.rgba[3];
                s.push_str("q\n");
                if alpha < 0.95 && ctx.gs_obj.contains_key(&gs_key(alpha)) {
                    let _ = write!(s, "/GS{} gs\n", gs_key(alpha));
                }
                for c in 0..3 {
                    num(&mut s, st.rgba[c].clamp(0.0, 1.0));
                    s.push(' ');
                }
                s.push_str("RG\n");
                num(&mut s, width);
                s.push_str(" w\n1 J\n1 j\n");
                let first = &st.points[0];
                num(&mut s, first[0]);
                s.push(' ');
                num(&mut s, PAGE_H - first[1]);
                s.push_str(" m\n");
                for seg in geom::smooth(&st.points, geom::MAX_GAP) {
                    match seg {
                        geom::Seg::Line(p) => {
                            num(&mut s, p[0]);
                            s.push(' ');
                            num(&mut s, PAGE_H - p[1]);
                            s.push_str(" l\n");
                        }
                        geom::Seg::Curve(c1, c2, p) => {
                            num(&mut s, c1[0]);
                            s.push(' ');
                            num(&mut s, PAGE_H - c1[1]);
                            s.push(' ');
                            num(&mut s, c2[0]);
                            s.push(' ');
                            num(&mut s, PAGE_H - c2[1]);
                            s.push(' ');
                            num(&mut s, p[0]);
                            s.push(' ');
                            num(&mut s, PAGE_H - p[1]);
                            s.push_str(" c\n");
                        }
                    }
                }
                s.push_str("S\nQ\n");
            }
            Item::Image(im) => {
                let Some(&Some(obj)) = ctx.img_obj.get(&im.attachment) else {
                    continue;
                };
                s.push_str("q\n");
                num(&mut s, im.w);
                s.push_str(" 0 0 ");
                num(&mut s, im.h);
                s.push(' ');
                num(&mut s, im.x);
                s.push(' ');
                num(&mut s, PAGE_H - im.y - im.h);
                let _ = write!(s, " cm\n/Im{obj} Do\nQ\n");
            }
            Item::Sticky(st) => {
                s.push_str("q\n");
                for c in 0..3 {
                    num(&mut s, st.rgb[c].clamp(0.0, 1.0));
                    s.push(' ');
                }
                s.push_str("rg\n");
                num(&mut s, st.x);
                s.push(' ');
                num(&mut s, PAGE_H - st.y - st.h);
                s.push(' ');
                num(&mut s, st.w);
                s.push(' ');
                num(&mut s, st.h);
                s.push_str(" re\nf\nQ\n");
            }
            Item::Text(t) => {
                s.push_str("q\n0.1176 0.1059 0.1059 rg\nBT\n/F1 ");
                num(&mut s, t.size);
                s.push_str(" Tf\n");
                num(&mut s, t.x);
                s.push(' ');
                num(&mut s, PAGE_H - (t.y + t.size * 0.8));
                s.push_str(" Td\n");
                pdf_escape(&t.text, &mut s);
                s.push_str("\nTj\nET\nQ\n");
            }
        }
    }
    s
}

pub fn document_to_pdf(pages: &[Page], doc: &Document) -> Vec<u8> {
    let mut img_order: Vec<&String> = Vec::new();
    let mut img_seen: BTreeSet<&str> = BTreeSet::new();
    let mut gs_keys: BTreeSet<u16> = BTreeSet::new();

    for page in pages {
        for item in &page.items {
            match item {
                Item::Image(im) => {
                    if img_seen.insert(im.attachment.as_str()) {
                        img_order.push(&im.attachment);
                    }
                }
                Item::Stroke(st) if st.rgba[3] < 0.95 => {
                    gs_keys.insert(gs_key(st.rgba[3]));
                }
                _ => {}
            }
        }
    }

    let mut img_obj: HashMap<String, Option<usize>> = HashMap::new();
    let mut img_blobs: Vec<(usize, u32, u32, bool, Vec<u8>)> = Vec::new();
    let mut next_obj = 5usize;
    for att in &img_order {
        let prepared = doc
            .attachments
            .get(att.as_str())
            .and_then(|bytes| prepare_image(bytes));
        match prepared {
            Some((w, h, gray, data)) => {
                img_obj.insert(att.to_string(), Some(next_obj));
                img_blobs.push((next_obj, w, h, gray, data));
                next_obj += 1;
            }
            None => {
                img_obj.insert(att.to_string(), None);
            }
        }
    }

    let mut gs_obj: HashMap<u16, usize> = HashMap::new();
    for k in &gs_keys {
        gs_obj.insert(*k, next_obj);
        next_obj += 1;
    }

    let base = next_obj;
    let total = base + pages.len() * 2;

    let ctx = Ctx { img_obj, gs_obj };
    let contents: Vec<String> = pages.iter().map(|p| page_content(p, &ctx)).collect();

    let mut objs: Vec<Obj> = (0..total).map(|_| Obj::Text(String::new())).collect();
    objs[1] = Obj::Text("<< /Type /Catalog /Pages 2 0 R >>".into());
    let mut kids = String::new();
    for i in 0..pages.len() {
        let _ = write!(kids, "{} 0 R ", base + i * 2);
    }
    objs[2] = Obj::Text(format!(
        "<< /Type /Pages /Kids [{kids}] /Count {} >>",
        pages.len()
    ));
    objs[3] = Obj::Text(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into(),
    );

    let mut res = String::from("<< /Font << /F1 3 0 R >> /XObject <<");
    for (obj, ..) in &img_blobs {
        let _ = write!(res, " /Im{obj} {obj} 0 R");
    }
    res.push_str(" >> /ExtGState <<");
    let mut gs_entries: Vec<(u16, usize)> = ctx.gs_obj.iter().map(|(k, v)| (*k, *v)).collect();
    gs_entries.sort_unstable();
    for (k, obj) in &gs_entries {
        let _ = write!(res, " /GS{k} {obj} 0 R");
    }
    res.push_str(" >> >>");
    objs[4] = Obj::Text(res);

    for (obj, w, h, gray, data) in img_blobs {
        let cs = if gray { "DeviceGray" } else { "DeviceRGB" };
        let dict = format!(
            "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /{cs} \
             /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>",
            data.len()
        );
        objs[obj] = Obj::Stream(dict, data);
    }

    for (k, obj) in &gs_entries {
        let a = *k as f32 / 1000.0;
        objs[*obj] = Obj::Text(format!("<< /Type /ExtGState /ca {a} /CA {a} >>"));
    }

    for (i, content) in contents.iter().enumerate() {
        objs[base + i * 2] = Obj::Text(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] \
             /Resources 4 0 R /Contents {} 0 R >>",
            base + i * 2 + 1
        ));
        objs[base + i * 2 + 1] = Obj::Stream(
            format!("<< /Length {} >>", content.len()),
            content.clone().into_bytes(),
        );
    }

    let mut out: Vec<u8> = Vec::with_capacity(1 << 20);
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets: Vec<u64> = vec![0; total];
    for i in 1..total {
        offsets[i] = out.len() as u64;
        out.extend_from_slice(format!("{i} 0 obj\n").as_bytes());
        match &objs[i] {
            Obj::Text(t) => out.extend_from_slice(t.as_bytes()),
            Obj::Stream(dict, data) => {
                out.extend_from_slice(dict.as_bytes());
                out.extend_from_slice(b"\nstream\n");
                out.extend_from_slice(data);
                out.extend_from_slice(b"\nendstream");
            }
        }
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    out.extend_from_slice(format!("xref\n0 {total}\n0000000000 65535 f \n").as_bytes());
    for i in 1..total {
        out.extend_from_slice(format!("{:010} 00000 n \n", offsets[i]).as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {total} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n")
            .as_bytes(),
    );
    out
}
