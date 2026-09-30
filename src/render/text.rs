use std::sync::OnceLock;

use fontdue::{Font, FontSettings};
use tiny_skia::Pixmap;

const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/Library/Fonts/Arial.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    "C:\\Windows\\Fonts\\arial.ttf",
];

fn load_font() -> Option<Font> {
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                return Some(font);
            }
        }
    }
    if let Ok(out) = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif"])
        .output()
    {
        if out.status.success() {
            if let Ok(path) = String::from_utf8(out.stdout) {
                if let Ok(bytes) = std::fs::read(path.trim()) {
                    if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                        return Some(font);
                    }
                }
            }
        }
    }
    None
}

pub fn system_font() -> Option<&'static Font> {
    static FONT: OnceLock<Option<Font>> = OnceLock::new();
    FONT.get_or_init(load_font).as_ref()
}

pub fn draw_text_line(
    pm: &mut Pixmap,
    text: &str,
    x: f32,
    baseline: f32,
    px: f32,
    rgb: [f32; 3],
    alpha: f32,
) {
    let Some(font) = system_font() else {
        return;
    };
    if px <= 1.0 || text.is_empty() {
        return;
    }
    let cr = (rgb[0].clamp(0.0, 1.0) * 255.0).round() as f32;
    let cg = (rgb[1].clamp(0.0, 1.0) * 255.0).round() as f32;
    let cb = (rgb[2].clamp(0.0, 1.0) * 255.0).round() as f32;
    let a_full = alpha.clamp(0.0, 1.0) * 255.0;

    let pw = pm.width() as i32;
    let ph = pm.height() as i32;
    let data = pm.data_mut();
    let mut pen = x;

    for ch in text.chars() {
        let (m, cov) = font.rasterize(ch, px);
        if m.width > 0 && m.height > 0 && !cov.is_empty() {
            let gx0 = pen.round() as i32 + m.xmin;
            let gy0 = baseline.round() as i32 - m.ymin - m.height as i32;
            let mw = m.width as i32;
            for row in 0..m.height as i32 {
                let py = gy0 + row;
                if py < 0 || py >= ph {
                    continue;
                }
                for col in 0..mw {
                    let pxx = gx0 + col;
                    if pxx < 0 || pxx >= pw {
                        continue;
                    }
                    let coverage = cov[(row * mw + col) as usize] as f32;
                    if coverage == 0.0 {
                        continue;
                    }
                    let a = coverage * a_full / 255.0;
                    let inv = 1.0 - a;
                    let idx = ((py as usize) * (pw as usize) + pxx as usize) * 4;
                    data[idx] = (cr * a + data[idx] as f32 * inv).round() as u8;
                    data[idx + 1] = (cg * a + data[idx + 1] as f32 * inv).round() as u8;
                    data[idx + 2] = (cb * a + data[idx + 2] as f32 * inv).round() as u8;
                    data[idx + 3] =
                        (255.0 * a + data[idx + 3] as f32 * inv).round().min(255.0) as u8;
                }
            }
        }
        pen += m.advance_width;
    }
}
