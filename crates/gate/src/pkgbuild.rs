//! Just enough shell structure to tell which function a PKGBUILD line is in
//! and to compare function bodies between commits. Not a parser: brace
//! depth counting on lines, which is what makepkg's own `pkgbuild-introspection`
//! style tools get away with too.

use std::collections::BTreeMap;

/// A function's name and the inclusive 1-based line range of its definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub name: String,
    pub start: usize,
    pub end: usize,
}

fn header_name(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let t = t.strip_prefix("function ").map(str::trim_start).unwrap_or(t);
    let (name, rest) = t.split_once('(')?;
    let name = name.trim_end();
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'@' | b'+'))
    {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(')')?.trim_start();
    if rest.is_empty() || rest.starts_with('{') {
        Some(name)
    } else {
        None
    }
}

/// Brace depth change contributed by a line, ignoring quoted text and
/// comments as far as a line-local scan can.
fn depth_delta(line: &str) -> i32 {
    let mut d = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (_, '\\') => escaped = true,
            (Some(q), x) if x == q => quote = None,
            (Some(_), _) => {}
            (None, '\'') | (None, '"') => quote = Some(c),
            (None, '#') => break,
            (None, '$') => {
                // ${var} and $(cmd) are balanced; skip the opener so the
                // closer is not counted either.
                if let Some('{') = chars.peek() {
                    chars.next();
                    skip_balanced(&mut chars, '{', '}');
                } else if let Some('(') = chars.peek() {
                    chars.next();
                    skip_balanced(&mut chars, '(', ')');
                }
            }
            (None, '{') => d += 1,
            (None, '}') => d -= 1,
            _ => {}
        }
    }
    d
}

fn skip_balanced(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, open: char, close: char) {
    let mut depth = 1;
    for c in chars.by_ref() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return;
            }
        }
    }
}

/// Function definitions with their line spans, in file order.
pub fn spans(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut current: Option<(String, usize, i32)> = None;
    for (i, line) in text.lines().enumerate() {
        let lineno = i + 1;
        match &mut current {
            None => {
                if let Some(name) = header_name(line) {
                    let d = depth_delta(line);
                    if d <= 0 && line.contains('{') {
                        // `f() { :; }` on one line.
                        out.push(Span { name: name.to_string(), start: lineno, end: lineno });
                    } else {
                        current = Some((name.to_string(), lineno, d.max(0)));
                    }
                }
            }
            Some((name, start, depth)) => {
                if *depth == 0 {
                    // Header without its brace; the brace comes on this line.
                    *depth += depth_delta(line).max(0);
                    if *depth == 0 {
                        continue;
                    }
                } else {
                    *depth += depth_delta(line);
                }
                if *depth <= 0 {
                    out.push(Span { name: name.clone(), start: *start, end: lineno });
                    current = None;
                }
            }
        }
    }
    if let Some((name, start, _)) = current {
        out.push(Span { name, start, end: text.lines().count() });
    }
    out
}

/// Name of the function containing a 1-based line, if any.
pub fn function_at(spans: &[Span], line: usize) -> Option<&str> {
    spans.iter().find(|s| s.start <= line && line <= s.end).map(|s| s.name.as_str())
}

/// Function bodies keyed by name, normalised for comparison: trimmed lines,
/// blank and comment-only lines dropped. A function defined twice keeps the
/// last definition, as bash does.
pub fn functions(text: &str) -> BTreeMap<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = BTreeMap::new();
    for s in spans(text) {
        let body: Vec<&str> = lines[s.start - 1..s.end]
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        out.insert(s.name, body.join("\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PKGBUILD: &str = r#"pkgname=foo
pkgver=1.0
source=("https://x/${pkgname}-${pkgver}.tar.gz")

prepare() {
    cd "$pkgname-$pkgver"   # comment with }
    echo "{ not a brace"
}

build()
{
    make
}
package() { install -Dm755 foo "$pkgdir/usr/bin/foo"; }

function check() {
    if true; then
        make test
    fi
}
"#;

    #[test]
    fn finds_every_function_style() {
        let s = spans(PKGBUILD);
        let names: Vec<(&str, usize, usize)> = s.iter().map(|x| (x.name.as_str(), x.start, x.end)).collect();
        assert_eq!(names, [("prepare", 5, 8), ("build", 10, 13), ("package", 14, 14), ("check", 16, 20)]);
        assert_eq!(function_at(&s, 6), Some("prepare"));
        assert_eq!(function_at(&s, 2), None);
    }

    #[test]
    fn bodies_normalise() {
        let f = functions(PKGBUILD);
        assert_eq!(f["build"], "build()\n{\nmake\n}");
        let g = functions(&PKGBUILD.replace("    make\n", "    make  \n\n    # hi\n"));
        assert_eq!(f["build"], g["build"], "whitespace and comments do not count");
    }

    #[test]
    fn variables_do_not_look_like_functions() {
        assert!(spans("x=$(echo a)\ny=\"${z}\"\n").is_empty());
    }
}
