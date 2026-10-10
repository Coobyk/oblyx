//! Verify the .ntb reader against sample notebooks in /tmp/opencode/ntb/.
//!
//! Run: cargo run --example ntb_probe -- /tmp/opencode/ntb/Eth1.ntb

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/opencode/ntb/Eth1.ntb"));
    let doc = oblyx::ntb::open(&path)?;
    let m = &doc.meta;
    println!("== {}", path.display());
    println!(
        "  note_guid={:02x?} ts={} version={} elements={}",
        &m.note_guid[..8],
        m.timestamp_ms,
        m.version,
        m.element_count
    );
    println!(
        "  device_upper={} device_lower={}",
        m.device_uuid_upper, m.device_uuid_lower
    );
    println!(
        "  strokes={} shapes={} pages={} texts={} assets={} thumbnail={} manifest={}B",
        doc.strokes.len(),
        doc.shapes.len(),
        doc.pages.len(),
        doc.texts.len(),
        doc.assets.len(),
        doc.thumbnail.is_some(),
        doc.manifest.len()
    );
    for (i, p) in doc.pages.iter().enumerate() {
        println!("    page[{i}]: {:.0}×{:.0} pt", p.width, p.height);
    }
    for (i, t) in doc.texts.iter().enumerate() {
        println!("    text[{i}]: font={:?} size={:.1}", t.font, t.size);
    }
    // Stroke stats
    if !doc.strokes.is_empty() {
        let widths: std::collections::HashMap<u32, usize> = doc
            .strokes
            .iter()
            .map(|s| (s.width.to_bits(), 1))
            .fold(Default::default(), |mut acc, (k, v)| {
                *acc.entry(k).or_insert(0) += v;
                acc
            });
        println!("  width histogram (top 6):");
        let mut wh: Vec<_> = widths.into_iter().collect();
        wh.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        for (bits, count) in wh.into_iter().take(6) {
            println!("    {:.4} × {count}", f32::from_bits(bits));
        }
        let ys: Vec<f32> = doc.strokes.iter().map(|s| s.anchor_y).collect();
        let xs: Vec<f32> = doc.strokes.iter().map(|s| s.anchor_x).collect();
        println!(
            "  anchor X range [{:.1}, {:.1}]  Y range [{:.1}, {:.1}]",
            xs.iter().cloned().fold(f32::MAX, f32::min),
            xs.iter().cloned().fold(f32::MIN, f32::max),
            ys.iter().cloned().fold(f32::MAX, f32::min),
            ys.iter().cloned().fold(f32::MIN, f32::max),
        );
        let total_pts: usize = doc.strokes.iter().map(|s| s.point_count).sum();
        println!("  total point records = {total_pts}");
        let n_audio: usize = doc.strokes.iter().filter(|s| s.audio_duration_ms.is_some()).count();
        let n_tool: usize = doc.strokes.iter().filter(|s| s.tool.is_some()).count();
        let n_style: usize = doc.strokes.iter().filter(|s| s.style.is_some()).count();
        println!("  with audio_duration={n_audio} tool={n_tool} style={n_style}");
        // First few strokes
        for s in doc.strokes.iter().take(5) {
            println!(
                "    anchor=({:6.1},{:6.1}) w={:.3} n={} tool={:?} style={:?} audio={:?}",
                s.anchor_x, s.anchor_y, s.width, s.point_count, s.tool, s.style, s.audio_duration_ms
            );
        }
    }
    // Stroke colour histogram (raw 0xAABBGGRR).
    let mut stroke_cols: std::collections::HashMap<u32, usize> = Default::default();
    for s in &doc.strokes {
        *stroke_cols.entry(s.color.unwrap_or(0xff000000)).or_insert(0) += 1;
    }
    if stroke_cols.len() > 1 {
        println!("  stroke colours:");
        let mut sc: Vec<_> = stroke_cols.into_iter().collect();
        sc.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        for (v, count) in sc.into_iter().take(8) {
            let [r, g, b, a] = oblyx::ntb::rgba_from_u32(v);
            println!(
                "    {v:#010x} = #{:02x}{:02x}{:02x} a={a:.2} × {count}",
                (r * 255.0) as u8,
                (g * 255.0) as u8,
                (b * 255.0) as u8
            );
        }
    }

    // Shape stats
    if !doc.shapes.is_empty() {
        let mut kinds: std::collections::HashMap<&str, usize> = Default::default();
        for s in &doc.shapes {
            let kind = match &s.geom {
                oblyx::ntb::NtbShapeGeom::Line { .. } => "line",
                oblyx::ntb::NtbShapeGeom::Box { ellipse: true, .. } => "ellipse",
                oblyx::ntb::NtbShapeGeom::Box { ellipse: false, .. } => "rect",
                oblyx::ntb::NtbShapeGeom::Polygon { .. } => "polygon",
            };
            *kinds.entry(kind).or_insert(0) += 1;
        }
        println!("  shapes:");
        let mut ks: Vec<_> = kinds.into_iter().collect();
        ks.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        for (kind, count) in ks {
            println!("    {kind} × {count}");
        }
        let n_fill = doc.shapes.iter().filter(|s| s.fill.is_some()).count();
        println!("    with fill: {n_fill}");
        for s in doc.shapes.iter().take(8) {
            let geom_desc = match &s.geom {
                oblyx::ntb::NtbShapeGeom::Line { x1, y1, x2, y2, .. } => {
                    format!("({x1:.1},{y1:.1})->({x2:.1},{y2:.1})")
                }
                oblyx::ntb::NtbShapeGeom::Box { x, y, w, h, .. } => {
                    format!("({x:.1},{y:.1}) {w:.1}×{h:.1}")
                }
                oblyx::ntb::NtbShapeGeom::Polygon { points } => {
                    format!("{} pts", points.len())
                }
            };
            println!(
                "    {geom_desc} w={:.3} rot={:.1} colour={:?} fill={:?}",
                s.border_width,
                s.rotation,
                s.color.map(|v| format!("{v:#010x}")),
                s.fill.map(|v| format!("{v:#010x}"))
            );
        }
    }

    Ok(())
}
