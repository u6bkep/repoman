//! Dependency planning for one stream: resolve every build dependency of the
//! member packages against the stream's sync index and the AUR packages we
//! know, order builds topologically, and report what is still unknown.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use repoman_alpm::DepSpec;
use repoman_alpm::srcinfo::Srcinfo;
use repoman_alpm::syncdb::SyncIndex;
use serde::Serialize;

/// Inputs for one pkgbase on the stream being planned.
pub struct Input<'a> {
    pub pkgbase: &'a str,
    pub srcinfo: &'a Srcinfo,
    /// Pinned provider choices from package policy: dep name → pkgname.
    pub provider_choices: &'a HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PlannedBuild {
    pub pkgbase: String,
    /// Official and AUR package names to install into the build root.
    pub install: Vec<String>,
    /// AUR package bases this build depends on (direct).
    pub aur_deps: Vec<String>,
    /// Provider picks that were not pinned by policy: (dep, chosen, alternatives).
    pub ambiguous_providers: Vec<(String, String, Vec<String>)>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Unresolved {
    pub pkgbase: String,
    pub dep: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Plan {
    /// Builds in dependency order.
    pub builds: Vec<PlannedBuild>,
    /// Dependencies satisfied by neither the stream nor any known AUR
    /// package. The caller looks these up on the AUR.
    pub unresolved: Vec<Unresolved>,
    /// Packages that could not be ordered because of a dependency cycle.
    pub cycles: Vec<String>,
}

/// `known_aur` maps a provided name (pkgname or provides entry) to the
/// pkgbase that provides it, for every AUR package the coordinator knows,
/// member of this stream or not. Members are planned; non-member providers
/// show up in `aur_deps` and the caller marks them required.
pub fn plan(stream_index: &SyncIndex, arch: &str, members: &[Input<'_>], known_aur: &HashMap<String, String>) -> Plan {
    let mut plan = Plan::default();
    let member_set: BTreeSet<&str> = members.iter().map(|m| m.pkgbase).collect();
    let mut builds: BTreeMap<String, PlannedBuild> = BTreeMap::new();

    for m in members {
        let mut install: BTreeSet<String> = BTreeSet::new();
        let mut aur_deps: BTreeSet<String> = BTreeSet::new();
        let mut ambiguous = Vec::new();
        for dep in m.srcinfo.build_deps(arch) {
            match resolve(stream_index, &dep, m.provider_choices, known_aur) {
                Resolution::Official { chosen, alternatives } => {
                    if !alternatives.is_empty() {
                        ambiguous.push((dep.to_string(), chosen.clone(), alternatives));
                    }
                    install.insert(chosen);
                }
                Resolution::Aur { pkgbase, pkgname } => {
                    if pkgbase != m.pkgbase {
                        aur_deps.insert(pkgbase);
                    }
                    install.insert(pkgname);
                }
                Resolution::Unknown => plan.unresolved.push(Unresolved {
                    pkgbase: m.pkgbase.to_string(),
                    dep: dep.to_string(),
                }),
            }
        }
        builds.insert(
            m.pkgbase.to_string(),
            PlannedBuild {
                pkgbase: m.pkgbase.to_string(),
                install: install.into_iter().collect(),
                aur_deps: aur_deps.into_iter().collect(),
                ambiguous_providers: ambiguous,
            },
        );
    }

    // Topological order over member-to-member edges; non-member AUR deps are
    // assumed already published and impose no ordering here.
    let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, b) in &builds {
        indegree.entry(name.as_str()).or_insert(0);
        for d in &b.aur_deps {
            if member_set.contains(d.as_str()) {
                *indegree.entry(name.as_str()).or_insert(0) += 1;
                dependents.entry(d.as_str()).or_default().push(name.as_str());
            }
        }
    }
    let mut ready: Vec<&str> = indegree.iter().filter(|(_, d)| **d == 0).map(|(n, _)| *n).collect();
    ready.sort();
    let mut order: Vec<String> = Vec::new();
    while let Some(n) = ready.pop() {
        order.push(n.to_string());
        if let Some(ds) = dependents.get(n) {
            for d in ds {
                let e = indegree.get_mut(d).unwrap();
                *e -= 1;
                if *e == 0 {
                    ready.push(d);
                    ready.sort_by(|a, b| b.cmp(a)); // pop() takes the smallest name
                }
            }
        }
    }
    for (n, d) in &indegree {
        if *d > 0 {
            plan.cycles.push(n.to_string());
        }
    }
    plan.builds = order.into_iter().filter_map(|n| builds.remove(&n)).collect();
    plan
}

enum Resolution {
    Official { chosen: String, alternatives: Vec<String> },
    Aur { pkgbase: String, pkgname: String },
    Unknown,
}

fn resolve(index: &SyncIndex, dep: &DepSpec, choices: &HashMap<String, String>, known_aur: &HashMap<String, String>) -> Resolution {
    let sat = index.satisfiers(dep);
    if !sat.is_empty() {
        // A direct name match is preferred, as alpm does.
        if let Some(direct) = sat.iter().find(|p| p.name == dep.name) {
            return Resolution::Official {
                chosen: direct.name.clone(),
                alternatives: vec![],
            };
        }
        if let Some(pick) = choices.get(&dep.name) {
            if let Some(p) = sat.iter().find(|p| &p.name == pick) {
                return Resolution::Official {
                    chosen: p.name.clone(),
                    alternatives: vec![],
                };
            }
        }
        let mut names: Vec<String> = sat.iter().map(|p| p.name.clone()).collect();
        names.sort();
        let chosen = names.remove(0);
        return Resolution::Official {
            chosen,
            alternatives: names,
        };
    }
    if let Some(pick) = choices.get(&dep.name) {
        if let Some(pb) = known_aur.get(pick) {
            return Resolution::Aur {
                pkgbase: pb.clone(),
                pkgname: pick.clone(),
            };
        }
    }
    if let Some(pb) = known_aur.get(&dep.name) {
        return Resolution::Aur {
            pkgbase: pb.clone(),
            pkgname: dep.name.clone(),
        };
    }
    Resolution::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use repoman_alpm::syncdb::SyncPackage;

    fn pkg(name: &str, version: &str, provides: &[&str]) -> SyncPackage {
        SyncPackage {
            name: name.into(),
            version: version.into(),
            filename: format!("{name}-{version}-x86_64.pkg.tar.zst"),
            provides: provides.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn srcinfo(text: &str) -> Srcinfo {
        Srcinfo::parse(text).unwrap()
    }

    #[test]
    fn resolves_official_aur_and_unknown_with_ordering() {
        let mut idx = SyncIndex::new();
        idx.add_repo(
            "core",
            vec![
                pkg("glibc", "2.44-1", &[]),
                pkg("cmake", "3.30-1", &[]),
                pkg("jre17-openjdk", "17-1", &["java-runtime=17"]),
                pkg("jre21-openjdk", "21-1", &["java-runtime=21"]),
            ],
        );
        let a = srcinfo("pkgbase = liba\n\tpkgver = 1\n\tpkgrel = 1\n\tmakedepends = cmake\npkgname = liba\n");
        let b = srcinfo(
            "pkgbase = appb\n\tpkgver = 1\n\tpkgrel = 1\n\tdepends = liba\n\tdepends = java-runtime\n\tdepends = glibc\n\tmakedepends = notexist\npkgname = appb\n",
        );
        let c = srcinfo("pkgbase = toolc\n\tpkgver = 1\n\tpkgrel = 1\n\tdepends = appb\n\tdepends = extern-aur\npkgname = toolc\n");
        let none = HashMap::new();
        let mut choices = HashMap::new();
        choices.insert("java-runtime".to_string(), "jre21-openjdk".to_string());
        let mut known = HashMap::new();
        known.insert("liba".to_string(), "liba".to_string());
        known.insert("appb".to_string(), "appb".to_string());
        known.insert("toolc".to_string(), "toolc".to_string());
        known.insert("extern-aur".to_string(), "extern-aur".to_string());

        let members = vec![
            Input { pkgbase: "toolc", srcinfo: &c, provider_choices: &none },
            Input { pkgbase: "appb", srcinfo: &b, provider_choices: &choices },
            Input { pkgbase: "liba", srcinfo: &a, provider_choices: &none },
        ];
        let plan = plan(&idx, "x86_64", &members, &known);
        let order: Vec<&str> = plan.builds.iter().map(|b| b.pkgbase.as_str()).collect();
        assert_eq!(order, ["liba", "appb", "toolc"]);
        let appb = &plan.builds[1];
        assert_eq!(appb.install, ["glibc", "jre21-openjdk", "liba"]);
        assert_eq!(appb.aur_deps, ["liba"]);
        assert!(appb.ambiguous_providers.is_empty(), "pinned by policy");
        let toolc = &plan.builds[2];
        assert_eq!(toolc.aur_deps, ["appb", "extern-aur"]);
        assert_eq!(plan.unresolved, [Unresolved { pkgbase: "appb".into(), dep: "notexist".into() }]);
        assert!(plan.cycles.is_empty());
    }

    #[test]
    fn unpinned_virtual_dep_picks_first_and_reports_alternatives() {
        let mut idx = SyncIndex::new();
        idx.add_repo(
            "extra",
            vec![pkg("jre17-openjdk", "17-1", &["java-runtime"]), pkg("jre21-openjdk", "21-1", &["java-runtime"])],
        );
        let s = srcinfo("pkgbase = x\n\tpkgver = 1\n\tpkgrel = 1\n\tdepends = java-runtime\npkgname = x\n");
        let none = HashMap::new();
        let plan = plan(&idx, "x86_64", &[Input { pkgbase: "x", srcinfo: &s, provider_choices: &none }], &HashMap::new());
        let b = &plan.builds[0];
        assert_eq!(b.install, ["jre17-openjdk"]);
        assert_eq!(b.ambiguous_providers, [("java-runtime".to_string(), "jre17-openjdk".to_string(), vec!["jre21-openjdk".to_string()])]);
    }

    #[test]
    fn detects_cycles() {
        let idx = SyncIndex::new();
        let a = srcinfo("pkgbase = a\n\tpkgver = 1\n\tpkgrel = 1\n\tdepends = b\npkgname = a\n");
        let b = srcinfo("pkgbase = b\n\tpkgver = 1\n\tpkgrel = 1\n\tdepends = a\npkgname = b\n");
        let none = HashMap::new();
        let mut known = HashMap::new();
        known.insert("a".to_string(), "a".to_string());
        known.insert("b".to_string(), "b".to_string());
        let plan = plan(
            &idx,
            "x86_64",
            &[Input { pkgbase: "a", srcinfo: &a, provider_choices: &none }, Input { pkgbase: "b", srcinfo: &b, provider_choices: &none }],
            &known,
        );
        assert!(plan.builds.is_empty());
        assert_eq!(plan.cycles, ["a", "b"]);
    }
}
