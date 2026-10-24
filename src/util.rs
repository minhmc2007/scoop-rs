//! Small shared helpers: git invocation, hashing, fs utilities, formatting.

use crate::error::{Result, ScoopError};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn format_time(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Run git with the given arguments, returning trimmed stdout.
pub fn git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    // Be a good citizen inside bucket repos: never rewrite line endings or
    // drag user config into our syncs. This also keeps us from ever committing
    // anything to a bucket repo - we only ever read from them.
    let fixed_args = {
        let mut v: Vec<String> = vec![
            "-c".into(),
            "core.autocrlf=false".into(),
            "-c".into(),
            "advice.detachedHead=false".into(),
        ];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.args(&fixed_args);
    let out = cmd.output().map_err(|e| ScoopError::Git {
        op: args.join(" "),
        stderr: format!("failed to spawn git: {e} (is git installed and on PATH?)"),
    })?;
    if !out.status.success() {
        return Err(ScoopError::Git {
            op: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha256,
    Md5,
}

/// Guess the hash algorithm from digest length (scoop default is sha256).
pub fn guess_algo(digest: &str) -> HashAlgo {
    match digest.trim().len() {
        32 => HashAlgo::Md5,
        _ => HashAlgo::Sha256,
    }
}

pub fn hash_file(path: &Path, algo: HashAlgo) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    match algo {
        HashAlgo::Sha256 => {
            let mut hasher = Sha256::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            Ok(hex::encode(hasher.finalize()))
        }
        HashAlgo::Md5 => {
            use md5::Digest as _;
            let mut hasher = md5::Md5::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            Ok(hex::encode(hasher.finalize()))
        }
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Verify a file against a scoop-style hash spec: a concrete digest, a list of
/// acceptable digests, or the special value "skip" / "" which disables checks.
pub fn verify_hash(file: &Path, accepted: &[String]) -> Result<bool> {
    if accepted.is_empty() {
        return Ok(true);
    }
    let real_sha = hash_file(file, HashAlgo::Sha256)?;
    for h in accepted {
        let h = h.trim();
        if h.is_empty() || h.eq_ignore_ascii_case("skip") {
            return Ok(true);
        }
        let algo = guess_algo(h);
        let real = if algo == HashAlgo::Md5 {
            hash_file(file, HashAlgo::Md5)?
        } else {
            real_sha.clone()
        };
        if real.eq_ignore_ascii_case(h) {
            return Ok(true);
        }
    }
    Err(ScoopError::HashMismatch {
        file: file.display().to_string(),
        expected: accepted.join(" or "),
        actual: real_sha,
    })
}

/// Make a path component safe for the filesystem (used for version dirs).
pub fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            _ => c,
        })
        .collect();
    let trimmed = cleaned.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

/// True if `p` is a symlink (unix) or a junction/symlink directory (Windows).
pub fn is_link(p: &Path) -> bool {
    match std::fs::symlink_metadata(p) {
        Ok(md) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileTypeExt;
                md.file_type().is_symlink() || md.file_type().is_symlink_dir()
            }
            #[cfg(not(windows))]
            {
                md.file_type().is_symlink()
            }
        }
        Err(_) => false,
    }
}

/// remove_dir_all that also defeats read-only attributes (git objects on
/// Windows) and long paths.
pub fn remove_dir_all_robust(p: &Path) -> Result<()> {
    if !p.exists() {
        return Ok(());
    }
    match std::fs::remove_dir_all(p) {
        Ok(()) => Ok(()),
        Err(_) => {
            force_writable(p)?;
            std::fs::remove_dir_all(p).map_err(Into::into)
        }
    }
}

fn force_writable(p: &Path) -> Result<()> {
    if p.is_dir() {
        for entry in std::fs::read_dir(p)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                force_writable(&entry.path())?;
            } else {
                set_rw(&entry.path());
            }
        }
        set_rw(p);
    }
    Ok(())
}

fn set_rw(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644));
    }
    #[cfg(windows)]
    {
        if let Ok(md) = std::fs::metadata(p) {
            let mut perms = md.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false); // clear the readonly bit
            let _ = std::fs::set_permissions(p, perms);
        }
    }
}

/// Recursive directory copy that ignores symlinks pointing outside and
/// preserves executability on unix.
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)?;
        }
        // symlinks/junctions inside archives are deliberately not followed
    }
    Ok(())
}

/// Filename that a URL points at (scoop-style cache naming uses this).
pub fn url_filename(u: &str) -> String {
    match url::Url::parse(u) {
        Ok(parsed) => {
            let last = parsed
                .path()
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or("download");
            percent_decode(last)
        }
        Err(_) => u.rsplit('/').next().unwrap_or("download").to_string(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Create a directory junction (Windows) or a symlink (unix) at `link`
/// pointing to `target`.
pub fn make_dir_link(link: &Path, target: &Path) -> Result<()> {
    if is_link(link) {
        remove_link(link)?;
    } else if link.exists() {
        // a real directory sits where the link should go - move it aside
        let backup = link.with_extension("dir.bak");
        remove_link(&backup).ok();
        std::fs::rename(link, &backup)?;
        remove_link(&backup).ok();
    }

    #[cfg(windows)]
    {
        let out = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()?;
        if !out.status.success() {
            // fall back to a plain symlink for dirs (needs dev mode)
            std::os::windows::fs::symlink_dir(target, link).map_err(|e| {
                ScoopError::other(format!(
                    "failed to create junction {} -> {}: {} ({})",
                    link.display(),
                    target.display(),
                    String::from_utf8_lossy(&out.stderr).trim(),
                    e
                ))
            })?;
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).map_err(|e| {
            ScoopError::other(format!(
                "failed to create symlink {} -> {}: {}",
                link.display(),
                target.display(),
                e
            ))
        })
    }
}

/// Remove a link without following it.
pub fn remove_link(link: &Path) -> Result<()> {
    if !link.exists() && !is_link(link) {
        return Ok(());
    }
    match std::fs::symlink_metadata(link) {
        Ok(md) => {
            // On Windows, symlink_metadata may describe a directory junction as
            // a reparse point rather than a regular directory. Use the Windows
            // file-type extension to select RemoveDirectoryW for junctions;
            // RemoveFileW returns ERROR_ACCESS_DENIED for directory junctions.
            #[cfg(windows)]
            let is_directory = {
                use std::os::windows::fs::FileTypeExt;
                md.is_dir() || md.file_type().is_symlink_dir()
            };
            #[cfg(not(windows))]
            let is_directory = md.is_dir();

            if is_directory {
                // Junctions appear as directories but remove_dir removes only
                // the link, never the target directory.
                std::fs::remove_dir(link).map_err(|e| {
                    ScoopError::other(format!(
                        "failed to remove directory link {}: {e}",
                        link.display()
                    ))
                })?;
            } else {
                std::fs::remove_file(link).map_err(|e| {
                    ScoopError::other(format!("failed to remove link {}: {e}", link.display()))
                })?;
            }
            Ok(())
        }
        Err(_) => Ok(()),
    }
}

/// Hardlink a file, falling back to a real copy when hardlinks are
/// unavailable (different volume, exotic fs).
pub fn hard_link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    remove_link(dst)?;
    if dst.is_dir() {
        remove_dir_all_robust(dst)?;
    }
    if std::fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

/// Walk a directory recursively collecting relative file paths + sizes.
pub fn walk_files(dir: &Path) -> Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    walk_inner(dir, dir, &mut out)?;
    Ok(out)
}

fn walk_inner(root: &Path, cur: &Path, out: &mut Vec<(PathBuf, u64)>) -> Result<()> {
    for entry in std::fs::read_dir(cur)? {
        let entry = entry?;
        let p = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_inner(root, &p, out)?;
        } else if ft.is_file() {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_path_buf();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((rel, size));
        }
    }
    Ok(())
}

pub fn is_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// Print only when stdout is interactive-ish and verbosity allows.
pub struct Output {
    pub quiet: bool,
}

impl Output {
    pub fn info(&self, msg: &str) {
        if !self.quiet {
            println!("{msg}");
        }
    }
    pub fn warn(&self, msg: &str) {
        eprintln!("{} {}", console::style("warning:").yellow().bold(), msg);
    }
    pub fn success(&self, msg: &str) {
        if !self.quiet {
            println!("{} {}", console::style("OK").green().bold(), msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_filename_basics() {
        assert_eq!(
            url_filename("https://example.com/a/b/tool-1.2.zip"),
            "tool-1.2.zip"
        );
        assert_eq!(
            url_filename("https://x.io/d/file.exe?token=abc"),
            "file.exe"
        );
        assert_eq!(url_filename("https://x.io/"), "download");
    }

    #[test]
    fn remove_directory_link_preserves_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"keep").unwrap();

        make_dir_link(&link, &target).unwrap();
        assert!(is_link(&link));

        remove_link(&link).unwrap();

        assert!(target.join("keep.txt").is_file());
        assert!(target.is_dir());
        assert!(!is_link(&link));
        assert!(!link.exists());
    }

    #[test]
    fn sanitize_components() {
        assert_eq!(sanitize_component("1.2/3"), "1.2-3");
        assert_eq!(sanitize_component("nightly."), "nightly");
    }

    #[test]
    fn hash_skip_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.bin");
        std::fs::write(&f, b"hello").unwrap();
        assert!(verify_hash(&f, &["skip".into()]).unwrap());
        assert!(verify_hash(&f, &[]).unwrap());
        let sha = sha256_hex(b"hello");
        assert!(verify_hash(&f, &[sha]).unwrap());
        assert!(verify_hash(&f, &[sha256_hex(b"nope")]).is_err());
    }
}
