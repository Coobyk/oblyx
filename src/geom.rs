pub enum Seg {
    Line([f32; 2]),
    Curve([f32; 2], [f32; 2], [f32; 2]),
}

pub const MAX_GAP: f32 = 15.0;

fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

pub fn smooth(pts: &[[f32; 2]], max_gap: f32) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::with_capacity(pts.len().saturating_sub(1));
    if pts.len() < 2 {
        return out;
    }
    // A 2-point path is a single segment (shape lines, straight strokes);
    // gap-splitting would discard both endpoints as 1-point runs.
    if pts.len() == 2 {
        out.push(Seg::Line(pts[1]));
        return out;
    }

    let mut run_start = 0usize;
    let mut i = 1;
    while i <= pts.len() {
        let brk = i == pts.len() || dist(pts[i - 1], pts[i]) > max_gap;
        if brk {
            emit_run(&pts[run_start..i], &mut out);
            run_start = i;
        }
        i += 1;
    }
    out
}

fn emit_run(run: &[[f32; 2]], out: &mut Vec<Seg>) {
    if run.len() < 2 {
        return;
    }
    if run.len() == 2 {
        out.push(Seg::Line(run[1]));
        return;
    }
    let m = run.len();
    for j in 0..m - 1 {
        let p1 = run[j];
        let p2 = run[j + 1];
        let prev = if j == 0 { run[0] } else { run[j - 1] };
        let next = if j + 2 < m { run[j + 2] } else { run[j + 1] };
        let c1 = [
            p1[0] + (p2[0] - prev[0]) / 6.0,
            p1[1] + (p2[1] - prev[1]) / 6.0,
        ];
        let c2 = [
            p2[0] - (next[0] - p1[0]) / 6.0,
            p2[1] - (next[1] - p1[1]) / 6.0,
        ];
        out.push(Seg::Curve(c1, c2, p2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_point_far_line_is_not_dropped() {
        // Shape lines (e.g. highlight underlines) are 2-point paths with
        // endpoints far beyond MAX_GAP; they must still emit one segment.
        let pts = [[180.0, 288.0], [320.0, 288.0]];
        let segs = smooth(&pts, MAX_GAP);
        assert_eq!(segs.len(), 1);
        match &segs[0] {
            Seg::Line(p) => assert_eq!(*p, [320.0, 288.0]),
            Seg::Curve(..) => panic!("expected a straight line segment"),
        }
    }

    #[test]
    fn multi_point_gap_split_still_works() {
        // Three points with a large gap between the 2nd and 3rd: the first
        // run (2 points) emits a line; the trailing 1-point run is dropped.
        let pts = [[0.0, 0.0], [10.0, 0.0], [500.0, 0.0]];
        let segs = smooth(&pts, MAX_GAP);
        assert_eq!(segs.len(), 1);
        match &segs[0] {
            Seg::Line(p) => assert_eq!(*p, [10.0, 0.0]),
            Seg::Curve(..) => panic!("expected a straight line segment"),
        }
    }
}
