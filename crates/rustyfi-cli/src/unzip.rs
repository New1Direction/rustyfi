//! Minimal, zip-slip-safe archive helpers for the CLI.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Extract `archive` (raw zip bytes) into `dest`, stripping the leading path
/// component when every entry shares one (so `crate/Cargo.toml` lands as
/// `dest/Cargo.toml`). Returns the number of files written.
pub fn extract_bytes_stripping_root(archive: &[u8], dest: &Path) -> io::Result<usize> {
    let reader = io::Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let root = common_root(&mut zip);
    fs::create_dir_all(dest)?;
    let mut written = 0;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        }; // zip-slip guard
        let rel = strip_root(&name, root.as_deref());
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = safe_join(dest, &rel)?;
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&out)?;
        io::copy(&mut entry, &mut f)?;
        written += 1;
    }
    Ok(written)
}

/// Extract a source `.zip` file on disk into `dest` (no root stripping — we keep
/// the project's own layout so package detection sees real directories).
pub fn extract_file(archive_path: &Path, dest: &Path) -> io::Result<()> {
    let bytes = fs::read(archive_path)?;
    let reader = io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    fs::create_dir_all(dest)?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        // Skip macOS junk so it isn't mistaken for source.
        if name.components().any(|c| c.as_os_str() == "__MACOSX") {
            continue;
        }
        let out = safe_join(dest, &name)?;
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&out)?;
        io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}

/// The shared first path component of every file entry, if there is exactly one.
fn common_root<R: io::Read + io::Seek>(zip: &mut zip::ZipArchive<R>) -> Option<String> {
    let mut root: Option<String> = None;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index(i) else {
            return None;
        };
        let name = entry.enclosed_name()?;
        let first = name.components().next();
        let Some(Component::Normal(seg)) = first else {
            return None;
        };
        let seg = seg.to_string_lossy().to_string();
        match &root {
            None => root = Some(seg),
            Some(r) if *r != seg => return None,
            _ => {}
        }
    }
    root
}

fn strip_root(name: &Path, root: Option<&str>) -> PathBuf {
    match root {
        Some(r) => name.strip_prefix(r).unwrap_or(name).to_path_buf(),
        None => name.to_path_buf(),
    }
}

/// Join `rel` onto `base`, rejecting any component that would escape `base`.
fn safe_join(base: &Path, rel: &Path) -> io::Result<PathBuf> {
    let mut out = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe path component in archive",
                ))
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal() {
        let base = Path::new("/out");
        assert!(safe_join(base, Path::new("../etc/passwd")).is_err());
        assert!(safe_join(base, Path::new("/abs")).is_err());
        assert!(safe_join(base, Path::new("a/b.rs"))
            .unwrap()
            .ends_with("a/b.rs"));
    }

    #[test]
    fn strip_root_removes_common_prefix() {
        assert_eq!(
            strip_root(Path::new("crate/Cargo.toml"), Some("crate")),
            PathBuf::from("Cargo.toml")
        );
        assert_eq!(strip_root(Path::new("a/b"), None), PathBuf::from("a/b"));
    }
}
