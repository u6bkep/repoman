//! Parser for pacman repository database files (.db.tar.*)
//!
//! Repo databases contain directories for each package, with `desc` files
//! using a `%FIELD%\nvalue\n` format.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

/// Package entry parsed from a repo database desc file
#[derive(Debug, Clone, Default)]
pub struct RepoPkgEntry {
    pub name: String,
    pub base: String,
    pub version: String,
    pub desc: Option<String>,
    pub arch: String,
    pub depends: Vec<String>,
    pub makedepends: Vec<String>,
    pub optdepends: Vec<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
    pub filename: String,
}

/// Parsed repo database contents
#[derive(Default)]
pub struct RepoDbContents {
    /// All packages indexed by pkgname
    pub packages: HashMap<String, RepoPkgEntry>,
    /// Packages grouped by pkgbase
    pub by_pkgbase: HashMap<String, Vec<RepoPkgEntry>>,
}

/// Parse a pacman repository database file
///
/// Supports .db.tar.zst, .db.tar.gz, .db.tar.xz, and uncompressed .db.tar
pub fn parse_repo_db(path: &Path) -> Result<RepoDbContents> {
    let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");

    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open repo database: {}", path.display()))?;

    // Detect compression from extension and create appropriate reader
    let reader: Box<dyn Read> = if filename.ends_with(".tar.zst") || filename.ends_with(".zst") {
        Box::new(zstd::Decoder::new(file).context("Failed to create zstd decoder")?)
    } else if filename.ends_with(".tar.gz") || filename.ends_with(".gz") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else if filename.ends_with(".tar.xz") || filename.ends_with(".xz") {
        Box::new(xz2::read::XzDecoder::new(file))
    } else {
        // Assume uncompressed tar
        Box::new(file)
    };

    let mut archive = tar::Archive::new(reader);
    let mut packages: HashMap<String, RepoPkgEntry> = HashMap::new();

    for entry in archive.entries().context("Failed to read tar entries")? {
        let mut entry = entry.context("Failed to read tar entry")?;
        let path = entry.path().context("Failed to get entry path")?;
        let path_str = path.to_string_lossy().to_string();

        // We only care about desc files
        if !path_str.ends_with("/desc") {
            continue;
        }

        let mut content = String::new();
        entry
            .read_to_string(&mut content)
            .with_context(|| format!("Failed to read desc file: {}", path_str))?;

        match parse_desc_file(&content) {
            Ok(pkg_entry) => {
                if !pkg_entry.name.is_empty() {
                    packages.insert(pkg_entry.name.clone(), pkg_entry);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to parse desc file {}: {}", path_str, e);
            }
        }
    }

    // Group by pkgbase
    let mut by_pkgbase: HashMap<String, Vec<RepoPkgEntry>> = HashMap::new();
    for pkg in packages.values() {
        by_pkgbase
            .entry(pkg.base.clone())
            .or_default()
            .push(pkg.clone());
    }

    Ok(RepoDbContents {
        packages,
        by_pkgbase,
    })
}

/// Parse a desc file content into a RepoPkgEntry
///
/// Desc files use the format:
/// ```text
/// %FIELD%
/// value
///
/// %ANOTHER%
/// value1
/// value2
/// ```
fn parse_desc_file(content: &str) -> Result<RepoPkgEntry> {
    let mut entry = RepoPkgEntry::default();
    let mut current_field: Option<String> = None;
    let mut current_values: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.starts_with('%') && line.ends_with('%') {
            // Save previous field
            if let Some(field) = current_field.take() {
                apply_field(&mut entry, &field, &current_values);
            }
            current_field = Some(line[1..line.len() - 1].to_string());
            current_values.clear();
        } else if !line.is_empty() {
            current_values.push(line.to_string());
        }
    }

    // Don't forget the last field
    if let Some(field) = current_field {
        apply_field(&mut entry, &field, &current_values);
    }

    // If BASE is not set, default to NAME
    if entry.base.is_empty() && !entry.name.is_empty() {
        entry.base = entry.name.clone();
    }

    Ok(entry)
}

/// Apply parsed field values to a RepoPkgEntry
fn apply_field(entry: &mut RepoPkgEntry, field: &str, values: &[String]) {
    match field {
        "NAME" => {
            if let Some(v) = values.first() {
                entry.name = v.clone();
            }
        }
        "BASE" => {
            if let Some(v) = values.first() {
                entry.base = v.clone();
            }
        }
        "VERSION" => {
            if let Some(v) = values.first() {
                entry.version = v.clone();
            }
        }
        "DESC" => {
            if let Some(v) = values.first() {
                entry.desc = Some(v.clone());
            }
        }
        "ARCH" => {
            if let Some(v) = values.first() {
                entry.arch = v.clone();
            }
        }
        "FILENAME" => {
            if let Some(v) = values.first() {
                entry.filename = v.clone();
            }
        }
        "DEPENDS" => {
            entry.depends = values.to_vec();
        }
        "MAKEDEPENDS" => {
            entry.makedepends = values.to_vec();
        }
        "OPTDEPENDS" => {
            entry.optdepends = values.to_vec();
        }
        "PROVIDES" => {
            entry.provides = values.to_vec();
        }
        "CONFLICTS" => {
            entry.conflicts = values.to_vec();
        }
        _ => {
            // Ignore other fields (CSIZE, ISIZE, MD5SUM, SHA256SUM, etc.)
        }
    }
}

/// Strip version constraint from a dependency string
/// e.g., "foo>=1.0" -> "foo", "bar" -> "bar"
pub fn strip_version_constraint(dep: &str) -> &str {
    dep.split(['>', '<', '=', ':']).next().unwrap_or(dep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_desc_file() {
        let content = r#"%FILENAME%
example-1.0.0-1-x86_64.pkg.tar.zst

%NAME%
example

%BASE%
example

%VERSION%
1.0.0-1

%DESC%
An example package

%ARCH%
x86_64

%DEPENDS%
glibc
openssl>=1.0

%PROVIDES%
libexample.so=1-64
"#;

        let entry = parse_desc_file(content).unwrap();
        assert_eq!(entry.name, "example");
        assert_eq!(entry.base, "example");
        assert_eq!(entry.version, "1.0.0-1");
        assert_eq!(entry.desc, Some("An example package".to_string()));
        assert_eq!(entry.arch, "x86_64");
        assert_eq!(entry.depends, vec!["glibc", "openssl>=1.0"]);
        assert_eq!(entry.provides, vec!["libexample.so=1-64"]);
        assert_eq!(entry.filename, "example-1.0.0-1-x86_64.pkg.tar.zst");
    }

    #[test]
    fn test_parse_desc_file_no_base() {
        let content = r#"%NAME%
simple-pkg

%VERSION%
2.0-1
"#;

        let entry = parse_desc_file(content).unwrap();
        assert_eq!(entry.name, "simple-pkg");
        assert_eq!(entry.base, "simple-pkg"); // Should default to name
        assert_eq!(entry.version, "2.0-1");
    }

    #[test]
    fn test_strip_version_constraint() {
        assert_eq!(strip_version_constraint("foo"), "foo");
        assert_eq!(strip_version_constraint("foo>=1.0"), "foo");
        assert_eq!(strip_version_constraint("bar<2.0"), "bar");
        assert_eq!(strip_version_constraint("baz=1.0.0-1"), "baz");
        assert_eq!(strip_version_constraint("lib>=1.0<2.0"), "lib");
    }
}
