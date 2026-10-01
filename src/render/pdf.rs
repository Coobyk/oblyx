use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::Cursor;

use crate::doc::{Document, Item, PAGE_H, PAGE_W, Page, ShapeEl, ShapeKind};
use crate::geom;
use crate::render::png::decode_rgba_image;

enum Obj {
    Text(String),
    Stream(String, Vec<u8>),
}

fn gs_key(alpha: f32) -> u16 {
    ((alpha * 1000.0).round() as u16).clamp(1, 949)
}

fn stroke_gs_key(alpha: f32) -> u16 {
    5000 + gs_key(alpha)
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

fn poly_ops(pts: &[[f32; 2]], close: bool, out: &mut String) {
    let Some(first) = pts.first() else {
        return;
    };
    num(out, first[0]);
    out.push(' ');
    num(out, PAGE_H - first[1]);
    out.push_str(" m\n");
    for p in &pts[1..] {
        num(out, p[0]);
        out.push(' ');
        num(out, PAGE_H - p[1]);
        out.push_str(" l\n");
    }
    if close {
        out.push_str("h\n");
    }
}

fn ellipse_ops(sh: &ShapeEl, out: &mut String) {
    let (cx, cy, rx, ry) = (sh.x, sh.y, sh.w, sh.h);
    let (cs, sn) = (sh.rotation.cos(), sh.rotation.sin());
    let map = |x: f32, y: f32| -> [f32; 2] { [cx + x * cs - y * sn, cy + x * sn + y * cs] };
    const K: f32 = 0.552_284_7;
    const Q: f32 = std::f32::consts::FRAC_PI_2;
    let start = map(rx, 0.0);
    num(out, start[0]);
    out.push(' ');
    num(out, PAGE_H - start[1]);
    out.push_str(" m\n");
    let mut a0 = 0.0f32;
    for _ in 0..4 {
        let a1 = a0 + Q;
        let (c0, s0) = (a0.cos(), a0.sin());
        let (c1, s1) = (a1.cos(), a1.sin());
        let c1p = map(rx * c0 - K * rx * s0, ry * s0 + K * ry * c0);
        let c2p = map(rx * c1 + K * rx * s1, ry * s1 - K * ry * c1);
        let p1 = map(rx * c1, ry * s1);
        num(out, c1p[0]);
        out.push(' ');
        num(out, PAGE_H - c1p[1]);
        out.push(' ');
        num(out, c2p[0]);
        out.push(' ');
        num(out, PAGE_H - c2p[1]);
        out.push(' ');
        num(out, p1[0]);
        out.push(' ');
        num(out, PAGE_H - p1[1]);
        out.push_str(" c\n");
        a0 = a1;
    }
    out.push_str("h\n");
}

fn round_rect_ops(sh: &ShapeEl, out: &mut String) {
    let r = sh.radius.min(sh.w * 0.5).min(sh.h * 0.5);
    let (cs, sn) = (sh.rotation.cos(), sh.rotation.sin());
    let map = |x: f32, y: f32| -> [f32; 2] {
        let (dx, dy) = (x - sh.x, y - sh.y);
        [sh.x + dx * cs - dy * sn, sh.y + dx * sn + dy * cs]
    };
    const K: f32 = 0.552_284_7;
    let (x, y, w, h) = (sh.x, sh.y, sh.w, sh.h);
    let pt = map(x + r, y);
    num(out, pt[0]);
    out.push(' ');
    num(out, PAGE_H - pt[1]);
    out.push_str(" m\n");
    let line = |ax: f32, ay: f32, out: &mut String| {
        let p = map(ax, ay);
        num(out, p[0]);
        out.push(' ');
        num(out, PAGE_H - p[1]);
        out.push_str(" l\n");
    };
    let curve = |p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], out: &mut String| {
        let a = map(p1[0], p1[1]);
        let b = map(p2[0], p2[1]);
        let c = map(p3[0], p3[1]);
        for q in [a, b, c] {
            num(out, q[0]);
            out.push(' ');
            num(out, PAGE_H - q[1]);
            out.push(' ');
        }
        out.push_str("c\n");
    };
    line(x + w - r, y, out);
    curve(
        [x + w - r + K * r, y],
        [x + w, y + r - K * r],
        [x + w, y + r],
        out,
    );
    line(x + w, y + h - r, out);
    curve(
        [x + w, y + h - r + K * r],
        [x + w - r + K * r, y + h],
        [x + w - r, y + h],
        out,
    );
    line(x + r, y + h, out);
    curve(
        [x + r - K * r, y + h],
        [x, y + h - r + K * r],
        [x, y + h - r],
        out,
    );
    line(x, y + r, out);
    curve([x, y + r - K * r], [x + r - K * r, y], [x + r, y], out);
    out.push_str("h\n");
}

fn shape_ops(sh: &ShapeEl) -> String {
    let mut s = String::new();
    match sh.kind {
        ShapeKind::Rect => {
            if sh.radius > 0.01 {
                round_rect_ops(sh, &mut s);
            } else {
                let (cs, sn) = (sh.rotation.cos(), sh.rotation.sin());
                let corners = [
                    [sh.x, sh.y],
                    [sh.x + sh.w, sh.y],
                    [sh.x + sh.w, sh.y + sh.h],
                    [sh.x, sh.y + sh.h],
                ];
                let pts: Vec<[f32; 2]> = corners
                    .iter()
                    .map(|p| {
                        let (dx, dy) = (p[0] - sh.x, p[1] - sh.y);
                        [sh.x + dx * cs - dy * sn, sh.y + dx * sn + dy * cs]
                    })
                    .collect();
                poly_ops(&pts, true, &mut s);
            }
        }
        ShapeKind::Ellipse => ellipse_ops(sh, &mut s),
        ShapeKind::Polygon => poly_ops(&sh.points, true, &mut s),
        ShapeKind::Triangle | ShapeKind::Diamond => {
            let pts: Vec<[f32; 2]> = if matches!(sh.kind, ShapeKind::Triangle) {
                vec![
                    [sh.x + sh.w * 0.5, sh.y],
                    [sh.x + sh.w, sh.y + sh.h],
                    [sh.x, sh.y + sh.h],
                ]
            } else {
                vec![
                    [sh.x + sh.w * 0.5, sh.y],
                    [sh.x + sh.w, sh.y + sh.h * 0.5],
                    [sh.x + sh.w * 0.5, sh.y + sh.h],
                    [sh.x, sh.y + sh.h * 0.5],
                ]
            };
            let (cs, sn) = (sh.rotation.cos(), sh.rotation.sin());
            let rot: Vec<[f32; 2]> = pts
                .iter()
                .map(|p| {
                    let (dx, dy) = (p[0] - sh.x, p[1] - sh.y);
                    [sh.x + dx * cs - dy * sn, sh.y + dx * sn + dy * cs]
                })
                .collect();
            poly_ops(&rot, true, &mut s);
        }
    }
    s
}

fn gs_emitted(out: &mut String, alpha: f32, ctx: &Ctx) {
    if alpha < 0.95 && ctx.gs_obj.contains_key(&gs_key(alpha)) {
        let _ = write!(out, "/GS{} gs\n", gs_key(alpha));
    }
}

fn page_content(page: &Page, ctx: &Ctx) -> String {
    let mut s = String::with_capacity(64 * 1024);
    s.push_str("q\n0.5454567 0 0 0.5454567 0 0 cm\n");
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
                if alpha < 0.95 && ctx.gs_obj.contains_key(&stroke_gs_key(alpha)) {
                    let _ = write!(s, "/GS{} gs\n", stroke_gs_key(alpha));
                }
                for c in 0..3 {
                    num(&mut s, st.rgba[c].clamp(0.0, 1.0));
                    s.push(' ');
                }
                s.push_str("RG\n");
                num(&mut s, width);
                s.push_str(" w\n1 J\n1 j\n");
                if let Some(d) = st.dash {
                    s.push('[');
                    num(&mut s, d[0]);
                    s.push(' ');
                    num(&mut s, d[1]);
                    s.push_str("] 0 d\n");
                }
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
                s.push_str("q\n");
                for c in 0..3 {
                    num(&mut s, t.rgb[c]);
                    s.push(' ');
                }
                s.push_str("rg\nBT\n");
                s.push_str(if t.bold { "/F2 " } else { "/F1 " });
                num(&mut s, t.size);
                s.push_str(" Tf\n");
                num(&mut s, t.x);
                s.push(' ');
                num(&mut s, PAGE_H - t.y);
                s.push_str(" Td\n");
                pdf_escape(&t.text, &mut s);
                s.push_str("\nTj\nET\nQ\n");
            }
            Item::Background(bg) => {
                for (x, y, w, h, rgb) in &bg.rects {
                    s.push_str("q\n");
                    for c in 0..3 {
                        num(&mut s, rgb[c].clamp(0.0, 1.0));
                        s.push(' ');
                    }
                    s.push_str("rg\n");
                    num(&mut s, *x);
                    s.push(' ');
                    num(&mut s, PAGE_H - y - h);
                    s.push(' ');
                    num(&mut s, *w);
                    s.push(' ');
                    num(&mut s, *h);
                    s.push_str(" re\nf\nQ\n");
                }
            }
            Item::Shape(sh) => {
                let ops = shape_ops(sh);
                if sh.fill[3] >= 0.004 {
                    s.push_str("q\n");
                    gs_emitted(&mut s, sh.fill[3], ctx);
                    for c in 0..3 {
                        num(&mut s, sh.fill[c].clamp(0.0, 1.0));
                        s.push(' ');
                    }
                    s.push_str("rg\n");
                    s.push_str(&ops);
                    s.push_str("f\nQ\n");
                }
                if sh.stroke[3] >= 0.004 && sh.width > 0.0 {
                    s.push_str("q\n");
                    gs_emitted(&mut s, sh.stroke[3], ctx);
                    for c in 0..3 {
                        num(&mut s, sh.stroke[c].clamp(0.0, 1.0));
                        s.push(' ');
                    }
                    s.push_str("RG\n");
                    num(&mut s, sh.width);
                    s.push_str(" w\n");
                    if let Some(d) = sh.dash {
                        s.push_str("1 J\n");
                        s.push('[');
                        num(&mut s, d[0]);
                        s.push(' ');
                        num(&mut s, d[1]);
                        s.push_str("] 0 d\n");
                    }
                    s.push_str(&ops);
                    s.push_str("S\nQ\n");
                }
            }
            Item::Path(p) => {
                if p.points.len() >= 2 && p.width > 0.0 {
                    s.push_str("q\n");
                    gs_emitted(&mut s, p.rgba[3], ctx);
                    for c in 0..3 {
                        num(&mut s, p.rgba[c].clamp(0.0, 1.0));
                        s.push(' ');
                    }
                    s.push_str("RG\n");
                    num(&mut s, p.width);
                    s.push_str(" w\n1 J\n1 j\n");
                    poly_ops(&p.points, false, &mut s);
                    s.push_str("S\nQ\n");
                }
                if let Some(head) = p.head {
                    s.push_str("q\n");
                    for c in 0..3 {
                        num(&mut s, p.rgba[c].clamp(0.0, 1.0));
                        s.push(' ');
                    }
                    s.push_str("rg\n");
                    poly_ops(&head, true, &mut s);
                    s.push_str("f\nQ\n");
                }
            }
            Item::FillPath(fp) => {
                if fp.rgba[3] >= 0.004 {
                    s.push_str("q\n");
                    gs_emitted(&mut s, fp.rgba[3], ctx);
                    for c in 0..3 {
                        num(&mut s, fp.rgba[c].clamp(0.0, 1.0));
                        s.push(' ');
                    }
                    s.push_str("rg\n");
                    for c in 0..3 {
                        num(&mut s, fp.rgba[c].clamp(0.0, 1.0));
                        s.push(' ');
                    }
                    s.push_str("RG\n2 w\n1 J\n1 j\n");
                    for c in &fp.contours {
                        poly_ops(c, true, &mut s);
                    }
                    s.push_str("B\nQ\n");
                }
            }
            Item::Connector(_) => {}
        }
    }
    s.push_str("Q\n");
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
                    gs_keys.insert(stroke_gs_key(st.rgba[3]));
                }
                Item::Shape(sh) => {
                    if sh.fill[3] < 0.95 {
                        gs_keys.insert(gs_key(sh.fill[3]));
                    }
                    if sh.stroke[3] < 0.95 {
                        gs_keys.insert(gs_key(sh.stroke[3]));
                    }
                }
                Item::Path(p) if p.rgba[3] < 0.95 => {
                    gs_keys.insert(gs_key(p.rgba[3]));
                }
                Item::FillPath(fp) if fp.rgba[3] < 0.95 => {
                    gs_keys.insert(gs_key(fp.rgba[3]));
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

    let bold_obj = next_obj;
    next_obj += 1;

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
    objs[bold_obj] = Obj::Text(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
            .into(),
    );

    let mut res = format!("<< /Font << /F1 3 0 R /F2 {bold_obj} 0 R >> /XObject <<");
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
        if *k >= 5000 {
            let a = (*k - 5000) as f32 / 1000.0;
            objs[*obj] = Obj::Text(format!(
                "<< /Type /ExtGState /ca {a} /CA {a} /BM /Multiply >>"
            ));
        } else {
            let a = *k as f32 / 1000.0;
            objs[*obj] = Obj::Text(format!("<< /Type /ExtGState /ca {a} /CA {a} >>"));
        }
    }

    for (i, content) in contents.iter().enumerate() {
        objs[base + i * 2] = Obj::Text(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595.28 841.89] \
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
