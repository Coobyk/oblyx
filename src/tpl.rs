use anyhow::{Result, bail};

pub const STROKE_FORMAT: &str = "vuA(v)A(S(uu))A(S(uuuu))vA(f)";
pub const VARWIDTH_FORMAT: &str =
    "vuA(v)A(S(uuuuu))A(S(uuuuuuuuuuu))A(S(uu))A(v)A(S(uu))A(S(uuuu))A(u)";
pub const VECTOR_FORMAT: &str = "vA(v)A(u)A(u)A(v)A(v)A(u)A(u)A(u)A(u)A(v)";

#[derive(Debug, Clone)]
pub enum Tv {
    Int(u64),
    Float(u64),
    Arr(Vec<Tv>),
    Struct(Vec<Tv>),
}

impl Tv {
    pub fn as_u32(&self) -> u32 {
        match self {
            Tv::Int(v) => *v as u32,
            _ => 0,
        }
    }
    pub fn as_slice(&self) -> &[Tv] {
        match self {
            Tv::Arr(v) | Tv::Struct(v) => v,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Scalar(char),
    Group(char, Vec<Node>),
}

fn parse_format(fmt: &str) -> Result<Vec<Node>> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut pos = 0;

    fn node(chars: &[char], pos: &mut usize) -> Result<Node> {
        let ch = *chars
            .get(*pos)
            .ok_or_else(|| anyhow::anyhow!("tpl: format eof"))?;
        if ch == 'A' || ch == 'S' {
            *pos += 1;
            if chars.get(*pos) != Some(&'(') {
                bail!("tpl: expected ( in format");
            }
            *pos += 1;
            let mut kids = Vec::new();
            while chars.get(*pos) != Some(&')') {
                kids.push(node(chars, pos)?);
            }
            *pos += 1;
            if kids.is_empty() {
                bail!("tpl: empty group");
            }
            return Ok(Node::Group(ch, kids));
        }
        *pos += 1;
        Ok(Node::Scalar(ch))
    }

    let mut nodes = Vec::new();
    while pos < chars.len() {
        nodes.push(node(&chars, &mut pos)?);
    }
    Ok(nodes)
}

fn scalar_size(ch: char) -> Result<usize> {
    Ok(match ch {
        'c' => 1,
        'j' | 'v' => 2,
        'i' | 'u' => 4,
        'I' | 'U' | 'f' => 8,
        _ => bail!("tpl: unknown scalar type {ch}"),
    })
}

fn read(buf: &[u8], pos: &mut usize, node: &Node) -> Result<Tv> {
    match node {
        Node::Group('A', kids) => {
            if *pos + 4 > buf.len() {
                bail!("tpl: array count eof");
            }
            let count = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap()) as usize;
            *pos += 4;
            let mut items = Vec::with_capacity(count.min(1 << 20));
            for _ in 0..count {
                items.push(read(buf, pos, &kids[0])?);
            }
            Ok(Tv::Arr(items))
        }
        Node::Group('S', kids) => {
            let mut values = Vec::with_capacity(kids.len());
            for kid in kids {
                values.push(read(buf, pos, kid)?);
            }
            Ok(Tv::Struct(values))
        }
        Node::Group(k, _) => bail!("tpl: unknown group {k}"),
        Node::Scalar(ch) => {
            let size = scalar_size(*ch)?;
            if *pos + size > buf.len() {
                bail!("tpl: scalar eof");
            }
            let raw = &buf[*pos..*pos + size];
            *pos += size;
            let v = match size {
                1 => raw[0] as u64,
                2 => u16::from_le_bytes(raw.try_into().unwrap()) as u64,
                4 => u32::from_le_bytes(raw.try_into().unwrap()) as u64,
                _ => u64::from_le_bytes(raw.try_into().unwrap()),
            };
            if *ch == 'f' {
                Ok(Tv::Float(v))
            } else {
                Ok(Tv::Int(v))
            }
        }
    }
}

pub fn decode_tpl(img: &[u8]) -> Result<(String, Vec<Tv>)> {
    if img.len() < 9 || &img[..4] != b"tpl\0" {
        bail!("missing tpl magic");
    }
    let total = u32::from_le_bytes(img[4..8].try_into().unwrap()) as usize;
    if total != img.len() {
        bail!("tpl length field mismatch");
    }
    let nul = img[8..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| p + 8)
        .ok_or_else(|| anyhow::anyhow!("tpl: no format terminator"))?;
    let fmt = std::str::from_utf8(&img[8..nul])?.to_string();
    let mut pos = nul + 1;
    let nodes = parse_format(&fmt)?;
    let mut values = Vec::with_capacity(nodes.len());
    for node in &nodes {
        values.push(read(img, &mut pos, node)?);
    }
    if pos != img.len() {
        bail!("tpl trailing bytes");
    }
    Ok((fmt, values))
}

pub struct StrokeGeometry {
    pub width_bits: u32,
    pub starts: Vec<[f32; 2]>,
    pub segments: Vec<[f32; 4]>,
    pub dash: Option<[f32; 2]>,
}

pub fn stroke_geometry(img: &[u8]) -> Result<Option<StrokeGeometry>> {
    let (fmt, values) = decode_tpl(img)?;
    if fmt == STROKE_FORMAT {
        return Ok(standard_geometry(&values));
    }
    if fmt == VARWIDTH_FORMAT {
        return Ok(varwidth_geometry(&values));
    }
    if fmt == VECTOR_FORMAT {
        return Ok(vector_geometry(&values));
    }
    Ok(None)
}

fn varwidth_geometry(values: &[Tv]) -> Option<StrokeGeometry> {
    if values.len() < 5 {
        return None;
    }
    let width_bits = values[1].as_u32();
    let mut starts = Vec::new();
    for item in values[3].as_slice() {
        let m = item.as_slice();
        if m.len() >= 2 {
            starts.push([f32::from_bits(m[0].as_u32()), f32::from_bits(m[1].as_u32())]);
        }
    }
    let mut segments = Vec::new();
    for item in values[4].as_slice() {
        let m = item.as_slice();
        if m.len() >= 11 {
            segments.push([
                f32::from_bits(m[1].as_u32()),
                f32::from_bits(m[2].as_u32()),
                f32::from_bits(m[6].as_u32()),
                f32::from_bits(m[7].as_u32()),
            ]);
        }
    }
    if starts.is_empty() {
        return None;
    }
    Some(StrokeGeometry {
        width_bits,
        starts,
        segments,
        dash: None,
    })
}

fn standard_geometry(values: &[Tv]) -> Option<StrokeGeometry> {
    if values.len() < 5 {
        return None;
    }
    let width_bits = values[1].as_u32();
    let mut starts = Vec::new();
    for item in values[3].as_slice() {
        let m = item.as_slice();
        if m.len() >= 2 {
            starts.push([f32::from_bits(m[0].as_u32()), f32::from_bits(m[1].as_u32())]);
        }
    }
    let mut segments = Vec::new();
    for item in values[4].as_slice() {
        let m = item.as_slice();
        if m.len() >= 4 {
            segments.push([
                f32::from_bits(m[0].as_u32()),
                f32::from_bits(m[1].as_u32()),
                f32::from_bits(m[2].as_u32()),
                f32::from_bits(m[3].as_u32()),
            ]);
        }
    }
    let dash = match values.get(6) {
        Some(Tv::Arr(items)) => {
            let mut floats = items
                .iter()
                .filter_map(|x| match x {
                    Tv::Float(v) => Some(*v),
                    _ => None,
                })
                .collect::<Vec<_>>();
            floats.truncate(2);
            let pair: Vec<f32> = floats
                .iter()
                .flat_map(|v| [f32::from_bits(*v as u32), f32::from_bits((*v >> 32) as u32)])
                .collect();
            if pair.len() == 4 {
                let (seg, gap) = (pair[0], pair[2]);
                if seg.is_finite() && gap.is_finite() && seg >= 0.0 && gap > 0.0 {
                    Some([seg, gap])
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    };
    Some(StrokeGeometry {
        width_bits,
        starts,
        segments,
        dash,
    })
}

fn vector_geometry(values: &[Tv]) -> Option<StrokeGeometry> {
    if values.len() < 4 {
        return None;
    }
    let mut starts = Vec::new();
    let mut segments = Vec::new();
    let mut prev: Option<[f32; 2]> = None;
    let mut w_sum = 0.0f32;
    let mut w_cnt = 0u32;
    let nodes = values[3].as_slice();
    for m in nodes.chunks(3) {
        if m.len() < 3 {
            break;
        }
        let p = [f32::from_bits(m[0].as_u32()), f32::from_bits(m[1].as_u32())];
        if !p[0].is_finite() || !p[1].is_finite() {
            continue;
        }
        let hw = f32::from_bits(m[2].as_u32());
        if hw.is_finite() && hw > 0.0 {
            w_sum += hw;
            w_cnt += 1;
        }
        if let Some(q) = prev {
            segments.push([q[0], q[1], p[0], p[1]]);
        } else {
            starts.push(p);
        }
        prev = Some(p);
    }
    if starts.is_empty() {
        return None;
    }
    let mean_hw = if w_cnt > 0 {
        w_sum / w_cnt as f32
    } else {
        0.78
    };
    let width = (mean_hw * 2.0).clamp(0.1, 40.0);
    Some(StrokeGeometry {
        width_bits: width.to_bits(),
        starts,
        segments,
        dash: None,
    })
}

pub fn polyline(starts: &[[f32; 2]], segments: &[[f32; 4]]) -> Vec<[f32; 2]> {
    let mut pts: Vec<[f32; 2]> = Vec::with_capacity(1 + segments.len());
    let Some(first) = starts.first() else {
        return pts;
    };
    pts.push(*first);
    for s in segments {
        if let Some(last) = pts.last() {
            if (last[0] - s[0]).abs() > 1e-3 || (last[1] - s[1]).abs() > 1e-3 {
                pts.push([s[0], s[1]]);
            }
        }
        pts.push([s[2], s[3]]);
    }
    pts
}

fn split_runs(pts: &[[f32; 2]], th: f32) -> Vec<Vec<[f32; 2]>> {
    let mut runs: Vec<Vec<[f32; 2]>> = Vec::new();
    for p in pts {
        match runs.last_mut() {
            Some(run)
                if (run.last().unwrap()[0] - p[0]).hypot(run.last().unwrap()[1] - p[1]) <= th =>
            {
                run.push(*p);
            }
            _ => runs.push(vec![*p]),
        }
    }
    runs
}

fn pair_runs(runs: Vec<Vec<[f32; 2]>>) -> Vec<Vec<[f32; 2]>> {
    let mut out = Vec::with_capacity(runs.len() / 2);
    let mut it = runs.into_iter();
    while let (Some(mut a), Some(b)) = (it.next(), it.next()) {
        a.extend(b);
        if a.len() >= 3 {
            out.push(a);
        }
    }
    out
}

pub fn vector_fill_geometry(img: &[u8]) -> Result<Option<(Vec<Vec<[f32; 2]>>, bool)>> {
    let (fmt, values) = decode_tpl(img)?;
    if fmt != VECTOR_FORMAT {
        return Ok(None);
    }
    let mut colored = false;
    if let Some(nodes) = values.get(3) {
        for m in nodes.as_slice().chunks(3) {
            if m.len() < 3 {
                break;
            }
            if f32::from_bits(m[0].as_u32()) == 0.0 && f32::from_bits(m[1].as_u32()) == 0.0 {
                colored = true;
            }
        }
    }
    let mut pts: Vec<[f32; 2]> = Vec::new();
    if let Some(nodes) = values.get(8) {
        for m in nodes.as_slice().chunks(2) {
            if m.len() < 2 {
                break;
            }
            let x = f32::from_bits(m[0].as_u32());
            let y = f32::from_bits(m[1].as_u32());
            if x.is_finite() && y.is_finite() {
                pts.push([x, y]);
            }
        }
    }
    if pts.len() < 3 {
        return Ok(Some((Vec::new(), colored)));
    }
    if !colored {
        return Ok(None);
    }
    let dashes = values.get(4).map(|v| v.as_slice().len()).unwrap_or(0);
    let mut contours = Vec::new();
    if dashes > 1 {
        let mut gaps: Vec<f32> = pts
            .windows(2)
            .map(|w| (w[0][0] - w[1][0]).hypot(w[0][1] - w[1][1]))
            .filter(|g| *g > 0.0)
            .collect();
        gaps.sort_by(|a, b| a.total_cmp(b));
        gaps.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
        let mut cands: Vec<f32> = Vec::with_capacity(gaps.len());
        if let Some(&g0) = gaps.first() {
            cands.push(g0 * 0.5);
        }
        for w in gaps.windows(2) {
            cands.push((w[0] + w[1]) * 0.5);
        }
        let mut nth: Option<Vec<Vec<[f32; 2]>>> = None;
        for th in cands {
            let runs = split_runs(&pts, th);
            if runs.len() == dashes * 2 {
                contours = pair_runs(runs);
                break;
            }
            if nth.is_none() && runs.len() == dashes {
                nth = Some(runs);
            }
        }
        if contours.is_empty() {
            if let Some(runs) = nth {
                contours = runs.into_iter().filter(|r| r.len() >= 3).collect();
            }
        }
    }
    if contours.is_empty() {
        return Ok(None);
    }
    Ok(Some((contours, colored)))
}
