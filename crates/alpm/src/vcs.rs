//! makepkg VCS source entries: `[name::]git+URL[#fragment]`. Shared by the
//! coordinator (upstream probing) and the worker (reading the checkout).

/// A parsed makepkg VCS source: `[name::]git+URL[#fragment]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsSource {
    pub url: String,
    /// `branch=x`, `tag=x`, or `commit=x`.
    pub fragment: Option<(String, String)>,
}

impl VcsSource {
    pub fn parse(source: &str) -> Option<VcsSource> {
        let spec = match source.split_once("::") {
            Some((_, rest)) => rest,
            None => source,
        };
        let spec = spec.strip_prefix("git+").or_else(|| {
            if spec.starts_with("git://") {
                Some(spec)
            } else {
                None
            }
        })?;
        let (url, frag) = match spec.split_once('#') {
            Some((u, f)) => (u, Some(f)),
            None => (spec, None),
        };
        // Strip a `?signed` query.
        let url = url.split('?').next().unwrap_or(url).to_string();
        let fragment = frag.and_then(|f| f.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string()));
        Some(VcsSource { url, fragment })
    }

    pub fn is_pinned(&self) -> bool {
        matches!(&self.fragment, Some((k, _)) if k == "commit" || k == "tag")
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vcs_sources() {
        let s = VcsSource::parse("yay::git+https://github.com/Jguer/yay.git#branch=main").unwrap();
        assert_eq!(s.url, "https://github.com/Jguer/yay.git");
        assert_eq!(s.fragment, Some(("branch".into(), "main".into())));
        assert!(!s.is_pinned());
        let t = VcsSource::parse("git+https://x/y.git?signed#tag=v1.2").unwrap();
        assert_eq!(t.url, "https://x/y.git");
        assert!(t.is_pinned());
        assert!(VcsSource::parse("https://x/y.tar.gz").is_none());
        assert!(VcsSource::parse("foo.patch").is_none());
    }
}
