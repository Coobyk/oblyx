use std::io::Cursor;

use anyhow::{Context, Result};
use tiny_skia::{
    BlendMode, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash,
    Transform,
};

use crate::doc::{
    Background, Document, FillPathEl, ImageEl, Item, PAGE_H, PAGE_W, Page, PathEl, ShapeEl,
    ShapeKind, Sticky, Stroke as StrokeData, TextBox,
};
use crate::geom;
use crate::render::text::draw_text_line;

type RgbaImage = (u32, u32, Vec<u8>);

fn decode_jpeg(bytes: &[u8]) -> Option<RgbaImage> {
    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    dec.read_info().ok()?;
    let (w, h) = (dec.info()?.width, dec.info()?.height);
    drop(dec);
    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    let raw = dec.decode().ok()?;
    let format = dec.info()?.pixel_format;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    match format {
        jpeg_decoder::PixelFormat::RGB24 => {
            for px in raw.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        jpeg_decoder::PixelFormat::L8 => {
            for &v in &raw {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        _ => {
            for px in raw.chunks_exact(4) {
                let (c0, c1, c2, k) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
                rgba.push((c0 * k / 255) as u8);
                rgba.push((c1 * k / 255) as u8);
                rgba.push((c2 * k / 255) as u8);
                rgba.push(255);
            }
        }
    }
    Some((w as u32, h as u32, rgba))
}

fn decode_png(bytes: &[u8]) -> Option<RgbaImage> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let buf_size = reader.output_buffer_size().unwrap_or(0);
    if buf_size == 0 {
        return None;
    }
    let mut buf = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let ct = reader.output_color_type().0;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    match ct {
        png::ColorType::Rgb => {
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        png::ColorType::Rgba => {
            rgba.extend_from_slice(&buf);
        }
        png::ColorType::Grayscale => {
            for &v in &buf {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for px in buf.chunks_exact(2) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
        }
        png::ColorType::Indexed => return None,
    }
    Some((w, h, rgba))
}

pub(crate) fn decode_rgba_image(bytes: &[u8]) -> Option<RgbaImage> {
    if bytes.len() > 3 && bytes[..3] == [0xff, 0xd8, 0xff] {
        decode_jpeg(bytes)
    } else if bytes.len() > 4 && &bytes[..4] == b"\x89PNG" {
        decode_png(bytes)
    } else {
        None
    }
}

fn sample_bilinear(data: &[u8], w: i32, h: i32, x: f32, y: f32) -> [f32; 4] {
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let cx0 = x0.clamp(0, w - 1);
    let cy0 = y0.clamp(0, h - 1);
    let cx1 = (x0 + 1).clamp(0, w - 1);
    let cy1 = (y0 + 1).clamp(0, h - 1);
    let mut out = [0.0f32; 4];
    for ch in 0..4 {
        let p00 = data[((cy0 * w + cx0) as usize) * 4 + ch] as f32;
        let p10 = data[((cy0 * w + cx1) as usize) * 4 + ch] as f32;
        let p01 = data[((cy1 * w + cx0) as usize) * 4 + ch] as f32;
        let p11 = data[((cy1 * w + cx1) as usize) * 4 + ch] as f32;
        let top = p00 + (p10 - p00) * tx;
        let bot = p01 + (p11 - p01) * tx;
        out[ch] = top + (bot - top) * ty;
    }
    out
}

fn draw_image(pm: &mut Pixmap, im: &ImageEl, bytes: &[u8], scale: f32) {
    let Some((sw, sh, rgba)) = decode_rgba_image(bytes) else {
        return;
    };
    let sw = sw as i32;
    let sh = sh as i32;
    if sw <= 0 || sh <= 0 {
        return;
    }
    let x0 = im.x * scale;
    let y0 = im.y * scale;
    let dw = im.w * scale;
    let dh = im.h * scale;
    if dw < 0.5 || dh < 0.5 {
        return;
    }
    let pw = pm.width() as i32;
    let ph = pm.height() as i32;
    let px_start = x0.floor() as i32;
    let px_end = (x0 + dw).ceil() as i32;
    let py_start = y0.floor() as i32;
    let py_end = (y0 + dh).ceil() as i32;
    let data = pm.data_mut();

    for py in py_start.max(0)..py_end.min(ph) {
        let fy = ((py as f32 + 0.5) - y0) / dh * sh as f32 - 0.5;
        for px in px_start.max(0)..px_end.min(pw) {
            let fx = ((px as f32 + 0.5) - x0) / dw * sw as f32 - 0.5;
            let s = sample_bilinear(&rgba, sw, sh, fx, fy);
            let a = s[3].clamp(0.0, 255.0);
            let idx = ((py as usize) * (pw as usize) + px as usize) * 4;
            if a >= 254.5 {
                data[idx] = s[0].round() as u8;
                data[idx + 1] = s[1].round() as u8;
                data[idx + 2] = s[2].round() as u8;
                data[idx + 3] = 255;
            } else {
                let af = a / 255.0;
                let inv = 1.0 - af;
                data[idx] = (s[0] * af + data[idx] as f32 * inv).round() as u8;
                data[idx + 1] = (s[1] * af + data[idx + 1] as f32 * inv).round() as u8;
                data[idx + 2] = (s[2] * af + data[idx + 2] as f32 * inv).round() as u8;
                data[idx + 3] = (255.0 * af + data[idx + 3] as f32 * inv).round().min(255.0) as u8;
            }
        }
    }
}

fn draw_stroke(pm: &mut Pixmap, st: &StrokeData, scale: f32) {
    if st.points.len() < 2 {
        return;
    }
    let width = if st.width.is_finite() && st.width > 0.0 {
        st.width
    } else {
        1.5
    };
    let mut pb = PathBuilder::new();
    let first = &st.points[0];
    pb.move_to(first[0] * scale, first[1] * scale);
    for seg in geom::smooth(&st.points, geom::MAX_GAP) {
        match seg {
            geom::Seg::Line(p) => pb.line_to(p[0] * scale, p[1] * scale),
            geom::Seg::Curve(c1, c2, p) => pb.cubic_to(
                c1[0] * scale,
                c1[1] * scale,
                c2[0] * scale,
                c2[1] * scale,
                p[0] * scale,
                p[1] * scale,
            ),
        }
    }
    let Some(path) = pb.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    if st.rgba[3] < 0.95 {
        paint.blend_mode = BlendMode::Multiply;
    }
    paint.set_color_rgba8(
        (st.rgba[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (st.rgba[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (st.rgba[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        (st.rgba[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    );
    let stroke = Stroke {
        width: (width * scale).max(0.5),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        dash: st
            .dash
            .and_then(|d| StrokeDash::new(vec![d[0] * scale, d[1] * scale], 0.0)),
        ..Stroke::default()
    };
    pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn draw_sticky(pm: &mut Pixmap, st: &Sticky, scale: f32) {
    let x = st.x * scale;
    let y = st.y * scale;
    let w = st.w * scale;
    let h = st.h * scale;
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color_rgba8(
        (st.rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (st.rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (st.rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        255,
    );
    pm.fill_rect(rect, &paint, Transform::identity(), None);
}

fn draw_text(pm: &mut Pixmap, t: &TextBox, scale: f32) {
    draw_text_line(
        pm,
        &t.text,
        t.x * scale,
        t.y * scale,
        t.size * scale,
        t.rgb,
        1.0,
        t.bold,
    );
}

fn color_paint(rgba: [f32; 4]) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color_rgba8(
        (rgba[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgba[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgba[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgba[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    );
    paint
}

fn draw_background(pm: &mut Pixmap, bg: &Background, scale: f32) {
    for (x, y, w, h, rgb) in &bg.rects {
        let (x, y, w, h) = (x * scale, y * scale, w * scale, h * scale);
        let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) else {
            continue;
        };
        let paint = color_paint([rgb[0], rgb[1], rgb[2], 1.0]);
        pm.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

fn draw_shape(pm: &mut Pixmap, sh: &ShapeEl, scale: f32) {
    let mut pb = PathBuilder::new();
    match sh.kind {
        ShapeKind::Rect if sh.radius > 0.01 => {
            let r = sh.radius.min(sh.w * 0.5).min(sh.h * 0.5);
            let (cs, sn) = (sh.rotation.cos(), sh.rotation.sin());
            let map = |x: f32, y: f32| {
                let (dx, dy) = (x - sh.x, y - sh.y);
                (
                    (sh.x + dx * cs - dy * sn) * scale,
                    (sh.y + dx * sn + dy * cs) * scale,
                )
            };
            const K: f32 = 0.552_284_7;
            let (x, y, w, h) = (sh.x, sh.y, sh.w, sh.h);
            let (sx, sy) = map(x + r, y);
            pb.move_to(sx, sy);
            let (ax, ay) = map(x + w - r, y);
            pb.line_to(ax, ay);
            let (a, b) = map(x + w - r + K * r, y);
            let (d, e) = map(x + w, y + r - K * r);
            let (g, hh) = map(x + w, y + r);
            pb.cubic_to(a, b, d, e, g, hh);
            let (ax, ay) = map(x + w, y + h - r);
            pb.line_to(ax, ay);
            let (a, b) = map(x + w, y + h - r + K * r);
            let (d, e) = map(x + w - r + K * r, y + h);
            let (g, hh) = map(x + w - r, y + h);
            pb.cubic_to(a, b, d, e, g, hh);
            let (ax, ay) = map(x + r, y + h);
            pb.line_to(ax, ay);
            let (a, b) = map(x + r - K * r, y + h);
            let (d, e) = map(x, y + h - r + K * r);
            let (g, hh) = map(x, y + h - r);
            pb.cubic_to(a, b, d, e, g, hh);
            let (ax, ay) = map(x, y + r);
            pb.line_to(ax, ay);
            let (a, b) = map(x, y + r - K * r);
            let (d, e) = map(x + r - K * r, y);
            let (g, hh) = map(x + r, y);
            pb.cubic_to(a, b, d, e, g, hh);
            pb.close();
        }
        ShapeKind::Rect | ShapeKind::Triangle | ShapeKind::Diamond => {
            let base: Vec<[f32; 2]> = match sh.kind {
                ShapeKind::Triangle => vec![
                    [sh.x + sh.w * 0.5, sh.y],
                    [sh.x + sh.w, sh.y + sh.h],
                    [sh.x, sh.y + sh.h],
                ],
                ShapeKind::Diamond => vec![
                    [sh.x + sh.w * 0.5, sh.y],
                    [sh.x + sh.w, sh.y + sh.h * 0.5],
                    [sh.x + sh.w * 0.5, sh.y + sh.h],
                    [sh.x, sh.y + sh.h * 0.5],
                ],
                _ => vec![
                    [sh.x, sh.y],
                    [sh.x + sh.w, sh.y],
                    [sh.x + sh.w, sh.y + sh.h],
                    [sh.x, sh.y + sh.h],
                ],
            };
            let (cs, sn) = (sh.rotation.cos(), sh.rotation.sin());
            let mut iter = base.iter().map(|p| {
                let (dx, dy) = (p[0] - sh.x, p[1] - sh.y);
                (
                    (sh.x + dx * cs - dy * sn) * scale,
                    (sh.y + dx * sn + dy * cs) * scale,
                )
            });
            let Some((x0, y0)) = iter.next() else {
                return;
            };
            pb.move_to(x0, y0);
            for (x, y) in iter {
                pb.line_to(x, y);
            }
            pb.close();
        }
        ShapeKind::Ellipse => {
            let (cx, cy, rx, ry) = (sh.x, sh.y, sh.w, sh.h);
            let (cs, sn) = (sh.rotation.cos(), sh.rotation.sin());
            let map = |x: f32, y: f32| {
                (
                    (cx + x * cs - y * sn) * scale,
                    (cy + x * sn + y * cs) * scale,
                )
            };
            const K: f32 = 0.552_284_7;
            const Q: f32 = std::f32::consts::FRAC_PI_2;
            let (sx, sy) = map(rx, 0.0);
            pb.move_to(sx, sy);
            let mut a0 = 0.0f32;
            for _ in 0..4 {
                let a1 = a0 + Q;
                let (c0, s0) = (a0.cos(), a0.sin());
                let (c1, s1) = (a1.cos(), a1.sin());
                let (x1, y1) = map(rx * c0 - K * rx * s0, ry * s0 + K * ry * c0);
                let (x2, y2) = map(rx * c1 + K * rx * s1, ry * s1 - K * ry * c1);
                let (x3, y3) = map(rx * c1, ry * s1);
                pb.cubic_to(x1, y1, x2, y2, x3, y3);
                a0 = a1;
            }
            pb.close();
        }
        ShapeKind::Polygon => {
            let mut iter = sh.points.iter();
            let Some(first) = iter.next() else {
                return;
            };
            pb.move_to(first[0] * scale, first[1] * scale);
            for p in iter {
                pb.line_to(p[0] * scale, p[1] * scale);
            }
            pb.close();
        }
    }
    let Some(path) = pb.finish() else {
        return;
    };
    if sh.fill[3] >= 0.004 {
        let paint = color_paint(sh.fill);
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if sh.stroke[3] >= 0.004 && sh.width > 0.0 {
        let paint = color_paint(sh.stroke);
        let stroke = Stroke {
            width: (sh.width * scale).max(0.5),
            line_cap: if sh.dash.is_some() {
                LineCap::Round
            } else {
                Stroke::default().line_cap
            },
            dash: sh
                .dash
                .and_then(|d| StrokeDash::new(vec![d[0] * scale, d[1] * scale], 0.0)),
            ..Stroke::default()
        };
        pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn draw_path(pm: &mut Pixmap, p: &PathEl, scale: f32) {
    if p.points.len() >= 2 && p.width > 0.0 {
        let mut pb = PathBuilder::new();
        pb.move_to(p.points[0][0] * scale, p.points[0][1] * scale);
        for pt in &p.points[1..] {
            pb.line_to(pt[0] * scale, pt[1] * scale);
        }
        if let Some(path) = pb.finish() {
            let paint = color_paint(p.rgba);
            let stroke = Stroke {
                width: (p.width * scale).max(0.5),
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                ..Stroke::default()
            };
            pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
    if let Some(head) = p.head {
        let mut pb = PathBuilder::new();
        pb.move_to(head[0][0] * scale, head[0][1] * scale);
        pb.line_to(head[1][0] * scale, head[1][1] * scale);
        pb.line_to(head[2][0] * scale, head[2][1] * scale);
        pb.close();
        if let Some(path) = pb.finish() {
            let paint = color_paint(p.rgba);
            pm.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

fn draw_fill_path(pm: &mut Pixmap, fp: &FillPathEl, scale: f32) {
    if fp.rgba[3] < 0.004 {
        return;
    }
    let mut pb = PathBuilder::new();
    let mut any = false;
    for c in &fp.contours {
        let Some(first) = c.first() else {
            continue;
        };
        pb.move_to(first[0] * scale, first[1] * scale);
        for p in &c[1..] {
            pb.line_to(p[0] * scale, p[1] * scale);
        }
        pb.close();
        any = true;
    }
    if !any {
        return;
    }
    if let Some(path) = pb.finish() {
        let paint = color_paint(fp.rgba);
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        let stroke = Stroke {
            width: 2.0 * scale,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

pub fn page_to_png(page: &Page, doc: &Document, scale: f32) -> Result<Vec<u8>> {
    let w = (PAGE_W * scale).round().max(1.0) as u32;
    let h = (PAGE_H * scale).round().max(1.0) as u32;
    let mut pm = Pixmap::new(w, h).context("create pixmap")?;
    pm.fill(tiny_skia::Color::WHITE);

    for item in &page.items {
        match item {
            Item::Stroke(st) => draw_stroke(&mut pm, st, scale),
            Item::Image(im) => {
                if let Some(bytes) = doc.attachments.get(&im.attachment) {
                    crate::vlog!(1, "draw asset {}: {} bytes", im.attachment, bytes.len());
                    draw_image(&mut pm, im, bytes, scale);
                } else {
                    crate::vlog!(1, "asset {}: attachment missing, skipped", im.attachment);
                }
            }
            Item::Sticky(st) => draw_sticky(&mut pm, st, scale),
            Item::Text(t) => draw_text(&mut pm, t, scale),
            Item::Background(bg) => draw_background(&mut pm, bg, scale),
            Item::Shape(sh) => draw_shape(&mut pm, sh, scale),
            Item::Path(p) => draw_path(&mut pm, p, scale),
            Item::FillPath(fp) => draw_fill_path(&mut pm, fp, scale),
            Item::Connector(_) => {}
        }
    }

    Ok(pm.encode_png()?)
}
