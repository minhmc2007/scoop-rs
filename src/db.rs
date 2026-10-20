//! Local package database - the pacman-style core of scoop-rs.
//!
//! One SQLite file at `<scoop root>/db/scoop-rs.db` (upstream Scoop never
//! touches that directory, so both clients coexist cleanly). It plays the role
//! of pacman's `/var/lib/pacman/`:
//!
//!   packages       - like pacman's local/<pkg>/desc: what is installed,
//!                    from where, when, with which metadata
//!   package_files  - pacman -Ql style installed-file lists
//!   package_deps   - dependency edges (for orphan reporting)
//!   package_shims  - shim names this client created for a package
//!   buckets        - known buckets, git url, last sync
//!   search_index   - cached manifest metadata for instant search/info/status
//!   meta           - schema_version + bookkeeping (pacman's sync/ db state)

use crate::error::Result;
use crate::version::Version;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

pub const SCHEMA_VERSION: i64 = 1;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS packages (
    name           TEXT PRIMARY KEY,
    version        TEXT NOT NULL,
    bucket         TEXT,
    install_dir    TEXT NOT NULL,
    installed_at_ms INTEGER NOT NULL,
    updated_at_ms  INTEGER,
    size_bytes     INTEGER DEFAULT 0,
    held           INTEGER DEFAULT 0,
    manifest_json  TEXT
);

CREATE TABLE IF NOT EXISTS package_files (
    package_name TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    size_bytes   INTEGER DEFAULT 0,
    PRIMARY KEY (package_name, path)
);

CREATE TABLE IF NOT EXISTS package_deps (
    package_name TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
    dep          TEXT NOT NULL,
    PRIMARY KEY (package_name, dep)
);

CREATE TABLE IF NOT EXISTS package_shims (
    package_name TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
    shim         TEXT NOT NULL,
    PRIMARY KEY (package_name, shim)
);

CREATE TABLE IF NOT EXISTS buckets (
    name            TEXT PRIMARY KEY,
    url             TEXT NOT NULL,
    head_commit     TEXT,
    last_sync_ms    INTEGER,
    manifest_count  INTEGER DEFAULT 0,
    indexed_at_ms   INTEGER
);

CREATE TABLE IF NOT EXISTS search_index (
    name        TEXT NOT NULL,
    bucket      TEXT NOT NULL,
    version     TEXT NOT NULL,
    description TEXT,
    bins        TEXT,
    mtime_ms    INTEGER NOT NULL,
    PRIMARY KEY (name, bucket)
);

CREATE INDEX IF NOT EXISTS idx_search_name ON search_index(name);
CREATE INDEX IF NOT EXISTS idx_files_pkg   ON package_files(package_name);
"#];

#[derive(Debug, Clone)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    pub bucket: Option<String>,
    pub install_dir: String,
    pub installed_at_ms: i64,
    pub size_bytes: i64,
    pub held: bool,
    pub manifest_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BucketRow {
    pub name: String,
    pub url: String,
    pub commit: Option<String>,
    pub last_sync_ms: Option<i64>,
    pub manifest_count: i64,
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub name: String,
    pub bucket: String,
    pub version: String,
    pub description: Option<String>,
    pub bins: Option<String>,
    pub mtime_ms: i64,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;",
        )?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_ro(path: &Path) -> Result<Db> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        let current: i64 = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let v = (i + 1) as i64;
            if v > current {
                conn.execute_batch(sql)?;
                conn.execute(
                    "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![v.to_string()],
                )?;
            }
        }
        Ok(())
    }

    // ---------- meta ----------

    pub fn get_meta(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get::<_, String>(0)
        })
        .ok()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---------- installed packages ----------

    pub fn upsert_package(&self, p: &InstalledPackage) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO packages(name, version, bucket, install_dir, installed_at_ms, updated_at_ms, size_bytes, held, manifest_json)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(name) DO UPDATE SET
                version = excluded.version,
                bucket = excluded.bucket,
                install_dir = excluded.install_dir,
                updated_at_ms = excluded.updated_at_ms,
                size_bytes = excluded.size_bytes,
                held = excluded.held,
                manifest_json = excluded.manifest_json",
            params![
                p.name,
                p.version,
                p.bucket,
                p.install_dir,
                p.installed_at_ms,
                crate::util::now_ms() as i64,
                p.size_bytes,
                p.held as i64,
                p.manifest_json
            ],
        )?;
        Ok(())
    }

    pub fn get_package(&self, name: &str) -> Result<Option<InstalledPackage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, version, bucket, install_dir, installed_at_ms, size_bytes, held, manifest_json
             FROM packages WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], map_package)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn list_packages(&self) -> Result<Vec<InstalledPackage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, version, bucket, install_dir, installed_at_ms, size_bytes, held, manifest_json
             FROM packages ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], map_package)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn delete_package(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM packages WHERE name = ?1", params![name])?;
        Ok(())
    }

    pub fn is_installed(&self, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM packages WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn set_held(&self, name: &str, held: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE packages SET held = ?2 WHERE name = ?1",
            params![name, held as i64],
        )?;
        Ok(())
    }

    // ---------- files / deps / shims ----------

    pub fn replace_files(&self, pkg: &str, files: &[(std::path::PathBuf, u64)]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM package_files WHERE package_name = ?1",
            params![pkg],
        )?;
        for (p, size) in files {
            conn.execute(
                "INSERT OR IGNORE INTO package_files(package_name, path, size_bytes) VALUES(?1, ?2, ?3)",
                params![pkg, p.to_string_lossy(), *size as i64],
            )?;
        }
        Ok(())
    }

    pub fn files_of(&self, pkg: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT path FROM package_files WHERE package_name = ?1 ORDER BY path")?;
        let rows = stmt.query_map(params![pkg], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn replace_deps(&self, pkg: &str, deps: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM package_deps WHERE package_name = ?1",
            params![pkg],
        )?;
        for d in deps {
            conn.execute(
                "INSERT OR IGNORE INTO package_deps(package_name, dep) VALUES(?1, ?2)",
                params![pkg, d],
            )?;
        }
        Ok(())
    }

    pub fn deps_of(&self, pkg: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT dep FROM package_deps WHERE package_name = ?1 ORDER BY dep")?;
        let rows = stmt.query_map(params![pkg], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Dependencies that are not required by any other installed package.
    pub fn orphan_deps(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT d.dep FROM package_deps d
             WHERE d.dep IN (SELECT name FROM packages)
               AND NOT EXISTS (
                    SELECT 1 FROM package_deps r
                    WHERE r.dep = d.dep AND r.package_name != d.package_name
               )
             ORDER BY d.dep",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn replace_shims(&self, pkg: &str, shims: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM package_shims WHERE package_name = ?1",
            params![pkg],
        )?;
        for s in shims {
            conn.execute(
                "INSERT OR IGNORE INTO package_shims(package_name, shim) VALUES(?1, ?2)",
                params![pkg, s],
            )?;
        }
        Ok(())
    }

    pub fn shims_of(&self, pkg: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT shim FROM package_shims WHERE package_name = ?1 ORDER BY shim")?;
        let rows = stmt.query_map(params![pkg], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    // ---------- buckets ----------

    pub fn upsert_bucket(&self, b: &BucketRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO buckets(name, url, head_commit, last_sync_ms, manifest_count, indexed_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET
                url = excluded.url,
                head_commit = excluded.head_commit,
                last_sync_ms = excluded.last_sync_ms,
                manifest_count = excluded.manifest_count,
                indexed_at_ms = excluded.indexed_at_ms",
            params![b.name, b.url, b.commit, b.last_sync_ms, b.manifest_count, crate::util::now_ms() as i64],
        )?;
        Ok(())
    }

    pub fn get_bucket(&self, name: &str) -> Result<Option<BucketRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, url, head_commit, last_sync_ms, manifest_count FROM buckets WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], map_bucket)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn list_buckets(&self) -> Result<Vec<BucketRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, url, head_commit, last_sync_ms, manifest_count FROM buckets ORDER BY name",
        )?;
        let rows = stmt.query_map([], map_bucket)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn delete_bucket(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM buckets WHERE name = ?1", params![name])?;
        conn.execute("DELETE FROM search_index WHERE bucket = ?1", params![name])?;
        Ok(())
    }

    // ---------- search index ----------

    pub fn replace_bucket_index(&self, bucket: &str, rows: &[IndexEntry]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM search_index WHERE bucket = ?1",
            params![bucket],
        )?;
        for r in rows {
            conn.execute(
                "INSERT OR REPLACE INTO search_index(name, bucket, version, description, bins, mtime_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![r.name, r.bucket, r.version, r.description, r.bins, r.mtime_ms],
            )?;
        }
        Ok(())
    }

    pub fn update_index_entry(&self, r: &IndexEntry) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO search_index(name, bucket, version, description, bins, mtime_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![r.name, r.bucket, r.version, r.description, r.bins, r.mtime_ms],
        )?;
        Ok(())
    }

    pub fn index_entries_for_bucket(&self, bucket: &str) -> Result<Vec<IndexEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, bucket, version, description, bins, mtime_ms FROM search_index WHERE bucket = ?1",
        )?;
        let rows = stmt.query_map(params![bucket], map_index)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn lookup_index(&self, name: &str, bucket: &str) -> Result<Option<IndexEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, bucket, version, description, bins, mtime_ms
             FROM search_index WHERE name = ?1 AND bucket = ?2",
        )?;
        let mut rows = stmt.query_map(params![name, bucket], map_index)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Bucket-aware lookup: bare name first, then any "bucket/name" form.
    pub fn search_index_entries(&self, query: &str, limit: usize) -> Result<Vec<IndexEntry>> {
        let conn = self.conn.lock().unwrap();
        let like = format!("%{}%", query.replace(['%', '_'], ""));
        let mut stmt = conn.prepare(
            "SELECT name, bucket, version, description, bins, mtime_ms FROM search_index
             WHERE name LIKE ?1 OR description LIKE ?1 OR bins LIKE ?1
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![like, limit as i64], map_index)?;
        let mut entries: Vec<IndexEntry> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        // rank: exact match, prefix match, then substring; buckets keep main first
        let q = query.to_lowercase();
        entries.sort_by(|a, b| {
            let sa = score_entry(a, &q);
            let sb = score_entry(b, &q);
            sa.cmp(&sb)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.bucket.cmp(&b.bucket))
        });
        entries.truncate(limit);
        Ok(entries)
    }

    pub fn all_index_entries(&self, limit: usize) -> Result<Vec<IndexEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, bucket, version, description, bins, mtime_ms FROM search_index
             ORDER BY name COLLATE NOCASE LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], map_index)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn index_size(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM search_index", [], |r| r.get(0))?;
        Ok(n)
    }

    /// Delete a single search_index row (used by index pruning).
    pub fn delete_index_row(&self, bucket: &str, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM search_index WHERE bucket = ?1 AND name = ?2",
            params![bucket, name],
        )?;
        Ok(())
    }
}

fn score_entry(e: &IndexEntry, q: &str) -> i32 {
    let name = e.name.to_lowercase();
    if name == q {
        0
    } else if name.starts_with(q) {
        1
    } else if name.contains(q) {
        2
    } else {
        3
    }
}

fn map_package(r: &rusqlite::Row) -> std::result::Result<InstalledPackage, rusqlite::Error> {
    Ok(InstalledPackage {
        name: r.get(0)?,
        version: r.get(1)?,
        bucket: r.get(2)?,
        install_dir: r.get(3)?,
        installed_at_ms: r.get(4)?,
        size_bytes: r.get(5)?,
        held: r.get::<_, i64>(6)? != 0,
        manifest_json: r.get(7)?,
    })
}

fn map_bucket(r: &rusqlite::Row) -> std::result::Result<BucketRow, rusqlite::Error> {
    Ok(BucketRow {
        name: r.get(0)?,
        url: r.get(1)?,
        commit: r.get(2)?,
        last_sync_ms: r.get(3)?,
        manifest_count: r.get(4)?,
    })
}

fn map_index(r: &rusqlite::Row) -> std::result::Result<IndexEntry, rusqlite::Error> {
    Ok(IndexEntry {
        name: r.get(0)?,
        bucket: r.get(1)?,
        version: r.get(2)?,
        description: r.get(3)?,
        bins: r.get(4)?,
        mtime_ms: r.get(5)?,
    })
}

/// Compare an installed version against an available one.
pub fn is_upgradable(installed: &str, available: &str) -> bool {
    let i = Version::new(installed);
    let a = Version::new(available);
    a > i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn migrations_and_crud_roundtrip() {
        let (_d, db) = temp_db();
        assert_eq!(db.get_meta("schema_version").as_deref(), Some("1"));

        let pkg = InstalledPackage {
            name: "git".into(),
            version: "2.43.0".into(),
            bucket: Some("main".into()),
            install_dir: "/x/apps/git/2.43.0".into(),
            installed_at_ms: 1700000000000,
            size_bytes: 1234,
            held: false,
            manifest_json: Some("{}".into()),
        };
        db.upsert_package(&pkg).unwrap();
        assert!(db.is_installed("git").unwrap());
        let got = db.get_package("git").unwrap().unwrap();
        assert_eq!(got.version, "2.43.0");

        db.replace_files("git", &[(std::path::PathBuf::from("bin/git.exe"), 100)])
            .unwrap();
        assert_eq!(db.files_of("git").unwrap(), vec!["bin/git.exe"]);
        db.replace_shims("git", &["git".into(), "gitk".into()])
            .unwrap();
        assert_eq!(db.shims_of("git").unwrap().len(), 2);
        db.replace_deps("git", &["less".into()]).unwrap();
        assert_eq!(db.deps_of("git").unwrap(), vec!["less"]);

        db.delete_package("git").unwrap();
        assert!(!db.is_installed("git").unwrap());
        // cascading deletes
        assert!(db.files_of("git").unwrap().is_empty());
        assert!(db.shims_of("git").unwrap().is_empty());
    }

    #[test]
    fn bucket_and_index_flow() {
        let (_d, db) = temp_db();
        db.upsert_bucket(&BucketRow {
            name: "main".into(),
            url: "https://github.com/ScoopInstaller/Main".into(),
            commit: Some("abc".into()),
            last_sync_ms: Some(1),
            manifest_count: 2,
        })
        .unwrap();
        let rows = db.replace_bucket_index(
            "main",
            &[
                IndexEntry {
                    name: "aria2".into(),
                    bucket: "main".into(),
                    version: "1.36.0".into(),
                    description: Some("dl".into()),
                    bins: Some("aria2c.exe".into()),
                    mtime_ms: 1,
                },
                IndexEntry {
                    name: "arpanet".into(),
                    bucket: "main".into(),
                    version: "0.1".into(),
                    description: None,
                    bins: None,
                    mtime_ms: 1,
                },
            ],
        );
        rows.unwrap();
        assert_eq!(db.index_size().unwrap(), 2);
        let hits = db.search_index_entries("aria2", 10).unwrap();
        assert_eq!(hits[0].name, "aria2");
        let hits = db.search_index_entries("ar", 10).unwrap();
        assert!(hits.len() >= 2);
    }

    #[test]
    fn orphan_detection() {
        let (_d, db) = temp_db();
        let mk = |n: &str| InstalledPackage {
            name: n.into(),
            version: "1".into(),
            bucket: None,
            install_dir: n.into(),
            installed_at_ms: 1,
            size_bytes: 0,
            held: false,
            manifest_json: None,
        };
        db.upsert_package(&mk("base")).unwrap();
        db.upsert_package(&mk("tool")).unwrap();
        db.replace_deps("tool", &["base".into()]).unwrap();
        assert_eq!(db.orphan_deps().unwrap(), vec!["base"]);
    }

    #[test]
    fn reopen_preserves_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        {
            let db = Db::open(&path).unwrap();
            db.set_meta("hello", "world").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.get_meta("hello").as_deref(), Some("world"));
        assert_eq!(db.get_meta("schema_version").as_deref(), Some("1"));
    }
}
