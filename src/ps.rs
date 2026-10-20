//! PowerShell interop layer - the "hybrid" part of scoop-rs.
//!
//! Native Rust handles everything hot-path (downloads, extraction, hashing,
//! database, shims). PowerShell is only launched when a manifest genuinely
//! requires it: `installer.script`, `pre_install`, `post_install`,
//! `uninstall.script`, or Start Menu shortcuts.
//!
//! The wrapper script recreates the variable surface Scoop manifests expect
//! ($dir, $manifest, $version, $arch, $persist_dir, ...), so bucket scripts
//! keep working unmodified.

use crate::error::{Result, ScoopError};
use std::path::Path;

pub struct PsContext<'a> {
    pub app: &'a str,
    pub version: &'a str,
    pub dir: &'a Path,
    pub persist_dir: &'a Path,
    pub fname: &'a str,
    pub manifest_json: &'a str,
    pub arch: &'a str,
}

impl<'a> PsContext<'a> {
    #[allow(dead_code)]
    fn wrapper(&self, snippets: &[String]) -> String {
        let esc_dir = ps_quote(&self.dir.display().to_string());
        let esc_persist = ps_quote(&self.persist_dir.display().to_string());
        let esc_fname = ps_quote(self.fname);
        let esc_app = ps_quote(self.app);
        let esc_version = ps_quote(self.version);
        let esc_arch = ps_quote(self.arch);
        let json = ps_quote(self.manifest_json);

        let mut script = format!(
            r#"$ErrorActionPreference = 'Stop'
$dir = {esc_dir}
$persist_dir = {esc_persist}
$fname = {esc_fname}
$app = {esc_app}
$version = {esc_version}
$arch = {esc_arch}
$bits = 64
$global = $false
$manifest = {json} | ConvertFrom-Json
"#
        );
        // $bits per arch
        if self.arch == "32bit" {
            script = script.replace("$bits = 64", "$bits = 32");
        }
        for s in snippets {
            script.push_str(s);
            script.push('\n');
        }
        script
    }
}

#[allow(dead_code)]
fn ps_quote(s: &str) -> String {
    let inner = s.replace('\'', "''");
    format!("'{inner}'")
}

/// Run PowerShell snippets with a scoop-compatible variable surface.
#[cfg(windows)]
pub fn run_snippets(ctx: &PsContext, snippets: &[String], what: &str) -> Result<()> {
    if snippets.is_empty() {
        return Ok(());
    }
    let script = ctx.wrapper(snippets);
    let tmp = std::env::temp_dir().join(format!(
        "scoop-rs-{}-{}-{}.ps1",
        ctx.app,
        std::process::id(),
        crate::util::now_ms()
    ));
    std::fs::write(&tmp, script)?;
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&tmp)
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(ScoopError::other(format!(
                "failed to launch powershell.exe for {what}: {e}"
            )));
        }
    };
    let _ = std::fs::remove_file(&tmp);
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Err(ScoopError::PowerShell {
            code,
            output: String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr),
        });
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn run_snippets(_ctx: &PsContext, snippets: &[String], _what: &str) -> Result<()> {
    if snippets.is_empty() {
        return Ok(());
    }
    Err(ScoopError::other(
        "this manifest requires PowerShell script execution, which is only available on Windows",
    ))
}

/// Create Start Menu shortcuts via WScript.Shell COM (Windows only).
#[cfg(windows)]
pub fn create_shortcuts(version_dir: &Path, shortcuts: &[Vec<String>], app: &str) -> Result<()> {
    if shortcuts.is_empty() {
        return Ok(());
    }
    let dir_q = ps_quote(&version_dir.display().to_string());
    let mut script = String::from(
        "$progs = [Environment]::GetFolderPath('Programs')\n$ws = New-Object -ComObject WScript.Shell\n",
    );
    for sc in shortcuts {
        if sc.len() < 3 {
            continue;
        }
        let exe = ps_quote(&version_dir.join(&sc[0]).display().to_string());
        let subdir = ps_quote(&sc[1]);
        let nm = &sc[2];
        let name = ps_quote(&format!("{nm}.lnk"));
        script.push_str(&format!(
            r#"$lnk = $ws.CreateShortcut((Join-Path (Join-Path $progs {subdir}) {name}))
$lnk.TargetPath = {exe}
$lnk.WorkingDirectory = {dir_q}
$lnk.Save()
"#
        ));
    }
    let _ = app;
    let tmp = std::env::temp_dir().join(format!("scoop-rs-shortcuts-{}.ps1", std::process::id()));
    std::fs::write(&tmp, script)?;
    let res = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&tmp)
        .status();
    let _ = std::fs::remove_file(&tmp);
    match res {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(ScoopError::other(format!("shortcut creation failed ({s})"))),
        Err(e) => Err(ScoopError::other(format!("shortcut creation failed: {e}"))),
    }
}

#[cfg(not(windows))]
pub fn create_shortcuts(_version_dir: &Path, _shortcuts: &[Vec<String>], _app: &str) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn remove_shortcuts(shortcuts: &[Vec<String>]) -> Result<()> {
    if shortcuts.is_empty() {
        return Ok(());
    }
    let mut script = String::from("$progs = [Environment]::GetFolderPath('Programs')\n");
    for sc in shortcuts {
        if sc.len() < 3 {
            continue;
        }
        let path = ps_quote(&format!("{}\\{}\\{}.lnk", "$progs", sc[1], sc[2]));
        script.push_str(&format!(
            "Remove-Item -Force -ErrorAction SilentlyContinue {path}\n"
        ));
    }
    let tmp = std::env::temp_dir().join(format!("scoop-rs-rmshortcuts-{}.ps1", std::process::id()));
    std::fs::write(&tmp, script)?;
    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&tmp)
        .status()
        .ok();
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

#[cfg(not(windows))]
pub fn remove_shortcuts(_shortcuts: &[Vec<String>]) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_sets_scoop_variables() {
        let ctx = PsContext {
            app: "tool",
            version: "1.0",
            dir: Path::new("/x/apps/tool/1.0"),
            persist_dir: Path::new("/x/persist/tool"),
            fname: "tool.zip",
            manifest_json: "{\"version\":\"1.0\"}",
            arch: "64bit",
        };
        let w = ctx.wrapper(&["Write-Host $dir".into()]);
        assert!(w.contains("$dir = '/x/apps/tool/1.0'"));
        assert!(w.contains("$manifest = '{\"version\":\"1.0\"}' | ConvertFrom-Json"));
        assert!(w.contains("Write-Host $dir"));
        assert!(w.contains("$bits = 64"));
    }

    #[test]
    fn quoting_escapes_single_quotes() {
        assert_eq!(ps_quote("it's"), "'it''s'");
    }
}
