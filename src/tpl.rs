use anyhow::{Result, bail};

pub const STROKE_FORMAT: &str = "vuA(v)A(S(uu))A(S(uuuu))vA(f)";

#[derive(Debug, Clone)]
pub enum Tv {
    Int(u64),
    Float,
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
                Ok(Tv::Float)
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
}

pub fn stroke_geometry(img: &[u8]) -> Result<Option<StrokeGeometry>> {
    let (fmt, values) = decode_tpl(img)?;
    if fmt != STROKE_FORMAT || values.len() < 5 {
        return Ok(None);
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
    Ok(Some(StrokeGeometry {
        width_bits,
        starts,
        segments,
    }))
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
