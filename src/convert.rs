use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::doc::{Document, Page, PageSource};
use crate::render::{pdf::document_to_pdf_with, png::page_to_png, svg::page_to_svg};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fmt {
    Svg,
    Png,
    Pdf,
}

impl Fmt {
    pub fn name(self) -> &'static str {
        match self {
            Fmt::Svg => "svg",
            Fmt::Png => "png",
            Fmt::Pdf => "pdf",
        }
    }
}

pub fn parse_formats(spec: &str) -> Result<Vec<Fmt>> {
    let mut out: Vec<Fmt> = Vec::new();
    for part in spec.split(',').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "svg" => {
                if !out.contains(&Fmt::Svg) {
                    out.push(Fmt::Svg)
                }
            }
            "png" => {
                if !out.contains(&Fmt::Png) {
                    out.push(Fmt::Png)
                }
            }
            "pdf" => {
                if !out.contains(&Fmt::Pdf) {
                    out.push(Fmt::Pdf)
                }
            }
            "all" => {
                for f in [Fmt::Svg, Fmt::Png, Fmt::Pdf] {
                    if !out.contains(&f) {
                        out.push(f)
                    }
                }
            }
            other => bail!("unknown format {other:?} (expected svg, png, pdf or all)"),
        }
    }
    if out.is_empty() {
        bail!("no output format given");
    }
    Ok(out)
}

pub fn is_goodnotes(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("goodnotes"))
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("read directory {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, out)?;
        } else if is_goodnotes(&path) {
            out.push(path);
        }
    }
    Ok(())
}

pub fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for input in inputs {
        let meta = std::fs::metadata(input).with_context(|| format!("stat {}", input.display()))?;
        if meta.is_dir() {
            walk_dir(input, &mut files)?;
        } else if is_goodnotes(input) {
            files.push(input.clone());
        } else {
            bail!("{}: not a .goodnotes file", input.display());
        }
    }
    if files.is_empty() {
        bail!("no .goodnotes files found in the given inputs");
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn uniquify(path: PathBuf, used: &mut HashSet<PathBuf>) -> PathBuf {
    if used.insert(path.clone()) {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    let parent = path.parent().map(Path::to_path_buf);
    let mut n = 2;
    loop {
        let name = match &ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        };
        let cand = match &parent {
            Some(p) => p.join(name),
            None => PathBuf::from(name),
        };
        if used.insert(cand.clone()) {
            return cand;
        }
        n += 1;
    }
}

/// One input file paired with the concrete output paths it will produce.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Job {
    pub file: PathBuf,
    pub targets: Vec<(Fmt, PathBuf)>,
}

pub fn plan_jobs(files: &[PathBuf], formats: &[Fmt], output: &Option<PathBuf>) -> Vec<Job> {
    let mut used: HashSet<PathBuf> = HashSet::new();
    files
        .iter()
        .map(|file| {
            let base = match output {
                Some(dir) => dir.clone(),
                None => file
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(".")),
            };
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "output".into());
            let mut dir_path: Option<PathBuf> = None;
            let mut targets = Vec::new();
            for &fmt in formats {
                match fmt {
                    Fmt::Pdf => {
                        let p = uniquify(base.join(format!("{stem}.pdf")), &mut used);
                        targets.push((Fmt::Pdf, p));
                    }
                    Fmt::Svg | Fmt::Png => {
                        let dir = dir_path
                            .get_or_insert_with(|| uniquify(base.join(stem.clone()), &mut used));
                        targets.push((fmt, dir.clone()));
                    }
                }
            }
            Job {
                file: file.clone(),
                targets,
            }
        })
        .collect()
}

/// Settings that drive a conversion run. Shared by the CLI and the GUI.
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    pub formats: Vec<Fmt>,
    pub output: Option<PathBuf>,
    pub dpi: f32,
    pub page: Option<String>,
    pub include_deleted: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            formats: vec![Fmt::Svg],
            output: None,
            dpi: 144.0,
            page: None,
            include_deleted: false,
        }
    }
}

impl ConvertOptions {
    pub fn validate(&self) -> Result<()> {
        if self.formats.is_empty() {
            bail!("no output format given");
        }
        if !self.dpi.is_finite() || self.dpi <= 0.0 {
            bail!("DPI must be a positive number");
        }
        Ok(())
    }
}

/// Per-page progress while one file converts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobProgress {
    /// Pages decoded so far (preparing phase).
    Decode { done: usize, total: usize },
    /// Pages rendered so far (writing phase, one range per target format).
    Render { done: usize, total: usize },
}

pub fn decode_pages(
    doc: &Document,
    filter: Option<&str>,
    include_deleted: bool,
) -> Result<Vec<Page>> {
    decode_pages_with(doc, filter, include_deleted, &|_| {})
}

pub fn decode_pages_with(
    doc: &Document,
    filter: Option<&str>,
    include_deleted: bool,
    progress: &(dyn Fn(JobProgress) + Sync),
) -> Result<Vec<Page>> {
    let filter_upper = filter.map(|f| f.to_ascii_uppercase());
    let sources: Vec<&PageSource> = doc
        .pages
        .iter()
        .filter(|p| match &filter_upper {
            Some(f) => p.uuid.to_ascii_uppercase().contains(f.as_str()),
            None => true,
        })
        .collect();
    let total = sources.len();
    let done = AtomicUsize::new(0);
    sources
        .par_iter()
        .map(|src| {
            let page = doc
                .decode_page(src, include_deleted)
                .with_context(|| format!("decode page {}", src.uuid))?;
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress(JobProgress::Decode { done: n, total });
            Ok(page)
        })
        .collect()
}

/// Outcome of converting one input file.
#[derive(Clone, Debug)]
pub struct JobOutcome {
    pub pages: usize,
    pub written: usize,
    pub took: Duration,
}

pub fn convert_job(job: &Job, options: &ConvertOptions) -> Result<JobOutcome> {
    convert_job_with(job, options, |_| {})
}

pub fn convert_job_with(
    job: &Job,
    options: &ConvertOptions,
    progress: impl Fn(JobProgress) + Sync,
) -> Result<JobOutcome> {
    let started = Instant::now();
    crate::vlog!(1, "opening {}", job.file.display());
    let doc = Document::open(&job.file)?;
    if doc.schema != 0 && doc.schema != 35 {
        eprintln!(
            "{}: warning: unexpected schema {} (expected 35)",
            job.file.display(),
            doc.schema
        );
    }
    crate::vlog!(
        1,
        "{}: index has {} page(s), {} attachment(s)",
        job.file.display(),
        doc.pages.len(),
        doc.attachments.len()
    );
    crate::vlog!(1, "{}: decoding page(s)", job.file.display());
    let pages = decode_pages_with(
        &doc,
        options.page.as_deref(),
        options.include_deleted,
        &progress,
    )?;
    crate::vlog!(1, "{}: decoded {} page(s)", job.file.display(), pages.len());
    let scale = (options.dpi / 72.0).max(0.1);
    let mut written = 0usize;

    for (fmt, path) in &job.targets {
        crate::vlog!(
            1,
            "{}: writing {} -> {}",
            job.file.display(),
            fmt.name(),
            path.display()
        );
        match fmt {
            Fmt::Pdf => {
                if pages.is_empty() {
                    continue;
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let total = pages.len();
                let prog = &progress;
                let bytes = document_to_pdf_with(&pages, &doc, move |done| {
                    crate::vlog!(1, "  pdf page {done}/{total}");
                    prog(JobProgress::Render { done, total });
                });
                std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
                crate::vlog!(1, "{}: wrote {}", job.file.display(), path.display());
                written += 1;
            }
            Fmt::Svg => {
                std::fs::create_dir_all(path)?;
                let total = pages.len();
                let done = AtomicUsize::new(0);
                let prog = &progress;
                pages
                    .par_iter()
                    .map(|page| {
                        let svg = page_to_svg(page, &doc);
                        std::fs::write(path.join(format!("{}.svg", page.uuid)), svg)
                            .with_context(|| format!("write {}", page.uuid))?;
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        crate::vlog!(1, "  svg page {}/{total}: {}", n, page.uuid);
                        prog(JobProgress::Render { done: n, total });
                        Ok(())
                    })
                    .collect::<Result<Vec<_>>>()?;
                written += pages.len();
            }
            Fmt::Png => {
                std::fs::create_dir_all(path)?;
                let total = pages.len();
                let done = AtomicUsize::new(0);
                let prog = &progress;
                pages
                    .par_iter()
                    .map(|page| {
                        let png = page_to_png(page, &doc, scale)
                            .with_context(|| format!("render {}", page.uuid))?;
                        std::fs::write(path.join(format!("{}.png", page.uuid)), png)
                            .with_context(|| format!("write {}", page.uuid))?;
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        crate::vlog!(1, "  png page {}/{total}: {}", n, page.uuid);
                        prog(JobProgress::Render { done: n, total });
                        Ok(())
                    })
                    .collect::<Result<Vec<_>>>()?;
                written += pages.len();
            }
        }
    }
    crate::vlog!(
        1,
        "{}: done, {} output(s) in {:.2}s",
        job.file.display(),
        written,
        started.elapsed().as_secs_f32()
    );
    Ok(JobOutcome {
        pages: pages.len(),
        written,
        took: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_formats() {
        assert_eq!(parse_formats("svg").unwrap(), vec![Fmt::Svg]);
        assert_eq!(
            parse_formats("pdf, svg ,pdf").unwrap(),
            vec![Fmt::Pdf, Fmt::Svg]
        );
        assert_eq!(
            parse_formats("all").unwrap(),
            vec![Fmt::Svg, Fmt::Png, Fmt::Pdf]
        );
        assert!(parse_formats("jpeg").is_err());
        assert!(parse_formats("").is_err());
    }

    #[test]
    fn plans_pdf_and_page_dir_targets() {
        let files = vec![PathBuf::from("/tmp/a/notebook.goodnotes")];
        let jobs = plan_jobs(&files, &[Fmt::Pdf, Fmt::Svg], &None);
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].targets[0],
            (Fmt::Pdf, PathBuf::from("/tmp/a/notebook.pdf"))
        );
        assert_eq!(
            jobs[0].targets[1],
            (Fmt::Svg, PathBuf::from("/tmp/a/notebook"))
        );
    }

    #[test]
    fn plan_uniquifies_colliding_names() {
        let files = vec![
            PathBuf::from("/x/one/notebook.goodnotes"),
            PathBuf::from("/x/two/notebook.goodnotes"),
        ];
        let out = PathBuf::from("/out");
        let jobs = plan_jobs(&files, &[Fmt::Pdf], &Some(out.clone()));
        assert_eq!(jobs[0].targets[0].1, PathBuf::from("/out/notebook.pdf"));
        assert_eq!(jobs[1].targets[0].1, PathBuf::from("/out/notebook-2.pdf"));
    }

    #[test]
    fn validates_options() {
        let mut opts = ConvertOptions::default();
        assert!(opts.validate().is_ok());
        opts.dpi = 0.0;
        assert!(opts.validate().is_err());
        opts.dpi = 144.0;
        opts.formats.clear();
        assert!(opts.validate().is_err());
    }
}
