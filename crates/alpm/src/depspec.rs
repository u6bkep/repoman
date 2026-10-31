//! Dependency and provides specifications: `name`, `name>=1.2`,
//! `libfoo.so=3-64`, and optdepend `name: reason`.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::version::vercmp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl Cmp {
    fn matches(self, ord: Ordering) -> bool {
        match self {
            Cmp::Lt => ord == Ordering::Less,
            Cmp::Le => ord != Ordering::Greater,
            Cmp::Eq => ord == Ordering::Equal,
            Cmp::Ge => ord != Ordering::Less,
            Cmp::Gt => ord == Ordering::Greater,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Cmp::Lt => "<",
            Cmp::Le => "<=",
            Cmp::Eq => "=",
            Cmp::Ge => ">=",
            Cmp::Gt => ">",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepSpec {
    pub name: String,
    pub constraint: Option<(Cmp, String)>,
    /// Text after `: ` in an optdepend.
    pub description: Option<String>,
}

impl DepSpec {
    pub fn parse(s: &str) -> DepSpec {
        let (spec, description) = match s.split_once(": ") {
            Some((a, b)) => (a.trim(), Some(b.trim().to_string())),
            None => (s.trim(), None),
        };
        // Longest operators first.
        for (op_str, op) in [
            (">=", Cmp::Ge),
            ("<=", Cmp::Le),
            ("=", Cmp::Eq),
            (">", Cmp::Gt),
            ("<", Cmp::Lt),
        ] {
            if let Some(i) = spec.find(op_str) {
                let name = spec[..i].to_string();
                let ver = spec[i + op_str.len()..].to_string();
                return DepSpec {
                    name,
                    constraint: Some((op, ver)),
                    description,
                };
            }
        }
        DepSpec {
            name: spec.to_string(),
            constraint: None,
            description,
        }
    }

    pub fn unversioned(name: &str) -> DepSpec {
        DepSpec {
            name: name.to_string(),
            constraint: None,
            description: None,
        }
    }

    /// Does a package or provide entry with this `name` and `version`
    /// satisfy the spec? A provide with no version satisfies any
    /// constraint, as in alpm.
    pub fn satisfied_by(&self, name: &str, version: Option<&str>) -> bool {
        if self.name != name {
            return false;
        }
        match (&self.constraint, version) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some((op, want)), Some(have)) => op.matches(vercmp(have, want)),
        }
    }

    /// Parse a provides entry such as `libfoo.so=3-64` into name and
    /// optional version, for indexing.
    pub fn provide_parts(s: &str) -> (&str, Option<&str>) {
        match s.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (s, None),
        }
    }
}

impl fmt::Display for DepSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some((op, v)) = &self.constraint {
            write!(f, "{}{}", op.as_str(), v)?;
        }
        if let Some(d) = &self.description {
            write!(f, ": {d}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operators() {
        let d = DepSpec::parse("python>=3.12");
        assert_eq!(d.name, "python");
        assert_eq!(d.constraint, Some((Cmp::Ge, "3.12".into())));
        assert_eq!(DepSpec::parse("glibc").constraint, None);
        assert_eq!(
            DepSpec::parse("libfoo.so=3-64").constraint,
            Some((Cmp::Eq, "3-64".into()))
        );
        let o = DepSpec::parse("cups: printing support");
        assert_eq!(o.name, "cups");
        assert_eq!(o.description.as_deref(), Some("printing support"));
    }

    #[test]
    fn satisfaction() {
        let d = DepSpec::parse("python>=3.12");
        assert!(d.satisfied_by("python", Some("3.13.1-2")));
        assert!(!d.satisfied_by("python", Some("3.11.9-1")));
        assert!(d.satisfied_by("python", None), "unversioned provide");
        assert!(!d.satisfied_by("python2", Some("3.13")));
        let so = DepSpec::parse("libfoo.so=3-64");
        assert!(so.satisfied_by("libfoo.so", Some("3-64")));
        assert!(!so.satisfied_by("libfoo.so", Some("4-64")));
        // Dep without pkgrel matches any pkgrel.
        assert!(DepSpec::parse("gcc=14.2.1").satisfied_by("gcc", Some("14.2.1-3")));
    }

    #[test]
    fn display_round_trip() {
        for s in ["python>=3.12", "glibc", "libfoo.so=3-64", "cups: printing support"] {
            assert_eq!(DepSpec::parse(s).to_string(), s);
        }
    }
}
