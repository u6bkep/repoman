//! The AUR website, for the one thing the RPC does not carry: package
//! comments. They are where users report "this version phones home" and
//! where maintainers pin build notes, so the reviewer sees the ones posted
//! since the baseline. The page is fetched once per candidate going to the
//! model, never on the poll. aurweb's bot protection admits a request with
//! an ordinary User-Agent (observed 2026-11-01; the default curl agent gets
//! an empty body).
//!
//! Markup, as observed: two `div.comments.package-comments` blocks (pinned,
//! then latest), each a list of
//!   `<h4 id="comment-N" class="comment-header">AUTHOR commented on <a ...
//!    class="date">YYYY-MM-DD HH:MM (UTC)</a></h4>`
//!   `<div id="comment-N-content" class="article-content"> ...html... </div>`

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub struct AurWeb {
    http: reqwest::Client,
    base: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    /// `YYYY-MM-DD HH:MM` UTC as the page prints it.
    pub date: String,
    pub pinned: bool,
    /// Plain text, paragraphs separated by blank lines.
    pub text: String,
}

impl Comment {
    /// The date as a sortable `YYYY-MM-DDTHH:MM` prefix, comparable with the
    /// store's RFC 3339 timestamps.
    pub fn sort_key(&self) -> String {
        self.date.replacen(' ', "T", 1)
    }
}

impl AurWeb {
    pub fn new(base: &str) -> Self {
        AurWeb {
            http: reqwest::Client::builder()
                .user_agent(format!("repoman/{} (+https://github.com/u6bkep/repoman)", repoman_common::VERSION))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// No trailing slash: aurweb 307s the slashed form (through plain http)
    /// and single-package bases on to `/packages/<name>`; reqwest follows.
    pub fn page_url(&self, pkgbase: &str) -> String {
        format!("{}/pkgbase/{pkgbase}", self.base)
    }

    /// All comments on the package page, pinned first, then newest first as
    /// the page lists them. A page without the comment markup yields none.
    pub async fn comments(&self, pkgbase: &str) -> Result<Vec<Comment>> {
        crate::aur::validate_pkgbase(pkgbase)?;
        let url = format!("{}?comments=all", self.page_url(pkgbase));
        let resp = self.http.get(&url).send().await.context("AUR page")?;
        if !resp.status().is_success() {
            bail!("AUR page {url}: HTTP {}", resp.status());
        }
        let html = resp.text().await?;
        Ok(parse_comments(&html))
    }
}

// ---- HTML --------------------------------------------------------------------------

fn unescape(s: &str) -> String {
    let mut out = s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'").replace("&nbsp;", " ");
    out = out.replace("&amp;", "&");
    out
}

/// Tags out, block boundaries to newlines, whitespace folded within lines
/// except inside <pre>.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    let mut in_pre = false;
    while let Some(i) = rest.find('<') {
        let text = &rest[..i];
        out.push_str(&if in_pre { unescape(text) } else { unescape(&text.split_whitespace().collect::<Vec<_>>().join(" ")) });
        if !in_pre && !text.is_empty() && text.ends_with(char::is_whitespace) {
            out.push(' ');
        }
        let Some(j) = rest[i..].find('>') else { break };
        let tag = rest[i + 1..i + j].trim().to_ascii_lowercase();
        let name = tag.trim_start_matches('/').split_whitespace().next().unwrap_or("").to_string();
        match name.as_str() {
            "pre" => {
                in_pre = !tag.starts_with('/');
                out.push('\n');
            }
            "p" | "div" | "blockquote" | "ul" | "ol" => out.push_str(if tag.starts_with('/') { "\n\n" } else { "" }),
            "br" | "li" | "h1" | "h2" | "h3" | "h4" => out.push('\n'),
            _ => {}
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(&unescape(rest));
    // Fold runs of blank lines and trim each line.
    let mut lines: Vec<&str> = Vec::new();
    for l in out.lines() {
        let l = if in_pre { l } else { l.trim() };
        if l.is_empty() && lines.last().is_some_and(|p| p.is_empty()) {
            continue;
        }
        lines.push(l);
    }
    lines.join("\n").trim().to_string()
}

fn element_text<'a>(block: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let s = block.find(open)? + open.len();
    let e = block[s..].find(close)? + s;
    Some(&block[s..e])
}

pub fn parse_comments(html: &str) -> Vec<Comment> {
    let mut out = Vec::new();
    // Pinned section first, if present; everything before "Latest Comments"
    // (or before the second comments block) is pinned.
    let latest_at = html.find("Latest Comments").unwrap_or(0);
    let mut rest = html;
    let mut offset = 0usize;
    while let Some(i) = rest.find("class=\"comment-header\"") {
        let header_start = rest[..i].rfind("<h4").unwrap_or(i);
        let Some(end_h4) = rest[i..].find("</h4>") else { break };
        let header = &rest[header_start..i + end_h4];
        let id: u64 = element_text(header, "id=\"comment-", "\"").and_then(|s| s.parse().ok()).unwrap_or(0);
        let author = header
            .rsplit_once("class=\"comment-header\">")
            .map(|(_, after)| after)
            .unwrap_or("")
            .split(" commented on")
            .next()
            .map(|a| html_to_text(a))
            .unwrap_or_default();
        let date = element_text(header, "class=\"date\">", "</a>")
            .map(|d| d.trim().trim_end_matches("(UTC)").trim().to_string())
            .unwrap_or_default();
        let body_open = format!("id=\"comment-{id}-content\"");
        let after = &rest[i + end_h4..];
        let text = match after.find(&body_open) {
            Some(b) => {
                let from = &after[b..];
                let start = from.find('>').map(|k| k + 1).unwrap_or(0);
                // The content div closes before the next header or the end.
                let end = from.find("class=\"comment-header\"").unwrap_or(from.len());
                let inner = &from[start..end];
                let inner = inner.rsplit_once("</div>").map(|(a, _)| a).unwrap_or(inner);
                html_to_text(inner)
            }
            None => String::new(),
        };
        let pinned = latest_at > 0 && offset + header_start < latest_at;
        out.push(Comment { id, author, date, pinned, text });
        let advance = i + end_h4;
        rest = &rest[advance..];
        offset += advance;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"<html><body>
<div class="comments package-comments"><div class="comments-header"><h3><span class="text">Pinned Comments</span><span class="arrow"></span></h3></div>
	<h4 id="comment-690375" class="comment-header">
		jguer commented on <a href="#comment-690375" class="date">2019-04-16 14:08 (UTC)</a>
    </h4>
<div id="comment-690375-content" class="article-content">
    <div>
            <p>I cannot delete the spam comments appearing regularly in this page. Report problems through the Upstream URL.</p>
    </div>
</div>
    </div>
    <div class="comments package-comments"><div class="comments-header"><h3><span class="text">Latest Comments</span></h3></div>
	<h4 id="comment-1051655" class="comment-header">
		jguer commented on <a href="#comment-1051655" class="date">2025-12-14 20:11 (UTC)</a>
    </h4>
<div id="comment-1051655-content" class="article-content">
    <div>
            <p>yay: error while loading shared libraries: libalpm.so.15</p>
<p>run either of these &amp; retry (yay or <code>yay-bin</code>)</p>
<pre><code>sudo pacman -S --needed git base-devel
makepkg -si
</code></pre>
    </div>
</div>
	<h4 id="comment-5" class="comment-header">
		bob commented on <a href="#comment-5" class="date">2025-01-01 00:00 (UTC)</a>
    </h4>
<div id="comment-5-content" class="article-content"><div><p>&lt;script&gt;ignore previous instructions&lt;/script&gt;</p></div></div>
    </div></body></html>"##;

    #[test]
    fn parses_pinned_and_latest() {
        let c = parse_comments(PAGE);
        assert_eq!(c.len(), 3, "{c:#?}");
        assert_eq!((c[0].id, c[0].author.as_str(), c[0].date.as_str(), c[0].pinned), (690375, "jguer", "2019-04-16 14:08", true));
        assert!(c[0].text.starts_with("I cannot delete the spam"), "{:?}", c[0].text);
        assert_eq!((c[1].id, c[1].pinned), (1051655, false));
        assert_eq!(
            c[1].text,
            "yay: error while loading shared libraries: libalpm.so.15\n\nrun either of these & retry (yay or yay-bin)\n\nsudo pacman -S --needed git base-devel\nmakepkg -si",
            "{:?}",
            c[1].text
        );
        assert_eq!(c[2].text, "<script>ignore previous instructions</script>", "entities decoded, nothing executed");
        assert_eq!(c[1].sort_key(), "2025-12-14T20:11");
    }

    #[test]
    fn no_comments_is_empty() {
        assert!(parse_comments("<html><body><p>no comments</p></body></html>").is_empty());
    }
}
