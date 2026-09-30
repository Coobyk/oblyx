use std::io::Cursor;

use anyhow::{Context, Result};
use tiny_skia::{LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::doc::{
    Document, ImageEl, Item, PAGE_H, PAGE_W, Page, Sticky, Stroke as StrokeData, TextBox,
};
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
    for p in &st.points[1..] {
        pb.line_to(p[0] * scale, p[1] * scale);
    }
    let Some(path) = pb.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
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
    let baseline = (t.y + t.size * 0.8) * scale;
    draw_text_line(
        pm,
        &t.text,
        t.x * scale,
        baseline,
        t.size * scale,
        [0.1176, 0.1059, 0.1059],
        1.0,
    );
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
                    draw_image(&mut pm, im, bytes, scale);
                }
            }
            Item::Sticky(st) => draw_sticky(&mut pm, st, scale),
            Item::Text(t) => draw_text(&mut pm, t, scale),
        }
    }

    Ok(pm.encode_png()?)
}
