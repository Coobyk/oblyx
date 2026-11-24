// scan pen stroke colors in a converted .ntb output
use std::collections::BTreeMap;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let doc = oblyx::ntb::parse_bytes(&bytes).unwrap();
    let pages = oblyx::ntb::to_pages(&doc, 842.0);
    let mut colors: BTreeMap<String, usize> = BTreeMap::new();
    for p in &pages {
        for item in &p.items {
            if let oblyx::doc::Item::Stroke(s) = item {
                // pens are opaque; highlighters have alpha < 1
                if s.rgba[3] > 0.99 {
                    let key = format!("{:.3},{:.3},{:.3}", s.rgba[0], s.rgba[1], s.rgba[2]);
                    *colors.entry(key).or_default() += 1;
                }
            }
        }
    }
    let mut v: Vec<_> = colors.into_iter().collect();
    v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (c, n) in v.iter().take(8) { println!("{c} x{n}"); }
}
