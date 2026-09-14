use anyhow::Result;
use brokerrouter::scanner_worker::{self, Config};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Isolated, non-billable media security workload; no database access")]
struct Cli {
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    ManifestRevision,
    Run {
        #[arg(long)]
        once: bool,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config: Config = scanner_worker::read_json(&cli.config)?;
    match cli.command {
        Command::ManifestRevision => println!("{}", config.manifest.revision()?),
        Command::Run { once } => {
            scanner_worker::run(config, std::env::var("BROKERROUTER_SCANNER_TOKEN")?, once).await?
        }
    }
    Ok(())
}
