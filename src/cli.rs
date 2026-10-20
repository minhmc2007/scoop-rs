//! clap CLI definitions.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "scoop-rs",
    version,
    about = "A fast, native-Rust Scoop client with a pacman-style local database",
    long_about = "scoop-rs is a native-Rust client for the Scoop package ecosystem.\n\
                 It reads the same buckets and manifests as PowerShell Scoop, stores its\n\
                 state in a pacman-style local SQLite database, and never modifies the\n\
                 upstream Scoop repos or bucket contents.\n\n\
                 Common ops (search/info/list/status) never launch PowerShell - that is\n\
                 where most of Scoop's startup latency comes from."
)]
pub struct Cli {
    /// Suppress informational output
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Disable download progress bars
    #[arg(long, global = true)]
    pub no_progress: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Install apps (dependencies resolved and installed first)
    Install {
        /// App names: `app`, `bucket/app`, or a local manifest path
        apps: Vec<String>,
        /// Reinstall even if already installed
        #[arg(long)]
        force: bool,
        /// Do not resolve/install dependencies
        #[arg(long)]
        skip_deps: bool,
        /// Use the download cache without re-verifying remote availability
        #[arg(long, default_value_t = true)]
        use_cached: bool,
    },
    /// Uninstall apps (keeps persist data unless --purge)
    Uninstall {
        apps: Vec<String>,
        /// Also remove the persist directory
        #[arg(long)]
        purge: bool,
    },
    /// Update buckets, apps, or list available updates
    Update {
        /// Apps to update; empty = only show what is updatable
        apps: Vec<String>,
        /// Update every updatable app
        #[arg(long)]
        all: bool,
        /// Do not git-sync buckets first
        #[arg(long)]
        no_sync: bool,
    },
    /// List installed packages (pacman -Q style)
    List {
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// Also list the installed files of each (or the named) package
        #[arg(long)]
        files: bool,
    },
    /// Search the bucket index (instant, served from the local database)
    Search {
        query: String,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show details about an app
    Info {
        app: String,
        /// Run strict manifest schema validation and show diagnostics
        #[arg(long)]
        validate: bool,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show installed vs available versions (fast: no git, no PowerShell)
    Status {
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Manage buckets (git repos of manifests)
    #[command(subcommand)]
    Bucket(BucketCmd),
    /// Validate a manifest file against the schema checker
    Validate {
        /// Path to a manifest .json file
        file: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum BucketCmd {
    /// Add a bucket (clones it with --depth 1)
    Add {
        name: String,
        /// Custom git URL or `owner/repo`; defaults to the official map
        #[arg(long)]
        url: Option<String>,
    },
    /// List installed buckets
    #[command(alias = "ls")]
    List,
    /// List known buckets (official map + live fetch)
    Known,
    /// Remove a bucket
    Remove {
        name: String,
        #[arg(long)]
        force: bool,
    },
}
