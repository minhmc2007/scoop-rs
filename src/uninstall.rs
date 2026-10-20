//! Uninstall pipeline (scoop-compatible, DB-driven).
//!
//! Order: shims -> uninstall script/installer -> shortcuts -> env -> `current`
//! junction -> version dir -> DB rows (files/deps/shims cascade) -> persist
//! retained unless `--purge` -> repoint `current` if other versions remain ->
//! pacman-style orphan hint.

use crate::config::Root;
use crate::db::Db;
use crate::error::{Result, ScoopError};
use crate::install::Ctx;
use crate::ps::{remove_shortcuts, run_snippets, PsContext};
use crate::util::{is_link, remove_dir_all_robust, remove_link, Output};
use std::path::Path;

pub struct UninstallOptions {
    pub purge: bool,
}

pub fn uninstall_one(ctx: &Ctx, name: &str, opts: &UninstallOptions) -> Result<()> {
    let pkg = ctx
        .db
        .get_package(name)?
        .ok_or_else(|| ScoopError::NotInstalled(name.to_string()))?;

    let version_dir = Path::new(&pkg.install_dir).to_path_buf();
    let current = ctx.root.current_dir(name);
    ctx.out
        .info(&format!("Uninstalling '{name}' ({})...", pkg.version));

    // parse the stored manifest for cleanup details
    let rview = pkg.manifest_json.as_deref().and_then(|raw| {
        let v: serde_json::Value = serde_json::from_str(raw).ok()?;
        crate::manifest::Manifest::parse(raw).ok().and_then(|m| {
            crate::manifest::Manifest::resolve_arch(&m, &v, crate::manifest::current_arch()).ok()
        })
    });

    // 1) shims
    let mut shim_names = ctx.db.shims_of(name)?;
    if shim_names.is_empty() {
        if let Some(rv) = &rview {
            shim_names = rv
                .bins
                .iter()
                .map(|b| {
                    b.alias.clone().unwrap_or_else(|| {
                        Path::new(&b.path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| b.path.clone())
                    })
                })
                .collect();
        }
    }
    crate::shims::remove_shims(&ctx.root.path, &shim_names)?;

    // 2) uninstall scripts / installers
    let uninstall_result = run_uninstaller(ctx, name, &pkg.install_dir, &rview);
    if let Err(e) = uninstall_result {
        ctx.out.warn(&format!(
            "uninstall script failed: {e} (continuing cleanup)"
        ));
    }

    // 3) shortcuts
    if let Some(rv) = &rview {
        remove_shortcuts(&rv.shortcuts)?;
    }

    // 4) env entries
    if let Some(rv) = &rview {
        for p in &rv.env_add_path {
            let full = version_dir.join(p);
            #[cfg(windows)]
            {
                crate::envwin::remove_from_user_path(&full)?;
            }
            #[cfg(not(windows))]
            let _ = full;
        }
        for (k, _) in &rv.env_set {
            #[cfg(windows)]
            crate::envwin::remove_user_env(k)?;
            #[cfg(not(windows))]
            let _ = k;
        }
    }

    // 5) remove current junction + version dir
    if is_link(&current) {
        remove_link(&current)?;
    }
    if version_dir.exists() {
        remove_dir_all_robust(&version_dir)?;
    }

    // 6) persist
    if opts.purge {
        let persist = ctx.root.persist_dir(name);
        if persist.exists() {
            remove_dir_all_robust(&persist)?;
            ctx.out.info("Persist directory removed (--purge)");
        }
    } else if ctx.root.persist_dir(name).exists() {
        ctx.out
            .info("Persist directory kept (use --purge to remove)");
    }

    // 7) DB rows (files/deps/shims cascade with the package row)
    ctx.db.delete_package(name)?;

    // 8) repoint current if other versions remain
    let app_dir = ctx.root.app_dir(name);
    if app_dir.is_dir() {
        let mut remaining: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(&app_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                !crate::util::is_link(&e.path())
                    && e.path().is_dir()
                    && !e.file_name().to_string_lossy().starts_with('.')
            })
            .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
            .collect();
        if !remaining.is_empty() {
            remaining.sort_by(|a, b| {
                crate::version::Version::new(&a.0).cmp(&crate::version::Version::new(&b.0))
            });
            let best = remaining.pop().unwrap();
            crate::util::make_dir_link(&current, &best.1)?;
            ctx.out
                .info(&format!("'current' now points to version {}", best.0));
        } else {
            // nothing left at all - drop the empty app directory
            let empty = std::fs::read_dir(&app_dir)?.filter_map(|e| e.ok()).count() == 0;
            if empty {
                std::fs::remove_dir(&app_dir)?;
            }
        }
    }

    // 9) pacman-style orphan hint
    let orphans = ctx.db.orphan_deps()?;
    if !orphans.is_empty() {
        ctx.out.info(&format!(
            "Dependencies no longer required by any package: {}",
            orphans.join(", ")
        ));
    }

    ctx.out
        .success(&format!("'{name}' was uninstalled successfully"));
    Ok(())
}

fn run_uninstaller(
    ctx: &Ctx,
    name: &str,
    install_dir: &str,
    rview: &Option<crate::manifest::ResolvedManifest>,
) -> Result<()> {
    let dir = Path::new(install_dir);
    if let Some(rv) = rview {
        if let Some(un) = &rv.uninstall {
            if !un.script.is_empty() {
                let manifest_json = {
                    ctx.db
                        .get_package(name)?
                        .and_then(|p| p.manifest_json)
                        .unwrap_or_else(|| "{}".into())
                };
                let pctx = PsContext {
                    app: name,
                    version: "0",
                    dir,
                    persist_dir: &ctx.root.persist_dir(name),
                    fname: "",
                    manifest_json: &manifest_json,
                    arch: crate::manifest::current_arch(),
                };
                return run_snippets(&pctx, &un.script, "uninstall");
            }
            if !un.file.is_empty() {
                let file = dir.join(&un.file[0]);
                if file.is_file() {
                    ctx.out
                        .info(&format!("Running uninstaller {}...", file.display()));
                    return run_process(&file, &un.args);
                }
                return Ok(());
            }
        }
        // implicit uninstallers per installer type
        if let Some(inst) = &rv.installer {
            let kind = inst.kind.clone().unwrap_or_default().to_ascii_lowercase();
            if kind.contains("inno") {
                for cand in ["unins000.exe", "unins001.exe"] {
                    let p = dir.join(cand);
                    if p.is_file() {
                        return run_process(
                            &p,
                            &["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTORE", "/SP-"]
                                .iter()
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>(),
                        );
                    }
                }
            } else if kind.contains("nullsoft") || kind.contains("nsis") {
                let p = dir.join("uninstall.exe");
                if p.is_file() {
                    return run_process(&p, &["/S".to_string()]);
                }
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn run_process(exe: &Path, args: &[String]) -> Result<()> {
    let status = std::process::Command::new(exe).args(args).status()?;
    if !status.success() {
        return Err(ScoopError::other(format!(
            "uninstaller exited with {status}"
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_process(exe: &Path, _args: &[String]) -> Result<()> {
    Err(ScoopError::UnsupportedPayload(format!(
        "running uninstaller {} requires Windows",
        exe.display()
    )))
}

/// Convenience wrapper used by the CLI (builds a Ctx from parts).
pub fn uninstall_apps(
    root: &Root,
    db: &Db,
    out: &Output,
    opts: &crate::install::InstallOptions,
    names: &[String],
    purge: bool,
) -> Result<()> {
    let ctx = Ctx {
        root,
        db,
        out,
        opts,
    };
    let uopts = UninstallOptions { purge };
    for name in names {
        uninstall_one(&ctx, name, &uopts)?;
    }
    Ok(())
}
