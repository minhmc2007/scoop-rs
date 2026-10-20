//! Bucket management: clone/sync git repos and maintain the search index.
//!
//! Buckets are consumed read-only - scoop-rs only ever runs `git clone`,
//! `git pull --ff-only` and `git fetch`+`reset` against them, never commits.
//! Syncs are shallow (--depth 1) so `update` stays fast even for big buckets.

use crate::config::Root;
use crate::db::{BucketRow, Db, IndexEntry};
use crate::error::{Result, ScoopError};
use crate::util::{self, now_ms, Output};
use serde_json::Value;
use std::collections::HashMap;

use std::time::Duration;

/// The official bucket map (mirror of Scoop's buckets.json), used when
/// offline or as a fallback.
pub fn known_buckets() -> Vec<(&'static str, &'static str)> {
    vec![
        ("main", "https://github.com/ScoopInstaller/Main"),
        ("extras", "https://github.com/ScoopInstaller/Extras"),
        ("versions", "https://github.com/ScoopInstaller/Versions"),
        ("nirsoft", "https://github.com/ScoopInstaller/Nirsoft"),
        ("php", "https://github.com/ScoopInstaller/PHP"),
        ("nerd-fonts", "https://github.com/ScoopInstaller/Nerdfont"),
        (
            "nonportable",
            "https://github.com/ScoopInstaller/Nonportable",
        ),
        ("java", "https://github.com/ScoopInstaller/Java"),
        ("games", "https://github.com/Calinou/scoop-games"),
    ]
}

/// Fetch the live bucket map from the Scoop repo (cached in the DB meta).
pub fn fetch_known_buckets(db: &Db) -> Vec<(String, String)> {
    if let Some(cached) = db.get_meta("known_buckets") {
        if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&cached) {
            let age = db
                .get_meta("known_buckets_at")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if now_ms().saturating_sub(age) < 24 * 3600 * 1000 {
                let mut v: Vec<(String, String)> = map.into_iter().collect();
                v.sort();
                return v;
            }
        }
    }
    let url = "https://raw.githubusercontent.com/ScoopInstaller/Scoop/master/buckets.json";
    let fetched: Option<HashMap<String, String>> = (|| {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .ok()?;
        let text = client.get(url).send().ok()?.text().ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let mut map = HashMap::new();
        for (k, item) in v.as_object()? {
            let src = item.get("source")?.as_str()?.to_string();
            map.insert(k.clone(), src);
        }
        Some(map)
    })();
    match fetched {
        Some(map) if !map.is_empty() => {
            db.set_meta(
                "known_buckets",
                &serde_json::to_string(&map).unwrap_or_default(),
            )
            .ok();
            db.set_meta("known_buckets_at", &now_ms().to_string()).ok();
            let mut v: Vec<(String, String)> = map.into_iter().collect();
            v.sort();
            v
        }
        _ => known_buckets()
            .into_iter()
            .map(|(k, u)| (k.to_string(), u.to_string()))
            .collect(),
    }
}

pub fn resolve_bucket_url(name: &str, explicit: Option<&str>, db: &Db) -> String {
    if let Some(u) = explicit {
        return normalize_url(u);
    }
    for (k, u) in fetch_known_buckets(db) {
        if k == name {
            return u;
        }
    }
    format!("https://github.com/ScoopInstaller/{}", capitalize(name))
}

fn normalize_url(u: &str) -> String {
    let u = u.trim();
    if u.ends_with(".git") || u.contains("://") || u.starts_with("git@") {
        u.to_string()
    } else {
        format!("https://github.com/{u}")
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn add_bucket(root: &Root, db: &Db, name: &str, url: Option<&str>, out: &Output) -> Result<()> {
    let dir = root.bucket_dir(name);
    if dir.is_dir() {
        return Err(ScoopError::BucketExists(
            name.into(),
            dir.display().to_string(),
        ));
    }
    let url = resolve_bucket_url(name, url, db);
    out.info(&format!("Cloning '{name}' bucket from {url} ..."));
    std::fs::create_dir_all(root.buckets()).map_err(ScoopError::from)?;
    util::git(
        Some(&root.buckets()),
        &[
            "clone",
            "--depth",
            "1",
            "--single-branch",
            &url,
            &dir.display().to_string(),
        ],
    )?;
    let commit = util::git(Some(&dir), &["rev-parse", "--short", "HEAD"]).ok();
    let n = index_bucket(root, db, name)?;
    db.upsert_bucket(&BucketRow {
        name: name.into(),
        url: url.clone(),
        commit,
        last_sync_ms: Some(now_ms() as i64),
        manifest_count: n as i64,
    })?;
    out.success(&format!("bucket '{name}' added ({n} manifests indexed)"));
    Ok(())
}

pub fn remove_bucket(root: &Root, db: &Db, name: &str) -> Result<()> {
    if db.get_bucket(name)?.is_none() && !root.bucket_dir(name).is_dir() {
        return Err(ScoopError::BucketMissing(name.into()));
    }
    db.delete_bucket(name)?;
    let dir = root.bucket_dir(name);
    if dir.is_dir() {
        util::remove_dir_all_robust(&dir)?;
    }
    Ok(())
}

/// Rebuild the search index for one bucket from its manifest files.
/// Returns the number of manifests indexed.
pub fn index_bucket(root: &Root, db: &Db, bucket: &str) -> Result<usize> {
    let dir = root.bucket_dir(bucket);
    if !dir.is_dir() {
        return Err(ScoopError::BucketMissing(bucket.into()));
    }
    let mut entries = Vec::new();
    let mut warnings = 0usize;
    // official Scoop buckets keep manifests in `<repo>/bucket/*.json`;
    // some third-party buckets keep them at the repo root - scan both
    let mut manifest_dirs = vec![dir.clone()];
    let sub = dir.join("bucket");
    if sub.is_dir() {
        manifest_dirs.push(sub);
    }
    for mdir in manifest_dirs {
        for file in std::fs::read_dir(&mdir)? {
            let file = file?;
            let p = file.path();
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if !name.to_ascii_lowercase().ends_with(".json") || name.starts_with('.') {
                continue;
            }
            let app = name.trim_end_matches(".json").to_string();
            if app.is_empty() {
                continue;
            }
            let mtime = file
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let text = match std::fs::read_to_string(&p) {
                Ok(t) => t,
                Err(_) => {
                    warnings += 1;
                    continue;
                }
            };
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warnings += 1;
                    eprintln!("  warning: {bucket}/{name}: {e}");
                    continue;
                }
            };
            let version = v
                .get("version")
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => "unknown".into(),
                })
                .unwrap_or_else(|| "unknown".into());
            let description = v
                .get("description")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let bins = v.get("bin").and_then(|b| match b {
                Value::String(s) => Some(s.clone()),
                Value::Array(a) => a.first().map(|f| match f {
                    Value::String(s) => s.clone(),
                    Value::Array(pair) => pair
                        .first()
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    _ => String::new(),
                }),
                _ => None,
            });
            entries.push(IndexEntry {
                name: app,
                bucket: bucket.into(),
                version,
                description,
                bins,
                mtime_ms: mtime,
            });
        }
    }
    let n = entries.len();
    db.replace_bucket_index(bucket, &entries)?;
    if warnings > 0 {
        eprintln!("  warning: {warnings} manifest(s) in '{bucket}' could not be parsed");
    }
    Ok(n)
}

/// Incrementally refresh the index: only re-parse manifests newer than the
/// indexed mtime. Cheap enough to run before every search/status/install.
pub fn ensure_fresh_index(root: &Root, db: &Db, out: &Output) -> Result<()> {
    for b in db.list_buckets()? {
        let dir = root.bucket_dir(&b.name);
        if !dir.is_dir() {
            continue;
        }
        let mut manifest_jsons: Vec<std::path::PathBuf> = Vec::new();
        for d in [dir.clone(), dir.join("bucket")] {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.filter_map(|e| e.ok()) {
                    let name = e.file_name().to_string_lossy().to_lowercase();
                    if name.ends_with(".json") && !name.starts_with('.') {
                        manifest_jsons.push(e.path());
                    }
                }
            }
        }
        let stale = manifest_jsons.iter().any(|p| {
            let mt = std::fs::metadata(p)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let app = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let cached = db
                .lookup_index(&app, &b.name)
                .ok()
                .flatten()
                .map(|i| i.mtime_ms)
                .unwrap_or(-1);
            mt > cached
        });
        if stale {
            out.info(&format!("Refreshing index for bucket '{}'...", b.name));
            let n = index_bucket(root, db, &b.name)?;
            db.upsert_bucket(&BucketRow {
                manifest_count: n as i64,
                ..b.clone()
            })?;
        }
    }
    Ok(())
}

/// git-pull one bucket (shallow-friendly). Falls back to fetch + hard reset
/// onto the tracked upstream branch (we only ever hold pristine upstream
/// content in these repos, so a reset never loses anything user-made).
pub fn sync_bucket(root: &Root, db: &Db, name: &str, _out: &Output) -> Result<()> {
    let dir = root.bucket_dir(name);
    if !dir.is_dir() {
        return Err(ScoopError::BucketMissing(name.into()));
    }
    let ok = util::git(Some(&dir), &["pull", "--ff-only", "origin"]).is_ok();
    if !ok {
        let branch = util::git(Some(&dir), &["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_else(|_| "master".into());
        let fetch_ok = util::git(Some(&dir), &["fetch", "origin", "--depth", "1"]).is_ok();
        if !fetch_ok {
            // repo may not be shallow; try full fetch
            util::git(Some(&dir), &["fetch", "origin"])?;
        }
        util::git(
            Some(&dir),
            &["reset", "--hard", &format!("origin/{branch}")],
        )?;
    }
    let commit = util::git(Some(&dir), &["rev-parse", "--short", "HEAD"]).ok();
    let n = index_bucket(root, db, name)?;
    let url = db.get_bucket(name)?.map(|b| b.url).unwrap_or_default();
    db.upsert_bucket(&BucketRow {
        name: name.into(),
        url,
        commit,
        last_sync_ms: Some(now_ms() as i64),
        manifest_count: n as i64,
    })?;
    Ok(())
}

pub fn sync_all(root: &Root, db: &Db, out: &Output) -> Result<()> {
    let buckets = db.list_buckets()?;
    for b in &buckets {
        if !root.bucket_dir(&b.name).is_dir() {
            out.warn(&format!("bucket '{}' is registered but its directory is missing - run `scoop-rs bucket add {}`", b.name, b.name));
            continue;
        }
        out.info(&format!("Syncing bucket '{}' ...", b.name));
        match sync_bucket(root, db, &b.name, out) {
            Ok(()) => out.success(&format!("bucket '{}'", b.name)),
            Err(e) => out.warn(&format!("bucket '{}' sync failed: {e}", b.name)),
        }
    }
    Ok(())
}

/// Bucket iteration order for bare-name resolution: main first, rest alpha.
pub fn bucket_priority_order(buckets: &[BucketRow]) -> Vec<BucketRow> {
    let mut v = buckets.to_vec();
    v.sort_by(|a, b| {
        let pa = if a.name == "main" {
            (0, a.name.clone())
        } else {
            (1, a.name.clone())
        };
        let pb = if b.name == "main" {
            (0, b.name.clone())
        } else {
            (1, b.name.clone())
        };
        pa.cmp(&pb)
    });
    v
}

/// All buckets: DB-registered ones plus any bucket directory found on disk
/// (e.g. cloned by PowerShell Scoop before scoop-rs ever ran). Disk entries
/// that are not yet indexed get indexed on the fly.
pub fn all_buckets(root: &Root, db: &Db) -> Result<Vec<BucketRow>> {
    let mut known: Vec<BucketRow> = db.list_buckets()?;
    let dirs = root
        .buckets()
        .is_dir()
        .then(|| std::fs::read_dir(root.buckets()).unwrap());
    if let Some(dirs) = dirs {
        for entry in dirs.filter_map(|e| e.ok()) {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || known.iter().any(|b| b.name == name) {
                continue;
            }
            // a valid bucket dir holds at least one .json manifest
            let has_manifests = std::fs::read_dir(entry.path())
                .map(|rd| {
                    rd.filter_map(|e| e.ok()).any(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .to_lowercase()
                            .ends_with(".json")
                    })
                })
                .unwrap_or(false);
            if !has_manifests {
                continue;
            }
            let n = index_bucket(root, db, &name).unwrap_or(0);
            let row = BucketRow {
                name: name.clone(),
                url: "(discovered on disk)".into(),
                commit: None,
                last_sync_ms: Some(now_ms() as i64),
                manifest_count: n as i64,
            };
            db.upsert_bucket(&row)?;
            known.push(row);
        }
    }
    Ok(known)
}

/// Ensure at least the main bucket exists before the first install.
pub fn ensure_main_bucket(root: &Root, db: &Db, out: &Output) -> Result<()> {
    if !db.list_buckets()?.iter().any(|b| b.name == "main") && !root.bucket_dir("main").is_dir() {
        out.info("Adding the 'main' bucket (default)... ");
        add_bucket(root, db, "main", None, out)?;
    }
    Ok(())
}

/// Remove stray index rows for manifests that disappeared from disk.
pub fn prune_index(root: &Root, db: &Db) -> Result<()> {
    for b in db.list_buckets()? {
        let dir = root.bucket_dir(&b.name);
        if !dir.is_dir() {
            continue;
        }
        for entry in db.index_entries_for_bucket(&b.name)? {
            if !root
                .manifest_file(&b.name, &entry.name)
                .map(|p| p.is_file())
                .unwrap_or(false)
            {
                db.delete_index_row(&b.name, &entry.name)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_buckets_shape() {
        let kb = known_buckets();
        assert!(kb.iter().any(|(n, _)| *n == "main"));
        assert!(kb.iter().all(|(_, u)| u.starts_with("https://")));
    }

    #[test]
    fn url_normalization() {
        assert_eq!(
            normalize_url("ScoopInstaller/Java"),
            "https://github.com/ScoopInstaller/Java"
        );
        assert_eq!(
            normalize_url("https://gitlab.com/x/y"),
            "https://gitlab.com/x/y"
        );
    }

    #[test]
    fn priority_puts_main_first() {
        let mk = |n: &str| BucketRow {
            name: n.into(),
            url: "u".into(),
            commit: None,
            last_sync_ms: None,
            manifest_count: 0,
        };
        let ordered = bucket_priority_order(&[mk("versions"), mk("extras"), mk("main")]);
        assert_eq!(ordered[0].name, "main");
        assert_eq!(ordered[1].name, "extras");
    }

    #[test]
    fn indexing_a_fake_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::from_path(dir.path());
        std::fs::create_dir_all(root.bucket_dir("test")).unwrap();
        std::fs::write(
            root.bucket_dir("test").join("app1.json"),
            r#"{"version":"1.0","description":"demo","bin":"app1.exe"}"#,
        )
        .unwrap();
        std::fs::write(
            root.bucket_dir("test").join(".hidden.json"),
            r#"{"version":"0"}"#,
        )
        .unwrap();
        std::fs::write(
            root.bucket_dir("test").join("broken.json"),
            r#"{"version":"#,
        )
        .unwrap();
        let db = crate::db::Db::open(&root.db_file()).unwrap();
        let n = index_bucket(&root, &db, "test").unwrap();
        assert_eq!(n, 1); // hidden + broken skipped, app1 indexed
        let hits = db.search_index_entries("app1", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].version, "1.0");
    }
}
