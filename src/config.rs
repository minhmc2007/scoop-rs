//! Scoop root resolution and directory layout.
//!
//! Resolution order (identical to upstream Scoop so both clients agree on the
//! same install):
//!   1. `SCOOP` environment variable
//!   2. `root_path` from Scoop's own config (`~/.config/scoop/config.json`)
//!   3. default `~/scoop`
//!
//! scoop-rs stores its own database under `<root>/db/` - a directory upstream
//! Scoop never touches, which keeps the two clients perfectly side by side.

use crate::error::{Result, ScoopError};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct ScoopConfig {
    pub value: Value,
}

impl ScoopConfig {
    pub fn load() -> ScoopConfig {
        let path = config_path();
        let value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        ScoopConfig { value }
    }

    pub fn str_key(&self, key: &str) -> Option<String> {
        self.value
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

pub fn config_path() -> PathBuf {
    // Scoop itself uses ~/.config/scoop/config.json on every platform
    if let Some(home) = dirs::home_dir() {
        home.join(".config").join("scoop").join("config.json")
    } else {
        PathBuf::from(".config/scoop/config.json")
    }
}

#[derive(Debug, Clone)]
pub struct Root {
    pub path: PathBuf,
}

impl Root {
    pub fn resolve() -> Result<Root> {
        let mut path = None;
        if let Ok(env) = std::env::var("SCOOP") {
            if !env.trim().is_empty() {
                path = Some(PathBuf::from(env));
            }
        }
        if path.is_none() {
            if let Some(cfg_root) = ScoopConfig::load().str_key("root_path") {
                if !cfg_root.trim().is_empty() {
                    path = Some(PathBuf::from(cfg_root));
                }
            }
        }
        if path.is_none() {
            let home = dirs::home_dir()
                .ok_or_else(|| ScoopError::other("cannot determine home directory"))?;
            path = Some(home.join("scoop"));
        }
        Ok(Root {
            path: path.unwrap(),
        })
    }

    /// Allow tests (and power users) to point at an arbitrary layout.
    pub fn from_path(p: impl Into<PathBuf>) -> Root {
        Root { path: p.into() }
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for d in [
            self.path.clone(),
            self.apps(),
            self.buckets(),
            self.shims(),
            self.cache(),
            self.persist(),
            self.db_dir(),
        ] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    pub fn apps(&self) -> PathBuf {
        self.path.join("apps")
    }
    pub fn app_dir(&self, app: &str) -> PathBuf {
        self.apps().join(app)
    }
    pub fn version_dir(&self, app: &str, version: &str) -> PathBuf {
        self.app_dir(app)
            .join(crate::util::sanitize_component(version))
    }
    pub fn current_dir(&self, app: &str) -> PathBuf {
        self.app_dir(app).join("current")
    }
    pub fn buckets(&self) -> PathBuf {
        self.path.join("buckets")
    }
    pub fn bucket_dir(&self, name: &str) -> PathBuf {
        self.buckets().join(name)
    }
    pub fn shims(&self) -> PathBuf {
        self.path.join("shims")
    }
    pub fn cache(&self) -> PathBuf {
        // scoop honors SCOOP_CACHE for a shared cache location
        if let Ok(c) = std::env::var("SCOOP_CACHE") {
            if !c.trim().is_empty() {
                return PathBuf::from(c);
            }
        }
        self.path.join("cache")
    }
    pub fn persist(&self) -> PathBuf {
        self.path.join("persist")
    }
    pub fn persist_dir(&self, app: &str) -> PathBuf {
        self.persist().join(app)
    }
    pub fn db_dir(&self) -> PathBuf {
        self.path.join("db")
    }
    pub fn db_file(&self) -> PathBuf {
        self.db_dir().join("scoop-rs.db")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.db_dir().join("scoop-rs.lock")
    }

    /// Cache file name following scoop's exact convention so both clients
    /// share one download cache: `app#version#filename`.
    pub fn cache_file(&self, app: &str, version: &str, url: &str) -> PathBuf {
        let fname = crate::util::url_filename(url);
        self.cache().join(format!(
            "{}#{}#{}",
            crate::util::sanitize_component(app),
            crate::util::sanitize_component(version),
            fname
        ))
    }

    /// Read a bucket-local manifest file if it exists. Official Scoop buckets
    /// store manifests in a `bucket/` subdirectory; older buckets at the root.
    pub fn manifest_file(&self, bucket: &str, app: &str) -> Option<PathBuf> {
        let dir = self.bucket_dir(bucket);
        [
            dir.join(format!("{app}.json")),
            dir.join("bucket").join(format!("{app}.json")),
        ]
        .into_iter()
        .find(|c| c.is_file())
    }

    pub fn app_exists(&self, app: &str) -> bool {
        self.app_dir(app).is_dir()
    }
}

/// Proxy configured in Scoop's config.json ("host:port" or "user:pass@host:port").
pub fn scoop_proxy() -> Option<String> {
    let cfg = ScoopConfig::load();
    cfg.str_key("proxy").filter(|s| !s.is_empty())
}

pub fn path_display(p: &Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_paths() {
        let root = Root::from_path("/tmp/fakeroot");
        assert_eq!(root.app_dir("git"), PathBuf::from("/tmp/fakeroot/apps/git"));
        assert_eq!(
            root.current_dir("git"),
            PathBuf::from("/tmp/fakeroot/apps/git/current")
        );
        assert_eq!(
            root.cache_file("7zip", "23.01", "https://www.7-zip.org/a/7z2301-x64.exe"),
            PathBuf::from("/tmp/fakeroot/cache/7zip#23.01#7z2301-x64.exe")
        );
        root.ensure_layout().unwrap();
        assert!(root.db_dir().is_dir());
    }
}
