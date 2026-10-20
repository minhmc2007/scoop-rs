//! The install / update pipeline (scoop-compatible semantics).
//!
//! Flow per app: resolve -> plan deps -> download (parallel, resumable,
//! hash-verified) -> pre_install (PS only when present) -> extract or run
//! installer -> persist -> shims -> shortcuts -> env -> `current` junction ->
//! DB record -> post_install -> notes.
//!
//! The DB row is the pacman-style source of truth for later list/uninstall.

use crate::config::Root;
use crate::db::{Db, InstalledPackage};
use crate::download::{download_all, DownloadItem, DownloadOpts};
use crate::error::{Result, ScoopError};
use crate::extract::PayloadKind;
use crate::manifest::{current_arch, ResolvedManifest};
use crate::ps::{run_snippets, PsContext};
use crate::resolve::{self, Resolved};
use crate::util::{self, is_link, make_dir_link, remove_dir_all_robust, remove_link, Output};
use std::path::Path;

pub struct InstallOptions {
    pub force: bool,
    pub skip_deps: bool,
    pub no_progress: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        InstallOptions {
            force: false,
            skip_deps: false,
            no_progress: !util::is_tty(),
        }
    }
}

pub struct Ctx<'a> {
    pub root: &'a Root,
    pub db: &'a Db,
    pub out: &'a Output,
    pub opts: &'a InstallOptions,
}

/// Install the named apps (plus dependencies). Returns names actually installed.
pub fn install_specs(ctx: &Ctx, specs: &[String]) -> Result<Vec<String>> {
    if specs.is_empty() {
        return Ok(vec![]);
    }
    crate::buckets::ensure_main_bucket(ctx.root, ctx.db, ctx.out)?;
    crate::buckets::ensure_fresh_index(ctx.root, ctx.db, ctx.out)?;

    let plan = if ctx.opts.skip_deps {
        specs
            .iter()
            .map(|s| {
                let (_b, n) = resolve::split_spec(s);
                resolve::DepNode {
                    spec: s.clone(),
                    name: n.to_string(),
                    bucket: None,
                }
            })
            .collect::<Vec<_>>()
    } else {
        resolve::plan(ctx.root, ctx.db, specs, ctx.opts.force)?
    };

    let mut installed = Vec::new();
    for node in &plan {
        if ctx.db.is_installed(&node.name).unwrap_or(false) && !ctx.opts.force {
            ctx.out.info(&format!(
                "'{}' is already installed. Use --force to reinstall.",
                node.name
            ));
            continue;
        }
        install_resolved(ctx, &node.spec)?;
        installed.push(node.name.clone());
    }
    Ok(installed)
}

/// Resolve `spec` fresh and install it end-to-end. Returns installed version.
pub fn install_resolved(ctx: &Ctx, spec: &str) -> Result<String> {
    let resolved = resolve::resolve(ctx.root, ctx.db, spec)?;
    let name = resolved.name.clone();
    let rview = crate::manifest::Manifest::resolve_arch(
        &resolved.manifest,
        &serde_json::from_str(&resolved.raw_json)?,
        current_arch(),
    )?;
    for w in &rview.warnings {
        ctx.out.warn(&format!("manifest '{}': {w}", name));
    }
    if rview.urls.is_empty() {
        return Err(ScoopError::ManifestInvalid {
            path: resolved.manifest_path.display().to_string(),
            problems: "no download url for this architecture".into(),
        });
    }
    let version = rview.version.clone();
    let version_dir = ctx.root.version_dir(&name, &version);

    ctx.out.info(&format!(
        "Installing '{name}' ({version}) [{}]",
        resolved.bucket
    ));

    if !ctx.opts.force && ctx.db.is_installed(&name).unwrap_or(false) {
        ctx.out.info(&format!(
            "'{name}' is already installed. Use --force to reinstall."
        ));
        return Ok(ctx
            .db
            .get_package(&name)?
            .map(|p| p.version)
            .unwrap_or(version));
    }
    if version_dir.exists() {
        if ctx.opts.force {
            let current = ctx.root.current_dir(&name);
            if is_link(&current) {
                remove_link(&current)?;
            }
            remove_dir_all_robust(&version_dir)?;
        } else {
            return Err(ScoopError::other(format!(
                "directory {} already exists - run with --force to overwrite, or `scoop-rs uninstall {name}` first",
                version_dir.display()
            )));
        }
    }
    std::fs::create_dir_all(&version_dir)?;

    // ---- downloads ----
    let mut items = Vec::new();
    for (i, url) in rview.urls.iter().enumerate() {
        let hash = pick_hash(&rview.hashes, i);
        items.push(DownloadItem {
            url: url.clone(),
            dest: ctx.root.cache_file(&name, &version, url),
            expected_hashes: hash.into_iter().collect(),
        });
    }
    download_all(
        &items,
        &DownloadOpts {
            no_progress: ctx.opts.no_progress,
            jobs: 4,
        },
    )?;

    // ---- pre_install (PowerShell only when the manifest has snippets) ----
    ps_step(
        ctx,
        &resolved,
        &rview,
        &version_dir,
        &version,
        &rview.pre_install,
        "pre_install",
    )?;

    // ---- payload: extract / run installer ----
    install_payload(ctx, &resolved, &rview, &version_dir, &items, &version)?;

    // ---- persist ----
    apply_persist(ctx.root, &name, &version_dir, &rview.persist, ctx.out)?;

    // ---- shims ----
    let shims = crate::shims::create_shims(&ctx.root.path, &version_dir, &rview.bins)?;
    for w in &shims.warnings {
        ctx.out.warn(w);
    }

    // ---- shortcuts (Start Menu, Windows only) ----
    crate::ps::create_shortcuts(&version_dir, &rview.shortcuts, &name)?;

    // ---- env ----
    apply_env(ctx, &rview, &version_dir)?;

    // ---- current junction ----
    make_dir_link(&ctx.root.current_dir(&name), &version_dir)?;

    // ---- record in the local database ----
    let files = util::walk_files(&version_dir)?;
    let size: u64 = files.iter().map(|(_, s)| s).sum();
    let pkg = InstalledPackage {
        name: name.clone(),
        version: version.clone(),
        bucket: Some(resolved.bucket.clone()),
        install_dir: version_dir.display().to_string(),
        installed_at_ms: util::now_ms() as i64,
        size_bytes: size as i64,
        held: false,
        manifest_json: Some(resolved.raw_json.clone()),
    };
    ctx.db.upsert_package(&pkg)?;
    ctx.db.replace_files(&name, &files)?;
    ctx.db.replace_deps(&name, &rview.depends)?;
    ctx.db.replace_shims(&name, &shims.names)?;

    // install.json mirrors what upstream Scoop writes (plain manifest) so the
    // two clients (and tools reading install.json) stay interoperable
    std::fs::write(version_dir.join("install.json"), &resolved.raw_json)?;

    // ---- post_install ----
    ps_step(
        ctx,
        &resolved,
        &rview,
        &version_dir,
        &version,
        &rview.post_install,
        "post_install",
    )?;

    // ---- notes ----
    for note in &rview.notes {
        ctx.out.info(&format!("Notes: {note}"));
    }
    ctx.out
        .success(&format!("'{name}' ({version}) was installed successfully"));
    Ok(version)
}

fn pick_hash(hashes: &[String], idx: usize) -> Vec<String> {
    if hashes.is_empty() {
        vec![]
    } else if hashes.len() == 1 {
        vec![hashes[0].clone()]
    } else {
        hashes.get(idx).cloned().into_iter().collect()
    }
}

fn pick_indexed(specs: &[String], idx: usize) -> Option<String> {
    if specs.is_empty() {
        None
    } else if specs.len() == 1 {
        Some(specs[0].clone())
    } else {
        specs.get(idx).cloned()
    }
}

fn ps_step(
    ctx: &Ctx,
    resolved: &Resolved,
    rview: &ResolvedManifest,
    dir: &Path,
    version: &str,
    snippets: &[String],
    what: &str,
) -> Result<()> {
    if snippets.is_empty() {
        return Ok(());
    }
    let fname = rview
        .urls
        .first()
        .map(|u| util::url_filename(u))
        .unwrap_or_default();
    let manifest_json = resolved.raw_json.clone();
    let pctx = PsContext {
        app: &resolved.name,
        version,
        dir,
        persist_dir: &ctx.root.persist_dir(&resolved.name),
        fname: &fname,
        manifest_json: &manifest_json,
        arch: current_arch(),
    };
    ctx.out
        .info(&format!("Running {what} script (PowerShell)..."));
    run_snippets(&pctx, snippets, what)
}

fn install_payload(
    ctx: &Ctx,
    resolved: &Resolved,
    rview: &ResolvedManifest,
    version_dir: &Path,
    items: &[DownloadItem],
    version: &str,
) -> Result<()> {
    let installer = rview.installer.clone().unwrap_or_default();
    let explicit_type = installer
        .kind
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let type_is = |k: &str| explicit_type.contains(k);

    for (i, item) in items.iter().enumerate() {
        let fname = item
            .dest
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "payload".into());
        let kind = crate::extract::detect(&fname);
        let has_script = !installer.script.is_empty();

        // per-url extraction targets
        let extract_to = pick_indexed(&rview.extract_to, i);
        let extract_dir = pick_indexed(&rview.extract_dir, i);

        let target_dir = match &extract_to {
            Some(sub) => {
                let d = version_dir.join(sub);
                std::fs::create_dir_all(&d)?;
                d
            }
            None => version_dir.to_path_buf(),
        };

        match kind {
            PayloadKind::Msi => {
                run_msi(ctx, &item.dest, &installer, &target_dir)?;
            }
            k if crate::extract::is_archive(k) => {
                let tmp = version_dir
                    .parent()
                    .unwrap_or(version_dir)
                    .join(format!(".scoop-rs-tmp-{}", std::process::id()));
                remove_dir_all_robust(&tmp)?;
                crate::extract::extract(&item.dest, &tmp, k)?;
                move_extracted(&tmp, extract_dir.as_deref(), &target_dir)?;
                remove_dir_all_robust(&tmp)?;
            }
            PayloadKind::Exe => {
                let is_named_installer = installer
                    .file
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(&fname));
                if is_named_installer
                    || type_is("inno")
                    || type_is("nullsoft")
                    || type_is("nsis")
                    || type_is("burn")
                    || has_script
                {
                    // real installer payload
                    if is_named_installer || !has_script || type_is("inno") || type_is("nullsoft") {
                        run_exe_installer(ctx, &item.dest, &installer, &target_dir)?;
                    }
                } else {
                    // portable single-file executable
                    let dest_file = target_dir.join(&fname);
                    std::fs::copy(&item.dest, &dest_file)?;
                }
            }
            _ => {
                // unknown payload: place it in the install dir (scoop-like)
                let dest_file = target_dir.join(&fname);
                std::fs::copy(&item.dest, &dest_file)?;
            }
        }
    }

    // installer.script runs after the payload placement (scoop semantics)
    if !installer.script.is_empty() {
        ps_step(
            ctx,
            resolved,
            rview,
            version_dir,
            version,
            &installer.script,
            "installer",
        )?;
    }
    Ok(())
}

/// Move extracted content into place, honoring `extract_dir` (inner subdir).
fn move_extracted(tmp: &Path, extract_dir: Option<&str>, target: &Path) -> Result<()> {
    let src = match extract_dir {
        Some(sub) => {
            let p = tmp.join(sub);
            if !p.is_dir() {
                return Err(ScoopError::other(format!(
                    "extract_dir '{sub}' not found in archive"
                )));
            }
            p
        }
        None => tmp.to_path_buf(),
    };
    for entry in std::fs::read_dir(&src)? {
        let entry = entry?;
        let to = target.join(entry.file_name());
        if entry.path().is_dir() {
            util::copy_dir_all(&entry.path(), &to)?;
        } else {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn run_msi(
    ctx: &Ctx,
    msi: &Path,
    installer: &crate::manifest::Installer,
    target_dir: &Path,
) -> Result<()> {
    ctx.out
        .info("Installing MSI payload (msiexec administrative extract)...");
    let mut args = vec![
        "/a".to_string(),
        msi.display().to_string(),
        format!("TARGETDIR={}", target_dir.display()),
        "/qn".to_string(),
    ];
    if !installer.args.is_empty() {
        args = installer.args.clone();
        args.insert(0, "/i".into());
        args.insert(1, msi.display().to_string());
    }
    let status = std::process::Command::new("msiexec").args(&args).status()?;
    if !status.success() {
        return Err(ScoopError::other(format!("msiexec failed with {status}")));
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_msi(_ctx: &Ctx, msi: &Path, _i: &crate::manifest::Installer, _t: &Path) -> Result<()> {
    Err(ScoopError::UnsupportedPayload(format!(
        "MSI payload {} requires Windows (msiexec)",
        msi.display()
    )))
}

#[cfg(windows)]
fn run_exe_installer(
    ctx: &Ctx,
    exe: &Path,
    installer: &crate::manifest::Installer,
    dir: &Path,
) -> Result<()> {
    let kind = installer
        .kind
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let args: Vec<String> = if !installer.args.is_empty() {
        installer.args.clone()
    } else if kind.contains("inno") {
        vec![
            "/VERYSILENT".into(),
            "/SUPPRESSMSGBOXES".into(),
            "/NORESTART".into(),
            "/SP-".into(),
            format!("/DIR={}", dir.display()),
        ]
    } else if kind.contains("nullsoft") || kind.contains("nsis") {
        vec!["/S".into(), format!("/D={}", dir.display())]
    } else {
        vec![]
    };
    ctx.out
        .info(&format!("Running installer {} ...", exe.display()));
    let status = std::process::Command::new(exe).args(&args).status()?;
    if !status.success() {
        return Err(ScoopError::other(format!("installer exited with {status}")));
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_exe_installer(
    _ctx: &Ctx,
    exe: &Path,
    _i: &crate::manifest::Installer,
    _d: &Path,
) -> Result<()> {
    Err(ScoopError::UnsupportedPayload(format!(
        "installer {} requires Windows",
        exe.display()
    )))
}

fn apply_persist(
    root: &Root,
    name: &str,
    version_dir: &Path,
    entries: &[crate::manifest::PersistEntry],
    _out: &Output,
) -> Result<()> {
    for e in entries {
        let rel = &e.source;
        let persist_rel = e.target.clone().unwrap_or_else(|| e.source.clone());
        let in_app = version_dir.join(rel);
        let in_persist = root.persist_dir(name).join(&persist_rel);

        if let Some(parent) = in_persist.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut was_dir = false;
        if is_link(&in_app) {
            // stale link from a previous install - refresh below
            remove_link(&in_app)?;
        } else if in_app.exists() {
            was_dir = in_app.is_dir();
            // first install: move real content into persist (existing persist
            // content wins - that is what the user kept across updates)
            if !in_persist.exists() {
                if was_dir {
                    util::copy_dir_all(&in_app, &in_persist)?;
                } else {
                    std::fs::copy(&in_app, &in_persist)?;
                }
            }
            if was_dir {
                remove_dir_all_robust(&in_app)?;
            } else {
                std::fs::remove_file(&in_app)?;
            }
        }

        // ensure something exists to link to
        if !in_persist.exists() {
            if was_dir
                || rel.ends_with('/')
                || rel.ends_with('\\')
                || persist_rel.ends_with('/')
                || persist_rel.ends_with('\\')
            {
                std::fs::create_dir_all(&in_persist)?;
            } else {
                std::fs::write(&in_persist, b"")?;
            }
        }

        if in_persist.is_dir() {
            make_dir_link(&in_app, &in_persist)?;
        } else {
            util::hard_link_or_copy(&in_persist, &in_app)?;
        }
    }
    Ok(())
}

#[allow(unused_variables)]
fn apply_env(ctx: &Ctx, rview: &ResolvedManifest, version_dir: &Path) -> Result<()> {
    for p in &rview.env_add_path {
        let full = version_dir.join(p.replace("$dir", &version_dir.display().to_string()));
        #[cfg(windows)]
        {
            if crate::envwin::add_to_user_path(&full)? {
                ctx.out
                    .info(&format!("Added to user PATH: {}", full.display()));
            }
        }
        #[cfg(not(windows))]
        {
            let _ = full;
        }
    }
    for (k, v) in &rview.env_set {
        let value = v.replace("$dir", &version_dir.display().to_string());
        #[cfg(windows)]
        crate::envwin::set_user_env(k, &value)?;
        #[cfg(not(windows))]
        let _ = (k, value);
    }
    Ok(())
}

/// Remove every version directory of `name` except `keep_version`.
pub fn cleanup_old_versions(root: &Root, name: &str, keep_version: &str) -> Result<usize> {
    let app_dir = root.app_dir(name);
    let mut removed = 0;
    if !app_dir.is_dir() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(&app_dir)? {
        let entry = entry?;
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname == "current" || fname.starts_with(".scoop-rs-tmp") {
            continue;
        }
        let is_link = is_link(&entry.path());
        if !is_link && entry.path().is_dir() && fname != util::sanitize_component(keep_version) {
            remove_dir_all_robust(&entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Update one app to the newest manifest version. Returns Some(new_version)
/// when an update happened.
pub fn update_one(ctx: &Ctx, name: &str) -> Result<Option<String>> {
    let pkg = match ctx.db.get_package(name)? {
        Some(p) => p,
        None => return Err(ScoopError::NotInstalled(name.into())),
    };
    if pkg.held {
        ctx.out.info(&format!(
            "'{name}' is held; skipping (hold is stored in the local DB)"
        ));
        return Ok(None);
    }
    // resolve the bucket manifest (bucket-qualified first)
    let spec = match pkg.bucket.as_deref() {
        Some(b) if b != "local" && b != "(scoop)" => format!("{b}/{name}"),
        _ => name.to_string(),
    };
    let resolved = match resolve::resolve(ctx.root, ctx.db, &spec) {
        Ok(r) => r,
        Err(e) => {
            ctx.out.warn(&format!("cannot resolve '{name}': {e}"));
            return Ok(None);
        }
    };
    let available = resolved.manifest.version_str();
    if !crate::db::is_upgradable(&pkg.version, &available) && !ctx.opts.force {
        ctx.out
            .info(&format!("'{name}' ({}) is up to date", pkg.version));
        return Ok(None);
    }
    ctx.out.info(&format!(
        "Updating '{name}': {} -> {available}",
        pkg.version
    ));
    // an update is a forced reinstall of the newest manifest; deps are
    // already satisfied (they were checked at install time)
    let force_opts = InstallOptions {
        force: true,
        skip_deps: true,
        no_progress: ctx.opts.no_progress,
    };
    let fctx = Ctx {
        root: ctx.root,
        db: ctx.db,
        out: ctx.out,
        opts: &force_opts,
    };
    let new_version = install_resolved(&fctx, &spec)?;
    // remove old version dirs once the new one is in place
    cleanup_old_versions(ctx.root, name, &new_version)?;
    Ok(Some(new_version))
}

/// One row of `scoop-rs status`: (name, installed, available, bucket, updatable).
pub type StatusRow = (String, String, String, String, bool);

/// Compare every installed package against the indexed manifests.
/// Returns rows for `status`: (name, installed, available, bucket, upgradable).
pub fn compute_status(ctx: &Ctx) -> Result<Vec<StatusRow>> {
    let mut rows = Vec::new();
    for pkg in ctx.db.list_packages()? {
        let spec = match pkg.bucket.as_deref() {
            Some(b) if b != "local" && b != "(scoop)" => format!("{}/{}", b, pkg.name),
            _ => pkg.name.clone(),
        };
        let available = match resolve::resolve(ctx.root, ctx.db, &spec) {
            Ok(r) => r.manifest.version_str(),
            Err(_) => "-".to_string(),
        };
        let up = available != "-" && crate::db::is_upgradable(&pkg.version, &available);
        rows.push((
            pkg.name.clone(),
            pkg.version.clone(),
            available,
            pkg.bucket.clone().unwrap_or_else(|| "-".into()),
            up,
        ));
    }
    Ok(rows)
}

/// Import existing PowerShell-Scoop installs (apps/*/current/install.json)
/// into our database so `list`/`uninstall`/`status` work immediately.
pub fn import_existing(root: &Root, db: &Db, out: &Output) -> Result<usize> {
    let apps = root.apps();
    if !apps.is_dir() {
        return Ok(0);
    }
    let mut imported = 0;
    for app in std::fs::read_dir(&apps)? {
        let app = app?;
        if !app.file_type()?.is_dir() {
            continue;
        }
        let name = app.file_name().to_string_lossy().to_string();
        if name == "scoop" {
            continue; // scoop itself is not a package
        }
        let install_json = root.current_dir(&name).join("install.json");
        if !install_json.is_file() {
            continue;
        }
        if db.get_package(&name)?.is_some() {
            continue;
        }
        let raw = std::fs::read_to_string(&install_json)?;
        let manifest = match crate::manifest::Manifest::parse(&raw) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let version = manifest.version_str();
        let files = util::walk_files(&root.current_dir(&name))?;
        let size: u64 = files.iter().map(|(_, s)| s).sum();
        let shims: Vec<String> = manifest
            .bin
            .as_ref()
            .map(crate::manifest::parse_bins_public)
            .unwrap_or_default();
        let pkg = InstalledPackage {
            name: name.clone(),
            version: version.clone(),
            bucket: Some("(scoop)".into()),
            install_dir: root
                .current_dir(&name)
                .read_link()
                .map(|p| {
                    if p.is_absolute() {
                        p.display().to_string()
                    } else {
                        app.path().join(p).display().to_string()
                    }
                })
                .unwrap_or_else(|_| root.current_dir(&name).display().to_string()),
            installed_at_ms: util::now_ms() as i64,
            size_bytes: size as i64,
            held: false,
            manifest_json: Some(raw),
        };
        db.upsert_package(&pkg)?;
        db.replace_files(&name, &files)?;
        db.replace_shims(&name, &shims)?;
        imported += 1;
    }
    if imported > 0 {
        out.info(&format!(
            "Imported {imported} existing package(s) from the Scoop layout into the local database"
        ));
    }
    Ok(imported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_selection_pairwise_and_single() {
        let h = pick_hash(&["aaa".into(), "bbb".into()], 1);
        assert_eq!(h, vec!["bbb"]);
        let h = pick_hash(&["single".into()], 7);
        assert_eq!(h, vec!["single"]);
        let h = pick_hash(&[], 0);
        assert!(h.is_empty());
    }

    #[test]
    fn extract_to_picks_index_or_single() {
        assert_eq!(
            pick_indexed(&["a".into(), "b".into()], 0).as_deref(),
            Some("a")
        );
        assert_eq!(pick_indexed(&["only".into()], 3).as_deref(), Some("only"));
        assert!(pick_indexed(&[], 0).is_none());
    }

    #[test]
    fn version_comparison_for_updates() {
        assert!(crate::db::is_upgradable("1.0.0", "1.0.1"));
        assert!(!crate::db::is_upgradable("2.0", "1.9.9"));
        assert!(!crate::db::is_upgradable("1.0", "1.0"));
    }
}
