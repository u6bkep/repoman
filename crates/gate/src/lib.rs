//! The deterministic half of the gate. Given the baseline and head of an AUR
//! package (SRCINFO, PKGBUILD, install scripts, file lists) it classifies the
//! change as routine, metadata, or structural, runs a danger lint over the
//! scripts, and derives a floor risk the LLM reviewer may raise but never
//! lower. Everything here is pure: no network, no clock, no store.

pub mod classify;
pub mod lint;
pub mod pkgbuild;

use serde::{Deserialize, Serialize};

pub use classify::{DiffClass, Reason, classify};
pub use lint::{Finding, lint_pkgbuild, lint_script};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Risk> {
        match s {
            "low" => Some(Risk::Low),
            "medium" => Some(Risk::Medium),
            "high" => Some(Risk::High),
            _ => None,
        }
    }
}

/// What the gate knows about one change. Texts are the committed files at
/// each commit; `None` for a file that does not exist there.
pub struct GateInput<'a> {
    pub old: &'a repoman_alpm::srcinfo::Srcinfo,
    pub new: &'a repoman_alpm::srcinfo::Srcinfo,
    pub old_pkgbuild: Option<&'a str>,
    pub new_pkgbuild: &'a str,
    /// (file name, contents) for install scripts and hooks at the head.
    pub new_scripts: &'a [(String, String)],
    /// Same for the baseline, to tell changed scripts from unchanged ones.
    pub old_scripts: &'a [(String, String)],
    /// Repository file listings at each commit.
    pub old_files: &'a [String],
    pub new_files: &'a [String],
    /// Maintainer or co-maintainer set differs from the baseline snapshot.
    pub maintainer_changed: bool,
    /// Regenerated SRCINFO (from an inspect job) differed from the committed one.
    pub srcinfo_mismatch: Option<bool>,
    /// Whether a dependency name is satisfiable from the official repos.
    pub is_official: &'a dyn Fn(&str) -> bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub diff_class: DiffClass,
    pub floor_risk: Risk,
    pub reasons: Vec<Reason>,
    pub findings: Vec<Finding>,
}

/// Run the whole deterministic gate.
pub fn evaluate(input: &GateInput<'_>) -> Verdict {
    let mut reasons = classify::classify_change(input);
    let mut findings = lint_pkgbuild(input.new_pkgbuild);
    for (name, text) in input.new_scripts {
        let changed = input
            .old_scripts
            .iter()
            .find(|(n, _)| n == name)
            .is_none_or(|(_, t)| t != text);
        let mut f = lint_script(name, text);
        if !changed {
            // An unchanged install script was reviewed with the baseline;
            // keep its findings visible but do not let them re-raise risk.
            for x in &mut f {
                x.severity = Risk::Low;
                x.note = Some("unchanged since baseline".into());
            }
        }
        findings.extend(f);
    }
    if let Some(old) = input.old_pkgbuild {
        // Findings already present at the baseline do not re-raise risk
        // either; the change under review is the delta.
        let baseline: Vec<String> = lint_pkgbuild(old).into_iter().map(|f| f.rule_and_excerpt()).collect();
        for f in &mut findings {
            if f.file == "PKGBUILD" && baseline.contains(&f.rule_and_excerpt()) {
                f.severity = Risk::Low;
                f.note = Some("present at baseline".into());
            }
        }
    }
    let diff_class = reasons.iter().map(|r| r.class).max().unwrap_or(DiffClass::Routine);
    let mut floor = match diff_class {
        DiffClass::Routine | DiffClass::Metadata => Risk::Low,
        DiffClass::Structural => Risk::Medium,
    };
    for r in &reasons {
        floor = floor.max(r.risk);
    }
    for f in &findings {
        floor = floor.max(f.severity);
    }
    reasons.sort_by(|a, b| b.risk.cmp(&a.risk).then(b.class.cmp(&a.class)));
    findings.sort_by(|a, b| b.severity.cmp(&a.severity));
    Verdict {
        diff_class,
        floor_risk: floor,
        reasons,
        findings,
    }
}

#[cfg(test)]
mod tests;
