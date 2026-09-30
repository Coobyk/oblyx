use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy)]
pub enum Wire<'a> {
    Varint(u64),
    Fixed64,
    Bytes(&'a [u8]),
    Fixed32(u32),
}

#[derive(Debug, Clone, Copy)]
pub struct Field<'a> {
    pub num: u32,
    pub wire: Wire<'a>,
}

pub fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = match buf.get(*pos) {
            Some(&b) => b,
            None => bail!("varint: unexpected end of input"),
        };
        *pos += 1;
        value |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            bail!("varint too long");
        }
    }
}

pub fn parse(buf: &[u8]) -> Result<Vec<Field<'_>>> {
    let mut out: Vec<Field<'_>> = Vec::with_capacity(8);
    let mut pos = 0;
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos)?;
        let num = (key >> 3) as u32;
        let wire = match (key & 7) as u8 {
            0 => Wire::Varint(read_varint(buf, &mut pos)?),
            1 => {
                if pos + 8 > buf.len() {
                    bail!("fixed64: eof");
                }
                pos += 8;
                Wire::Fixed64
            }
            2 => {
                let n = read_varint(buf, &mut pos)? as usize;
                let s = buf
                    .get(pos..pos + n)
                    .ok_or_else(|| anyhow::anyhow!("bytes: eof"))?;
                pos += n;
                Wire::Bytes(s)
            }
            5 => {
                let s = buf
                    .get(pos..pos + 4)
                    .ok_or_else(|| anyhow::anyhow!("fixed32: eof"))?;
                pos += 4;
                Wire::Fixed32(u32::from_le_bytes(s.try_into().unwrap()))
            }
            w => bail!("unsupported wire type {w}"),
        };
        out.push(Field { num, wire });
    }
    Ok(out)
}

pub fn records(buf: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let n = read_varint(buf, &mut pos)? as usize;
        let s = buf
            .get(pos..pos + n)
            .ok_or_else(|| anyhow::anyhow!("record: eof"))?;
        pos += n;
        out.push(s);
    }
    Ok(out)
}

pub fn bytes_of<'a>(fs: &[Field<'a>], num: u32) -> Option<&'a [u8]> {
    fs.iter().find(|f| f.num == num).and_then(|f| match f.wire {
        Wire::Bytes(b) => Some(b),
        _ => None,
    })
}

pub fn varint_of(fs: &[Field<'_>], num: u32) -> Option<u64> {
    fs.iter().find(|f| f.num == num).and_then(|f| match f.wire {
        Wire::Varint(v) => Some(v),
        _ => None,
    })
}

pub fn fixed32_of(fs: &[Field<'_>], num: u32) -> Option<u32> {
    fs.iter().find(|f| f.num == num).and_then(|f| match f.wire {
        Wire::Fixed32(v) => Some(v),
        _ => None,
    })
}

pub fn field_nums(fs: &[Field<'_>]) -> Vec<u32> {
    fs.iter().map(|f| f.num).collect()
}

pub fn f32_bits(v: u32) -> f32 {
    f32::from_bits(v)
}

pub fn as_uuid(v: &[u8]) -> Option<&str> {
    if v.len() == 36 && v.iter().filter(|&&b| b == b'-').count() == 4 {
        std::str::from_utf8(v).ok()
    } else {
        None
    }
}
