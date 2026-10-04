use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use rayon::prelude::*;

use oblyx::convert::{
    ConvertOptions, Job, ZipSink, collect_inputs, convert_job, convert_job_sink, is_zip,
    parse_formats, plan_jobs,
};
use oblyx::mem::{JobLimiter, MemBudget};

#[derive(Parser, Debug)]
#[command(
    name = "oblyx",
    version,
    about = "Convert GoodNotes .goodnotes archives to SVG, PDF and PNG"
)]
struct Args {
    /// .goodnotes files, zip archives of notebooks, or directories to scan recursively
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

    /// Verbose logging: -v shows stages, pages and assets; -vv adds every stroke
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Also render deleted (tombstoned) objects
    #[arg(long)]
    include_deleted: bool,

    /// Pack all outputs into one zip archive instead of a folder tree.
    /// Optional path: --archive or --archive=out.zip (auto-named otherwise).
    /// Archive jobs convert sequentially.
    #[arg(
        long,
        value_name = "FILE",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true
    )]
    archive: Option<String>,
}

fn run(args: &Args) -> Result<()> {
    oblyx::verbose::set(args.verbose);
    let formats = parse_formats(&args.format)?;
    let options = ConvertOptions {
        formats,
        output: args.output.clone(),
        dpi: args.dpi,
        page: args.page.clone(),
        include_deleted: args.include_deleted,
    };
    options.validate()?;
    let files = collect_inputs(&args.inputs)?;
    let jobs = plan_jobs(&files, &options.formats, &options.output);

    if let Some(spec) = &args.archive {
        return run_archive(args, &options, &jobs, spec);
    }

    let pool = match args.jobs {
        Some(n) => Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(n.max(1))
                .build()
                .context("build thread pool")?,
        ),
        None => None,
    };

    let limiter = JobLimiter::new(
        MemBudget::from_env(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
    );

    let started = Instant::now();
    let job_results: Vec<Result<_>> = match &pool {
        Some(p) => p.install(|| {
            jobs.par_iter()
                .map(|j| limiter.run(|| convert_job(j, &options)))
                .collect()
        }),
        None => jobs
            .par_iter()
            .map(|j| limiter.run(|| convert_job(j, &options)))
            .collect(),
    };
    let elapsed = started.elapsed();

    let mut pages_total = 0usize;
    let mut written_total = 0usize;
    let mut files_ok = 0usize;
    let mut failed = 0usize;

    for (job, result) in jobs.iter().zip(&job_results) {
        match result {
            Ok(outcome) => {
                files_ok += 1;
                pages_total += outcome.pages;
                written_total += outcome.written;
                if !args.quiet {
                    let fmts: Vec<&str> = job.targets.iter().map(|(f, _)| f.name()).collect();
                    println!(
                        "{}: {} page(s) -> {} in {:.2}s",
                        job.label(),
                        outcome.pages,
                        fmts.join("+"),
                        outcome.took.as_secs_f32()
                    );
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: error: {e:#}", job.label());
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

/// Convert every job into a single zip archive (sequential — one shared writer).
fn run_archive(args: &Args, options: &ConvertOptions, jobs: &[Job], spec: &str) -> Result<()> {
    let path = if spec.is_empty() {
        let dir = match &options.output {
            Some(dir) => dir.clone(),
            None => {
                let first = &args.inputs[0];
                if first.is_dir() {
                    first.clone()
                } else {
                    first
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from("."))
                }
            }
        };
        let name = if args.inputs.len() == 1 && is_zip(&args.inputs[0]) {
            args.inputs[0]
                .file_stem()
                .map(|s| format!("{}-converted.zip", s.to_string_lossy()))
                .unwrap_or_else(|| "converted.zip".into())
        } else {
            "converted.zip".into()
        };
        dir.join(name)
    } else {
        PathBuf::from(spec)
    };

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }

    let output_identity = if path.exists() {
        std::fs::canonicalize(&path).with_context(|| format!("resolve {}", path.display()))?
    } else {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let parent = std::fs::canonicalize(parent)
            .with_context(|| format!("resolve output directory {}", parent.display()))?;
        parent.join(path.file_name().unwrap_or_default())
    };
    for input in &args.inputs {
        if input.exists() && std::fs::canonicalize(input).ok().as_ref() == Some(&output_identity) {
            bail!(
                "archive output {} would overwrite an input file",
                path.display()
            );
        }
    }

    let file =
        std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let sink = ZipSink::new(std::io::BufWriter::new(file));

    let started = Instant::now();
    let mut pages_total = 0usize;
    let mut written_total = 0usize;
    let mut files_ok = 0usize;
    let mut failed = 0usize;

    for job in jobs {
        match convert_job_sink(job, options, &sink, |_| {}) {
            Ok(outcome) => {
                files_ok += 1;
                pages_total += outcome.pages;
                written_total += outcome.written;
                if !args.quiet {
                    let fmts: Vec<&str> = job.targets.iter().map(|(f, _)| f.name()).collect();
                    println!(
                        "{}: {} page(s) -> {} in {:.2}s",
                        job.label(),
                        outcome.pages,
                        fmts.join("+"),
                        outcome.took.as_secs_f32()
                    );
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: error: {e:#}", job.label());
            }
        }
    }

    let mut writer = sink.finish()?;
    std::io::Write::flush(&mut writer).context("flush archive")?;
    drop(writer);
    let elapsed = started.elapsed();
    if !args.quiet {
        println!("wrote {}", path.display());
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
