//! libalpm version comparison (`alpm_pkg_vercmp`), ported from
//! `lib/libalpm/version.c`. Versions are `[epoch:]pkgver[-pkgrel]`.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version<'a> {
    pub epoch: &'a str,
    pub pkgver: &'a str,
    pub pkgrel: Option<&'a str>,
}

impl<'a> Version<'a> {
    /// `parseEVR`: digits before the first ':' are the epoch, text after the
    /// last '-' is the release.
    pub fn parse(s: &'a str) -> Self {
        let (epoch, rest) = match s.find(':') {
            Some(i) if !s[..i].is_empty() && s[..i].bytes().all(|b| b.is_ascii_digit()) => {
                (&s[..i], &s[i + 1..])
            }
            _ => ("0", s),
        };
        let (pkgver, pkgrel) = match rest.rfind('-') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        Version {
            epoch,
            pkgver,
            pkgrel,
        }
    }
}

/// Compare two full version strings as pacman does. A missing release on
/// either side means releases are not compared.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let va = Version::parse(a);
    let vb = Version::parse(b);
    let r = rpmvercmp(va.epoch, vb.epoch);
    if r != Ordering::Equal {
        return r;
    }
    let r = rpmvercmp(va.pkgver, vb.pkgver);
    if r != Ordering::Equal {
        return r;
    }
    match (va.pkgrel, vb.pkgrel) {
        (Some(ra), Some(rb)) => rpmvercmp(ra, rb),
        _ => Ordering::Equal,
    }
}

/// Segment-wise comparison of two version fragments, alpm flavour.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let a = a.as_bytes();
    let b = b.as_bytes();
    let (mut i, mut j) = (0usize, 0usize);

    while i < a.len() && j < b.len() {
        let (si, sj) = (i, j);
        while i < a.len() && !a[i].is_ascii_alphanumeric() {
            i += 1;
        }
        while j < b.len() && !b[j].is_ascii_alphanumeric() {
            j += 1;
        }
        if i >= a.len() || j >= b.len() {
            break;
        }
        // Different separator lengths decide the comparison.
        if i - si != j - sj {
            return (i - si).cmp(&(j - sj));
        }
        let isnum = a[i].is_ascii_digit();
        let seg_a_start = i;
        let seg_b_start = j;
        if isnum {
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
        } else {
            while i < a.len() && a[i].is_ascii_alphabetic() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_alphabetic() {
                j += 1;
            }
        }
        let seg_a = &a[seg_a_start..i];
        let seg_b = &b[seg_b_start..j];
        if seg_b.is_empty() {
            // Segment types differ: numeric beats alphabetic.
            return if isnum { Ordering::Greater } else { Ordering::Less };
        }
        if isnum {
            let ta = trim_leading_zeros(seg_a);
            let tb = trim_leading_zeros(seg_b);
            if ta.len() != tb.len() {
                return ta.len().cmp(&tb.len());
            }
            let r = ta.cmp(tb);
            if r != Ordering::Equal {
                return r;
            }
        } else {
            let r = seg_a.cmp(seg_b);
            if r != Ordering::Equal {
                return r;
            }
        }
    }

    let rest_a = &a[i.min(a.len())..];
    let rest_b = &b[j.min(b.len())..];
    match (rest_a.is_empty(), rest_b.is_empty()) {
        (true, true) => Ordering::Equal,
        // Whichever has leftovers wins, unless the leftover is alphabetic.
        (true, false) => {
            if rest_b[0].is_ascii_alphabetic() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, _) => {
            if rest_a[0].is_ascii_alphabetic() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
    }
}

fn trim_leading_zeros(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&c| c == b'0').count();
    &s[n..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(a: &str, b: &str, expect: i32) {
        let got = match vercmp(a, b) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        };
        assert_eq!(got, expect, "vercmp({a:?}, {b:?})");
        let back = match vercmp(b, a) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        };
        assert_eq!(back, -expect, "vercmp({b:?}, {a:?}) symmetry");
    }

    /// Vectors from pacman's `test/util/vercmptest.sh`.
    #[test]
    fn pacman_vectors() {
        // all similar length, no pkgrel
        check("1.5.0", "1.5.0", 0);
        check("1.5.1", "1.5.0", 1);
        // mixed length
        check("1.5.1", "1.5", 1);
        // with pkgrel, simple
        check("1.5.0-1", "1.5.0-1", 0);
        check("1.5.0-1", "1.5.0-2", -1);
        check("1.5.0-1", "1.5.1-1", -1);
        check("1.5.0-2", "1.5.1-1", -1);
        // with pkgrel, mixed lengths
        check("1.5-1", "1.5.1-1", -1);
        check("1.5-2", "1.5.1-1", -1);
        check("1.5-2", "1.5.1-2", -1);
        // mixed pkgrel inclusion
        check("1.5", "1.5-1", 0);
        check("1.5-1", "1.5", 0);
        check("1.1-1", "1.1", 0);
        check("1.0-1", "1.1", -1);
        check("1.1-1", "1.0", 1);
        // alphanumeric versions
        check("1.5b-1", "1.5-1", -1);
        check("1.5b", "1.5", -1);
        check("1.5b-1", "1.5", -1);
        check("1.5b", "1.5.1", -1);
        // from the manpage
        check("1.0a", "1.0alpha", -1);
        check("1.0alpha", "1.0b", -1);
        check("1.0b", "1.0beta", -1);
        check("1.0beta", "1.0rc", -1);
        check("1.0rc", "1.0", -1);
        // going crazy? alpha-dotted versions
        check("1.5.a", "1.5", 1);
        check("1.5.b", "1.5.a", 1);
        check("1.5.1", "1.5.b", 1);
        // alpha dots and dashes
        check("1.5.b-1", "1.5.b", 0);
        check("1.5-1", "1.5.b", -1);
        // same/similar content, differing separators
        check("2.0", "2_0", 0);
        check("2.0_a", "2_0.a", 0);
        check("2.0a", "2.0.a", -1);
        check("2___a", "2_a", 1);
        // epoch included version comparisons
        check("0:1.0", "0:1.0", 0);
        check("0:1.0", "0:1.1", -1);
        check("1:1.0", "0:1.0", 1);
        check("1:1.0", "0:1.1", 1);
        check("1:1.0", "2:1.1", -1);
        // epoch + sometimes present pkgrel
        check("1:1.0", "0:1.0-1", 1);
        check("1:1.0-1", "0:1.1-1", 1);
        // epoch included on one version
        check("0:1.0", "1.0", 0);
        check("0:1.0", "1.1", -1);
        check("0:1.1", "1.0", 1);
        check("1:1.0", "1.0", 1);
        check("1:1.0", "1.1", 1);
        check("1:1.1", "1.1", 1);
    }

    #[test]
    fn parse_evr() {
        assert_eq!(
            Version::parse("2:1.2.3-4"),
            Version {
                epoch: "2",
                pkgver: "1.2.3",
                pkgrel: Some("4")
            }
        );
        assert_eq!(
            Version::parse("1.2.3"),
            Version {
                epoch: "0",
                pkgver: "1.2.3",
                pkgrel: None
            }
        );
        // A git-style pkgver containing '-' is split at the last dash only.
        assert_eq!(Version::parse("r12.abc-1").pkgrel, Some("1"));
    }
}
