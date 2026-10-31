//! `.SRCINFO` parser. One `pkgbase` section followed by one `pkgname`
//! section per split package. Package sections list only their overrides;
//! an empty value (`depends =`) clears the inherited array. Architecture
//! suffixed keys (`depends_x86_64`) are kept separately and merged by
//! `effective` for a requested architecture.

use std::collections::HashMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::depspec::DepSpec;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Section {
    /// Key → values in file order. Keys keep any `_arch` suffix.
    pub fields: Vec<(String, String)>,
}

impl Section {
    pub fn values(&self, key: &str) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
            .collect()
    }

    pub fn first(&self, key: &str) -> Option<&str> {
        self.values(key).into_iter().next()
    }

    pub fn has_key(&self, key: &str) -> bool {
        self.fields.iter().any(|(k, _)| k == key)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Srcinfo {
    pub pkgbase: String,
    pub base: Section,
    pub packages: Vec<(String, Section)>,
}

const ARRAY_KEYS: &[&str] = &[
    "arch",
    "license",
    "groups",
    "depends",
    "makedepends",
    "checkdepends",
    "optdepends",
    "provides",
    "conflicts",
    "replaces",
    "backup",
    "options",
    "source",
    "noextract",
    "validpgpkeys",
    "md5sums",
    "sha1sums",
    "sha224sums",
    "sha256sums",
    "sha384sums",
    "sha512sums",
    "b2sums",
];

impl Srcinfo {
    pub fn parse(text: &str) -> Result<Srcinfo> {
        let mut out = Srcinfo::default();
        let mut current: Option<Section> = None;
        let mut current_name: Option<String> = None;
        let mut in_base = false;

        let flush = |out: &mut Srcinfo, in_base: bool, name: Option<String>, sec: Option<Section>| {
            if let Some(sec) = sec {
                if in_base {
                    out.base = sec;
                } else if let Some(n) = name {
                    out.packages.push((n, sec));
                }
            }
        };

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bail!("malformed SRCINFO line: {raw:?}");
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "pkgbase" => {
                    flush(&mut out, in_base, current_name.take(), current.take());
                    out.pkgbase = value.to_string();
                    in_base = true;
                    current = Some(Section::default());
                }
                "pkgname" => {
                    flush(&mut out, in_base, current_name.take(), current.take());
                    in_base = false;
                    current_name = Some(value.to_string());
                    current = Some(Section::default());
                }
                _ => {
                    let Some(sec) = current.as_mut() else {
                        bail!("SRCINFO field before any pkgbase/pkgname: {raw:?}");
                    };
                    sec.fields.push((key.to_string(), value.to_string()));
                }
            }
        }
        flush(&mut out, in_base, current_name.take(), current.take());
        if out.pkgbase.is_empty() {
            bail!("SRCINFO has no pkgbase");
        }
        if out.packages.is_empty() {
            bail!("SRCINFO has no pkgname");
        }
        Ok(out)
    }

    pub fn pkgnames(&self) -> Vec<&str> {
        self.packages.iter().map(|(n, _)| n.as_str()).collect()
    }

    pub fn pkgver(&self) -> &str {
        self.base.first("pkgver").unwrap_or("")
    }

    pub fn pkgrel(&self) -> &str {
        self.base.first("pkgrel").unwrap_or("")
    }

    pub fn epoch(&self) -> Option<&str> {
        self.base.first("epoch")
    }

    /// Full version as pacman prints it: `[epoch:]pkgver-pkgrel`.
    pub fn version(&self) -> String {
        match self.epoch() {
            Some(e) if e != "0" => format!("{e}:{}-{}", self.pkgver(), self.pkgrel()),
            _ => format!("{}-{}", self.pkgver(), self.pkgrel()),
        }
    }

    pub fn is_vcs(&self) -> bool {
        self.pkgbase.ends_with("-git")
            || self.pkgbase.ends_with("-hg")
            || self.pkgbase.ends_with("-svn")
            || self.pkgbase.ends_with("-bzr")
            || self
                .base
                .values("source")
                .iter()
                .any(|s| s.starts_with("git+") || s.contains("::git+") || s.starts_with("git://"))
    }

    /// Effective values of an array key for one package on one arch:
    /// package override if present (an empty override clears), else base;
    /// plus the `_arch` variants from the same section.
    pub fn effective(&self, pkgname: &str, key: &str, arch: &str) -> Vec<String> {
        let pkg = self
            .packages
            .iter()
            .find(|(n, _)| n == pkgname)
            .map(|(_, s)| s);
        let arch_key = format!("{key}_{arch}");
        let from = |sec: &Section| -> Vec<String> {
            let mut v: Vec<String> = sec.values(key).into_iter().map(str::to_string).collect();
            v.extend(sec.values(&arch_key).into_iter().map(str::to_string));
            v
        };
        match pkg {
            Some(sec) if sec.has_key(key) || sec.has_key(&arch_key) => from(sec),
            _ => from(&self.base),
        }
    }

    pub fn effective_first(&self, pkgname: &str, key: &str, arch: &str) -> Option<String> {
        self.effective(pkgname, key, arch).into_iter().next()
    }

    /// Union of depends, makedepends, and checkdepends over all packages,
    /// as the build root needs them. Deduplicated by spec text.
    pub fn build_deps(&self, arch: &str) -> Vec<DepSpec> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (name, _) in &self.packages {
            for key in ["depends", "makedepends", "checkdepends"] {
                for d in self.effective(name, key, arch) {
                    if seen.insert(d.clone()) {
                        out.push(DepSpec::parse(&d));
                    }
                }
            }
        }
        out
    }

    /// Runtime depends per package.
    pub fn runtime_deps(&self, pkgname: &str, arch: &str) -> Vec<DepSpec> {
        self.effective(pkgname, "depends", arch)
            .iter()
            .map(|d| DepSpec::parse(d))
            .collect()
    }

    /// Everything the pkgbase provides, by package: own names plus
    /// `provides` entries. Used by the planner to resolve AUR dependencies.
    pub fn provided_names(&self, arch: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (name, _) in &self.packages {
            out.push(name.clone());
            for p in self.effective(name, "provides", arch) {
                out.push(DepSpec::provide_parts(&p).0.to_string());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn architectures(&self) -> Vec<&str> {
        self.base.values("arch")
    }

    /// Field-by-field diff for the gate: every (section, key) whose value
    /// list differs between two SRCINFOs.
    pub fn diff<'a>(&'a self, other: &'a Srcinfo) -> Vec<FieldChange> {
        let mut changes = Vec::new();
        let mut sections: Vec<(String, Option<&Section>, Option<&Section>)> = Vec::new();
        sections.push(("pkgbase".into(), Some(&self.base), Some(&other.base)));
        let mut names: Vec<&str> = self.pkgnames();
        for n in other.pkgnames() {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        for n in names {
            let a = self.packages.iter().find(|(x, _)| x == n).map(|(_, s)| s);
            let b = other.packages.iter().find(|(x, _)| x == n).map(|(_, s)| s);
            sections.push((n.to_string(), a, b));
        }
        for (section, a, b) in sections {
            let mut keys: Vec<&str> = Vec::new();
            for s in [a, b].into_iter().flatten() {
                for (k, _) in &s.fields {
                    if !keys.contains(&k.as_str()) {
                        keys.push(k);
                    }
                }
            }
            for key in keys {
                let va: Vec<&str> = a.map(|s| s.values(key)).unwrap_or_default();
                let vb: Vec<&str> = b.map(|s| s.values(key)).unwrap_or_default();
                if va != vb {
                    changes.push(FieldChange {
                        section: section.clone(),
                        key: key.to_string(),
                        before: va.into_iter().map(str::to_string).collect(),
                        after: vb.into_iter().map(str::to_string).collect(),
                    });
                }
            }
        }
        changes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldChange {
    pub section: String,
    pub key: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

impl FieldChange {
    pub fn is_array_key(&self) -> bool {
        let base = self.key.split('_').next().unwrap_or(&self.key);
        ARRAY_KEYS.contains(&base)
    }
}

/// Convenience: index of pkgname → Srcinfo for a set of package bases.
pub fn index_by_pkgname<'a>(infos: impl IntoIterator<Item = &'a Srcinfo>) -> HashMap<String, &'a Srcinfo> {
    let mut m = HashMap::new();
    for s in infos {
        for (n, _) in &s.packages {
            m.insert(n.clone(), s);
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
pkgbase = foo
	pkgdesc = Foo tool
	pkgver = 1.2.3
	pkgrel = 2
	epoch = 1
	url = https://example.org/foo
	arch = x86_64
	arch = aarch64
	license = MIT
	makedepends = cmake
	makedepends = git
	depends = glibc
	depends = libbar.so
	depends_x86_64 = nasm
	source = https://example.org/foo-1.2.3.tar.gz
	source = foo.patch
	sha256sums = aaaa
	sha256sums = bbbb
	validpgpkeys = ABCDEF

pkgname = foo
	provides = foo-cli=1.2.3

pkgname = foo-docs
	pkgdesc = Foo docs
	arch = any
	depends =
	optdepends = foo: the tool itself
"#;

    #[test]
    fn parses_base_and_split_packages() {
        let s = Srcinfo::parse(SAMPLE).unwrap();
        assert_eq!(s.pkgbase, "foo");
        assert_eq!(s.pkgnames(), ["foo", "foo-docs"]);
        assert_eq!(s.version(), "1:1.2.3-2");
        assert_eq!(s.architectures(), ["x86_64", "aarch64"]);
        assert_eq!(s.base.values("source").len(), 2);
    }

    #[test]
    fn effective_inherits_overrides_and_clears() {
        let s = Srcinfo::parse(SAMPLE).unwrap();
        assert_eq!(
            s.effective("foo", "depends", "x86_64"),
            ["glibc", "libbar.so", "nasm"]
        );
        assert_eq!(s.effective("foo", "depends", "aarch64"), ["glibc", "libbar.so"]);
        assert!(s.effective("foo-docs", "depends", "x86_64").is_empty(), "empty override clears");
        assert_eq!(s.effective("foo-docs", "optdepends", "x86_64"), ["foo: the tool itself"]);
        assert_eq!(s.effective_first("foo-docs", "pkgdesc", "x86_64").as_deref(), Some("Foo docs"));
        assert_eq!(s.effective_first("foo", "pkgdesc", "x86_64").as_deref(), Some("Foo tool"));
    }

    #[test]
    fn build_deps_union_and_provides() {
        let s = Srcinfo::parse(SAMPLE).unwrap();
        let deps: Vec<String> = s.build_deps("x86_64").iter().map(|d| d.to_string()).collect();
        assert_eq!(deps, ["glibc", "libbar.so", "nasm", "cmake", "git"]);
        assert_eq!(s.provided_names("x86_64"), ["foo", "foo-cli", "foo-docs"]);
        assert!(!s.is_vcs());
        let g = Srcinfo::parse("pkgbase = x-git\n\tpkgver = r1\n\tpkgrel = 1\n\tsource = git+https://x\npkgname = x-git\n").unwrap();
        assert!(g.is_vcs());
    }

    #[test]
    fn diff_reports_changed_fields_only() {
        let a = Srcinfo::parse(SAMPLE).unwrap();
        let b_text = SAMPLE
            .replace("pkgver = 1.2.3", "pkgver = 1.2.4")
            .replace("foo-1.2.3.tar.gz", "foo-1.2.4.tar.gz")
            .replace("sha256sums = aaaa", "sha256sums = cccc")
            .replace("\tprovides = foo-cli=1.2.3", "\tprovides = foo-cli=1.2.4\n\tinstall = foo.install");
        let b = Srcinfo::parse(&b_text).unwrap();
        let changes = a.diff(&b);
        let keys: Vec<(String, String)> = changes.iter().map(|c| (c.section.clone(), c.key.clone())).collect();
        assert_eq!(
            keys,
            [
                ("pkgbase".to_string(), "pkgver".to_string()),
                ("pkgbase".to_string(), "source".to_string()),
                ("pkgbase".to_string(), "sha256sums".to_string()),
                ("foo".to_string(), "provides".to_string()),
                ("foo".to_string(), "install".to_string()),
            ]
        );
        let install = changes.iter().find(|c| c.key == "install").unwrap();
        assert!(install.before.is_empty());
        assert_eq!(install.after, ["foo.install"]);
        assert!(!install.is_array_key());
        assert!(changes.iter().find(|c| c.key == "source").unwrap().is_array_key());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Srcinfo::parse("").is_err());
        assert!(Srcinfo::parse("pkgbase = x\n").is_err());
        assert!(Srcinfo::parse("depends = x\n").is_err());
    }
}
