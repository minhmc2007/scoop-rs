//! Command handlers gluing the CLI to the engine.

use crate::buckets;
use crate::cli::{BucketCmd, Command};
use crate::config::Root;
use crate::db::Db;
use crate::error::Result;
use crate::install::{install_specs, Ctx, InstallOptions};
use crate::uninstall;
use crate::util::Output;
use serde_json::json;

pub struct App {
    pub root: Root,
    pub db: Db,
    pub out: Output,
}

impl App {
    pub fn open(quiet: bool) -> Result<App> {
        let root = Root::resolve()?;
        root.ensure_layout()?;
        let db = Db::open(&root.db_file())?;
        let out = Output { quiet };
        // first-run import of existing PowerShell-Scoop installs
        let pkg_count = db.list_packages()?.len();
        if pkg_count == 0 {
            let _ = crate::install::import_existing(&root, &db, &out);
        }
        Ok(App { root, db, out })
    }

    fn ctx<'a>(&'a self, opts: &'a InstallOptions) -> Ctx<'a> {
        Ctx {
            root: &self.root,
            db: &self.db,
            out: &self.out,
            opts,
        }
    }
}

pub fn dispatch(app: &App, cmd: &Command, no_progress: bool) -> Result<i32> {
    match cmd {
        Command::Install {
            apps,
            force,
            skip_deps,
            use_cached: _use_cached,
        } => {
            if apps.is_empty() {
                app.out
                    .warn("nothing to install - specify at least one app");
                return Ok(1);
            }
            let opts = InstallOptions {
                force: *force,
                skip_deps: *skip_deps,
                no_progress,
            };
            let names: Vec<String> = apps.clone();
            install_specs(&app.ctx(&opts), &names)?;
            Ok(0)
        }
        Command::Uninstall { apps, purge } => {
            if apps.is_empty() {
                app.out
                    .warn("nothing to uninstall - specify at least one app");
                return Ok(1);
            }
            let opts = InstallOptions {
                force: false,
                skip_deps: true,
                no_progress,
            };
            uninstall::uninstall_apps(&app.root, &app.db, &app.out, &opts, apps, *purge)?;
            Ok(0)
        }
        Command::Update { apps, all, no_sync } => {
            let opts = InstallOptions {
                force: false,
                skip_deps: false,
                no_progress,
            };
            update_command(app, &opts, apps.clone(), *all, *no_sync)?;
            Ok(0)
        }
        Command::List { json, files } => {
            list_command(app, *json, *files)?;
            Ok(0)
        }
        Command::Search { query, json } => {
            search_command(app, query, *json)?;
            Ok(0)
        }
        Command::Info {
            app: name,
            validate,
            json,
        } => info_command(app, name, *validate, *json),
        Command::Status { json } => {
            status_command(app, *json)?;
            Ok(0)
        }
        Command::Bucket(bucket) => {
            bucket_command(app, bucket)?;
            Ok(0)
        }
        Command::Validate { file } => validate_command(app, file),
    }
}

fn update_command(
    app: &App,
    opts: &InstallOptions,
    apps: Vec<String>,
    all: bool,
    no_sync: bool,
) -> Result<()> {
    if !no_sync {
        buckets::sync_all(&app.root, &app.db, &app.out)?;
    }
    buckets::ensure_fresh_index(&app.root, &app.db, &app.out)?;

    let ctx = app.ctx(opts);
    if apps.is_empty() && !all {
        // status-only update listing
        let rows = crate::install::compute_status(&ctx)?;
        let updatable: Vec<_> = rows.iter().filter(|r| r.4).collect();
        if updatable.is_empty() {
            app.out.info("Everything is up to date.");
        } else {
            app.out.info("Updates available:");
            let mapped: Vec<(String, String, String, String, bool)> = updatable
                .iter()
                .map(|r| (r.0.clone(), r.1.clone(), r.2.clone(), r.3.clone(), r.4))
                .collect();
            print_status_table(&mapped);
            app.out
                .info("Run `scoop-rs update --all` (or `scoop-rs update <app>`) to apply.");
        }
        return Ok(());
    }

    let targets: Vec<String> = if all {
        app.db
            .list_packages()?
            .into_iter()
            .map(|p| p.name)
            .collect()
    } else {
        apps
    };
    let mut updated = 0;
    for name in &targets {
        if let Some(_v) = crate::install::update_one(&ctx, name)? {
            updated += 1;
        }
    }
    app.out.info(&format!("{updated} app(s) updated."));
    let orphans = app.db.orphan_deps()?;
    if !orphans.is_empty() {
        app.out.info(&format!(
            "Note: dependencies no longer required: {} (remove with `scoop-rs uninstall ...`)",
            orphans.join(", ")
        ));
    }
    Ok(())
}

fn list_command(app: &App, json: bool, files: bool) -> Result<()> {
    let pkgs = app.db.list_packages()?;
    if json {
        let mut out = Vec::new();
        for p in &pkgs {
            let mut o = json!({
                "name": p.name,
                "version": p.version,
                "bucket": p.bucket,
                "installed_at_ms": p.installed_at_ms,
                "size_bytes": p.size_bytes,
                "held": p.held,
            });
            if files {
                o["files"] = json!(app.db.files_of(&p.name)?);
            }
            out.push(o);
        }
        println!("{}", serde_json::to_string_pretty(&json!(out))?);
        return Ok(());
    }
    if pkgs.is_empty() {
        app.out
            .info("No packages installed. Try `scoop-rs install <app>`.");
        return Ok(());
    }
    println!(
        "{:<28} {:<16} {:<12} Installed",
        "Name", "Version", "Bucket"
    );
    for p in &pkgs {
        println!(
            "{:<28} {:<16} {:<12} {}",
            p.name,
            p.version,
            p.bucket.clone().unwrap_or_else(|| "-".into()),
            crate::util::format_time(p.installed_at_ms as u64)
        );
    }
    if files {
        for p in &pkgs {
            println!("\n{} {}:", console::style("files of").bold(), p.name);
            for f in app.db.files_of(&p.name)? {
                println!("  {f}");
            }
        }
    }
    Ok(())
}

fn search_command(app: &App, query: &str, json: bool) -> Result<()> {
    buckets::ensure_fresh_index(&app.root, &app.db, &app.out)?;
    let entries = if query.trim().is_empty() {
        app.db.all_index_entries(200)?
    } else {
        app.db.search_index_entries(query, 100)?
    };
    if json {
        let out: Vec<_> = entries
            .iter()
            .map(|e| json!({"name": e.name, "bucket": e.bucket, "version": e.version, "description": e.description}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!(out))?);
        return Ok(());
    }
    if entries.is_empty() {
        app.out.info(&format!(
            "No results for '{query}'. Add more buckets: `scoop-rs bucket add extras`"
        ));
        return Ok(());
    }
    println!(
        "{:<30} {:<12} {:<10} Description",
        "Name", "Version", "Bucket"
    );
    for e in &entries {
        println!(
            "{:<30} {:<12} {:<10} {}",
            e.name,
            e.version,
            e.bucket,
            e.description.clone().unwrap_or_default()
        );
    }
    Ok(())
}

fn info_command(app: &App, name: &str, validate: bool, json: bool) -> Result<i32> {
    buckets::ensure_fresh_index(&app.root, &app.db, &app.out)?;
    let resolved = crate::resolve::resolve(&app.root, &app.db, name)?;
    let raw: serde_json::Value = serde_json::from_str(&resolved.raw_json)?;
    let rview = crate::manifest::Manifest::resolve_arch(
        &resolved.manifest,
        &raw,
        crate::manifest::current_arch(),
    )?;
    if validate {
        let problems = crate::manifest::validate_manifest(&raw);
        if problems.is_empty() {
            app.out
                .success(&format!("manifest '{name}' passes schema validation"));
        } else {
            for p in &problems {
                app.out.warn(&format!("'{name}': {p}"));
            }
            return Ok(1);
        }
    }
    let installed = app.db.get_package(&resolved.name)?;
    if json {
        let o = json!({
            "name": resolved.name,
            "bucket": resolved.bucket,
            "manifest_path": resolved.manifest_path.display().to_string(),
            "manifest": raw,
            "installed": installed.as_ref().map(|p| json!({"version": p.version, "installed_at_ms": p.installed_at_ms})),
        });
        println!("{}", serde_json::to_string_pretty(&o)?);
        return Ok(0);
    }
    println!("Name:        {}", resolved.name);
    println!("Bucket:      {}", resolved.bucket);
    println!("Version:     {}", rview.version);
    if let Some(d) = &rview.description {
        println!("Description: {d}");
    }
    if let Some(h) = &rview.homepage {
        println!("Homepage:    {h}");
    }
    if let Some(l) = &rview.license {
        println!("License:     {l}");
    }
    if !rview.depends.is_empty() {
        println!("Depends:     {}", rview.depends.join(", "));
    }
    println!("Manifest:    {}", resolved.manifest_path.display());
    if let Some(p) = &installed {
        println!(
            "Installed:   {} (at {})",
            p.version,
            crate::util::format_time(p.installed_at_ms as u64)
        );
    }
    Ok(0)
}

fn status_command(app: &App, json: bool) -> Result<()> {
    buckets::ensure_fresh_index(&app.root, &app.db, &app.out)?;
    let opts = InstallOptions::default();
    let ctx = app.ctx(&opts);
    let rows = crate::install::compute_status(&ctx)?;
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|r| json!({"name": r.0, "installed": r.1, "available": r.2, "bucket": r.3, "updatable": r.4}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!(out))?);
        return Ok(());
    }
    if rows.is_empty() {
        app.out.info("No packages installed.");
        return Ok(());
    }
    print_status_table(
        &rows
            .iter()
            .map(|r| (r.0.clone(), r.1.clone(), r.2.clone(), r.3.clone(), r.4))
            .collect::<Vec<_>>(),
    );
    Ok(())
}

fn print_status_table(rows: &[(String, String, String, String, bool)]) {
    println!(
        "{:<28} {:<16} {:<16} {:<10} Update",
        "Name", "Installed", "Available", "Bucket"
    );
    for (name, inst, avail, bucket, up) in rows {
        let mark = if *up {
            console::style("yes").green().bold().to_string()
        } else {
            console::style("no").dim().to_string()
        };
        println!(
            "{:<28} {:<16} {:<16} {:<10} {}",
            name, inst, avail, bucket, mark
        );
    }
}

fn bucket_command(app: &App, bucket: &BucketCmd) -> Result<()> {
    match bucket {
        BucketCmd::Add { name, url } => {
            buckets::add_bucket(&app.root, &app.db, name, url.as_deref(), &app.out)?;
        }
        BucketCmd::List => {
            let rows = app.db.list_buckets()?;
            if rows.is_empty() {
                app.out
                    .info("No buckets added yet. Try `scoop-rs bucket add main`.");
                return Ok(());
            }
            println!("{:<14} {:<10} {:<12} Url", "Name", "Manifests", "Last sync");
            for b in &rows {
                println!(
                    "{:<14} {:<10} {:<12} {}",
                    b.name,
                    b.manifest_count,
                    b.last_sync_ms
                        .map(|m| crate::util::format_time(m as u64))
                        .unwrap_or_else(|| "never".into()),
                    b.url
                );
            }
        }
        BucketCmd::Known => {
            for (n, u) in buckets::fetch_known_buckets(&app.db) {
                println!("{:<14} {}", n, u);
            }
        }
        BucketCmd::Remove {
            name,
            force: _force,
        } => {
            buckets::remove_bucket(&app.root, &app.db, name)?;
            app.out.success(&format!("bucket '{name}' removed"));
        }
    }
    Ok(())
}

fn validate_command(app: &App, file: &str) -> Result<i32> {
    let path = std::path::PathBuf::from(file);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| crate::error::ScoopError::other(format!("cannot read {file}: {e}")))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| crate::error::ScoopError::ManifestInvalid {
            path: file.into(),
            problems: e.to_string(),
        })?;
    let problems = crate::manifest::validate_manifest(&v);
    if problems.is_empty() {
        app.out.success(&format!("{file} passes schema validation"));
        Ok(0)
    } else {
        for p in &problems {
            app.out.warn(&format!("{file}: {p}"));
        }
        Ok(1)
    }
}
