use std::path::Path;

fn main() {
    for f in ["Eth1", "P1", "Cambridge", "M1", "D1"] {
        let p = format!("/home/coobyk/Downloads/Telegram Desktop/{}.ntb", f);
        if !Path::new(&p).exists() { continue; }
        let Ok(doc) = oblyx::ntb::open(Path::new(&p)) else { println!("{}: parse fail", f); continue };
        println!("== {}: {} strokes total", f, doc.strokes.len());
        let mut hl = 0; let mut pen = 0;
        for s in &doc.strokes {
            let col = s.color.unwrap_or(0xff000000);
            let (r, g, b, a) = (col & 0xff, (col>>8)&0xff, (col>>16)&0xff, (col>>24)&0xff);
            match s.tool {
                Some(2) => { hl += 1; if hl <= 5 {
                    println!("  HL rgb=({},{},{}) a={} w={:.2} style={:?} pts={}",
                        r, g, b, a, s.width, s.style, s.point_count);
                }},
                _ => pen += 1,
            }
        }
        println!("  other-strokes={} highlighters={}", pen, hl);
        // pen width distribution for comparison
        let mut pw: Vec<f32> = doc.strokes.iter().filter(|s| s.tool != Some(2)).map(|s| s.width).collect();
        pw.sort_by(|x, y| x.partial_cmp(y).unwrap());
        if !pw.is_empty() { println!("  pen widths: min={:.2} med={:.2} max={:.2}", pw[0], pw[pw.len()/2], pw[pw.len()-1]); }
    }
}
