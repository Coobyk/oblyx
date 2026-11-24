// scan stroke colors + alphas across goodnotes files
use std::collections::HashMap;
use std::io::Read;

fn main() {
    let mut colors: HashMap<(u8,u8,u8,u8), usize> = HashMap::new();
    let zip_path = "/home/coobyk/Downloads/Backup 2026-10-12.zip";
    let f = std::fs::File::open(zip_path).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let names: Vec<String> = (0..z.len()).filter_map(|i| {
        let n = z.by_index(i).ok()?.name().to_string();
        if n.ends_with(".goodnotes") { Some(n) } else { None }
    }).collect();
    for name in names.iter().take(30) {
        let mut bytes = Vec::new();
        z.by_name(name).unwrap().read_to_end(&mut bytes).unwrap();
        let Ok(doc) = oblyx::doc::Document::from_bytes(bytes) else { continue };
        for p in doc.pages.iter() {
            let Ok(page) = doc.decode_page(p, false) else { continue };
            for item in &page.items {
                if let oblyx::doc::Item::Stroke(s) = item {
                    let c = ( (s.rgba[0]*255.0) as u8, (s.rgba[1]*255.0) as u8, (s.rgba[2]*255.0) as u8, (s.rgba[3]*255.0) as u8 );
                    *colors.entry(c).or_default() += 1;
                }
            }
        }
    }
    let mut v: Vec<_> = colors.into_iter().collect();
    v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (c, n) in v.iter().take(25) {
        println!("#{:02x}{:02x}{:02x} a={:3} (a={:.2}) count={}", c.0, c.1, c.2, c.3, c.3 as f32/255.0, n);
    }
}
