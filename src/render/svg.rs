use std::fmt::Write as _;

use base64::Engine as _;

use crate::doc::{Document, Item, PAGE_H, PAGE_W, Page};
use crate::render::image_mime;

fn color_css(rgba: &[f32; 4]) -> String {
    let r = (rgba[0] * 255.0).round() as u8;
    let g = (rgba[1] * 255.0).round() as u8;
    let b = (rgba[2] * 255.0).round() as u8;
    if rgba[3] < 0.95 {
        format!("rgba({r},{g},{b},{:.3})", rgba[3])
    } else {
        format!("#{r:02x}{g:02x}{b:02x}")
    }
}

fn rgb_css(rgb: &[f32; 3]) -> String {
    let r = (rgb[0] * 255.0).round() as u8;
    let g = (rgb[1] * 255.0).round() as u8;
    let b = (rgb[2] * 255.0).round() as u8;
    format!("rgb({r},{g},{b})")
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

pub fn page_to_svg(page: &Page, doc: &Document) -> String {
    let mut s = String::with_capacity(64 * 1024);
    let _ = write!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{PAGE_W}\" height=\"{PAGE_H}\" \
         viewBox=\"0 0 {PAGE_W} {PAGE_H}\"><rect width=\"{PAGE_W}\" height=\"{PAGE_H}\" fill=\"#ffffff\"/>"
    );
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
                s.push_str("<path d=\"M ");
                let _ = write!(s, "{} {}", st.points[0][0], st.points[0][1]);
                for p in &st.points[1..] {
                    let _ = write!(s, " L {} {}", p[0], p[1]);
                }
                let _ = write!(
                    s,
                    "\" fill=\"none\" stroke=\"{}\" stroke-width=\"{width}\" \
                     stroke-linecap=\"round\" stroke-linejoin=\"round\"/>",
                    color_css(&st.rgba)
                );
            }
            Item::Image(im) => {
                let Some(bytes) = doc.attachments.get(&im.attachment) else {
                    continue;
                };
                let Some(mime) = image_mime(bytes) else {
                    continue;
                };
                let b64 = base64::engine::general_purpose::STANDARD.encode(bytes.as_ref());
                let _ = write!(
                    s,
                    "<image x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" \
                     href=\"data:{mime};base64,{b64}\"/>",
                    im.x, im.y, im.w, im.h
                );
            }
            Item::Sticky(st) => {
                let _ = write!(
                    s,
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"8\" fill=\"{}\"/>",
                    st.x,
                    st.y,
                    st.w,
                    st.h,
                    rgb_css(&st.rgb)
                );
            }
            Item::Text(t) => {
                let baseline = t.y + t.size * 0.8;
                let family = t
                    .font
                    .clone()
                    .unwrap_or_else(|| "Helvetica Neue, Helvetica, Arial, sans-serif".to_string());
                let _ = write!(
                    s,
                    "<text x=\"{}\" y=\"{baseline}\" font-family=\"{}\" font-size=\"{}\" \
                     fill=\"#1e1b1b\">{}</text>",
                    t.x,
                    escape_xml(&family),
                    t.size,
                    escape_xml(&t.text)
                );
            }
        }
    }
    s.push_str("</svg>");
    s
}
