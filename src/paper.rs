use std::io::Read;

use crate::doc::{PAGE_H, PAGE_W};

type Rect = (f32, f32, f32, f32, [f32; 3]);

#[derive(Clone, Copy)]
struct M {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

fn ident() -> M {
    M {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    }
}

fn mul(x: &M, y: &M) -> M {
    M {
        a: x.a * y.a + x.b * y.c,
        b: x.a * y.b + x.b * y.d,
        c: x.c * y.a + x.d * y.c,
        d: x.c * y.b + x.d * y.d,
        e: x.e * y.a + x.f * y.c + y.e,
        f: x.e * y.b + x.f * y.d + y.f,
    }
}

fn apply(m: &M, p: [f32; 2]) -> [f32; 2] {
    [m.a * p[0] + m.c * p[1] + m.e, m.b * p[0] + m.d * p[1] + m.f]
}

fn find_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn media_box(src: &[u8]) -> Option<(f32, f32)> {
    let pos = find_from(src, 0, b"/MediaBox")?;
    let rest = &src[pos + 9..];
    let ob = find_from(rest, 0, b"[")?;
    let cb = find_from(rest, ob, b"]")?;
    let mut nums: Vec<f32> = Vec::new();
    let mut i = ob + 1;
    while i < cb {
        let b = rest[i];
        if b.is_ascii_whitespace() || b == b'[' || b == b']' {
            i += 1;
            continue;
        }
        if b.is_ascii_digit() || b == b'-' || b == b'+' || b == b'.' {
            let start = i;
            i += 1;
            while i < cb
                && (rest[i].is_ascii_digit()
                    || rest[i] == b'.'
                    || rest[i] == b'-'
                    || rest[i] == b'+'
                    || rest[i] == b'e'
                    || rest[i] == b'E')
            {
                i += 1;
            }
            if let Ok(v) = std::str::from_utf8(&rest[start..i])
                .unwrap_or("")
                .parse::<f32>()
            {
                nums.push(v);
            }
            continue;
        }
        i += 1;
    }
    if nums.len() >= 4 {
        let w = nums[2] - nums[0];
        let h = nums[3] - nums[1];
        if w > 1.0 && h > 1.0 {
            return Some((w, h));
        }
    }
    None
}

fn inflate(body: &[u8]) -> Option<Vec<u8>> {
    let trimmed = {
        let mut s = 0;
        while s < body.len() && body[s].is_ascii_whitespace() {
            s += 1;
        }
        &body[s..]
    };
    let mut out = Vec::new();
    if flate2::read::ZlibDecoder::new(trimmed)
        .read_to_end(&mut out)
        .is_ok()
        && !out.is_empty()
    {
        return Some(out);
    }
    None
}

/// Calls `f` with each stream's content, one at a time, so at most one
/// inflated stream is in memory at once (raw streams are passed as slices
/// into `src`). This matters because page backgrounds can be a 300 MB PDF.
fn for_each_stream(src: &[u8], mut f: impl FnMut(&[u8])) {
    let mut from = 0;
    while let Some(pos) = find_from(src, from, b"stream") {
        from = pos + 6;
        if pos >= 3 && &src[pos - 3..pos] == b"end" {
            continue;
        }
        let mut start = from;
        if start < src.len() && src[start] == b'\r' {
            start += 1;
        }
        if start < src.len() && src[start] == b'\n' {
            start += 1;
        }
        let Some(end) = find_from(src, start, b"endstream") else {
            break;
        };
        let mut body = &src[start..end];
        while body.last().is_some_and(|b| b.is_ascii_whitespace()) {
            body = &body[..body.len() - 1];
        }
        match inflate(body) {
            Some(data) => f(&data),
            None => f(body),
        }
        from = end + 9;
    }
}

fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

struct Ops<'a> {
    mh: f32,
    sx: f32,
    sy: f32,
    out: &'a mut Vec<Rect>,
    nums: Vec<f32>,
    path: Vec<[f32; 2]>,
    fill: [f32; 3],
    stack: Vec<M>,
    ctm: M,
}

impl<'a> Ops<'a> {
    fn emit(&mut self) {
        if self.path.len() < 2 {
            self.path.clear();
            return;
        }
        let mut x0 = f32::MAX;
        let mut y0 = f32::MAX;
        let mut x1 = f32::MIN;
        let mut y1 = f32::MIN;
        for p in &self.path {
            let t = apply(&self.ctm, *p);
            x0 = x0.min(t[0]);
            x1 = x1.max(t[0]);
            y0 = y0.min(t[1]);
            y1 = y1.max(t[1]);
        }
        self.path.clear();
        let x = x0 * self.sx;
        let w = (x1 - x0) * self.sx;
        let y = (self.mh - y1) * self.sy + 0.085;
        let h = (y1 - y0) * self.sy;
        if w < 1e-4 || h < 1e-4 {
            return;
        }
        let rgb = [
            self.fill[0].clamp(0.0, 1.0),
            self.fill[1].clamp(0.0, 1.0),
            self.fill[2].clamp(0.0, 1.0),
        ];
        self.out.push((x, y, w, h, rgb));
    }

    fn color(&mut self) {
        let n = self.nums.len();
        if n >= 3 {
            self.fill = [self.nums[n - 3], self.nums[n - 2], self.nums[n - 1]];
        } else if n == 1 {
            self.fill = [self.nums[0]; 3];
        }
    }

    fn push_pt(&mut self) {
        let n = self.nums.len();
        if n >= 2 {
            let p = [self.nums[n - 2], self.nums[n - 1]];
            if p[0].is_finite() && p[1].is_finite() {
                self.path.push(p);
            }
        }
    }
}

fn run(src: &[u8], mw: f32, mh: f32, out: &mut Vec<Rect>) {
    let mut st = Ops {
        mh,
        sx: PAGE_W / mw,
        sy: PAGE_H / mh,
        out,
        nums: Vec::new(),
        path: Vec::new(),
        fill: [0.0; 3],
        stack: Vec::new(),
        ctm: ident(),
    };
    let mut i = 0;
    while i < src.len() {
        let b = src[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'%' {
            while i < src.len() && src[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'(' {
            i += 1;
            let mut depth = 1usize;
            while i < src.len() && depth > 0 {
                match src[i] {
                    b'\\' => i += 2,
                    b'(' => {
                        depth += 1;
                        i += 1;
                    }
                    b')' => {
                        depth -= 1;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            continue;
        }
        if b == b'/' {
            i += 1;
            while i < src.len() && !src[i].is_ascii_whitespace() && !is_delim(src[i]) {
                i += 1;
            }
            continue;
        }
        if b == b'[' || b == b']' || b == b'{' || b == b'}' || b == b'<' || b == b'>' {
            i += 1;
            continue;
        }
        if b.is_ascii_digit() || b == b'-' || b == b'+' || b == b'.' {
            let start = i;
            i += 1;
            while i < src.len() {
                let c = src[i];
                if c.is_ascii_digit() || c == b'.' {
                    i += 1;
                } else if (c == b'+' || c == b'-')
                    && i > start
                    && (src[i - 1] == b'e' || src[i - 1] == b'E')
                {
                    i += 1;
                } else if c == b'e' || c == b'E' {
                    i += 1;
                } else {
                    break;
                }
            }
            if let Ok(v) = std::str::from_utf8(&src[start..i])
                .unwrap_or("")
                .parse::<f32>()
            {
                st.nums.push(v);
            }
            continue;
        }
        let start = i;
        while i < src.len() && !src[i].is_ascii_whitespace() && !is_delim(src[i]) {
            i += 1;
        }
        if start == i {
            i += 1;
            st.nums.clear();
            continue;
        }
        match &src[start..i] {
            b"q" => st.stack.push(st.ctm),
            b"Q" => {
                if let Some(m) = st.stack.pop() {
                    st.ctm = m;
                }
            }
            b"cm" => {
                let n = st.nums.len();
                if n >= 6 {
                    let m = M {
                        a: st.nums[n - 6],
                        b: st.nums[n - 5],
                        c: st.nums[n - 4],
                        d: st.nums[n - 3],
                        e: st.nums[n - 2],
                        f: st.nums[n - 1],
                    };
                    st.ctm = mul(&m, &st.ctm);
                }
            }
            b"m" => {
                st.path.clear();
                st.push_pt();
            }
            b"l" | b"y" => st.push_pt(),
            b"h" => {}
            b"re" => {
                let n = st.nums.len();
                if n >= 4 {
                    let (x, y, w, h) = (
                        st.nums[n - 4],
                        st.nums[n - 3],
                        st.nums[n - 2],
                        st.nums[n - 1],
                    );
                    if x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() {
                        st.path.push([x, y]);
                        st.path.push([x + w, y]);
                        st.path.push([x + w, y + h]);
                        st.path.push([x, y + h]);
                    }
                }
            }
            b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" => st.emit(),
            b"S" | b"s" | b"s*" | b"n" => st.path.clear(),
            b"scn" | b"sc" | b"rg" => st.color(),
            b"g" => {
                if let Some(&v) = st.nums.last() {
                    st.fill = [v; 3];
                }
            }
            _ => {}
        }
        st.nums.clear();
    }
}

pub fn paper_rects(bytes: &[u8]) -> Vec<Rect> {
    let mut out = Vec::new();
    if bytes.len() > 4 && &bytes[..4] == b"%PDF" {
        let (mw, mh) = media_box(bytes).unwrap_or((595.28, 841.89));
        for_each_stream(bytes, |s| run(s, mw, mh, &mut out));
        return out;
    }
    if let Some(data) = inflate(bytes) {
        let (mw, mh) = media_box(&data).unwrap_or((595.28, 841.89));
        for_each_stream(&data, |s| run(s, mw, mh, &mut out));
    }
    out
}
