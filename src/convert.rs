use std::collections::HashSet;
use std::io::Write;
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

pub fn is_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// One discovered input: a notebook file on disk, or a member inside a
/// `.zip` archive of notebooks.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InputFile {
    /// The notebook itself, or the containing `.zip` archive for `member`.
    pub path: PathBuf,
    /// Member name inside `path` when the notebook lives in a zip archive.
    pub member: Option<String>,
    /// Output subdirectory mirroring the member's folder inside the archive.
    pub out_subdir: Option<PathBuf>,
    /// File stem used for output names.
    pub stem: String,
}

impl InputFile {
    fn from_path(path: PathBuf) -> Self {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".into());
        Self {
            path,
            member: None,
            out_subdir: None,
            stem,
        }
    }

    /// Short name for lists: the member path inside a zip, else the file name.
    pub fn display_name(&self) -> String {
        match &self.member {
            Some(m) => m.clone(),
            None => self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.display().to_string()),
        }
    }

    /// Full label for logs: `path`, or `path!member` for zip members.
    pub fn label(&self) -> String {
        match &self.member {
            Some(m) => format!("{}!{}", self.path.display(), m),
            None => self.path.display().to_string(),
        }
    }
}

/// Split a zip entry name into `(subdirectory, stem)` for a `.goodnotes`
/// member. Returns `None` for non-notebooks, hidden files, `..` traversal
/// and `__MACOSX`/AppleDouble junk.
pub fn zip_member_layout(name: &str) -> Option<(Option<PathBuf>, String)> {
    let norm = name.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in norm.split('/') {
        match part {
            "" | "." => continue,
            ".." | "__MACOSX" => return None,
            p if p.starts_with('.') => return None,
            p => parts.push(p),
        }
    }
    let file = parts.pop()?;
    if !is_goodnotes(Path::new(file)) {
        return None;
    }
    let stem = Path::new(file).file_stem()?.to_string_lossy().into_owned();
    let out_subdir = if parts.is_empty() {
        None
    } else {
        Some(parts.iter().collect())
    };
    Some((out_subdir, stem))
}

fn walk_dir(dir: &Path, out: &mut Vec<InputFile>) -> Result<()> {
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
            out.push(InputFile::from_path(path));
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn collect_zip(zip_path: &Path, out: &mut Vec<InputFile>) -> Result<()> {
    let file =
        std::fs::File::open(zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a readable zip archive", zip_path.display()))?;
    for i in 0..archive.len() {
        let name = archive
            .by_index(i)
            .with_context(|| format!("{}: read zip entry {i}", zip_path.display()))?
            .name()
            .to_string();
        let Some((out_subdir, stem)) = zip_member_layout(&name) else {
            continue;
        };
        out.push(InputFile {
            path: zip_path.to_path_buf(),
            member: Some(name),
            out_subdir,
            stem,
        });
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn collect_zip(_: &Path, _: &mut Vec<InputFile>) -> Result<()> {
    bail!("zip input is not supported on this target")
}

pub fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<InputFile>> {
    let mut files = Vec::new();
    for input in inputs {
        let meta = std::fs::metadata(input).with_context(|| format!("stat {}", input.display()))?;
        if meta.is_dir() {
            walk_dir(input, &mut files)?;
        } else if is_goodnotes(input) {
            files.push(InputFile::from_path(input.clone()));
        } else if is_zip(input) {
            collect_zip(input, &mut files)?;
        } else {
            bail!("{}: not a .goodnotes or .zip file", input.display());
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
    pub member: Option<String>,
    /// Output root the targets live under. Archives store entries relative
    /// to it, so nested inputs keep their folder structure.
    pub root: PathBuf,
    pub targets: Vec<(Fmt, PathBuf)>,
}

impl Job {
    /// Full label for logs and UI: `path`, or `path!member` for zip members.
    pub fn label(&self) -> String {
        match &self.member {
            Some(m) => format!("{}!{}", self.file.display(), m),
            None => self.file.display().to_string(),
        }
    }
}

pub fn plan_jobs(files: &[InputFile], formats: &[Fmt], output: &Option<PathBuf>) -> Vec<Job> {
    let mut used: HashSet<PathBuf> = HashSet::new();
    files
        .iter()
        .map(|input| {
            let root = match output {
                Some(dir) => dir.clone(),
                None => input
                    .path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(".")),
            };
            let base = match &input.out_subdir {
                Some(sub) => root.join(sub),
                None => root.clone(),
            };
            let stem = input.stem.clone();
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
                file: input.path.clone(),
                member: input.member.clone(),
                root,
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

/// Where converted bytes go. `FolderSink` writes a directory tree;
/// `ZipSink` packs every job into one archive.
pub trait Sink: Sync {
    /// Store `bytes` for target `path`. `root` is the output root the target
    /// lives under — archives keep entries relative to it so nested inputs
    /// (e.g. zip members) preserve their folder structure.
    fn write(&self, root: &Path, path: &Path, bytes: Vec<u8>) -> Result<()>;
}

/// Writes targets straight to the file system (the default destination).
pub struct FolderSink;

impl Sink for FolderSink {
    fn write(&self, _root: &Path, path: &Path, bytes: Vec<u8>) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
    }
}

/// Packs every target into a single zip archive. Entry names are the target
/// paths relative to `root`, uniquified if two inputs would collide.
pub struct ZipSink<W: std::io::Write + std::io::Seek + Send> {
    inner: std::sync::Mutex<ZipInner<W>>,
}

struct ZipInner<W: std::io::Write + std::io::Seek> {
    zip: zip::ZipWriter<W>,
    used: HashSet<String>,
}

impl<W: std::io::Write + std::io::Seek + Send> ZipSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: std::sync::Mutex::new(ZipInner {
                zip: zip::ZipWriter::new(writer),
                used: HashSet::new(),
            }),
        }
    }

    /// Finish the archive and return the underlying writer.
    pub fn finish(self) -> Result<W> {
        let inner = self.inner.into_inner().unwrap_or_else(|e| e.into_inner());
        Ok(inner.zip.finish()?)
    }
}

/// Add `-{n}` before the final extension (after the last `/`).
fn zip_uniquify(name: &str, n: usize) -> String {
    let slash = name.rfind('/').map_or(0, |i| i + 1);
    match name[slash..].rfind('.') {
        Some(i) if i > 0 => format!("{}-{n}.{}", &name[..slash + i], &name[slash + i + 1..]),
        _ => format!("{name}-{n}"),
    }
}

impl<W: std::io::Write + std::io::Seek + Send> Sink for ZipSink<W> {
    fn write(&self, root: &Path, path: &Path, bytes: Vec<u8>) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let rel = if root.as_os_str().is_empty() {
            // already-relative target (the wasm archive path)
            path.to_path_buf()
        } else {
            match path.strip_prefix(root) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => path.file_name().map(PathBuf::from).unwrap_or_default(),
            }
        };
        let mut name = rel.to_string_lossy().replace('\\', "/");
        if !inner.used.insert(name.clone()) {
            let mut n = 2;
            loop {
                let cand = zip_uniquify(&name, n);
                if inner.used.insert(cand.clone()) {
                    name = cand;
                    break;
                }
                n += 1;
            }
        }
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        inner.zip.start_file(name, opts)?;
        inner.zip.write_all(&bytes)?;
        Ok(())
    }
}

pub fn decode_pages(
    doc: &Document,
    filter: Option<&str>,
    include_deleted: bool,
    dpi: f32,
) -> Result<Vec<Page>> {
    decode_pages_with(doc, filter, include_deleted, dpi, &|_| {})
}

pub fn decode_pages_with(
    doc: &Document,
    filter: Option<&str>,
    include_deleted: bool,
    dpi: f32,
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
                .decode_page(src, include_deleted, dpi)
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
    convert_job_sink(job, options, &FolderSink, progress)
}

pub fn convert_job_sink(
    job: &Job,
    options: &ConvertOptions,
    sink: &dyn Sink,
    progress: impl Fn(JobProgress) + Sync,
) -> Result<JobOutcome> {
    let started = Instant::now();
    let label = job.label();
    crate::vlog!(1, "opening {label}");
    let doc = match &job.member {
        Some(member) => Document::from_bytes(read_zip_member(&job.file, member)?)?,
        None => Document::open(&job.file)?,
    };
    if doc.schema != 0 && doc.schema != 35 {
        eprintln!(
            "{label}: warning: unexpected schema {} (expected 35)",
            doc.schema
        );
    }
    crate::vlog!(
        1,
        "{label}: index has {} page(s), {} attachment(s)",
        doc.pages.len(),
        doc.attachments.len()
    );
    crate::vlog!(1, "{label}: decoding page(s)");
    let pages = decode_pages_with(
        &doc,
        options.page.as_deref(),
        options.include_deleted,
        options.dpi,
        &progress,
    )?;
    crate::vlog!(1, "{label}: decoded {} page(s)", pages.len());
    let scale = (options.dpi / 72.0).max(0.1);
    let mut written = 0usize;

    for (fmt, path) in &job.targets {
        crate::vlog!(1, "{label}: writing {} -> {}", fmt.name(), path.display());
        match fmt {
            Fmt::Pdf => {
                if pages.is_empty() {
                    continue;
                }
                let total = pages.len();
                let prog = &progress;
                let bytes = document_to_pdf_with(&pages, &doc, move |done| {
                    crate::vlog!(1, "  pdf page {done}/{total}");
                    prog(JobProgress::Render { done, total });
                });
                sink.write(&job.root, path, bytes)?;
                crate::vlog!(1, "{label}: wrote {}", path.display());
                written += 1;
            }
            Fmt::Svg => {
                let total = pages.len();
                let done = AtomicUsize::new(0);
                let prog = &progress;
                pages
                    .par_iter()
                    .map(|page| {
                        let svg = page_to_svg(page, &doc);
                        sink.write(
                            &job.root,
                            &path.join(format!("{}.svg", page.uuid)),
                            svg.into_bytes(),
                        )
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
                let total = pages.len();
                let done = AtomicUsize::new(0);
                let prog = &progress;
                pages
                    .par_iter()
                    .map(|page| {
                        let png = page_to_png(page, &doc, scale)
                            .with_context(|| format!("render {}", page.uuid))?;
                        sink.write(&job.root, &path.join(format!("{}.png", page.uuid)), png)
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
        "{label}: done, {written} output(s) in {:.2}s",
        started.elapsed().as_secs_f32()
    );
    Ok(JobOutcome {
        pages: pages.len(),
        written,
        took: started.elapsed(),
    })
}

/// Read one member out of a notebook `.zip` on disk.
#[cfg(not(target_arch = "wasm32"))]
fn read_zip_member(zip_path: &Path, member: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    let file =
        std::fs::File::open(zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a readable zip archive", zip_path.display()))?;
    let mut entry = archive
        .by_name(member)
        .with_context(|| format!("{}: no member {member:?}", zip_path.display()))?;
    let mut buf = Vec::new();
    entry
        .read_to_end(&mut buf)
        .with_context(|| format!("read {member} from {}", zip_path.display()))?;
    Ok(buf)
}

#[cfg(target_arch = "wasm32")]
fn read_zip_member(_: &Path, _: &str) -> Result<Vec<u8>> {
    bail!("zip input members are not supported on this target")
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
        let files = vec![InputFile::from_path(PathBuf::from(
            "/tmp/a/notebook.goodnotes",
        ))];
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
        assert_eq!(jobs[0].root, PathBuf::from("/tmp/a"));
    }

    #[test]
    fn plan_uniquifies_colliding_names() {
        let files = vec![
            InputFile::from_path(PathBuf::from("/x/one/notebook.goodnotes")),
            InputFile::from_path(PathBuf::from("/x/two/notebook.goodnotes")),
        ];
        let out = PathBuf::from("/out");
        let jobs = plan_jobs(&files, &[Fmt::Pdf], &Some(out.clone()));
        assert_eq!(jobs[0].targets[0].1, PathBuf::from("/out/notebook.pdf"));
        assert_eq!(jobs[1].targets[0].1, PathBuf::from("/out/notebook-2.pdf"));
    }

    #[test]
    fn zip_member_layout_filters_and_splits() {
        assert_eq!(
            zip_member_layout("math/week 3/note.goodnotes"),
            Some((Some(PathBuf::from("math/week 3")), "note".into()))
        );
        assert_eq!(
            zip_member_layout("note.GOODNOTES"),
            Some((None, "note".into()))
        );
        // junk, hidden files and traversal are skipped
        assert_eq!(zip_member_layout("__MACOSX/._note.goodnotes"), None);
        assert_eq!(zip_member_layout("../note.goodnotes"), None);
        assert_eq!(zip_member_layout(".hidden/note.goodnotes"), None);
        assert_eq!(zip_member_layout("dir/._note.goodnotes"), None);
        assert_eq!(zip_member_layout("note.pdf"), None);
        assert_eq!(zip_member_layout("some-dir/"), None);
    }

    #[test]
    fn plan_preserves_zip_structure() {
        let files = vec![InputFile {
            path: PathBuf::from("/z/archive.zip"),
            member: Some("math/week3/note.goodnotes".into()),
            out_subdir: Some(PathBuf::from("math/week3")),
            stem: "note".into(),
        }];
        let jobs = plan_jobs(&files, &[Fmt::Pdf], &None);
        let job = &jobs[0];
        assert_eq!(job.targets[0].1, PathBuf::from("/z/math/week3/note.pdf"));
        // archives store entries relative to the job's output root
        let entry = job.targets[0].1.strip_prefix(&job.root).unwrap();
        assert_eq!(entry, PathBuf::from("math/week3/note.pdf"));
    }

    #[test]
    fn zip_sink_writes_relative_entries() {
        let sink = ZipSink::new(std::io::Cursor::new(Vec::new()));
        sink.write(Path::new("/x"), Path::new("/x/m/n.pdf"), b"one".to_vec())
            .unwrap();
        // same relative name from a different root gets uniquified
        sink.write(Path::new("/y"), Path::new("/y/m/n.pdf"), b"two".to_vec())
            .unwrap();
        sink.write(
            Path::new("/out"),
            Path::new("/out/a/n.pdf"),
            b"three".to_vec(),
        )
        .unwrap();
        let buf = sink.finish().unwrap().into_inner();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(buf)).unwrap();
        assert_eq!(archive.len(), 3);
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["a/n.pdf", "m/n-2.pdf", "m/n.pdf"]);
        let mut first = String::new();
        use std::io::Read;
        archive
            .by_name("m/n.pdf")
            .unwrap()
            .read_to_string(&mut first)
            .unwrap();
        assert_eq!(first, "one");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn collect_inputs_lists_zip_members() {
        let dir = std::env::temp_dir().join(format!("oblyx-zip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("batch.zip");
        {
            use std::io::Write;
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("top.goodnotes", opts).unwrap();
            zw.write_all(b"fake").unwrap();
            zw.start_file("math/week3/note.goodnotes", opts).unwrap();
            zw.write_all(b"fake").unwrap();
            zw.start_file("__MACOSX/._top.goodnotes", opts).unwrap();
            zw.write_all(b"junk").unwrap();
            zw.start_file("notes.pdf", opts).unwrap();
            zw.write_all(b"nope").unwrap();
            zw.finish().unwrap();
        }
        let found = collect_inputs(std::slice::from_ref(&zip_path)).unwrap();
        let members: Vec<&str> = found.iter().filter_map(|f| f.member.as_deref()).collect();
        assert_eq!(members, vec!["math/week3/note.goodnotes", "top.goodnotes"]);
        let note = &found[0];
        assert_eq!(note.out_subdir, Some(PathBuf::from("math/week3")));
        assert_eq!(note.stem, "note");
        assert_eq!(note.path, zip_path);
        let top = &found[1];
        assert_eq!(top.out_subdir, None);
        assert_eq!(top.stem, "top");
        let _ = std::fs::remove_dir_all(&dir);
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
