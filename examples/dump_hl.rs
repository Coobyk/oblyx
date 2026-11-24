use std::path::Path;

fn main() {
    let p = Path::new("/home/coobyk/Downloads/Telegram Desktop/P1.ntb");
    let data = std::fs::read(p).unwrap();
    // extract noteBundle from zip
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).unwrap();
    let mut bundle = Vec::new();
    for i in 0..z.len() {
        let mut e = z.by_index(i).unwrap();
        if e.name().ends_with("noteBundle") {
            std::io::Read::read_to_end(&mut e, &mut bundle).unwrap();
        }
    }
    let parts = oblyx::ntb::parse_bundle_public(&bundle).unwrap();
    println!("bundle parsed");
    drop(parts);
    // find raw highlighter payloads: scan for tool=2 in op payloads - use doc API instead
    let doc = oblyx::ntb::open(p).unwrap();
    for s in &doc.strokes {
        if s.tool == Some(2) {
            println!("HL width={} pts={} points={:?}", s.width, s.point_count,
                s.points.iter().take(6).collect::<Vec<_>>());
        }
    }
}
