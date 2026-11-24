// Build a calibration .ntb: width ladder + z-order tests.
use oblyx::doc::{Document, Item, Page, ShapeEl, ShapeKind, Stroke};
use std::io::Write;

fn line(x0: f32, y0: f32, x1: f32, y1: f32, width: f32, rgba: [f32; 4]) -> Item {
    Item::Stroke(Stroke {
        points: vec![[x0, y0], [(x0+x1)/2.0, (y0+y1)/2.0], [x1, y1]],
        width,
        rgba,
        dash: None,
        parented: false,
    })
}

fn rect(x: f32, y: f32, w: f32, h: f32, fill: [f32; 4]) -> Item {
    Item::Shape(ShapeEl {
        kind: ShapeKind::Rect,
        x, y, w, h,
        points: vec![],
        rotation: 0.0,
        fill,
        stroke: [0.0; 4],
        width: 0.0,
        dash: None,
        radius: 0.0,
        uuid: None,
        from_f9: false,
    })
}

fn scribble(x: f32, y: f32, rgba: [f32; 4]) -> Item {
    // zigzag over a 120x40 area
    let mut pts = vec![];
    for i in 0..=12 {
        pts.push([x + i as f32 * 10.0, y + if i % 2 == 0 { 0.0 } else { 40.0 }]);
    }
    Item::Stroke(Stroke { points: pts, width: 1.5, rgba, dash: None, parented: false })
}

fn main() {
    let black = [0.0, 0.0, 0.0, 1.0];
    let red = [1.0, 0.0, 0.0, 1.0];
    let bluefill = [0.2, 0.4, 1.0, 1.0];
    // goodnotes marker: alpha 0.5
    let yellow_hl = [1.0, 1.0, 0.2, 0.5];
    let mut items: Vec<Item> = Vec::new();

    // Section A: pen control widths (native base multiples), y 30..90
    let aw = [0.775, 1.55, 3.1];
    for (i, w) in aw.iter().enumerate() {
        let y = 30.0 + i as f32 * 25.0;
        items.push(line(40.0, y, 300.0, y, *w, black));
    }

    // Section B: highlighter width ladder, y 130..430, spaced 42pt
    let bw = [0.3875, 0.775, 1.55, 3.1, 6.2, 12.0, 24.0];
    for (i, w) in bw.iter().enumerate() {
        let y = 130.0 + i as f32 * 42.0;
        items.push(line(40.0, y, 300.0, y, *w, yellow_hl));
    }

    // Section C: z-order tests in 4 quadrants
    // C1 (top-left): blue rect FIRST, red scribble over -> expect red visible
    items.push(rect(40.0, 480.0, 140.0, 60.0, bluefill));
    items.push(scribble(45.0, 490.0, red));
    // C2 (top-right): red scribble FIRST, blue rect over -> does order matter?
    items.push(scribble(345.0, 490.0, red));
    items.push(rect(340.0, 480.0, 140.0, 60.0, bluefill));
    // C3 (bottom-left): yellow HL band first, black line through it
    items.push(line(40.0, 620.0, 180.0, 620.0, 24.0, yellow_hl));
    items.push(scribble(45.0, 600.0, black));
    // C4 (bottom-right): black scribble first, then yellow HL band across
    items.push(scribble(345.0, 600.0, black));
    items.push(line(340.0, 620.0, 480.0, 620.0, 24.0, yellow_hl));

    // Section D: layering with real-world order, y 700-800
    // D1 (left): black scribble FIRST, yellow HL band OVER it (highlight-after-ink)
    items.push(scribble(45.0, 700.0, black));
    items.push(line(40.0, 720.0, 180.0, 720.0, 15.0, yellow_hl));
    // D2 (right): double-stacked highlight (overlap darkening test)
    items.push(line(340.0, 715.0, 480.0, 715.0, 15.0, yellow_hl));
    items.push(line(340.0, 730.0, 480.0, 730.0, 15.0, yellow_hl));

    // labels: tiny black strokes as position markers at left margin
    for y in [30.0f32, 130.0, 480.0, 620.0] {
        items.push(line(20.0, y, 30.0, y, 2.0, black));
    }

    let doc = Document::empty();
    let pages = vec![Page::new("p0", items)];
    let ntb = oblyx::ntb_write::write_ntb_to_vec(&pages, &doc, "CalibW").unwrap();
    std::fs::File::create("/tmp/calibw.ntb").unwrap().write_all(&ntb).unwrap();
    println!("wrote /tmp/calibw.ntb ({} bytes)", ntb.len());
}
