use clap::Parser;

mod backend;
mod filesystem;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    socket_path: String,
    #[arg(long, default_value = "/dev/null")]
    shared_dir: String,  // Ignored, files are synthetic
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let _args = Args::parse();
    // Phase 7 Tasks 2-5 will fill in the daemon logic
    todo!()
}
