//! Readers for pacman sync databases: `<repo>.db` (package metadata) and
//! `<repo>.files` (file lists). Both are tar archives, gzip or zstd
//! compressed, with one directory per package holding `desc` and, for the
//! files database, `files`. Older databases split dependency fields into a
//! separate `depends` entry; both layouts are merged here.

use std::collections::HashMap;
use std::io::Read;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::depspec::DepSpec;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncPackage {
    pub name: String,
    pub version: String,
    pub base: Option<String>,
    pub desc: Option<String>,
    pub arch: Option<String>,
    pub filename: String,
    pub csize: Option<u64>,
    pub isize: Option<u64>,
    pub sha256sum: Option<String>,
    pub pgpsig: Option<String>,
    pub url: Option<String>,
    pub builddate: Option<i64>,
    pub packager: Option<String>,
    #[serde(default)]
    pub licenses: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub makedepends: Vec<String>,
    #[serde(default)]
    pub checkdepends: Vec<String>,
    #[serde(default)]
    pub optdepends: Vec<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub replaces: Vec<String>,
    /// Only populated from a `.files` database.
    #[serde(default)]
    pub files: Vec<String>,
}

/// Parse a `%KEY%` sectioned text file into key → values.
pub fn parse_sections(text: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('%') && line.ends_with('%') && line.len() >= 2 {
            let key = line[1..line.len() - 1].to_string();
            out.entry(key.clone()).or_default();
            current = Some(key);
        } else if line.is_empty() {
            // Blank lines separate sections but a section may legitimately
            // end without one; keep `current` so stray blanks are harmless.
        } else if let Some(k) = &current {
            out.get_mut(k).unwrap().push(line.to_string());
        }
    }
    out
}

fn take_one(m: &mut HashMap<String, Vec<String>>, key: &str) -> Option<String> {
    m.remove(key).and_then(|mut v| if v.is_empty() { None } else { Some(v.swap_remove(0)) })
}

fn take_all(m: &mut HashMap<String, Vec<String>>, key: &str) -> Vec<String> {
    m.remove(key).unwrap_or_default()
}

fn package_from_sections(mut m: HashMap<String, Vec<String>>) -> Result<SyncPackage> {
    let name = take_one(&mut m, "NAME").context("desc without %NAME%")?;
    let version = take_one(&mut m, "VERSION").with_context(|| format!("{name}: desc without %VERSION%"))?;
    Ok(SyncPackage {
        filename: take_one(&mut m, "FILENAME").unwrap_or_default(),
        name,
        version,
        base: take_one(&mut m, "BASE"),
        desc: take_one(&mut m, "DESC"),
        arch: take_one(&mut m, "ARCH"),
        csize: take_one(&mut m, "CSIZE").and_then(|s| s.parse().ok()),
        isize: take_one(&mut m, "ISIZE").and_then(|s| s.parse().ok()),
        sha256sum: take_one(&mut m, "SHA256SUM"),
        pgpsig: take_one(&mut m, "PGPSIG"),
        url: take_one(&mut m, "URL"),
        builddate: take_one(&mut m, "BUILDDATE").and_then(|s| s.parse().ok()),
        packager: take_one(&mut m, "PACKAGER"),
        licenses: take_all(&mut m, "LICENSE"),
        groups: take_all(&mut m, "GROUPS"),
        depends: take_all(&mut m, "DEPENDS"),
        makedepends: take_all(&mut m, "MAKEDEPENDS"),
        checkdepends: take_all(&mut m, "CHECKDEPENDS"),
        optdepends: take_all(&mut m, "OPTDEPENDS"),
        provides: take_all(&mut m, "PROVIDES"),
        conflicts: take_all(&mut m, "CONFLICTS"),
        replaces: take_all(&mut m, "REPLACES"),
        files: take_all(&mut m, "FILES"),
    })
}

/// Decompress by magic number: zstd, gzip, or raw tar.
fn decompress(bytes: &[u8]) -> Result<Box<dyn Read + '_>> {
    if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Ok(Box::new(zstd::Decoder::new(bytes)?))
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::read::GzDecoder::new(bytes)))
    } else {
        Ok(Box::new(bytes))
    }
}

/// Read every package in a `.db` or `.files` archive.
pub fn read_db(bytes: &[u8]) -> Result<Vec<SyncPackage>> {
    let reader = decompress(bytes)?;
    let mut archive = tar::Archive::new(reader);
    // dir name -> merged sections from desc / depends / files
    let mut per_dir: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if !matches!(file, "desc" | "depends" | "files") {
            continue;
        }
        let dir = path
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string();
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        let sections = parse_sections(&text);
        let slot = per_dir.entry(dir.clone()).or_insert_with(|| {
            order.push(dir.clone());
            HashMap::new()
        });
        for (k, v) in sections {
            slot.entry(k).or_default().extend(v);
        }
    }
    order
        .into_iter()
        .map(|dir| {
            let sections = per_dir.remove(&dir).unwrap();
            package_from_sections(sections).with_context(|| format!("entry {dir}"))
        })
        .collect()
}

/// Lookup structure over one or more repos of a stream.
#[derive(Debug, Default, Clone)]
pub struct SyncIndex {
    /// name -> (repo, package)
    by_name: HashMap<String, (String, SyncPackage)>,
    /// provided name -> list of (provider name, provided version)
    providers: HashMap<String, Vec<(String, Option<String>)>>,
    /// filename -> (repo, name)
    by_filename: HashMap<String, (String, String)>,
}

impl SyncIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a repo's packages. Repos added earlier take precedence for a
    /// duplicated name, matching pacman.conf ordering.
    pub fn add_repo(&mut self, repo: &str, packages: Vec<SyncPackage>) {
        for p in packages {
            if self.by_name.contains_key(&p.name) {
                continue;
            }
            for prov in &p.provides {
                let (pn, pv) = DepSpec::provide_parts(prov);
                self.providers
                    .entry(pn.to_string())
                    .or_default()
                    .push((p.name.clone(), pv.map(str::to_string)));
            }
            if !p.filename.is_empty() {
                self.by_filename
                    .insert(p.filename.clone(), (repo.to_string(), p.name.clone()));
            }
            self.by_name
                .insert(p.name.clone(), (repo.to_string(), p));
        }
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&SyncPackage> {
        self.by_name.get(name).map(|(_, p)| p)
    }

    pub fn repo_of(&self, name: &str) -> Option<&str> {
        self.by_name.get(name).map(|(r, _)| r.as_str())
    }

    pub fn by_filename(&self, filename: &str) -> Option<(&str, &SyncPackage)> {
        let (repo, name) = self.by_filename.get(filename)?;
        Some((repo.as_str(), self.get(name)?))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &SyncPackage)> {
        self.by_name.values().map(|(r, p)| (r.as_str(), p))
    }

    /// Packages satisfying a dependency spec: the package of that name if
    /// its version fits, then any provider whose provide fits. The direct
    /// name match comes first, as alpm prefers it.
    pub fn satisfiers(&self, dep: &DepSpec) -> Vec<&SyncPackage> {
        let mut out = Vec::new();
        if let Some(p) = self.get(&dep.name) {
            if dep.satisfied_by(&p.name, Some(&p.version)) {
                out.push(p);
            }
        }
        if let Some(provs) = self.providers.get(&dep.name) {
            for (provider, ver) in provs {
                if dep.satisfied_by(&dep.name, ver.as_deref()) {
                    if let Some(p) = self.get(provider) {
                        if !out.iter().any(|q| q.name == p.name) {
                            out.push(p);
                        }
                    }
                }
            }
        }
        out
    }

    pub fn satisfies(&self, dep: &DepSpec) -> bool {
        !self.satisfiers(dep).is_empty()
    }

    pub fn filenames(&self) -> impl Iterator<Item = &str> {
        self.by_filename.keys().map(String::as_str)
    }
}

#[cfg(test)]
pub mod testutil {
    //! Build small sync databases in memory for tests.
    use super::*;
    use std::io::Write;

    pub fn desc(p: &SyncPackage) -> String {
        let mut s = String::new();
        let one = |s: &mut String, k: &str, v: &str| {
            s.push_str(&format!("%{k}%\n{v}\n\n"));
        };
        let many = |s: &mut String, k: &str, vs: &[String]| {
            if !vs.is_empty() {
                s.push_str(&format!("%{k}%\n"));
                for v in vs {
                    s.push_str(v);
                    s.push('\n');
                }
                s.push('\n');
            }
        };
        one(&mut s, "FILENAME", &p.filename);
        one(&mut s, "NAME", &p.name);
        if let Some(b) = &p.base {
            one(&mut s, "BASE", b);
        }
        one(&mut s, "VERSION", &p.version);
        if let Some(d) = &p.desc {
            one(&mut s, "DESC", d);
        }
        if let Some(a) = &p.arch {
            one(&mut s, "ARCH", a);
        }
        if let Some(c) = p.csize {
            one(&mut s, "CSIZE", &c.to_string());
        }
        if let Some(c) = p.isize {
            one(&mut s, "ISIZE", &c.to_string());
        }
        if let Some(h) = &p.sha256sum {
            one(&mut s, "SHA256SUM", h);
        }
        many(&mut s, "LICENSE", &p.licenses);
        many(&mut s, "DEPENDS", &p.depends);
        many(&mut s, "MAKEDEPENDS", &p.makedepends);
        many(&mut s, "PROVIDES", &p.provides);
        many(&mut s, "CONFLICTS", &p.conflicts);
        s
    }

    pub fn build_db(packages: &[SyncPackage], zstd_compress: bool) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut ar = tar::Builder::new(&mut tar_bytes);
            for p in packages {
                let dir = format!("{}-{}", p.name, p.version);
                let body = desc(p);
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                ar.append_data(&mut h, format!("{dir}/desc"), body.as_bytes())
                    .unwrap();
                if !p.files.is_empty() {
                    let mut body = String::from("%FILES%\n");
                    for f in &p.files {
                        body.push_str(f);
                        body.push('\n');
                    }
                    let mut h = tar::Header::new_gnu();
                    h.set_size(body.len() as u64);
                    h.set_mode(0o644);
                    h.set_cksum();
                    ar.append_data(&mut h, format!("{dir}/files"), body.as_bytes())
                        .unwrap();
                }
            }
            ar.finish().unwrap();
        }
        if zstd_compress {
            zstd::encode_all(&tar_bytes[..], 3).unwrap()
        } else {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(&tar_bytes).unwrap();
            enc.finish().unwrap()
        }
    }

    pub fn pkg(name: &str, version: &str) -> SyncPackage {
        SyncPackage {
            name: name.into(),
            version: version.into(),
            filename: format!("{name}-{version}-x86_64.pkg.tar.zst"),
            arch: Some("x86_64".into()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn parses_sections_with_multi_values() {
        let m = parse_sections("%NAME%\nfoo\n\n%DEPENDS%\na\nb>=1\n\n%EMPTY%\n\n");
        assert_eq!(m["NAME"], vec!["foo"]);
        assert_eq!(m["DEPENDS"], vec!["a", "b>=1"]);
        assert!(m["EMPTY"].is_empty());
    }

    #[test]
    fn reads_zstd_and_gzip_databases() {
        let mut glibc = pkg("glibc", "2.44-1");
        glibc.provides = vec!["libc.so=6-64".into()];
        let mut py = pkg("python", "3.13.7-1");
        py.depends = vec!["glibc".into()];
        py.provides = vec!["python3".into()];
        py.files = vec!["usr/bin/python".into()];
        for z in [true, false] {
            let bytes = build_db(&[glibc.clone(), py.clone()], z);
            let pkgs = read_db(&bytes).unwrap();
            assert_eq!(pkgs.len(), 2);
            assert_eq!(pkgs[0].name, "glibc");
            assert_eq!(pkgs[1].depends, vec!["glibc"]);
            assert_eq!(pkgs[1].files, vec!["usr/bin/python"]);
        }
    }

    #[test]
    fn index_resolves_names_versions_and_provides() {
        let mut glibc = pkg("glibc", "2.44-1");
        glibc.provides = vec!["libc.so=6-64".into()];
        let mut py = pkg("python", "3.13.7-1");
        py.provides = vec!["python3".into(), "python-foo=1.0".into()];
        let mut idx = SyncIndex::new();
        idx.add_repo("core", vec![glibc]);
        idx.add_repo("extra", vec![py]);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.repo_of("python"), Some("extra"));
        assert!(idx.satisfies(&DepSpec::parse("python>=3.12")));
        assert!(!idx.satisfies(&DepSpec::parse("python>=3.14")));
        assert!(idx.satisfies(&DepSpec::parse("python3")));
        assert!(idx.satisfies(&DepSpec::parse("libc.so=6-64")));
        assert!(!idx.satisfies(&DepSpec::parse("libc.so=7-64")));
        assert!(idx.satisfies(&DepSpec::parse("python-foo>=0.9")));
        assert!(!idx.satisfies(&DepSpec::parse("python-foo>=2")));
        assert!(!idx.satisfies(&DepSpec::parse("rust")));
        let (repo, p) = idx.by_filename("glibc-2.44-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!((repo, p.name.as_str()), ("core", "glibc"));
    }

    #[test]
    fn first_repo_wins_on_duplicate_names() {
        let mut idx = SyncIndex::new();
        idx.add_repo("core", vec![pkg("foo", "2-1")]);
        idx.add_repo("extra", vec![pkg("foo", "9-1")]);
        assert_eq!(idx.get("foo").unwrap().version, "2-1");
        assert_eq!(idx.repo_of("foo"), Some("core"));
    }
}
