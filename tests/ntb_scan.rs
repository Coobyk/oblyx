//! Diagnostic: semantic scan of .ntb files — decode every bundle through our
//! own parser and flag non-finite values in the model (page sizes, stroke
//! points/widths, shape fields). Run:
//!   NTB_DIR=<dir> cargo test --test ntb_scan -- --nocapture
use std::io::Read;
use oblyx::ntb::NtbShapeGeom;

#[test]
fn scan_for_nonfinite() {
    let Ok(dir) = std::env::var("NTB_DIR") else {
        eprintln!("skipping: set NTB_DIR to a folder of .ntb files");
        return;
    };
    let mut files = Vec::new();
    for entry in walkdir(&dir) {
        if entry.ends_with(".ntb") {
            files.push(entry);
        }
    }
    files.sort();
    eprintln!("scanning {} ntb files under {}", files.len(), dir);
    let mut bad = 0;
    for path in &files {
        let f = std::fs::File::open(path).unwrap();
        let mut z = match zip::ZipArchive::new(f) {
            Ok(z) => z,
            Err(e) => {
                eprintln!("{path}: ZIP ERR {e}");
                bad += 1;
                continue;
            }
        };
        let mut bundle = Vec::new();
        match z.by_name("noteBundle") {
            Ok(mut m) => {
                let _ = m.read_to_end(&mut bundle);
            }
            Err(_) => {
                eprintln!("{path}: no noteBundle");
                bad += 1;
                continue;
            }
        }
        let Ok((_meta, strokes, pages, texts, shapes, _blocks, _note_text)) =
            oblyx::ntb::parse_bundle_public(&bundle)
        else {
            eprintln!("{path}: parse_bundle failed");
            bad += 1;
            continue;
        };
        let mut notes = Vec::new();
        let chk = |v: f32| !v.is_finite();
        for (i, s) in strokes.iter().enumerate() {
            if chk(s.width)
                || chk(s.anchor_x)
                || chk(s.anchor_y)
                || s.points.iter().any(|p| chk(p[0]) || chk(p[1]))
            {
                let nan_pts =
                    s.points.iter().filter(|p| chk(p[0]) || chk(p[1])).count();
                notes.push(format!(
                    "stroke[{i}] w={} anchor=({},{}) pts={}/{} nonfinite",
                    s.width, s.anchor_x, s.anchor_y, nan_pts, s.points.len()
                ));
            }
        }
        for (i, p) in pages.iter().enumerate() {
            if chk(p.width) || chk(p.height) {
                notes.push(format!("page[{i}] size={}x{}", p.width, p.height));
            }
        }
        for (i, t) in texts.iter().enumerate() {
            if chk(t.size) {
                notes.push(format!("text[{i}] size={}", t.size));
            }
        }
        for (i, sh) in shapes.iter().enumerate() {
            let mut fs = vec![sh.rotation, sh.border_width];
            match &sh.geom {
                NtbShapeGeom::Line { x1, y1, x2, y2, c1, c2 } => {
                    fs.extend([*x1, *y1, *x2, *y2]);
                    for c in [c1, c2].into_iter().flatten() {
                        fs.extend(*c);
                    }
                }
                NtbShapeGeom::Box { x, y, w, h, .. } => fs.extend([*x, *y, *w, *h]),
                NtbShapeGeom::Polygon { points } => {
                    fs.extend(points.iter().flat_map(|p| *p))
                }
            }
            if fs.iter().any(|v| chk(*v)) {
                notes.push(format!("shape[{i}] {:?}", sh));
            }
        }
        if !notes.is_empty() {
            bad += 1;
            eprintln!("{path}:");
            for n in notes.iter().take(8) {
                eprintln!("    {n}");
            }
            if notes.len() > 8 {
                eprintln!("    … +{} more", notes.len() - 8);
            }
        }
    }
    eprintln!(
        "done: {bad} files with model-level non-finite values / {}",
        files.len()
    );
}

fn walkdir(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}

#[test]
fn dump_translucent_strokes() {
    let Ok(dir) = std::env::var("GN_DUMP") else { return };
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "goodnotes").unwrap_or(false))
        .collect();
    files.sort();
    for p in files {
        let data = std::fs::read(&p).unwrap();
        let Ok(doc) = oblyx::doc::Document::from_bytes(data) else { continue };
        let mut n = 0usize; let mut tot = 0usize;
        for src in &doc.pages {
            let Ok(page) = doc.decode_page(src, false) else { continue };
            for it in &page.items {
                if let oblyx::doc::Item::Stroke(s) = it { tot += 1;
                    if s.rgba[3] < 0.995 { n += 1;
                        if n <= 5 { println!("{}: a={:.3} rgb=({:.2},{:.2},{:.2}) w={:.1} pts={}", p.file_name().unwrap().to_string_lossy(), s.rgba[3], s.rgba[0], s.rgba[1], s.rgba[2], s.width, s.points.len()); }
                    } } }
        }
        if n > 0 { println!("{}: {}/{} translucent", p.file_name().unwrap().to_string_lossy(), n, tot); }
    }
}

#[test]
fn dump_translucent_items() {
    let Ok(f) = std::env::var("GN_FILE") else { return };
    let data = std::fs::read(&f).unwrap();
    let doc = oblyx::doc::Document::from_bytes(data).unwrap();
    for src in &doc.pages {
        let Ok(page) = doc.decode_page(src, false) else { continue };
        for it in &page.items {
            let (k, a) = match it {
                oblyx::doc::Item::Stroke(s) => ("stroke", s.rgba[3]),
                oblyx::doc::Item::Shape(s) => ("shape", s.stroke[3].min(s.fill[3])),
                oblyx::doc::Item::Path(p) => ("path", p.rgba[3]),
                oblyx::doc::Item::FillPath(p) => ("fillpath", p.rgba[3]),
                oblyx::doc::Item::Connector(c) => ("conn", c.rgba[3]),
                _ => continue,
            };
            if a < 0.995 { println!("{k} a={a:.3}"); }
        }
    }
}

#[test]
fn count_emitted_alpha() {
    let Ok(f) = std::env::var("GN_FILE") else { return };
    let data = std::fs::read(&f).unwrap();
    let doc = oblyx::doc::Document::from_bytes(data).unwrap();
    let mut c = std::collections::HashMap::new();
    for src in &doc.pages {
        let Ok(page) = doc.decode_page(src, false) else { continue };
        for it in &page.items {
            let (k, a) = match it {
                oblyx::doc::Item::Stroke(s) => ("stroke", s.rgba[3]),
                oblyx::doc::Item::Shape(s) => ("shape_stroke", s.stroke[3]),
                oblyx::doc::Item::Path(p) => ("path", p.rgba[3]),
                oblyx::doc::Item::FillPath(p) => ("fillpath", p.rgba[3]),
                oblyx::doc::Item::Connector(c) => ("conn", c.rgba[3]),
                _ => continue,
            };
            *c.entry((k, a < 0.99)).or_insert(0usize) += 1;
        }
    }
    let mut v: Vec<_> = c.into_iter().collect();
    v.sort();
    for ((k, lo), n) in v { println!("{k} translucent={lo}: {n}"); }
}

#[test]
fn count_fillpath_contours() {
    let Ok(f) = std::env::var("GN_FILE") else { return };
    let data = std::fs::read(&f).unwrap();
    let doc = oblyx::doc::Document::from_bytes(data).unwrap();
    let mut tot = 0usize; let mut lo = 0usize;
    for src in &doc.pages {
        let Ok(page) = doc.decode_page(src, false) else { continue };
        for it in &page.items {
            if let oblyx::doc::Item::FillPath(p) = it {
                let n = p.contours.iter().filter(|c| c.len() >= 2).count();
                tot += n;
                if p.rgba[3] < 0.99 { lo += n; println!("fillpath a={:.3} contours={}", p.rgba[3], n); }
            }
        }
    }
    println!("total contour-inks={tot} translucent={lo}");
}
