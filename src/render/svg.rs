use std::fmt::Write as _;

use base64::Engine as _;

use crate::doc::{Document, Item, PAGE_H, PAGE_W, Page, ShapeKind};
use crate::geom;
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
                for seg in geom::smooth(&st.points, geom::MAX_GAP) {
                    match seg {
                        geom::Seg::Line(p) => {
                            let _ = write!(s, " L {} {}", p[0], p[1]);
                        }
                        geom::Seg::Curve(c1, c2, p) => {
                            let _ = write!(
                                s,
                                " C {} {} {} {} {} {}",
                                c1[0], c1[1], c2[0], c2[1], p[0], p[1]
                            );
                        }
                    }
                }
                let dash_attr = st
                    .dash
                    .map(|d| format!(" stroke-dasharray=\"{} {}\"", d[0], d[1]))
                    .unwrap_or_default();
                let blend_attr = if st.rgba[3] < 0.95 {
                    " style=\"mix-blend-mode:multiply\""
                } else {
                    ""
                };
                let _ = write!(
                    s,
                    "\" fill=\"none\" stroke=\"{}\" stroke-width=\"{width}\" \
                     stroke-linecap=\"round\" stroke-linejoin=\"round\"{dash_attr}{blend_attr}/>",
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
                let family = t
                    .font
                    .clone()
                    .unwrap_or_else(|| "Helvetica Neue, Helvetica, Arial, sans-serif".to_string());
                let rgb = rgb_css(&t.rgb);
                let weight = if t.bold { " font-weight=\"bold\"" } else { "" };
                let _ = write!(
                    s,
                    "<text x=\"{}\" y=\"{}\" font-family=\"{}\" font-size=\"{}\"{weight} \
                     fill=\"{rgb}\">{}</text>",
                    t.x,
                    t.y,
                    escape_xml(&family),
                    t.size,
                    escape_xml(&t.text)
                );
            }
            Item::Background(bg) => {
                for (x, y, w, h, rgb) in &bg.rects {
                    let _ = write!(
                        s,
                        "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{}\"/>",
                        rgb_css(rgb)
                    );
                }
            }
            Item::Shape(sh) => {
                let fill = if sh.fill[3] < 0.004 {
                    "none".to_string()
                } else {
                    color_css(&sh.fill)
                };
                let stroke = if sh.stroke[3] < 0.004 {
                    "none".to_string()
                } else {
                    color_css(&sh.stroke)
                };
                let deg = sh.rotation.to_degrees();
                let rot = if deg.abs() > 1e-4 {
                    format!(" transform=\"rotate({deg} {} {})\"", sh.x, sh.y)
                } else {
                    String::new()
                };
                let dash_attr = sh
                    .dash
                    .map(|d| {
                        format!(
                            " stroke-dasharray=\"{} {}\" stroke-linecap=\"round\"",
                            d[0], d[1]
                        )
                    })
                    .unwrap_or_default();
                match sh.kind {
                    ShapeKind::Rect => {
                        let rad = if sh.radius > 0.01 {
                            format!(" rx=\"{}\" ry=\"{}\"", sh.radius, sh.radius)
                        } else {
                            String::new()
                        };
                        let _ = write!(
                            s,
                            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" \
                             fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"{}\"{rad}{rot}{dash_attr}/>",
                            sh.x, sh.y, sh.w, sh.h, sh.width
                        );
                    }
                    ShapeKind::Ellipse => {
                        let _ = write!(
                            s,
                            "<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\" \
                             fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"{}\"{rot}{dash_attr}/>",
                            sh.x, sh.y, sh.w, sh.h, sh.width
                        );
                    }
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
                        let mut first = true;
                        s.push_str("<polygon points=\"");
                        for p in &pts {
                            if !first {
                                s.push(' ');
                            }
                            first = false;
                            let _ = write!(s, "{} {}", p[0], p[1]);
                        }
                        let _ = write!(
                            s,
                            "\" fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"{}\"{rot}{dash_attr}/>",
                            sh.width
                        );
                    }
                    ShapeKind::Polygon => {
                        if sh.points.len() >= 3 {
                            let mut first = true;
                            s.push_str("<polygon points=\"");
                            for p in &sh.points {
                                if !first {
                                    s.push(' ');
                                }
                                first = false;
                                let _ = write!(s, "{} {}", p[0], p[1]);
                            }
                            let _ = write!(
                                s,
                                "\" fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"{}\"{rot}{dash_attr}/>",
                                sh.width
                            );
                        }
                    }
                }
            }
            Item::Path(p) => {
                if p.points.len() >= 2 {
                    s.push_str("<path d=\"M ");
                    let _ = write!(s, "{} {}", p.points[0][0], p.points[0][1]);
                    for pt in &p.points[1..] {
                        let _ = write!(s, " L {} {}", pt[0], pt[1]);
                    }
                    let _ = write!(
                        s,
                        "\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\" \
                         stroke-linecap=\"round\" stroke-linejoin=\"round\"/>",
                        color_css(&p.rgba),
                        p.width
                    );
                }
                if let Some(head) = p.head {
                    let _ = write!(
                        s,
                        "<polygon points=\"{} {},{} {},{} {}\" fill=\"{}\"/>",
                        head[0][0],
                        head[0][1],
                        head[1][0],
                        head[1][1],
                        head[2][0],
                        head[2][1],
                        color_css(&p.rgba)
                    );
                }
            }
            Item::FillPath(fp) => {
                if fp.rgba[3] >= 0.004 {
                    s.push_str("<path d=\"");
                    for c in &fp.contours {
                        let Some(first) = c.first() else {
                            continue;
                        };
                        let _ = write!(s, "M {} {}", first[0], first[1]);
                        for p in &c[1..] {
                            let _ = write!(s, " L {} {}", p[0], p[1]);
                        }
                        s.push_str(" Z");
                    }
                    let _ = write!(
                        s,
                        "\" fill=\"{}\" fill-rule=\"nonzero\" stroke=\"{}\" stroke-width=\"2\"/>",
                        color_css(&fp.rgba),
                        color_css(&fp.rgba)
                    );
                }
            }
            Item::Connector(_) => {}
        }
    }
    s.push_str("</svg>");
    s
}
