use oblyx::convert::decode_pages;
use oblyx::doc::Document;
use oblyx::render::svg::page_to_svg;
use std::path::Path;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap();
    let page = args.next();
    let t = Instant::now();
    let doc = Document::open(Path::new(&path)).expect("open");
    eprintln!("open {} {:?}", path, t.elapsed());
    for p in &doc.pages {
        eprintln!("source {} data_len={}", p.uuid, p.data.len());
    }
    let filter = page.as_deref();
    if let Some(f) = filter {
        for p in doc.pages.iter().filter(|p| {
            p.uuid
                .to_ascii_uppercase()
                .contains(&f.to_ascii_uppercase())
        }) {
            eprintln!("decoding {} alone...", p.uuid);
            let t = Instant::now();
            let page = doc.decode_page(p, false).expect("decode");
            eprintln!("  decoded in {:?} items={}", t.elapsed(), page.items.len());
        }
    } else {
        let t = Instant::now();
        let pages = decode_pages(&doc, filter, false).expect("decode");
        eprintln!("decode {} pages in {:?}", pages.len(), t.elapsed());
        for p in &pages {
            let t = Instant::now();
            let svg = page_to_svg(p, &doc);
            eprintln!(
                "svg {} items={} len={} in {:?}",
                p.uuid,
                p.items.len(),
                svg.len(),
                t.elapsed()
            );
        }
    }
}
