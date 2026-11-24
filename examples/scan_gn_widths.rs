use std::io::Read;

fn main() {
    let zip_path = "/home/coobyk/Downloads/Backup 2026-10-12.zip";
    let f = std::fs::File::open(zip_path).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let names: Vec<String> = (0..z.len()).filter_map(|i| {
        let n = z.by_index(i).ok()?.name().to_string();
        if n.ends_with(".goodnotes") { Some(n) } else { None }
    }).collect();
    let mut hl_w = Vec::new();
    let mut pen_w = Vec::new();
    let mut dark_colors = std::collections::HashSet::new();
    for name in names.iter().take(40) {
        let mut bytes = Vec::new();
        z.by_name(name).unwrap().read_to_end(&mut bytes).unwrap();
        let Ok(doc) = oblyx::doc::Document::from_bytes(bytes) else { continue };
        for p in doc.pages.iter() {
            let Ok(page) = doc.decode_page(p, false) else { continue };
            for item in &page.items {
                if let oblyx::doc::Item::Stroke(s) = item {
                    if s.rgba[3] < 0.99 { hl_w.push(s.width); } else { pen_w.push(s.width); }
                    if s.rgba[3] > 0.99 && s.rgba[0] < 0.25 && s.rgba[1] < 0.25 && s.rgba[2] < 0.25 {
                        dark_colors.insert(((s.rgba[0]*255.0) as u8, (s.rgba[1]*255.0) as u8, (s.rgba[2]*255.0) as u8));
                    }
                }
            }
        }
    }
    let stats = |v: &mut Vec<f32>, label: &str| {
        v.sort_by(|a,b| a.partial_cmp(b).unwrap());
        if v.is_empty() { println!("{}: none", label); return; }
        println!("{}: n={} min={:.2} med={:.2} p90={:.2} max={:.2}", label, v.len(), v[0], v[v.len()/2], v[v.len()*9/10], v[v.len()-1]);
    };
    stats(&mut hl_w, "GN marker widths");
    stats(&mut pen_w, "GN pen widths");
    let mut dc: Vec<_> = dark_colors.into_iter().collect();
    dc.sort();
    println!("dark colors (<64/255): {:?}", dc.iter().take(20).collect::<Vec<_>>());
}
