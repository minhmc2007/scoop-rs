//! Archive extraction: zip / 7z / tar.gz / tar - all in native Rust, no
//! external 7zip/lessmsi dependency for the common cases. MSI and scripted
//! installers are executed on Windows by the installer pipeline instead.

use crate::error::{Result, ScoopError};
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    Zip,
    SevenZip,
    TarGz,
    Tar,
    TarBz2,
    TarXz,
    Msi,
    Exe,
    Other,
}

pub fn detect(fname: &str) -> PayloadKind {
    let f = fname.to_ascii_lowercase();
    if f.ends_with(".zip") {
        PayloadKind::Zip
    } else if f.ends_with(".7z") {
        PayloadKind::SevenZip
    } else if f.ends_with(".tar.gz") || f.ends_with(".tgz") {
        PayloadKind::TarGz
    } else if f.ends_with(".tar") {
        PayloadKind::Tar
    } else if f.ends_with(".tar.bz2") || f.ends_with(".tbz2") {
        PayloadKind::TarBz2
    } else if f.ends_with(".tar.xz") || f.ends_with(".txz") {
        PayloadKind::TarXz
    } else if f.ends_with(".msi") {
        PayloadKind::Msi
    } else if f.ends_with(".exe") {
        PayloadKind::Exe
    } else {
        PayloadKind::Other
    }
}

pub fn is_archive(kind: PayloadKind) -> bool {
    matches!(
        kind,
        PayloadKind::Zip
            | PayloadKind::SevenZip
            | PayloadKind::TarGz
            | PayloadKind::Tar
            | PayloadKind::TarBz2
            | PayloadKind::TarXz
    )
}

/// Extract `archive` into `dest`.
pub fn extract(archive: &Path, dest: &Path, kind: PayloadKind) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    match kind {
        PayloadKind::Zip => extract_zip(archive, dest),
        PayloadKind::SevenZip => extract_7z(archive, dest),
        PayloadKind::TarGz => {
            let f = std::fs::File::open(archive)?;
            let gz = flate2::read::GzDecoder::new(f);
            tar::Archive::new(gz).unpack(dest)?;
            Ok(())
        }
        PayloadKind::Tar => {
            let f = std::fs::File::open(archive)?;
            tar::Archive::new(f).unpack(dest)?;
            Ok(())
        }
        PayloadKind::TarBz2 => Err(ScoopError::UnsupportedPayload(
            ".tar.bz2 archives are not supported by the native extractor yet".into(),
        )),
        PayloadKind::TarXz => Err(ScoopError::UnsupportedPayload(
            ".tar.xz archives are not supported by the native extractor yet".into(),
        )),
        PayloadKind::Msi | PayloadKind::Exe | PayloadKind::Other => Err(
            ScoopError::UnsupportedPayload(format!("{} is not an archive", archive.display())),
        ),
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(f)
        .map_err(|e| ScoopError::other(format!("cannot open zip {}: {e}", archive.display())))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| {
            ScoopError::other(format!("bad zip entry {}#{}: {e}", archive.display(), i))
        })?;
        // mangled_name() sanitizes absolute paths and .. traversal (zip-slip)
        let out_path = dest.join(entry.mangled_name());
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out)?;
            set_exec_if_unix(&entry, &out_path);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_exec_if_unix<R: std::io::Read>(entry: &zip::read::ZipFile<R>, path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = entry.unix_mode() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn set_exec_if_unix<R: std::io::Read>(_entry: &zip::read::ZipFile<R>, _path: &Path) {}

fn extract_7z(archive: &Path, dest: &Path) -> Result<()> {
    sevenz_rust::decompress_file(archive, dest).map_err(|e| {
        ScoopError::other(format!(
            "7z extraction failed for {}: {e}",
            archive.display()
        ))
    })
}

/// Peek into an archive and report whether the payload is a single top-level
/// directory (useful diagnostics, mirrors scoop's behavior of stripping the
/// wrapper dir via extract_to).
pub fn archive_contains_single_dir(archive: &Path, kind: PayloadKind) -> bool {
    match kind {
        PayloadKind::Zip => {
            if let Ok(f) = std::fs::File::open(archive) {
                if let Ok(mut zip) = zip::ZipArchive::new(f) {
                    let mut first: Option<String> = None;
                    for i in 0..zip.len() {
                        if let Ok(e) = zip.by_index(i) {
                            let name = e.name().to_string();
                            if name.is_empty() {
                                continue;
                            }
                            let top = name.split(['/', '\\']).next().unwrap_or("").to_string();
                            if top.is_empty() {
                                continue;
                            }
                            match &first {
                                None => first = Some(top),
                                Some(prev) => {
                                    if prev != &top {
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                    return first.is_some();
                }
            }
            false
        }
        _ => false,
    }
}

/// Read a file fully (small helper reused by tests).
pub fn read_to_end(p: &Path) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(p)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_kinds() {
        assert_eq!(detect("tool-1.0.zip"), PayloadKind::Zip);
        assert_eq!(detect("TOOL.7Z"), PayloadKind::SevenZip);
        assert_eq!(detect("a.tar.gz"), PayloadKind::TarGz);
        assert_eq!(detect("b.tgz"), PayloadKind::TarGz);
        assert_eq!(detect("setup.exe"), PayloadKind::Exe);
        assert_eq!(detect("thing.msi"), PayloadKind::Msi);
        assert_eq!(detect("data.dat"), PayloadKind::Other);
    }

    #[test]
    fn extract_zip_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("t.zip");
        // build a zip with the zip crate writer
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            zw.start_file("sub/hello.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut zw, b"hello world").unwrap();
            zw.finish().unwrap();
        }
        let out = dir.path().join("out");
        extract(&zip_path, &out, PayloadKind::Zip).unwrap();
        let data = std::fs::read(out.join("sub/hello.txt")).unwrap();
        assert_eq!(data, b"hello world");
    }
}
