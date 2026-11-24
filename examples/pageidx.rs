fn main() {
    let bytes = std::fs::read("/tmp/oblyx_probe/english.goodnotes").unwrap();
    let doc = oblyx::doc::Document::from_bytes(bytes).unwrap();
    for (i, p) in doc.pages.iter().enumerate() {
        let uuid = &p.uuid;
        if uuid.starts_with("03A14650") { println!("page index: {} uuid {}", i, uuid); }
    }
    println!("total {}", doc.pages.len());
}
