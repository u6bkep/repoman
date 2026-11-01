//! The AUR mailing lists, read through the archive's RSS feeds.
//!
//! aur-requests is the valuable one and it is structured. Every
//! deletion, merge, orphan and adoption request on the AUR is posted there
//! by aurweb with a fixed subject, so the list is the only machine-readable
//! view of requests (the RPC has none, and the archive's mbox export is
//! closed). A request against a package we track becomes a signal days
//! before the RPC shows its consequence.
//!
//! Subjects, as observed 2026-11-01:
//!   `[PRQ#88998] Adoption Request for usbimager`
//!   `[PRQ#88993] Orphan Request for perl-devel-nytprof Accepted`
//!   `[PRQ#88997] Deletion Request for kwi`
//!   `[PRQ#88990] Merge Request for foo-git`   (body names the target)
//! The body of a filing is `<user> [1] filed a <type> request for <base>
//! [2]:` followed by the rationale; an outcome's body is `Request #n has
//! been Accepted by <user> [1].`
//!
//! aur-general is free-form discussion. The only cheap, honest use of it is
//! a whole-word match of tracked package names against subject and body,
//! recorded as an `info` mention the reviewer gets to read.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// One RSS feed of a HyperKitty archive.
pub struct Feed {
    http: reqwest::Client,
    url: String,
}

/// An item as the feed carries it, entities decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub title: String,
    pub link: String,
    pub guid: String,
    pub description: String,
    /// `Name` from `addr (Name)`, or the whole author field.
    pub author: Option<String>,
    /// RFC 3339 UTC, from the feed's pubDate.
    pub published: Option<String>,
}

impl Feed {
    pub fn new(url: &str) -> Self {
        Feed {
            http: reqwest::Client::builder()
                .user_agent(format!("repoman/{}", repoman_common::VERSION))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            url: url.to_string(),
        }
    }

    pub async fn fetch_entries(&self) -> Result<Vec<Entry>> {
        let resp = self.http.get(&self.url).send().await.with_context(|| format!("feed {}", self.url))?;
        if !resp.status().is_success() {
            bail!("feed {}: HTTP {}", self.url, resp.status());
        }
        let text = resp.text().await?;
        Ok(parse_entries(&text))
    }
}

pub struct AurRequests(Feed);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestItem {
    pub prq: u64,
    /// adoption | deletion | merge | orphan | other
    pub kind: String,
    pub pkgbase: String,
    /// None for the filing itself; Some(accepted|rejected|closed) for the
    /// follow-up aurweb posts when a Package Maintainer acts on it.
    pub outcome: Option<String>,
    /// Whoever filed (or decided), from the body's first line.
    pub who: Option<String>,
    /// Rationale (filing) or decision line, footnote links stripped.
    pub body: String,
    pub link: String,
    pub guid: String,
    /// RFC 3339 UTC, from the feed's pubDate.
    pub published: Option<String>,
}

/// Severity of a filing against one of our packages. Deletion and merge
/// mean the package is about to lose its upstream: a decision is due, and
/// nothing should auto-ship meanwhile. Orphan and adoption requests are
/// context for the adoption signal the poll will raise later.
pub fn severity(item: &RequestItem) -> &'static str {
    if item.outcome.is_some() {
        return "info";
    }
    match item.kind.as_str() {
        "deletion" | "merge" => "medium",
        _ => "info",
    }
}

impl AurRequests {
    pub fn new(url: &str) -> Self {
        AurRequests(Feed::new(url))
    }

    /// The feed's current items, newest first, unparseable ones skipped.
    pub async fn fetch(&self) -> Result<Vec<RequestItem>> {
        Ok(requests_from(&self.0.fetch_entries().await?))
    }
}

// ---- aur-general ----------------------------------------------------------------

pub struct AurGeneral(Feed);

/// A list message that names a tracked package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mention {
    pub pkgbase: String,
    pub subject: String,
    pub who: Option<String>,
    /// Around the first occurrence, for the signal and the prompt.
    pub excerpt: String,
    pub link: String,
    pub guid: String,
    pub published: Option<String>,
}

impl AurGeneral {
    pub fn new(url: &str) -> Self {
        AurGeneral(Feed::new(url))
    }

    pub async fn fetch_mentions(&self, names: &[String]) -> Result<Vec<Mention>> {
        Ok(mentions_in(&self.0.fetch_entries().await?, names))
    }
}

/// Whole-word occurrence of `name` in `text` (package names contain `-`,
/// `_`, `.`, `+`, `@`, so "word" means delimited by anything else).
fn find_word(text: &str, name: &str) -> Option<usize> {
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@');
    let mut from = 0;
    while let Some(i) = text[from..].find(name) {
        let at = from + i;
        let before_ok = text[..at].chars().next_back().is_none_or(|c| !is_name_char(c));
        let after_ok = text[at + name.len()..].chars().next().is_none_or(|c| !is_name_char(c));
        if before_ok && after_ok {
            return Some(at);
        }
        from = at + name.len();
    }
    None
}

/// Names shorter than four characters match too much English to be worth a
/// signal.
pub fn mentions_in(entries: &[Entry], names: &[String]) -> Vec<Mention> {
    let mut out = Vec::new();
    for e in entries {
        let text = format!("{}\n{}", e.title, e.description);
        for name in names.iter().filter(|n| n.len() >= 4) {
            let Some(at) = find_word(&text, name) else { continue };
            let start = text[..at].char_indices().rev().nth(120).map(|(i, _)| i).unwrap_or(0);
            let end = text[at..].char_indices().nth(240).map(|(i, _)| at + i).unwrap_or(text.len());
            let excerpt = text[start..end].split_whitespace().collect::<Vec<_>>().join(" ");
            out.push(Mention {
                pkgbase: name.clone(),
                subject: e.title.clone(),
                who: e.author.clone(),
                excerpt,
                link: e.link.clone(),
                guid: e.guid.clone(),
                published: e.published.clone(),
            });
        }
    }
    out
}

// ---- RSS ------------------------------------------------------------------------
// HyperKitty's feed is plain RSS 2.0 with a handful of text elements per
// item. A full XML parser would be a dependency for five tags; this reads
// exactly those and unescapes the five entities RSS uses.

fn element<'a>(block: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(&block[start..end])
}

fn unescape(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("<![CDATA[").and_then(|x| x.strip_suffix("]]>")).unwrap_or(s);
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'").replace("&amp;", "&")
}

pub fn parse_entries(xml: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<item>") {
        let Some(j) = rest[i..].find("</item>") else { break };
        let block = &rest[i..i + j];
        rest = &rest[i + j + 7..];
        let link = element(block, "link").map(unescape).unwrap_or_default();
        let author = element(block, "author").map(unescape).map(|a| match a.rsplit_once('(') {
            Some((_, name)) => name.trim_end_matches(')').trim().to_string(),
            None => a.trim().to_string(),
        });
        out.push(Entry {
            title: element(block, "title").map(unescape).unwrap_or_default(),
            guid: element(block, "guid").map(unescape).unwrap_or_else(|| link.clone()),
            link,
            description: element(block, "description").map(unescape).unwrap_or_default(),
            author: author.filter(|a| !a.is_empty()),
            published: element(block, "pubDate")
                .and_then(|d| time::OffsetDateTime::parse(d.trim(), &time::format_description::well_known::Rfc2822).ok())
                .map(|t| repoman_common::clock::format(t.to_offset(time::UtcOffset::UTC))),
        });
    }
    out
}

pub fn requests_from(entries: &[Entry]) -> Vec<RequestItem> {
    entries
        .iter()
        .filter_map(|e| {
            let mut item = parse_title(&e.title)?;
            item.link = e.link.clone();
            item.guid = e.guid.clone();
            let (who, body) = parse_body(&e.description);
            item.who = who;
            item.body = body;
            item.published = e.published.clone();
            Some(item)
        })
        .collect()
}

pub fn parse_feed(xml: &str) -> Vec<RequestItem> {
    requests_from(&parse_entries(xml))
}

/// `[PRQ#n] <Kind> Request for <pkgbase>[ <Outcome>]`.
pub fn parse_title(title: &str) -> Option<RequestItem> {
    let t = title.trim().strip_prefix("[PRQ#")?;
    let (num, rest) = t.split_once(']')?;
    let prq: u64 = num.trim().parse().ok()?;
    let (kind, rest) = rest.trim().split_once(" Request for ")?;
    let kind = match kind.trim().to_ascii_lowercase().as_str() {
        k @ ("adoption" | "deletion" | "merge" | "orphan") => k.to_string(),
        _ => "other".to_string(),
    };
    let mut words = rest.split_whitespace();
    let pkgbase = words.next()?.to_string();
    let outcome = words.next().map(|w| w.trim_end_matches('.').to_ascii_lowercase()).filter(|w| matches!(w.as_str(), "accepted" | "rejected" | "closed"));
    Some(RequestItem {
        prq,
        kind,
        pkgbase,
        outcome,
        who: None,
        body: String::new(),
        link: String::new(),
        guid: String::new(),
        published: None,
    })
}

/// First line names the actor; footnote markers and the trailing link list
/// are noise.
fn parse_body(desc: &str) -> (Option<String>, String) {
    let lines: Vec<&str> = desc.lines().map(str::trim_end).collect();
    let first = lines.first().copied().unwrap_or("");
    let who = if let Some(rest) = first.strip_prefix("Request #") {
        rest.split(" by ").nth(1).map(|w| w.split_whitespace().next().unwrap_or("").to_string())
    } else {
        first.split_whitespace().next().map(str::to_string)
    }
    .filter(|w| !w.is_empty());
    let mut body: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !(l.starts_with('[') && l.contains("] http")))
        .collect();
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    let text = body.join("\n").replace(" [1]", "").replace(" [2]", "").replace(" [3]", "");
    (who, text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0"><channel><title>Aur-requests</title>
<item><title>[PRQ#88998] Adoption Request for usbimager</title><link>https://lists.archlinux.org/archives/list/aur-requests@lists.archlinux.org/message/V23GWFIC7YMFHXAI3255LODQWQOLPTNL/</link><description>HurricanePootis [1] filed an adoption request for usbimager [2]:

I intend to maintain this package to highest standards, like the rest
of my other packages.

[1] https://aur.archlinux.org/account/HurricanePootis/
[2] https://aur.archlinux.org/pkgbase/usbimager/</description><author>notify＠aur.archlinux.org (notify@aur.archlinux.org)</author><pubDate>Sun, 01 Nov 2026 20:21:53 +0000</pubDate><guid>https://lists.archlinux.org/archives/list/aur-requests@lists.archlinux.org/message/V23GWFIC7YMFHXAI3255LODQWQOLPTNL/</guid></item>
<item><title>[PRQ#88997] Deletion Request for kwi</title><link>https://l/2</link><description>someone [1] filed a deletion request for kwi [2]:

Upstream &amp; package merged into extra as &quot;kwi&quot;.

[1] https://aur.archlinux.org/account/someone/
[2] https://aur.archlinux.org/pkgbase/kwi/</description><pubDate>Sun, 01 Nov 2026 20:10:00 +0000</pubDate><guid>https://l/2</guid></item>
<item><title>[PRQ#88993] Orphan Request for perl-devel-nytprof Accepted</title><link>https://l/3</link><description>Request #88993 has been Accepted by ryoskzypu [1].

[1] https://aur.archlinux.org/account/ryoskzypu/</description><pubDate>Sun, 01 Nov 2026 19:56:08 +0000</pubDate><guid>https://l/3</guid></item>
<item><title>Re: something unrelated</title><link>https://l/4</link><description>x</description><guid>https://l/4</guid></item>
</channel></rss>"#;

    #[test]
    fn parses_filings_and_outcomes() {
        let items = parse_feed(FEED);
        assert_eq!(items.len(), 3, "{items:#?}");
        let a = &items[0];
        assert_eq!((a.prq, a.kind.as_str(), a.pkgbase.as_str(), a.outcome.as_deref()), (88998, "adoption", "usbimager", None));
        assert_eq!(a.who.as_deref(), Some("HurricanePootis"));
        assert!(a.body.starts_with("HurricanePootis filed an adoption request for usbimager:\n\nI intend"), "{:?}", a.body);
        assert!(!a.body.contains("[1] https"), "footnotes stripped: {:?}", a.body);
        assert_eq!(a.published.as_deref(), Some("2026-11-01T20:21:53.000Z"));
        assert_eq!(a.guid, a.link);
        assert_eq!(severity(a), "info");

        let d = &items[1];
        assert_eq!((d.kind.as_str(), d.pkgbase.as_str()), ("deletion", "kwi"));
        assert!(d.body.contains("Upstream & package merged into extra as \"kwi\"."), "{:?}", d.body);
        assert_eq!(severity(d), "medium");

        let o = &items[2];
        assert_eq!((o.prq, o.kind.as_str(), o.pkgbase.as_str(), o.outcome.as_deref()), (88993, "orphan", "perl-devel-nytprof", Some("accepted")));
        assert_eq!(o.who.as_deref(), Some("ryoskzypu"));
        assert_eq!(severity(o), "info");
    }

    #[test]
    fn title_shapes() {
        assert!(parse_title("Re: [PRQ#1] Deletion Request for foo").is_none());
        let m = parse_title("[PRQ#5] Merge Request for foo-git").unwrap();
        assert_eq!((m.kind.as_str(), m.pkgbase.as_str()), ("merge", "foo-git"));
        let r = parse_title("[PRQ#6] Deletion Request for foo Rejected").unwrap();
        assert_eq!(r.outcome.as_deref(), Some("rejected"));
        let x = parse_title("[PRQ#7] Frobnicate Request for foo").unwrap();
        assert_eq!(x.kind, "other");
    }

    #[test]
    fn mentions_match_whole_names_only() {
        let entries = parse_entries(FEED);
        assert_eq!(entries[0].author.as_deref(), Some("notify@aur.archlinux.org"));
        let names = vec!["usbimager".to_string(), "kwi".to_string(), "image".to_string(), "perl-devel-nytprof".to_string()];
        let m = mentions_in(&entries, &names);
        let found: Vec<(&str, &str)> = m.iter().map(|x| (x.pkgbase.as_str(), x.subject.as_str())).collect();
        assert_eq!(
            found,
            [("usbimager", "[PRQ#88998] Adoption Request for usbimager"), ("perl-devel-nytprof", "[PRQ#88993] Orphan Request for perl-devel-nytprof Accepted")],
            "kwi is too short, image is inside usbimager: {m:#?}"
        );
        assert!(m[0].excerpt.contains("filed an adoption request for usbimager"), "{:?}", m[0].excerpt);
        assert_eq!(find_word("foo-git is not foo", "foo"), Some(15));
        assert_eq!(find_word("libfoo", "foo"), None);
    }
}
