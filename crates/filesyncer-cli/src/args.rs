use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "filesyncer", version, about = "Bidirectional directory sync")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scan, show the plan, and apply it after confirmation.
    Sync {
        /// Profile id.
        profile: String,
        /// Apply without prompting.
        #[arg(long)]
        yes: bool,
        /// Show the plan and stop.
        #[arg(long)]
        dry_run: bool,
        /// Proceed even if the deletion threshold is exceeded.
        #[arg(long)]
        force: bool,
    },
    /// List configured profiles.
    List,
    /// Create a profile and write its root markers.
    Add {
        /// Profile name, also used as its id for the state database and root markers.
        name: String,
        local: PathBuf,
        remote: PathBuf,
    },
    /// Show a profile's paths, last sync time, and tracked path count.
    Status { profile: String },
}
