//! Parallel download engine with resume + hash verification.
//!
//! Replaces PowerShell's Invoke-WebRequest (the single biggest source of
//! Scoop's slowness): concurrent transfers, HTTP Range resume from partial
//! `.part` files, streaming SHA256/MD5 verification, and live progress bars.

use crate::config::scoop_proxy;
use crate::error::{Result, ScoopError};
use crate::util::is_tty;
use console::style;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub struct DownloadItem {
    pub url: String,
    pub dest: PathBuf,
    pub expected_hashes: Vec<String>,
}

pub struct DownloadOpts {
    pub no_progress: bool,
    pub jobs: usize,
}

impl Default for DownloadOpts {
    fn default() -> Self {
        DownloadOpts {
            no_progress: !is_tty(),
            jobs: 4,
        }
    }
}

fn build_client() -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .tcp_nodelay(true)
        .user_agent("scoop-rs/0.1 (+https://github.com/ScoopInstaller/scoop; like scoop)");
    if let Some(proxy) = scoop_proxy() {
        let proxy_url = if proxy.contains("://") {
            proxy
        } else {
            format!("http://{proxy}")
        };
        builder = builder.proxy(
            reqwest::Proxy::all(&proxy_url)
                .map_err(|e| ScoopError::other(format!("invalid proxy config: {e}")))?,
        );
    }
    Ok(builder.build()?)
}

/// Download all items (deduplicated), verifying hashes. Items with a
/// complete, hash-valid cache file are skipped instantly.
pub fn download_all(items: &[DownloadItem], opts: &DownloadOpts) -> Result<()> {
    // dedupe by destination
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<&DownloadItem> = Vec::new();
    for it in items {
        if seen.insert(it.dest.clone()) {
            unique.push(it);
        }
    }

    let jobs = opts.jobs.clamp(1, 8).min(unique.len().max(1));
    let errors: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let remaining = Arc::new(AtomicUsize::new(unique.len()));
    let progress = (!opts.no_progress && is_tty()).then(indicatif::MultiProgress::new);

    std::thread::scope(|scope| {
        // worker pool pulling from a shared queue
        let queue: Arc<std::sync::Mutex<std::collections::VecDeque<usize>>> = Arc::default();
        {
            let mut q = queue.lock().unwrap();
            *q = (0..unique.len()).collect();
        }
        let items_ref: &[&DownloadItem] = &unique;
        for _ in 0..jobs {
            let queue = Arc::clone(&queue);
            let errors = Arc::clone(&errors);
            let remaining = Arc::clone(&remaining);
            let progress = progress.clone();
            scope.spawn(move || {
                let client = match build_client() {
                    Ok(c) => c,
                    Err(e) => {
                        errors.lock().unwrap().push(e.to_string());
                        return;
                    }
                };
                loop {
                    let idx = {
                        let mut q = queue.lock().unwrap();
                        match q.pop_front() {
                            Some(i) => i,
                            None => break,
                        }
                    };
                    let item = items_ref[idx];
                    if let Err(e) = download_one(&client, item, progress.as_ref()) {
                        errors.lock().unwrap().push(format!("{}: {e}", item.url));
                    }
                    remaining.fetch_sub(1, Ordering::SeqCst);
                }
            });
        }
    });

    let errs = errors.lock().unwrap().clone();
    if !errs.is_empty() {
        return Err(ScoopError::other(format!(
            "{} download(s) failed:\n  - {}",
            errs.len(),
            errs.join("\n  - ")
        )));
    }
    Ok(())
}

fn download_one(
    client: &reqwest::blocking::Client,
    item: &DownloadItem,
    progress: Option<&indicatif::MultiProgress>,
) -> Result<()> {
    // already cached & valid?
    if item.dest.is_file() {
        if crate::util::verify_hash(&item.dest, &item.expected_hashes).is_ok() {
            return Ok(());
        }
        // corrupt cache - remove and refetch
        std::fs::remove_file(&item.dest).ok();
    }
    if let Some(parent) = item.dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let part = PathBuf::from(format!("{}.part", item.dest.display()));
    let start_at = part.metadata().map(|m| m.len()).unwrap_or(0);

    let mut req = client.get(&item.url);
    if start_at > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={start_at}-"));
    }

    let pb = progress.map(|mp| {
        let pb = mp.add(indicatif::ProgressBar::new(start_at));
        pb.set_style(
            indicatif::ProgressStyle::with_template(
                "  {spinner:.cyan} {msg:.white} [{bar:30.cyan/blue}] {bytes}/{total_bytes} {eta}",
            )
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
            .progress_chars("=>-"),
        );
        let fname = item
            .dest
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        pb.set_message(fname);
        pb
    });

    let resp = req.send().map_err(|e| ScoopError::Download {
        url: item.url.clone(),
        reason: e.to_string(),
    })?;
    let status = resp.status();
    if !(status.is_success() || status == reqwest::StatusCode::NOT_MODIFIED) {
        return Err(ScoopError::Download {
            url: item.url.clone(),
            reason: format!("HTTP {status}"),
        });
    }

    let total = resp.content_length();
    let mut file = if status == reqwest::StatusCode::PARTIAL_CONTENT {
        // resume supported
        let mut f = std::fs::OpenOptions::new().append(true).open(&part)?;
        f.seek(std::io::SeekFrom::End(0)).ok();
        if let (Some(t), s) = (total, start_at) {
            set_len(&pb, t + s);
        }
        f
    } else {
        // full restart (server ignored Range or we had no partial)
        let f = std::fs::File::create(&part)?;
        if let Some(t) = total {
            set_len(&pb, t);
        }
        f
    };

    let mut reader = resp;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        if let Some(pb) = &pb {
            pb.inc(n as u64);
        }
    }
    drop(file);
    if let Some(pb) = &pb {
        pb.finish_and_clear();
    }

    let size = part.metadata().map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err(ScoopError::Download {
            url: item.url.clone(),
            reason: "downloaded file is empty".into(),
        });
    }

    // hash verification (streaming)
    let accepted: Vec<String> = item
        .expected_hashes
        .iter()
        .filter(|h| !h.is_empty() && !h.eq_ignore_ascii_case("skip"))
        .cloned()
        .collect();
    if !accepted.is_empty() {
        if let Err(crate::error::ScoopError::HashMismatch {
            expected, actual, ..
        }) = crate::util::verify_hash(&part, &accepted)
        {
            std::fs::remove_file(&part).ok();
            return Err(ScoopError::HashMismatch {
                file: item.dest.display().to_string(),
                expected,
                actual,
            });
        }
    }

    std::fs::rename(&part, &item.dest)?;
    Ok(())
}

fn set_len(pb: &Option<indicatif::ProgressBar>, len: u64) {
    if let Some(pb) = pb {
        pb.set_length(len);
    }
}

/// Pretty single-line summary used by the install pipeline.
pub fn print_cache_hit(dest: &Path) {
    if is_tty() {
        eprintln!("  {} {}", style("cached").green().bold(), dest.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_opts_sane() {
        let o = DownloadOpts::default();
        assert!(o.jobs >= 1);
    }

    // network flow is covered by tests/integration.rs with a local server
}
