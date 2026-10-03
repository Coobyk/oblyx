use std::path::PathBuf;

use clap::Parser;
use oblyx::gui::{Launch, run};

#[derive(Parser, Debug)]
#[command(
    name = "oblyx-gui",
    version,
    about = "GoodNotes .goodnotes converter with a native GUI"
)]
struct Args {
    /// .goodnotes files or folders to load on startup
    #[arg(value_name = "INPUT")]
    files: Vec<PathBuf>,

    /// Output directory (default: next to each input file)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Start converting immediately on launch
    #[arg(long)]
    start: bool,
}

fn main() {
    let args = Args::parse();
    run(Launch {
        files: args.files,
        output: args.output,
        start: args.start,
    });
}
