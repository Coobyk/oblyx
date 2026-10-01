mod bv4;
mod doc;
mod geom;
mod paper;
mod pb;
mod render;
mod tpl;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use rayon::prelude::*;

use doc::{Document, Page, PageSource};
use render::{pdf::document_to_pdf, png::page_to_png, svg::page_to_svg};

#[derive(Parser, Debug)]
#[command(
    name = "goodnotes-convert",
    version,
    about = "Convert GoodNotes .goodnotes archives to SVG, PDF and PNG"
)]
struct Args {
    /// .goodnotes files or directories (directories are scanned recursively)
    #[arg(required = true, value_name = "INPUT")]
    inputs: Vec<PathBuf>,

    /// Output format(s): svg, png, pdf, or all (comma-separated)
    #[arg(short, long, default_value = "svg")]
    format: String,

    /// Output directory (default: next to each input file)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Raster resolution for PNG output, in DPI
    #[arg(long, default_value_t = 144.0)]
    dpi: f32,

    /// Only convert pages whose UUID contains this text
    #[arg(long)]
    page: Option<String>,

    /// Number of parallel jobs (default: all CPU cores)
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Suppress per-file progress output
    #[arg(short, long)]
    quiet: bool,

    /// Also render deleted (tombstoned) objects
    #[arg(long)]
    include_deleted: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fmt {
    Svg,
    Png,
    Pdf,
}

impl Fmt {
    fn name(self) -> &'static str {
        match self {
            Fmt::Svg => "svg",
            Fmt::Png => "png",
            Fmt::Pdf => "pdf",
        }
    }
}

fn parse_formats(spec: &str) -> Result<Vec<Fmt>> {
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

fn is_goodnotes(path: &Path) -> bool {
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

fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
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

struct Job {
    file: PathBuf,
    targets: Vec<(Fmt, PathBuf)>,
}

fn plan_jobs(files: &[PathBuf], formats: &[Fmt], output: &Option<PathBuf>) -> Vec<Job> {
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

fn decode_pages(doc: &Document, filter: Option<&str>, include_deleted: bool) -> Result<Vec<Page>> {
    let filter_upper = filter.map(|f| f.to_ascii_uppercase());
    let sources: Vec<&PageSource> = doc
        .pages
        .iter()
        .filter(|p| match &filter_upper {
            Some(f) => p.uuid.to_ascii_uppercase().contains(f.as_str()),
            None => true,
        })
        .collect();
    sources
        .par_iter()
        .map(|src| {
            doc.decode_page(src, include_deleted)
                .with_context(|| format!("decode page {}", src.uuid))
        })
        .collect()
}

fn convert_job(job: &Job, args: &Args) -> Result<(usize, usize, std::time::Duration)> {
    let started = Instant::now();
    let doc = Document::open(&job.file)?;
    if doc.schema != 0 && doc.schema != 35 {
        eprintln!(
            "{}: warning: unexpected schema {} (expected 35)",
            job.file.display(),
            doc.schema
        );
    }
    let pages = decode_pages(&doc, args.page.as_deref(), args.include_deleted)?;
    let scale = (args.dpi / 72.0).max(0.1);
    let mut written = 0usize;

    for (fmt, path) in &job.targets {
        match fmt {
            Fmt::Pdf => {
                if pages.is_empty() {
                    continue;
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let bytes = document_to_pdf(&pages, &doc);
                std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
                written += 1;
            }
            Fmt::Svg => {
                std::fs::create_dir_all(path)?;
                pages
                    .par_iter()
                    .map(|page| {
                        let svg = page_to_svg(page, &doc);
                        std::fs::write(path.join(format!("{}.svg", page.uuid)), svg)
                            .with_context(|| format!("write {}", page.uuid))
                    })
                    .collect::<Result<Vec<_>>>()?;
                written += pages.len();
            }
            Fmt::Png => {
                std::fs::create_dir_all(path)?;
                pages
                    .par_iter()
                    .map(|page| {
                        let png = page_to_png(page, &doc, scale)
                            .with_context(|| format!("render {}", page.uuid))?;
                        std::fs::write(path.join(format!("{}.png", page.uuid)), png)
                            .with_context(|| format!("write {}", page.uuid))
                    })
                    .collect::<Result<Vec<_>>>()?;
                written += pages.len();
            }
        }
    }
    Ok((pages.len(), written, started.elapsed()))
}

fn run(args: &Args) -> Result<()> {
    let formats = parse_formats(&args.format)?;
    if !args.dpi.is_finite() || args.dpi <= 0.0 {
        bail!("--dpi must be a positive number");
    }
    let files = collect_inputs(&args.inputs)?;
    let jobs = plan_jobs(&files, &formats, &args.output);

    let pool = match args.jobs {
        Some(n) => Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(n.max(1))
                .build()
                .context("build thread pool")?,
        ),
        None => None,
    };

    let started = Instant::now();
    let job_results: Vec<Result<(usize, usize, std::time::Duration)>> = match &pool {
        Some(p) => p.install(|| jobs.par_iter().map(|j| convert_job(j, args)).collect()),
        None => jobs.par_iter().map(|j| convert_job(j, args)).collect(),
    };
    let elapsed = started.elapsed();

    let mut pages_total = 0usize;
    let mut written_total = 0usize;
    let mut files_ok = 0usize;
    let mut failed = 0usize;

    for (job, result) in jobs.iter().zip(&job_results) {
        match result {
            Ok((pages, written, took)) => {
                files_ok += 1;
                pages_total += pages;
                written_total += written;
                if !args.quiet {
                    let fmts: Vec<&str> = job.targets.iter().map(|(f, _)| f.name()).collect();
                    println!(
                        "{}: {pages} page(s) -> {} in {:.2}s",
                        job.file.display(),
                        fmts.join("+"),
                        took.as_secs_f32()
                    );
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: error: {e:#}", job.file.display());
            }
        }
    }

    if !args.quiet {
        println!(
            "converted {files_ok} file(s), {pages_total} page(s), {written_total} output(s) in {:.2}s",
            elapsed.as_secs_f32()
        );
    }
    if failed > 0 {
        bail!("{failed} file(s) failed to convert");
    }
    Ok(())
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(&args) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
