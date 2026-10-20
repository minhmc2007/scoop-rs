//! scoop-rs binary entry point.
//!
//! Mutating operations take an exclusive lock on <root>/db/scoop-rs.lock so
//! two installs never interleave (pacman-style locking).

use clap::Parser;
use scoop_rs::cli::Cli;
use scoop_rs::commands::{self, App};
use scoop_rs::error::Result;

fn main() {
    let cli = Cli::parse();
    let code = run(cli);
    std::process::exit(code);
}

fn run(cli: Cli) -> i32 {
    match inner(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {}", console::style("error:").red().bold(), e);
            1
        }
    }
}

fn inner(cli: &Cli) -> Result<i32> {
    // mutating commands need the lock; read-only ones do not
    let mutating = matches!(
        cli.command,
        scoop_rs::cli::Command::Install { .. }
            | scoop_rs::cli::Command::Uninstall { .. }
            | scoop_rs::cli::Command::Update { .. }
            | scoop_rs::cli::Command::Bucket(_)
    );

    let app = App::open(cli.quiet)?;

    if mutating {
        let lock_path = app.root.lock_file();
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        use fs2::FileExt;
        lock.try_lock_exclusive()
            .map_err(|_| scoop_rs::error::ScoopError::Locked(lock_path.display().to_string()))?;
    }

    commands::dispatch(&app, &cli.command, cli.no_progress)
}
