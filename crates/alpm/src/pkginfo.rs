//! `.PKGINFO` and `.BUILDINFO` readers for built packages. Both are
//! `key = value` files with repeated keys for arrays. Also a helper that
//! pulls them out of a package tarball without extracting the payload.

use std::collections::HashMap;
use std::io::Read;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyValues {
    pub fields: Vec<(String, String)>,
}

impl KeyValues {
    pub fn parse(text: &str) -> KeyValues {
        let mut fields = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                fields.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        KeyValues { fields }
    }

    pub fn first(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn all(&self, key: &str) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn to_map(&self) -> HashMap<String, Vec<String>> {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in &self.fields {
            m.entry(k.clone()).or_default().push(v.clone());
        }
        m
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PkgInfo {
    pub pkgname: String,
    pub pkgbase: String,
    pub pkgver: String,
    pub arch: String,
    pub pkgdesc: Option<String>,
    pub url: Option<String>,
    pub size: Option<u64>,
    pub packager: Option<String>,
    pub builddate: Option<i64>,
    pub depends: Vec<String>,
    pub makedepends: Vec<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
    pub replaces: Vec<String>,
    pub optdepends: Vec<String>,
    pub licenses: Vec<String>,
    pub backup: Vec<String>,
}

impl PkgInfo {
    pub fn parse(text: &str) -> Result<PkgInfo> {
        let kv = KeyValues::parse(text);
        let req = |k: &str| {
            kv.first(k)
                .map(str::to_string)
                .with_context(|| format!(".PKGINFO missing {k}"))
        };
        let pkgname = req("pkgname")?;
        Ok(PkgInfo {
            pkgbase: kv.first("pkgbase").map(str::to_string).unwrap_or_else(|| pkgname.clone()),
            pkgname,
            pkgver: req("pkgver")?,
            arch: req("arch")?,
            pkgdesc: kv.first("pkgdesc").map(str::to_string),
            url: kv.first("url").map(str::to_string),
            size: kv.first("size").and_then(|s| s.parse().ok()),
            packager: kv.first("packager").map(str::to_string),
            builddate: kv.first("builddate").and_then(|s| s.parse().ok()),
            depends: strs(kv.all("depend")),
            makedepends: strs(kv.all("makedepend")),
            provides: strs(kv.all("provides")),
            conflicts: strs(kv.all("conflict")),
            replaces: strs(kv.all("replaces")),
            optdepends: strs(kv.all("optdepend")),
            licenses: strs(kv.all("license")),
            backup: strs(kv.all("backup")),
        })
    }

    /// `name-version-arch.pkg.tar.<ext>` as makepkg names it.
    pub fn expected_filename(&self, ext: &str) -> String {
        format!("{}-{}-{}.pkg.tar.{}", self.pkgname, self.pkgver, self.arch, ext)
    }
}

fn strs(v: Vec<&str>) -> Vec<String> {
    v.into_iter().map(str::to_string).collect()
}

/// Metadata files from a package archive: `.PKGINFO`, `.BUILDINFO`, and
/// the list of payload paths (for the artifact diff). Stops reading the
/// payload bodies; only headers are walked.
#[derive(Debug, Clone, Default)]
pub struct PackageMeta {
    pub pkginfo: Option<PkgInfo>,
    pub buildinfo: Option<KeyValues>,
    pub install_script: Option<String>,
    pub paths: Vec<String>,
}

pub fn read_package_meta(bytes: &[u8]) -> Result<PackageMeta> {
    let reader: Box<dyn Read + '_> = if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Box::new(zstd::Decoder::new(bytes)?)
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z']) {
        anyhow::bail!("xz-compressed packages are not supported; use zstd");
    } else {
        Box::new(bytes)
    };
    let mut archive = tar::Archive::new(reader);
    let mut meta = PackageMeta::default();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        match path.as_str() {
            ".PKGINFO" => {
                let mut s = String::new();
                entry.read_to_string(&mut s)?;
                meta.pkginfo = Some(PkgInfo::parse(&s)?);
            }
            ".BUILDINFO" => {
                let mut s = String::new();
                entry.read_to_string(&mut s)?;
                meta.buildinfo = Some(KeyValues::parse(&s));
            }
            ".INSTALL" => {
                let mut s = String::new();
                entry.read_to_string(&mut s)?;
                meta.install_script = Some(s);
            }
            ".MTREE" | ".CHANGELOG" => {}
            p if !p.starts_with('.') => meta.paths.push(p.to_string()),
            _ => {}
        }
    }
    meta.pkginfo
        .as_ref()
        .context("package has no .PKGINFO")?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PKGINFO: &str = "# Generated by makepkg 7.1.0\npkgname = foo\npkgbase = foo\nxdata = pkgtype=pkg\npkgver = 1.2.3-2\npkgdesc = Foo\nurl = https://example.org\nbuilddate = 1700000000\npackager = Unknown Packager\nsize = 12345\narch = x86_64\nlicense = MIT\ndepend = glibc\ndepend = libbar.so=1-64\nprovides = foo-cli=1.2.3\nbackup = etc/foo.conf\n";

    #[test]
    fn parses_pkginfo() {
        let p = PkgInfo::parse(PKGINFO).unwrap();
        assert_eq!(p.pkgname, "foo");
        assert_eq!(p.pkgver, "1.2.3-2");
        assert_eq!(p.depends, ["glibc", "libbar.so=1-64"]);
        assert_eq!(p.size, Some(12345));
        assert_eq!(p.expected_filename("zst"), "foo-1.2.3-2-x86_64.pkg.tar.zst");
    }

    #[test]
    fn reads_meta_from_archive() {
        let mut tar_bytes = Vec::new();
        {
            let mut ar = tar::Builder::new(&mut tar_bytes);
            let add = |ar: &mut tar::Builder<&mut Vec<u8>>, path: &str, body: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                ar.append_data(&mut h, path, body).unwrap();
            };
            add(&mut ar, ".PKGINFO", PKGINFO.as_bytes());
            add(&mut ar, ".BUILDINFO", b"format = 2\ninstalled = glibc-2.44-1-x86_64\n");
            add(&mut ar, ".INSTALL", b"post_install() { :; }\n");
            add(&mut ar, "usr/bin/foo", b"\x7fELF....");
            add(&mut ar, "usr/lib/systemd/system/foo.service", b"[Unit]");
            ar.finish().unwrap();
        }
        let bytes = zstd::encode_all(&tar_bytes[..], 3).unwrap();
        let m = read_package_meta(&bytes).unwrap();
        assert_eq!(m.pkginfo.unwrap().pkgname, "foo");
        assert_eq!(m.buildinfo.unwrap().all("installed"), ["glibc-2.44-1-x86_64"]);
        assert!(m.install_script.unwrap().contains("post_install"));
        assert_eq!(m.paths, ["usr/bin/foo", "usr/lib/systemd/system/foo.service"]);
    }
}
