//! Scoop manifest parsing, architecture resolution and schema validation.
//!
//! Manifests come verbatim from the upstream buckets - scoop-rs is a pure
//! client and never requires any manifest or bucket change. All of Scoop's
//! polymorphic JSON shapes are supported (string | array | [[path, alias,
//! args]] | ...), with helpful diagnostics instead of hard failures for
//! exotic forms.

use crate::error::{Result, ScoopError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const KNOWN_ARCHES: [&str; 3] = ["64bit", "32bit", "arm64"];

fn string_or_vec<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("string or array of strings")
        }
        fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<Self::Value, E> {
            Ok(vec![s.to_string()])
        }
        fn visit_u64<E: serde::de::Error>(self, n: u64) -> std::result::Result<Self::Value, E> {
            Ok(vec![n.to_string()])
        }
        fn visit_i64<E: serde::de::Error>(self, n: i64) -> std::result::Result<Self::Value, E> {
            Ok(vec![n.to_string()])
        }
        fn visit_f64<E: serde::de::Error>(self, n: f64) -> std::result::Result<Self::Value, E> {
            Ok(vec![format!("{n}")])
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(item) = seq.next_element::<serde_json::Value>()? {
                match item {
                    serde_json::Value::String(s) => out.push(s),
                    serde_json::Value::Number(n) => out.push(n.to_string()),
                    other => out.push(other.to_string()),
                }
            }
            Ok(out)
        }
    }
    d.deserialize_any(V)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Installer {
    #[serde(deserialize_with = "string_or_vec")]
    pub file: Vec<String>,
    #[serde(deserialize_with = "string_or_vec")]
    pub args: Vec<String>,
    #[serde(deserialize_with = "string_or_vec")]
    pub script: Vec<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub keep: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Uninstaller {
    #[serde(deserialize_with = "string_or_vec")]
    pub file: Vec<String>,
    #[serde(deserialize_with = "string_or_vec")]
    pub args: Vec<String>,
    #[serde(deserialize_with = "string_or_vec")]
    pub script: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Manifest {
    #[serde(deserialize_with = "string_or_vec")]
    pub version: Vec<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<serde_json::Value>,
    pub notes: Option<serde_json::Value>,
    #[serde(deserialize_with = "string_or_vec")]
    pub depends: Vec<String>,
    pub architecture: Option<BTreeMap<String, serde_json::Value>>,
    pub url: Option<serde_json::Value>,
    pub hash: Option<serde_json::Value>,
    pub extract_dir: Option<serde_json::Value>,
    pub extract_to: Option<serde_json::Value>,
    pub bin: Option<serde_json::Value>,
    pub shortcuts: Option<serde_json::Value>,
    pub pre_install: Option<serde_json::Value>,
    pub installer: Option<Installer>,
    pub post_install: Option<serde_json::Value>,
    pub uninstall: Option<Uninstaller>,
    pub env_add_path: Option<serde_json::Value>,
    pub env_set: Option<serde_json::Value>,
    pub persist: Option<serde_json::Value>,
    pub checkver: Option<serde_json::Value>,
    pub autoupdate: Option<serde_json::Value>,
    pub suggest: Option<serde_json::Value>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest> {
        Ok(serde_json::from_str(text)?)
    }

    pub fn from_file(path: &Path) -> Result<Manifest> {
        let text = std::fs::read_to_string(path)?;
        Manifest::parse(&text)
    }

    pub fn version_str(&self) -> String {
        self.version
            .first()
            .cloned()
            .unwrap_or_else(|| "unknown".into())
    }

    /// Field from architecture block wins over the top-level one.
    /// (Resolution operates on the raw JSON Value; see resolve_arch.)
    /// Resolve the view of the manifest for a given architecture.
    /// `raw` must be the original JSON text this manifest was parsed from.
    pub fn resolve_arch_from_raw(raw: &str, arch: &str) -> Result<ResolvedManifest> {
        let v: serde_json::Value = serde_json::from_str(raw)?;
        let m: Manifest = serde_json::from_value(v.clone())?;
        Manifest::resolve_arch(&m, &v, arch)
    }

    pub fn resolve_arch(
        m: &Manifest,
        raw: &serde_json::Value,
        arch: &str,
    ) -> Result<ResolvedManifest> {
        let get = |key: &str| -> Option<&serde_json::Value> {
            m.architecture
                .as_ref()
                .and_then(|a| a.get(arch))
                .and_then(|av| av.get(key))
                .filter(|v| !v.is_null())
                .or_else(|| raw.get(key))
                .filter(|v| !v.is_null())
        };

        let urls = get("url")
            .map(|v| strings(v).unwrap_or_default())
            .unwrap_or_default();
        let hashes = get("hash")
            .map(|v| strings(v).unwrap_or_default())
            .unwrap_or_default();
        let extract_dir = get("extract_dir")
            .map(|v| strings(v).unwrap_or_default())
            .unwrap_or_default();
        let extract_to = get("extract_to")
            .map(|v| strings(v).unwrap_or_default())
            .unwrap_or_default();

        let (bins, bin_warnings) = get("bin").map(parse_bins).unwrap_or((vec![], vec![]));
        let (persists, persist_warnings) = get("persist")
            .map(parse_persist)
            .unwrap_or((vec![], vec![]));
        let env_add_path = get("env_add_path")
            .map(|v| strings(v).unwrap_or_default())
            .unwrap_or_default();
        let env_set = get("env_set")
            .and_then(|v| v.as_object().cloned())
            .map(|o| {
                o.into_iter()
                    .filter_map(|(k, v)| match v {
                        serde_json::Value::String(s) => Some((k, s)),
                        serde_json::Value::Number(n) => Some((k, n.to_string())),
                        serde_json::Value::Bool(b) => Some((k, b.to_string())),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let shortcuts = get("shortcuts")
            .and_then(|v| v.as_array().cloned())
            .map(|arr| {
                arr.into_iter()
                    .filter_map(|e| {
                        let items: Vec<String> = e
                            .as_array()?
                            .iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect();
                        (!items.is_empty()).then_some(items)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let installer = m
            .installer
            .clone()
            .or_else(|| get("installer").and_then(|v| serde_json::from_value(v.clone()).ok()));

        let mut warnings = bin_warnings;
        warnings.extend(persist_warnings);

        Ok(ResolvedManifest {
            version: m.version_str(),
            description: m.description.clone(),
            homepage: m.homepage.clone(),
            license: license_display(&m.license),
            notes: notes_strings(&m.notes),
            depends: m.depends.clone(),
            urls,
            hashes,
            extract_dir,
            extract_to,
            bins,
            shortcuts,
            pre_install: get("pre_install").and_then(strings).unwrap_or_default(),
            post_install: get("post_install").and_then(strings).unwrap_or_default(),
            installer,
            uninstall: m
                .uninstall
                .clone()
                .or_else(|| get("uninstall").and_then(|v| serde_json::from_value(v.clone()).ok())),
            env_add_path,
            env_set,
            persist: persists,
            warnings,
        })
    }
}

/// A manifest flattened for one architecture - everything downstream needs.
#[derive(Debug, Clone)]
pub struct ResolvedManifest {
    pub version: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub notes: Vec<String>,
    pub depends: Vec<String>,
    pub urls: Vec<String>,
    pub hashes: Vec<String>,
    pub extract_dir: Vec<String>,
    pub extract_to: Vec<String>,
    pub bins: Vec<BinEntry>,
    pub shortcuts: Vec<Vec<String>>,
    pub pre_install: Vec<String>,
    pub post_install: Vec<String>,
    pub installer: Option<Installer>,
    pub uninstall: Option<Uninstaller>,
    pub env_add_path: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub persist: Vec<PersistEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BinEntry {
    /// relative path inside the install dir (or the downloaded file name)
    pub path: String,
    pub alias: Option<String>,
    pub args: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PersistEntry {
    pub source: String,
    pub target: Option<String>,
}

pub fn current_arch() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    return "64bit";
    #[cfg(target_arch = "x86")]
    return "32bit";
    #[cfg(target_arch = "aarch64")]
    return "arm64";
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
    return "64bit";
}

fn strings(v: &serde_json::Value) -> Option<Vec<String>> {
    match v {
        serde_json::Value::String(s) => Some(vec![s.clone()]),
        serde_json::Value::Array(a) => Some(
            a.iter()
                .filter_map(|x| match x {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect(),
        ),
        serde_json::Value::Number(n) => Some(vec![n.to_string()]),
        _ => None,
    }
}

fn parse_bins(v: &serde_json::Value) -> (Vec<BinEntry>, Vec<String>) {
    fn push_str(bins: &mut Vec<BinEntry>, s: &str, warnings: &mut Vec<String>) {
        let path = s.trim();
        if path.is_empty() {
            warnings.push("empty bin entry".into());
            return;
        }
        bins.push(BinEntry {
            path: path.to_string(),
            alias: None,
            args: None,
        });
    }
    let mut bins = Vec::new();
    let mut warnings = Vec::new();
    match v {
        serde_json::Value::String(s) => push_str(&mut bins, s, &mut warnings),
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    serde_json::Value::String(s) => push_str(&mut bins, s, &mut warnings),
                    serde_json::Value::Array(pair) => {
                        let strs: Vec<String> = pair
                            .iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect();
                        if strs.is_empty() {
                            warnings.push(format!("malformed bin entry: {item}"));
                            continue;
                        }
                        bins.push(BinEntry {
                            path: strs[0].clone(),
                            alias: strs.get(1).filter(|s| !s.is_empty()).cloned(),
                            args: strs.get(2).filter(|s| !s.is_empty()).cloned(),
                        });
                    }
                    serde_json::Value::Object(_) => warnings.push(format!(
                        "unsupported bin object form: {item} (use \"path\" or [\"path\", \"alias\", \"args\"])"
                    )),
                    other => warnings.push(format!("unsupported bin entry: {other}")),
                }
            }
        }
        other => warnings.push(format!("bin must be a string or array, got: {other}")),
    }
    (bins, warnings)
}

/// Public helper: parse the `bin` field of a raw manifest value into
/// simple target paths (used when importing installs from upstream Scoop).
pub fn parse_bins_public(v: &serde_json::Value) -> Vec<String> {
    let (bins, _) = parse_bins(v);
    bins.into_iter().map(|b| b.path).collect()
}

fn parse_persist(v: &serde_json::Value) -> (Vec<PersistEntry>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    match v {
        serde_json::Value::String(s) => out.push(PersistEntry {
            source: s.clone(),
            target: None,
        }),
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    serde_json::Value::String(s) => out.push(PersistEntry {
                        source: s.clone(),
                        target: None,
                    }),
                    serde_json::Value::Array(pair) if pair.len() == 2 => {
                        let a = pair[0].as_str().unwrap_or_default();
                        let b = pair[1].as_str().unwrap_or_default();
                        out.push(PersistEntry {
                            source: a.into(),
                            target: Some(b.into()),
                        });
                    }
                    other => warnings.push(format!("unsupported persist entry: {other}")),
                }
            }
        }
        other => warnings.push(format!("persist must be a string or array, got: {other}")),
    }
    (out, warnings)
}

fn license_display(v: &Option<serde_json::Value>) -> Option<String> {
    match v {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Object(o)) => {
            let id = o.get("identifier").and_then(|x| x.as_str());
            let url = o.get("url").and_then(|x| x.as_str());
            match (id, url) {
                (Some(i), Some(u)) => Some(format!("{i} ({u})")),
                (Some(i), None) => Some(i.to_string()),
                (None, Some(u)) => Some(u.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn notes_strings(v: &Option<serde_json::Value>) -> Vec<String> {
    v.as_ref()
        .map(|n| strings(n).unwrap_or_default())
        .unwrap_or_default()
}

/// Structural validation ("schema check") for a manifest. Returns a list of
/// problems/warnings; empty list = healthy.
pub fn validate_manifest(v: &serde_json::Value) -> Vec<String> {
    let mut problems = Vec::new();
    let obj = match v {
        serde_json::Value::Object(o) => o,
        _ => {
            problems.push("manifest root must be a JSON object".into());
            return problems;
        }
    };

    // version
    match obj.get("version") {
        None => problems.push("missing required field: version".into()),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => {
            problems.push("version is empty".into())
        }
        Some(serde_json::Value::String(_)) | Some(serde_json::Value::Number(_)) => {}
        Some(other) => problems.push(format!("version must be a string, got: {other}")),
    }

    // urls: top level or per-arch
    let top_url = obj.get("url");
    let arch = obj.get("architecture").and_then(|a| a.as_object());
    if top_url.is_none() && arch.is_none() {
        problems.push("missing url (top-level or architecture block)".into());
    }
    if let Some(arch) = arch {
        for k in arch.keys() {
            if !KNOWN_ARCHES.contains(&k.as_str()) {
                problems.push(format!(
                    "unknown architecture key '{k}' (expected one of: {})",
                    KNOWN_ARCHES.join(", ")
                ));
            }
        }
        if arch.is_empty() {
            problems.push("architecture block is empty".into());
        }
        let any_arch_url = arch.values().any(|a| a.get("url").is_some());
        if !any_arch_url && top_url.is_none() {
            problems.push("no url found in any architecture block".into());
        }
    }

    // hash present? (warning, not fatal)
    let has_hash = obj.get("hash").is_some()
        || arch
            .map(|a| a.values().any(|av| av.get("hash").is_some()))
            .unwrap_or(false);
    if !has_hash {
        problems.push(
            "no hash specified - downloads will not be verified (recommended: sha256)".into(),
        );
    }

    // known-field check: near-miss typos (list mirrors the fields actually
    // used across the official Scoop buckets, incl. '##' comment convention)
    const KNOWN: [&str; 34] = [
        "version",
        "description",
        "homepage",
        "license",
        "notes",
        "depends",
        "architecture",
        "url",
        "hash",
        "extract_dir",
        "extract_to",
        "bin",
        "shortcuts",
        "pre_install",
        "installer",
        "post_install",
        "uninstall",
        "uninstaller",
        "pre_uninstall",
        "post_uninstall",
        "env_add_path",
        "env_set",
        "persist",
        "checkver",
        "autoupdate",
        "suggest",
        "ps",
        "psmodule",
        "innosetup",
        "script",
        "hooks",
        "cookie",
        "##",
        "comment",
    ];
    for k in obj.keys() {
        if !KNOWN.contains(&k.as_str()) && !KNOWN_ARCHES.contains(&k.as_str()) {
            let near = KNOWN
                .iter()
                .chain(KNOWN_ARCHES.iter())
                .find(|c| levenshtein_small(k, c) <= 2);
            match near {
                Some(candidate) => {
                    problems.push(format!("unknown field '{k}' - did you mean '{candidate}'?"))
                }
                None => problems.push(format!("unknown field '{k}' (typo?)")),
            }
        }
    }

    // installer type
    fn check_installer(iv: &serde_json::Value, problems: &mut Vec<String>) {
        if let Some(t) = iv.get("type").and_then(|x| x.as_str()) {
            const OK: [&str; 6] = ["msi", "inno", "innosetup", "nullsoft", "nsis", "burn"];
            if !OK.contains(&t.to_ascii_lowercase().as_str()) {
                problems.push(format!("unknown installer.type '{t}'"));
            }
        }
        if let Some(s) = iv.get("script") {
            if !s.is_string() && !s.is_array() {
                problems.push("installer.script must be a string or array of strings".into());
            }
        }
    }
    if let Some(iv) = obj.get("installer") {
        check_installer(iv, &mut problems);
    }
    if let Some(arch) = arch {
        for (name, av) in arch {
            if let Some(iv) = av.get("installer") {
                check_installer(iv, &mut problems);
            }
            if !av.is_object() {
                problems.push(format!("architecture.{name} must be an object"));
            }
        }
    }

    // depends shape
    if let Some(d) = obj.get("depends") {
        if !d.is_string() && !d.is_array() {
            problems.push("depends must be a string or array of strings".into());
        }
    }

    problems
}

fn levenshtein_small(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > 2 {
        return 99;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Parse + validate in one step; hard-fails only on structural breakage,
/// reporting schema warnings separately.
pub fn load_and_validate(path: &Path) -> Result<(Manifest, String, Vec<String>)> {
    let text = std::fs::read_to_string(path)?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| ScoopError::ManifestInvalid {
            path: path.display().to_string(),
            problems: e.to_string(),
        })?;
    let problems = validate_manifest(&v);
    let m = Manifest::parse(&text).map_err(|e| ScoopError::ManifestInvalid {
        path: path.display().to_string(),
        problems: e.to_string(),
    })?;
    Ok((m, text, problems))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": "1.2.3",
        "description": "demo tool",
        "homepage": "https://example.org",
        "license": { "identifier": "MIT", "url": "https://opensource.org/licenses/MIT" },
        "depends": [ "extras/vcredist2022", "7zip" ],
        "url": "https://example.org/tool-1.2.3.zip",
        "hash": "abc123",
        "bin": [ "tool.exe", [ "bin/other.exe", "other", "--flag" ] ],
        "persist": [ "config", [ "data/dir", "data" ] ],
        "env_add_path": "lib",
        "env_set": { "TOOL_HOME": "$dir" },
        "notes": [ "thanks for installing" ],
        "installer": { "script": [ "echo hi" ] },
        "architecture": {
            "64bit": { "url": "https://example.org/tool-1.2.3-x64.zip", "hash": "def456" },
            "arm64": { "url": "https://example.org/tool-1.2.3-arm.zip", "hash": "789" }
        }
    }"#;

    #[test]
    fn parse_sample() {
        let v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.version_str(), "1.2.3");
        let r = Manifest::resolve_arch(&m, &v, "64bit").unwrap();
        assert_eq!(r.urls, vec!["https://example.org/tool-1.2.3-x64.zip"]);
        assert_eq!(r.hashes, vec!["def456"]);
        assert_eq!(r.bins.len(), 2);
        assert_eq!(r.bins[1].alias.as_deref(), Some("other"));
        assert_eq!(r.bins[1].args.as_deref(), Some("--flag"));
        assert_eq!(r.persist.len(), 2);
        assert_eq!(r.persist[1].source, "data/dir");
        assert_eq!(r.persist[1].target.as_deref(), Some("data"));
        assert_eq!(r.env_set, vec![("TOOL_HOME".into(), "$dir".into())]);
        assert_eq!(
            r.license.as_deref(),
            Some("MIT (https://opensource.org/licenses/MIT)")
        );
        assert_eq!(r.depends, vec!["extras/vcredist2022", "7zip"]);
    }

    #[test]
    fn fallback_to_top_level_fields() {
        let v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let m = Manifest::parse(SAMPLE).unwrap();
        let r = Manifest::resolve_arch(&m, &v, "32bit").unwrap(); // not defined -> top-level
        assert_eq!(r.urls, vec!["https://example.org/tool-1.2.3.zip"]);
    }

    #[test]
    fn validation_catches_typos() {
        let bad = r#"{ "version": "1.0", "desciption": "typo", "url": "https://x/a.zip", "architecture": { "64biit": {} } }"#;
        let v: serde_json::Value = serde_json::from_str(bad).unwrap();
        let problems = validate_manifest(&v);
        assert!(problems
            .iter()
            .any(|p| p.contains("desciption") && p.contains("description")));
        assert!(problems.iter().any(|p| p.contains("64biit")));
        assert!(problems.iter().any(|p| p.contains("hash")));
    }

    #[test]
    fn validation_requires_version_and_url() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{ "description": "no version" }"#).unwrap();
        let problems = validate_manifest(&v);
        assert!(problems.iter().any(|p| p.contains("version")));
        assert!(problems.iter().any(|p| p.contains("url")));
    }

    #[test]
    fn number_version_ok() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{ "version": 42, "url": "https://x/a.zip" }"#).unwrap();
        let m: Manifest = serde_json::from_value(v).unwrap();
        assert_eq!(m.version_str(), "42");
    }
}
