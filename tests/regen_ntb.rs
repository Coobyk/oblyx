use std::io::Write;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().map(|x| x == "goodnotes").unwrap_or(false) {
            out.push(p);
        }
    }
}

#[test]
fn regen() {
    let outdir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/tmp/regen".into());
    std::fs::create_dir_all(&outdir).unwrap();
    let mut files = Vec::new();
    if let Ok(dir) = std::env::var("GN_DIR") {
        collect(Path::new(&dir), &mut files);
    } else if let Ok(f) = std::env::var("GN_FILE") {
        files.push(PathBuf::from(f));
    } else {
        return;
    }
    files.sort();
    for f in &files {
        let name = f.file_stem().unwrap().to_string_lossy();
        let mut out = format!("{}/{}.ntb", outdir, name);
        if std::path::Path::new(&out).exists() {
            let mut n = 2;
            while std::path::Path::new(&out).exists() {
                out = format!("{}/{}-{}.ntb", outdir, name, n);
                n += 1;
            }
        }
        let data = std::fs::read(f).unwrap();
        let doc = match oblyx::doc::Document::from_bytes(data) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("SKIP {}: {}", name, e);
                continue;
            }
        };
        let pages: Vec<_> = doc
            .pages
            .iter()
            .filter_map(|p| doc.decode_page(p, false).ok())
            .collect();
        match oblyx::ntb_write::write_ntb_to_vec(&pages, &doc, &name) {
            Ok(ntb) => {
                std::fs::File::create(&out).unwrap().write_all(&ntb).unwrap();
            }
            Err(e) => eprintln!("FAIL {}: {}", name, e),
        }
    }
    eprintln!("converted {} files to {}", files.len(), outdir);
}
