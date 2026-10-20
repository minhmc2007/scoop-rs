//! App-name resolution and dependency ordering.
//!
//! Accepts:
//!   * `app`          - searched across buckets (main first, others alphabetical)
//!   * `bucket/app`   - pinned to one bucket
//!   * `path/to/x.json` or `./x.json` - a local manifest file

use crate::buckets::bucket_priority_order;
use crate::config::Root;
use crate::db::Db;
use crate::error::{Result, ScoopError};
use crate::manifest::Manifest;
use std::path::PathBuf;

pub struct Resolved {
    pub name: String,
    pub bucket: String,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    pub raw_json: String,
}

pub fn resolve(root: &Root, db: &Db, spec: &str) -> Result<Resolved> {
    // local file?
    let as_path = PathBuf::from(spec);
    if spec.ends_with(".json") && as_path.is_file() {
        let name = as_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".into());
        let (manifest, raw_json, problems) = crate::manifest::load_and_validate(&as_path)?;
        warn_problems(&problems, &name);
        return Ok(Resolved {
            name,
            bucket: "local".into(),
            manifest_path: as_path,
            manifest,
            raw_json,
        });
    }

    // bucket/name form?
    if let Some((bucket, name)) = spec.split_once('/') {
        if let Some(p) = root.manifest_file(bucket, name) {
            let (manifest, raw_json, problems) = crate::manifest::load_and_validate(&p)?;
            warn_problems(&problems, name);
            return Ok(Resolved {
                name: name.into(),
                bucket: bucket.into(),
                manifest_path: p,
                manifest,
                raw_json,
            });
        }
        return Err(ScoopError::AppNotFound(spec.into()));
    }

    // bare name: prefer indexed bucket for speed, then verify the file exists
    let buckets = bucket_priority_order(&crate::buckets::all_buckets(root, db)?);
    for b in &buckets {
        if let Some(p) = root.manifest_file(&b.name, spec) {
            let (manifest, raw_json, problems) = crate::manifest::load_and_validate(&p)?;
            warn_problems(&problems, spec);
            return Ok(Resolved {
                name: spec.into(),
                bucket: b.name.clone(),
                manifest_path: p,
                manifest,
                raw_json,
            });
        }
    }
    Err(ScoopError::AppNotFound(spec.into()))
}

fn warn_problems(problems: &[String], name: &str) {
    if !problems.is_empty() {
        eprintln!(
            "{} manifest '{}' has schema warnings:",
            console::style("warning:").yellow().bold(),
            name
        );
        for p in problems {
            eprintln!("  - {p}");
        }
    }
}

/// Split "bucket/app" -> (Some(bucket), app); bare "app" -> (None, app).
pub fn split_spec(spec: &str) -> (Option<&str>, &str) {
    match spec.split_once('/') {
        Some((b, n)) if !b.is_empty() && !n.is_empty() => (Some(b), n),
        _ => (None, spec),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DepNode {
    pub spec: String,
    pub name: String,
    pub bucket: Option<String>,
}

/// Topologically ordered install plan for the given roots (roots last, like
/// pacman: dependencies first). Skips already-installed packages unless
/// `force` is set. Fails with MissingDependencies when a dep is unreachable.
struct PlanState<'a> {
    root: &'a Root,
    db: &'a Db,
    force: bool,
    order: Vec<DepNode>,
    seen: std::collections::HashSet<String>,
    in_stack: std::collections::HashSet<String>,
    missing: Vec<String>,
}

pub fn plan(root: &Root, db: &Db, specs: &[String], force: bool) -> Result<Vec<DepNode>> {
    let mut st = PlanState {
        root,
        db,
        force,
        order: Vec::new(),
        seen: std::collections::HashSet::new(),
        in_stack: std::collections::HashSet::new(),
        missing: Vec::new(),
    };

    for spec in specs {
        st.visit(spec)?;
    }

    if !st.missing.is_empty() {
        st.missing.sort();
        st.missing.dedup();
        return Err(ScoopError::MissingDependencies(st.missing.join(", ")));
    }
    Ok(st.order)
}

impl<'a> PlanState<'a> {
    fn visit(&mut self, spec: &str) -> Result<()> {
        let (bucket_hint, name) = split_spec(spec);
        let key = spec.to_string();

        if self.in_stack.contains(&key) {
            return Err(ScoopError::DependencyCycle(spec.into()));
        }
        if self.seen.contains(&key) {
            return Ok(());
        }
        // already installed? (only meaningful for bare deps)
        if !self.force {
            if let Ok(true) = self.db.is_installed(name) {
                self.seen.insert(key);
                return Ok(());
            }
        }

        // resolvable here?
        let resolved = match resolve(self.root, self.db, spec) {
            Ok(r) => r,
            Err(_) => {
                self.missing.push(spec.to_string());
                self.seen.insert(key);
                return Ok(());
            }
        };

        self.in_stack.insert(key.clone());
        let deps = resolved.manifest.depends.clone();
        for dep in &deps {
            self.visit(dep)?;
        }
        self.in_stack.remove(&key);
        self.seen.insert(key);

        self.order.push(DepNode {
            spec: spec.to_string(),
            name: name.to_string(),
            bucket: bucket_hint.map(|s| s.to_string()),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_specs() {
        assert_eq!(
            split_spec("extras/vcredist2022"),
            (Some("extras"), "vcredist2022")
        );
        assert_eq!(split_spec("git"), (None, "git"));
        assert_eq!(split_spec("/weird"), (None, "/weird"));
    }

    #[test]
    fn plan_orders_dependencies_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::from_path(dir.path());
        std::fs::create_dir_all(root.bucket_dir("main")).unwrap();
        std::fs::write(
            root.bucket_dir("main").join("lib.json"),
            r#"{"version":"1.0","url":"https://x/lib.zip"}"#,
        )
        .unwrap();
        std::fs::write(
            root.bucket_dir("main").join("tool.json"),
            r#"{"version":"2.0","url":"https://x/tool.zip","depends":["lib"]}"#,
        )
        .unwrap();
        let db = crate::db::Db::open(&root.db_file()).unwrap();
        crate::buckets::index_bucket(&root, &db, "main").unwrap();

        let plan = plan(&root, &db, &["tool".to_string()], false).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].name, "lib");
        assert_eq!(plan[1].name, "tool");
    }

    #[test]
    fn plan_reports_missing_deps() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::from_path(dir.path());
        std::fs::create_dir_all(root.bucket_dir("main")).unwrap();
        std::fs::write(
            root.bucket_dir("main").join("tool.json"),
            r#"{"version":"2.0","url":"https://x/t.zip","depends":["ghost/thing"]}"#,
        )
        .unwrap();
        let db = crate::db::Db::open(&root.db_file()).unwrap();
        crate::buckets::index_bucket(&root, &db, "main").unwrap();
        let err = plan(&root, &db, &["tool".to_string()], false).unwrap_err();
        assert!(err.to_string().contains("ghost/thing"));
    }
}
