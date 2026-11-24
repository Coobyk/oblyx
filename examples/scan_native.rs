use std::path::Path;

fn main() {
    let p = Path::new("/tmp/native_hl.ntb");
    let doc = oblyx::ntb::open(p).unwrap();
    println!("strokes: {}", doc.strokes.len());
    for s in &doc.strokes {
        let col = s.color.unwrap_or(0xff000000);
        let (r, g, b, a) = (col & 0xff, (col >> 8) & 0xff, (col >> 16) & 0xff, (col >> 24) & 0xff);
        println!(
            "tool={:?} style={:?} w={:.4} rgba=({},{},{},{}) pts={} anchor=({:.1},{:.1}) npts={}",
            s.tool, s.style, s.width, r, g, b, a, s.point_count, s.anchor_x, s.anchor_y, s.points.len()
        );
    }
}
