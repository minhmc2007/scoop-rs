//! Shim management - the executables scoop drops into `<root>\shims\`.
//!
//! For maximum compatibility with an existing Scoop install:
//!   * if upstream's `shim.exe` is found (installed by PowerShell Scoop), we
//!     copy it and write the same `name.shim` metadata format it reads
//!   * otherwise we generate a zero-dependency `name.cmd` (cmd.exe) plus a
//!     `name.ps1` (PowerShell) launcher.
//!
//! Both live in the same shims directory, on the same PATH entry.

use crate::error::Result;
use crate::manifest::BinEntry;
use std::path::{Path, PathBuf};

/// Locate upstream Scoop's shim.exe, if PowerShell Scoop is installed.
pub fn find_upstream_shim_exe(root: &Path) -> Option<PathBuf> {
    let candidates = [
        root.join("apps")
            .join("scoop")
            .join("current")
            .join("supporting")
            .join("shimexe")
            .join("bin")
            .join("shim.exe"),
        root.join("apps")
            .join("scoop")
            .join("current")
            .join("supporting")
            .join("shims")
            .join("shim.exe"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

fn sanitize_shim_name(bin_path: &str, alias: Option<&str>) -> String {
    let stem = alias.map(|a| a.to_string()).unwrap_or_else(|| {
        Path::new(bin_path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| bin_path.to_string())
    });
    let cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "app".into()
    } else {
        cleaned
    }
}

pub struct CreatedShims {
    pub names: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn create_shims(root: &Path, version_dir: &Path, bins: &[BinEntry]) -> Result<CreatedShims> {
    let shims_dir = root.join("shims");
    std::fs::create_dir_all(&shims_dir)?;
    let upstream = find_upstream_shim_exe(root);
    let mut names = Vec::new();
    let mut warnings = Vec::new();

    for bin in bins {
        let target = version_dir.join(&bin.path);
        if !target.is_file() {
            warnings.push(format!(
                "bin target '{}' not found in {} - shim skipped",
                bin.path,
                version_dir.display()
            ));
            continue;
        }
        let name = sanitize_shim_name(&bin.path, bin.alias.as_deref());
        let target_str = target.display().to_string();

        // upstream-format metadata (read by upstream shim.exe)
        let mut shim_meta = format!("path = {target_str}\n");
        if let Some(args) = &bin.args {
            shim_meta.push_str(&format!("args = {args}\n"));
        }
        std::fs::write(shims_dir.join(format!("{name}.shim")), shim_meta)?;

        if let Some(shim_exe) = &upstream {
            std::fs::copy(shim_exe, shims_dir.join(format!("{name}.exe")))?;
        }

        // cmd launcher (always written: works even without upstream shim.exe)
        let cmd_body = if bin.path.to_ascii_lowercase().ends_with(".ps1") {
            format!(
                "@echo off\npowershell -NoProfile -ExecutionPolicy Bypass -File \"{}\" %*\nexit /b %ERRORLEVEL%\n",
                target_str.replace('"', "")
            )
        } else {
            let mut body = format!("@echo off\n\"{}\" %*\n", target_str.replace('"', ""));
            if let Some(args) = &bin.args {
                body = format!(
                    "@echo off\n\"{}\" {} %*\n",
                    target_str.replace('"', ""),
                    args
                );
            }
            body.push_str("exit /b %ERRORLEVEL%\n");
            body
        };
        std::fs::write(shims_dir.join(format!("{name}.cmd")), cmd_body)?;

        // PowerShell launcher for pwsh users
        let ps_body = match &bin.args {
            Some(args) => format!("& '{target_str}' {args} @args\nexit $LASTEXITCODE\n"),
            None => format!("& '{target_str}' @args\nexit $LASTEXITCODE\n"),
        };
        std::fs::write(shims_dir.join(format!("{name}.ps1")), ps_body)?;

        names.push(name);
    }

    Ok(CreatedShims { names, warnings })
}

pub fn remove_shims(root: &Path, names: &[String]) -> Result<()> {
    let shims_dir = root.join("shims");
    for name in names {
        for ext in [".exe", ".shim", ".cmd", ".ps1"] {
            let p = shims_dir.join(format!("{name}{ext}"));
            if p.is_file() {
                std::fs::remove_file(&p)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_name_sanitization() {
        assert_eq!(sanitize_shim_name("bin/hugo.exe", None), "hugo");
        assert_eq!(sanitize_shim_name("bin/hugo.exe", Some("h")), "h");
        assert_eq!(sanitize_shim_name("weird name.exe", None), "weird-name");
        assert_eq!(sanitize_shim_name("tool.py", None), "tool");
        assert_eq!(sanitize_shim_name("x/y/z.jar", None), "z");
    }

    #[test]
    fn create_and_remove_shims() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let vdir = root.join("apps/fake/1.0");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("fake.exe"), b"MZ...").unwrap();

        let bins = vec![BinEntry {
            path: "fake.exe".into(),
            alias: None,
            args: None,
        }];
        let created = create_shims(root, &vdir, &bins).unwrap();
        assert_eq!(created.names, vec!["fake"]);
        assert!(root.join("shims/fake.cmd").is_file());
        assert!(root.join("shims/fake.shim").is_file());
        assert!(root.join("shims/fake.ps1").is_file());
        let shim = std::fs::read_to_string(root.join("shims/fake.shim")).unwrap();
        assert!(shim.contains(&format!("path = {}", vdir.join("fake.exe").display())));

        remove_shims(root, &created.names).unwrap();
        assert!(!root.join("shims/fake.cmd").exists());
    }

    #[test]
    fn missing_bin_produces_warning() {
        let dir = tempfile::tempdir().unwrap();
        let vdir = dir.path().join("v");
        std::fs::create_dir_all(&vdir).unwrap();
        let bins = vec![BinEntry {
            path: "nope.exe".into(),
            alias: None,
            args: None,
        }];
        let created = create_shims(dir.path(), &vdir, &bins).unwrap();
        assert!(created.names.is_empty());
        assert_eq!(created.warnings.len(), 1);
    }

    #[test]
    fn error_type_available() {
        use crate::error::ScoopError;
        let _ = ScoopError::other("x");
    }
}
