fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let doc = oblyx::ntb::parse_bytes(&bytes).unwrap();
    let pages = oblyx::ntb::to_pages(&doc, 842.0);
    let mut hl = 0usize;
    for p in &pages {
        for item in &p.items {
            if let oblyx::doc::Item::Stroke(s) = item {
                if s.rgba[3] < 0.99 { hl += 1; }
            }
        }
    }
    println!("{hl}");
}
