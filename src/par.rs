//! Thin wrappers over rayon that degrade to sequential iteration on wasm,
//! where spawning the global thread pool would panic.
//!
//! Everything in the crate that should run on multiple cores goes through
//! these functions instead of calling `par_iter` directly.

/// `par_iter().map(f).collect()` on native targets, `iter().map(f)` on wasm.
/// Order is preserved either way.
#[cfg(not(target_arch = "wasm32"))]
pub fn map<'a, T, R, F>(items: &'a [T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&'a T) -> R + Sync + Send,
{
    use rayon::prelude::*;
    items.par_iter().map(f).collect()
}

#[cfg(target_arch = "wasm32")]
pub fn map<'a, T, R, F>(items: &'a [T], f: F) -> Vec<R>
where
    F: Fn(&'a T) -> R,
{
    items.iter().map(f).collect()
}

/// Same as [`map`] but the closure also receives the item index.
#[cfg(not(target_arch = "wasm32"))]
pub fn mapi<'a, T, R, F>(items: &'a [T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &'a T) -> R + Sync + Send,
{
    use rayon::prelude::*;
    items.par_iter().enumerate().map(|(i, t)| f(i, t)).collect()
}

#[cfg(target_arch = "wasm32")]
pub fn mapi<'a, T, R, F>(items: &'a [T], f: F) -> Vec<R>
where
    F: Fn(usize, &'a T) -> R,
{
    items.iter().enumerate().map(|(i, t)| f(i, t)).collect()
}

/// Read several members out of one zip archive. On native targets each
/// member inflates on a rayon worker over its own `ZipArchive` view of the
/// shared bytes — central-directory parsing is cheap next to inflate, and
/// the archives are otherwise independent.
/// Returns one `Option` per name, preserving order (`None` = missing).
#[cfg(not(target_arch = "wasm32"))]
pub fn zip_members(data: &[u8], names: &[&str]) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
    use anyhow::Context;
    use rayon::prelude::*;
    use std::io::{Cursor, Read};
    names
        .par_iter()
        .map(|name| {
            let mut z = zip::ZipArchive::new(Cursor::new(data))
                .context("zip open for member read")?;
            let mut f = match z.by_name(name) {
                Ok(f) => f,
                Err(zip::result::ZipError::FileNotFound) => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("zip member {name}")),
            };
            let mut buf = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut buf)
                .with_context(|| format!("read zip member {name}"))?;
            Ok(Some(buf))
        })
        .collect()
}

#[cfg(target_arch = "wasm32")]
pub fn zip_members(data: &[u8], names: &[&str]) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
    use anyhow::Context;
    use std::io::{Cursor, Read};
    let mut z =
        zip::ZipArchive::new(Cursor::new(data)).context("zip open for member read")?;
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let mut f = match z.by_name(name) {
            Ok(f) => f,
            Err(zip::result::ZipError::FileNotFound) => {
                out.push(None);
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("zip member {name}")),
        };
        let mut buf = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut buf)
            .with_context(|| format!("read zip member {name}"))?;
        out.push(Some(buf));
    }
    Ok(out)
}
