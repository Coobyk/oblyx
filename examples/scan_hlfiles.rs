use std::io::Read;

fn main() {
    let zip_path = "/home/coobyk/Downloads/Backup 2026-10-12.zip";
    let f = std::fs::File::open(zip_path).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let names: Vec<String> = (0..z.len()).filter_map(|i| {
        let n = z.by_index(i).ok()?.name().to_string();
        if n.ends_with(".goodnotes") { Some(n) } else { None }
    }).collect();
    let mut counts: Vec<(String, usize)> = Vec::new();
    for name in &names {
        let mut bytes = Vec::new();
        z.by_name(name).unwrap().read_to_end(&mut bytes).unwrap();
        let Ok(doc) = oblyx::doc::Document::from_bytes(bytes) else { continue };
        let mut hl = 0usize;
        for p in doc.pages.iter() {
            let Ok(page) = doc.decode_page(p, false) else { continue };
            for item in &page.items {
                if let oblyx::doc::Item::Stroke(s) = item {
                    if s.rgba[3] < 0.99 { hl += 1; }
                }
            }
        }
        if hl > 0 { counts.push((name.clone(), hl)); }
    }
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (n, c) in counts.iter().take(10) { println!("{}: {} markers", n, c); }
}
