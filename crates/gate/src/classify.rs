//! Diff classification: what kind of change is this, field by field, and
//! what does each observation contribute to the floor risk.

use std::collections::BTreeSet;

use repoman_alpm::depspec::DepSpec;
use repoman_alpm::srcinfo::{FieldChange, Srcinfo};
use repoman_alpm::version::vercmp;
use serde::{Deserialize, Serialize};

use crate::{GateInput, Risk, pkgbuild};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffClass {
    Routine,
    Metadata,
    Structural,
}

impl DiffClass {
    pub fn as_str(self) -> &'static str {
        match self {
            DiffClass::Routine => "routine",
            DiffClass::Metadata => "metadata",
            DiffClass::Structural => "structural",
        }
    }
}

/// One observation about the change. `class` is what this observation alone
/// would make the change; the candidate's class is the maximum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub kind: String,
    pub detail: String,
    pub class: DiffClass,
    pub risk: Risk,
}

fn reason(kind: &str, detail: impl Into<String>, class: DiffClass, risk: Risk) -> Reason {
    Reason {
        kind: kind.to_string(),
        detail: detail.into(),
        class,
        risk,
    }
}

const CHECKSUM_KEYS: &[&str] = &["md5sums", "sha1sums", "sha224sums", "sha256sums", "sha384sums", "sha512sums", "b2sums"];
const METADATA_KEYS: &[&str] = &["pkgdesc", "license", "optdepends", "groups", "provides", "conflicts", "replaces", "changelog"];
const DEP_KEYS: &[&str] = &["depends", "makedepends", "checkdepends"];

/// Classify just the SRCINFO field changes. Exposed for tests and for the
/// CLI's candidate summary; `classify_change` is the full version.
pub fn classify(old: &Srcinfo, new: &Srcinfo, is_official: &dyn Fn(&str) -> bool) -> Vec<Reason> {
    let mut out = Vec::new();
    let old_names: BTreeSet<&str> = old.pkgnames().into_iter().collect();
    let new_names: BTreeSet<&str> = new.pkgnames().into_iter().collect();
    if old_names != new_names {
        let added: Vec<&str> = new_names.difference(&old_names).copied().collect();
        let removed: Vec<&str> = old_names.difference(&new_names).copied().collect();
        out.push(reason(
            "split_packages_changed",
            format!("added {added:?}, removed {removed:?}"),
            DiffClass::Structural,
            Risk::Medium,
        ));
    }
    let mut version_done = false;
    for ch in old.diff(new) {
        // Whole sections that appeared or vanished are covered above.
        if ch.section != "pkgbase" && !(old_names.contains(ch.section.as_str()) && new_names.contains(ch.section.as_str())) {
            continue;
        }
        let base = ch.key.split('_').next().unwrap_or(&ch.key);
        match base {
            "pkgver" | "pkgrel" | "epoch" => {
                if version_done {
                    continue;
                }
                version_done = true;
                let (a, b) = (old.version(), new.version());
                match vercmp(&b, &a) {
                    std::cmp::Ordering::Less => out.push(reason(
                        "version_backwards",
                        format!("{a} -> {b}"),
                        DiffClass::Structural,
                        Risk::High,
                    )),
                    _ => out.push(reason("version_bump", format!("{a} -> {b}"), DiffClass::Routine, Risk::Low)),
                }
            }
            k if CHECKSUM_KEYS.contains(&k) => {
                let had_skip = ch.before.iter().any(|v| v == "SKIP");
                let has_skip = ch.after.iter().any(|v| v == "SKIP");
                if has_skip && !had_skip {
                    out.push(reason(
                        "checksum_skip",
                        format!("{} now contains SKIP", ch.key),
                        DiffClass::Structural,
                        Risk::Medium,
                    ));
                } else {
                    out.push(reason("checksums", ch.key.clone(), DiffClass::Routine, Risk::Low));
                }
            }
            "source" => out.push(classify_source(&ch, old, new)),
            "url" => {
                let (ha, hb) = (hosts(&ch.before), hosts(&ch.after));
                if ha != hb {
                    out.push(reason(
                        "url_host_changed",
                        format!("{:?} -> {:?}", ch.before, ch.after),
                        DiffClass::Metadata,
                        Risk::Medium,
                    ));
                } else {
                    out.push(reason("metadata", ch.key.clone(), DiffClass::Metadata, Risk::Low));
                }
            }
            k if METADATA_KEYS.contains(&k) => out.push(reason("metadata", ch.key.clone(), DiffClass::Metadata, Risk::Low)),
            k if DEP_KEYS.contains(&k) => {
                let before: BTreeSet<String> = ch.before.iter().map(|d| DepSpec::parse(d).name).collect();
                let added: Vec<String> = ch
                    .after
                    .iter()
                    .map(|d| DepSpec::parse(d).name)
                    .filter(|n| !before.contains(n))
                    .collect();
                let unofficial: Vec<&String> = added.iter().filter(|n| !is_official(n)).collect();
                if unofficial.is_empty() {
                    out.push(reason(
                        "dependency_change",
                        format!("{}: added {added:?}", ch.key),
                        DiffClass::Metadata,
                        Risk::Low,
                    ));
                } else {
                    out.push(reason(
                        "new_aur_dependency",
                        format!("{}: {unofficial:?} not in the official repos", ch.key),
                        DiffClass::Structural,
                        Risk::Medium,
                    ));
                }
            }
            "install" => out.push(reason(
                "install_script_reference",
                format!("{:?} -> {:?}", ch.before, ch.after),
                DiffClass::Structural,
                Risk::High,
            )),
            "validpgpkeys" => out.push(reason(
                "signing_keys_changed",
                format!("{:?} -> {:?}", ch.before, ch.after),
                DiffClass::Structural,
                Risk::Medium,
            )),
            "arch" => out.push(reason(
                "arch_changed",
                format!("{:?} -> {:?}", ch.before, ch.after),
                DiffClass::Structural,
                Risk::Medium,
            )),
            _ => out.push(reason(
                "field_changed",
                format!("{}.{}: {:?} -> {:?}", ch.section, ch.key, ch.before, ch.after),
                DiffClass::Structural,
                Risk::Medium,
            )),
        }
    }
    out
}

/// Replace every spelling of the version in a source entry with a marker so
/// a bump that only substitutes versions compares equal.
fn template(entry: &str, version: &str) -> String {
    let mut s = entry.to_string();
    if version.is_empty() {
        return s;
    }
    let mut forms = vec![version.to_string()];
    if version.contains('.') {
        forms.push(version.replace('.', "_"));
        forms.push(version.replace('.', "-"));
    }
    // Longest first so "1.2.3" is replaced before a "1.2" prefix could be.
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    for f in forms {
        s = s.replace(&f, "@VER@");
    }
    s
}

fn url_of(entry: &str) -> &str {
    let e = entry.split_once("::").map(|(_, u)| u).unwrap_or(entry);
    e.split_once('#').map(|(u, _)| u).unwrap_or(e)
}

/// `host/owner` of each remote entry; local files contribute nothing.
fn hosts(entries: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for e in entries {
        let url = url_of(e);
        let Some((_, rest)) = url.split_once("://") else {
            continue;
        };
        let mut parts = rest.split('/');
        let host = parts.next().unwrap_or("").to_ascii_lowercase();
        let owner = parts.next().unwrap_or("").to_ascii_lowercase();
        out.insert(format!("{host}/{owner}"));
    }
    out
}

fn classify_source(ch: &FieldChange, old: &Srcinfo, new: &Srcinfo) -> Reason {
    let a: Vec<String> = ch.before.iter().map(|e| template(e, old.pkgver())).collect();
    let b: Vec<String> = ch.after.iter().map(|e| template(e, new.pkgver())).collect();
    if a == b {
        return reason("source_version_substitution", ch.key.clone(), DiffClass::Routine, Risk::Low);
    }
    let (ha, hb) = (hosts(&ch.before), hosts(&ch.after));
    let new_hosts: Vec<&String> = hb.difference(&ha).collect();
    if !new_hosts.is_empty() {
        return reason(
            "source_host_changed",
            format!("new source hosts {new_hosts:?}"),
            DiffClass::Structural,
            Risk::High,
        );
    }
    let added: Vec<&String> = ch.after.iter().filter(|e| !ch.before.contains(e)).collect();
    let removed: Vec<&String> = ch.before.iter().filter(|e| !ch.after.contains(e)).collect();
    reason(
        "source_changed",
        format!("{}: added {added:?}, removed {removed:?}", ch.key),
        DiffClass::Structural,
        Risk::Medium,
    )
}

fn is_script_name(name: &str) -> bool {
    name.ends_with(".install") || name.ends_with(".hook") || name.ends_with(".sh") || name.ends_with(".py") || name.ends_with(".pl")
}

/// The full classification: SRCINFO fields, PKGBUILD functions, repository
/// files, install scripts, maintainer, and SRCINFO regeneration.
pub fn classify_change(input: &GateInput<'_>) -> Vec<Reason> {
    let mut out = classify(input.old, input.new, input.is_official);

    if let Some(old_pb) = input.old_pkgbuild {
        let (fa, fb) = (pkgbuild::functions(old_pb), pkgbuild::functions(input.new_pkgbuild));
        let names: BTreeSet<&String> = fa.keys().chain(fb.keys()).collect();
        for name in names {
            match (fa.get(name), fb.get(name)) {
                (Some(a), Some(b)) if a == b => {}
                (Some(_), Some(_)) => out.push(reason("function_changed", format!("{name}()"), DiffClass::Structural, Risk::Medium)),
                (None, Some(_)) => out.push(reason("function_added", format!("{name}()"), DiffClass::Structural, Risk::Medium)),
                (Some(_), None) => out.push(reason("function_removed", format!("{name}()"), DiffClass::Structural, Risk::Medium)),
                (None, None) => unreachable!(),
            }
        }
    }

    let old_files: BTreeSet<&str> = input.old_files.iter().map(String::as_str).collect();
    let new_files: BTreeSet<&str> = input.new_files.iter().map(String::as_str).collect();
    for f in new_files.difference(&old_files) {
        if *f == ".SRCINFO" || *f == "PKGBUILD" || f.starts_with(".git") {
            continue;
        }
        let risk = if is_script_name(f) { Risk::High } else { Risk::Medium };
        out.push(reason("new_file", *f, DiffClass::Structural, risk));
    }
    for f in old_files.difference(&new_files) {
        if f.starts_with(".git") {
            continue;
        }
        out.push(reason("file_removed", *f, DiffClass::Metadata, Risk::Low));
    }

    for (name, text) in input.new_scripts {
        match input.old_scripts.iter().find(|(n, _)| n == name) {
            Some((_, old)) if old == text => {}
            Some(_) => out.push(reason("install_script_changed", name.clone(), DiffClass::Structural, Risk::High)),
            None => out.push(reason("install_script_added", name.clone(), DiffClass::Structural, Risk::High)),
        }
    }

    if input.maintainer_changed {
        out.push(reason(
            "maintainer_changed",
            "maintainer or co-maintainers differ from the baseline",
            DiffClass::Structural,
            Risk::Medium,
        ));
    }
    if input.srcinfo_mismatch == Some(true) {
        out.push(reason(
            "srcinfo_mismatch",
            "regenerated .SRCINFO differs from the committed one",
            DiffClass::Structural,
            Risk::High,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templating_catches_version_spellings() {
        assert_eq!(template("https://x/foo-1.2.3.tar.gz", "1.2.3"), "https://x/foo-@VER@.tar.gz");
        assert_eq!(template("foo-1_2_3.zip", "1.2.3"), "foo-@VER@.zip");
        assert_eq!(template("https://x/v1.2.3/foo.tar.gz", "1.2.3"), "https://x/v@VER@/foo.tar.gz");
        assert_eq!(template("foo.patch", "1.2.3"), "foo.patch");
    }

    #[test]
    fn hosts_ignore_fragments_and_local_files() {
        let h = hosts(&["foo.patch".into(), "x::git+https://GitHub.com/Owner/repo.git#tag=v1".into()]);
        assert_eq!(h.into_iter().collect::<Vec<_>>(), ["github.com/owner"]);
    }
}
