use anyhow::{Result, bail};

fn lz4_block(src: &[u8], expected: usize, out: &mut Vec<u8>) -> Result<()> {
    let start = out.len();
    out.reserve(expected);
    let mut pos = 0;
    while pos < src.len() {
        let token = src[pos];
        pos += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *src
                    .get(pos)
                    .ok_or_else(|| anyhow::anyhow!("lz4: literal eof"))?;
                pos += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        let lit_end = pos + lit;
        if lit_end > src.len() {
            bail!("lz4: literal out of bounds");
        }
        out.extend_from_slice(&src[pos..lit_end]);
        pos = lit_end;
        if pos >= src.len() {
            break;
        }
        if pos + 2 > src.len() {
            bail!("lz4: offset eof");
        }
        let offset = (src[pos] as usize) | ((src[pos + 1] as usize) << 8);
        pos += 2;
        if offset == 0 {
            bail!("lz4: zero offset");
        }
        let mut mlen = (token & 0x0f) as usize;
        if mlen == 15 {
            loop {
                let b = *src
                    .get(pos)
                    .ok_or_else(|| anyhow::anyhow!("lz4: match eof"))?;
                pos += 1;
                mlen += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        mlen += 4;
        let sidx = out
            .len()
            .checked_sub(offset)
            .filter(|&i| i >= start)
            .ok_or_else(|| anyhow::anyhow!("lz4: offset before block"))?;
        if offset >= mlen {
            let base = out.len();
            out.resize(base + mlen, 0);
            out.copy_within(sidx..sidx + mlen, base);
        } else {
            let base = out.len();
            out.resize(base + mlen, 0);
            for i in 0..mlen {
                let b = out[sidx + i];
                out[base + i] = b;
            }
        }
    }
    if out.len() - start != expected {
        bail!("lz4 size mismatch {} != {}", out.len() - start, expected);
    }
    Ok(())
}

pub fn decompress_bv4(blob: &[u8]) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0;
    loop {
        if pos + 4 > blob.len() {
            bail!("truncated bv4 stream");
        }
        let magic = &blob[pos..pos + 4];
        if magic == b"bv4$" {
            if pos + 4 != blob.len() {
                bail!("trailing bytes after bv4$");
            }
            return Ok(out);
        }
        if magic != b"bv41" && magic != b"bv4-" {
            bail!("not a bv4 block: {magic:?}");
        }
        if pos + 12 > blob.len() {
            bail!("truncated bv4 header");
        }
        let usize_ = u32::from_le_bytes(blob[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let stored = u32::from_le_bytes(blob[pos + 8..pos + 12].try_into().unwrap()) as usize;
        let end = pos
            .checked_add(12)
            .and_then(|p| p.checked_add(stored))
            .filter(|&e| e <= blob.len())
            .ok_or_else(|| anyhow::anyhow!("truncated bv4 block"))?;
        let payload = &blob[pos + 12..end];
        if magic == b"bv4-" {
            if payload.len() != usize_ {
                bail!("bv4- size mismatch");
            }
            out.extend_from_slice(payload);
        } else {
            lz4_block(payload, usize_, &mut out)?;
        }
        pos = end;
    }
}
