//! Windows user-environment management (HKCU\Environment): PATH updates,
//! env_set/env_add_path support, and Explorer notification. No-ops with a
//! friendly warning on non-Windows targets.

use crate::error::Result;
#[cfg(not(windows))]
use crate::error::ScoopError;
use std::path::Path;

#[cfg(windows)]
use winreg::enums::{RegType, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
#[cfg(windows)]
use winreg::RegKey;

#[cfg(windows)]
fn env_key() -> Result<RegKey> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    Ok(hkcu.open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)?)
}

/// Get the raw (unexpanded) user PATH and its registry type.
#[cfg(windows)]
pub fn get_user_path_raw() -> Result<(String, RegType)> {
    let key = env_key()?;
    match key.get_raw_value("Path") {
        Ok(rv) => {
            let s = regvalue_to_string(&rv);
            Ok((s, rv.vtype))
        }
        Err(_) => Ok((String::new(), RegType::REG_EXPAND_SZ)),
    }
}

#[cfg(windows)]
fn regvalue_to_string(rv: &winreg::RegValue) -> String {
    match rv.vtype {
        RegType::REG_SZ | RegType::REG_EXPAND_SZ => {
            // registry strings are UTF-16LE stored as raw bytes
            let units: Vec<u16> = rv
                .bytes
                .chunks(2)
                .map(|c| {
                    if c.len() == 2 {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        0
                    }
                })
                .collect();
            let s = String::from_utf16_lossy(&units);
            s.trim_end_matches('\0').to_string()
        }
        _ => String::new(),
    }
}

#[cfg(windows)]
fn write_raw(name: &str, value: &str, vtype: RegType) -> Result<()> {
    let key = env_key()?;
    let mut units: Vec<u16> = value.encode_utf16().collect();
    units.push(0);
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    key.set_raw_value(name, &winreg::RegValue { bytes, vtype })?;
    broadcast_env_change();
    Ok(())
}

/// Append `dir` to the user PATH if not already present.
#[cfg(windows)]
pub fn add_to_user_path(dir: &Path) -> Result<bool> {
    let dir_str = dir.display().to_string();
    let (raw, vtype) = get_user_path_raw()?;
    let expanded = expand_env_strings(&raw);
    let present = raw
        .split(';')
        .chain(expanded.split(';'))
        .any(|p| p.trim().eq_ignore_ascii_case(&dir_str));
    if present {
        return Ok(false);
    }
    let mut new_raw = raw.trim_end_matches(';').to_string();
    if !new_raw.is_empty() {
        new_raw.push(';');
    }
    new_raw.push_str(&dir_str);
    write_raw("Path", &new_raw, vtype)?;
    Ok(true)
}

/// Remove `dir` from the user PATH if present.
#[cfg(windows)]
pub fn remove_from_user_path(dir: &Path) -> Result<bool> {
    let dir_str = dir.display().to_string();
    let (raw, vtype) = get_user_path_raw()?;
    let kept: Vec<String> = raw
        .split(';')
        .filter(|p| !p.trim().eq_ignore_ascii_case(&dir_str))
        .map(|s| s.to_string())
        .collect();
    let new_raw = kept.join(";");
    if new_raw != raw {
        write_raw("Path", &new_raw, vtype)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(windows)]
pub fn set_user_env(name: &str, value: &str) -> Result<()> {
    let vtype = if value.contains('%') {
        RegType::REG_EXPAND_SZ
    } else {
        RegType::REG_SZ
    };
    write_raw(name, value, vtype)
}

#[cfg(windows)]
pub fn remove_user_env(name: &str) -> Result<()> {
    let key = env_key()?;
    let _ = key.delete_value(name);
    broadcast_env_change();
    Ok(())
}

/// Expand %VAR% references for de-duplication checks only.
#[cfg(windows)]
fn expand_env_strings(s: &str) -> String {
    // best effort without loading winapi string tables
    let mut out = s.to_string();
    for (k, v) in std::env::vars() {
        out = out.replace(&format!("%{k}%"), &v);
    }
    out
}

/// Notify running applications that the environment changed.
#[cfg(windows)]
fn broadcast_env_change() {
    // PowerShell one-shot using SendMessageTimeout(HWND_BROADCAST, WM_SETTINGCHANGE)
    let script = r#"$sig = '[DllImport("user32.dll", SetLastError = true)] public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);'
$type = Add-Type -MemberDefinition $sig -Name Win32Env -Namespace ScoopRs -PassThru
$res = [UIntPtr]::Zero
[void]$type::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$res)
"#;
    let tmp = std::env::temp_dir().join(format!("scoop-rs-env-{}.ps1", std::process::id()));
    if std::fs::write(&tmp, script).is_ok() {
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
    }
}

// ---------------- non-Windows stubs ----------------

#[cfg(not(windows))]
pub fn add_to_user_path(dir: &Path) -> Result<bool> {
    eprintln!(
        "note (non-Windows): add {} to your PATH manually if needed",
        dir.display()
    );
    Ok(false)
}

#[cfg(not(windows))]
pub fn remove_from_user_path(_dir: &Path) -> Result<bool> {
    Ok(false)
}

#[cfg(not(windows))]
pub fn set_user_env(_name: &str, _value: &str) -> Result<()> {
    Err(ScoopError::other("env_set is only supported on Windows"))
}

#[cfg(not(windows))]
pub fn remove_user_env(_name: &str) -> Result<()> {
    Ok(())
}
