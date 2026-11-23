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
        let Ok((_meta, strokes, pages, texts, shapes)) =
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
