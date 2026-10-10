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
    println!("  strokes={} assets={} thumbnail={} manifest={}B",
        doc.strokes.len(),
        doc.assets.len(),
        doc.thumbnail.is_some(),
        doc.manifest.len());
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
        // First few strokes
        for s in doc.strokes.iter().take(5) {
            println!(
                "    anchor=({:6.1},{:6.1}) w={:.3} n={}",
                s.anchor_x, s.anchor_y, s.width, s.point_count
            );
        }
    }
    Ok(())
}
