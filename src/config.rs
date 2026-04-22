//! Configuration loading, mirroring isso/isso.cfg and isso/config.py.
//!
//! We parse the same INI file the Python version reads so operators can run
//! isso against an unchanged deployment config.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const DEFAULT_SALT: &str = "Eech7co8Ohloopo9Ol6baimi";

#[derive(Debug, Clone)]
pub struct Config {
    pub general: General,
    pub admin: Admin,
    pub moderation: Moderation,
    pub server: Server,
    pub smtp: Smtp,
    pub guard: Guard,
    pub markup: Markup,
    pub hash: Hash,
    pub rss: Rss,
}

#[derive(Debug, Clone)]
pub struct General {
    pub dbpath: String,
    pub name: String,
    pub hosts: Vec<String>,
    pub max_age: Duration,
    pub notify: Vec<String>,
    pub reply_notifications: bool,
    pub log_file: String,
    pub gravatar: bool,
    pub gravatar_url: String,
    pub latest_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Admin {
    pub enabled: bool,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct Moderation {
    pub enabled: bool,
    pub approve_if_email_previously_approved: bool,
    pub purge_after: Duration,
}

#[derive(Debug, Clone)]
pub struct Server {
    pub listen: String,
    pub public_endpoint: String,
    pub reload: bool,
    pub profile: bool,
    pub trusted_proxies: Vec<String>,
    pub samesite: Option<String>,
    /// Directory containing the static assets (js/, css/, img/, demo/) the
    /// admin HTML and the JS client reference. Empty string = no static
    /// serving (useful if a reverse proxy hosts the assets). Defaults to
    /// the `static/` sibling of the working directory.
    pub static_dir: String,
}

#[derive(Debug, Clone, Default)]
pub struct Smtp {
    pub username: String,
    pub password: String,
    pub host: String,
    pub port: u16,
    pub security: String,
    pub to: String,
    pub from: String,
    pub timeout: u64,
}

#[derive(Debug, Clone)]
pub struct Guard {
    pub enabled: bool,
    pub ratelimit: u32,
    pub direct_reply: u32,
    pub reply_to_self: bool,
    pub require_author: bool,
    pub require_email: bool,
}

#[derive(Debug, Clone)]
pub struct Markup {
    pub renderer: String,
    pub allowed_elements: Vec<String>,
    pub allowed_attributes: Vec<String>,
    pub mistune_plugins: Vec<String>,
    pub mistune_parameters: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Hash {
    pub salt: String,
    pub algorithm: String,
}

#[derive(Debug, Clone)]
pub struct Rss {
    pub base: String,
    pub limit: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General {
                dbpath: "/tmp/comments.db".into(),
                name: String::new(),
                hosts: vec!["http://localhost:8080/".into()],
                max_age: Duration::from_secs(15 * 60),
                notify: Vec::new(),
                reply_notifications: false,
                log_file: String::new(),
                gravatar: false,
                gravatar_url: "https://www.gravatar.com/avatar/{}?d=identicon&s=55".into(),
                latest_enabled: false,
            },
            admin: Admin {
                enabled: false,
                password: "please_choose_a_strong_password".into(),
            },
            moderation: Moderation {
                enabled: false,
                approve_if_email_previously_approved: false,
                purge_after: Duration::from_secs(30 * 24 * 60 * 60),
            },
            server: Server {
                listen: "http://localhost:8080".into(),
                public_endpoint: String::new(),
                reload: false,
                profile: false,
                trusted_proxies: Vec::new(),
                samesite: None,
                static_dir: "static".into(),
            },
            smtp: Smtp {
                host: "localhost".into(),
                port: 587,
                security: "starttls".into(),
                timeout: 10,
                ..Default::default()
            },
            guard: Guard {
                enabled: true,
                ratelimit: 2,
                direct_reply: 3,
                reply_to_self: false,
                require_author: false,
                require_email: false,
            },
            markup: Markup {
                renderer: "mistune".into(),
                allowed_elements: Vec::new(),
                allowed_attributes: Vec::new(),
                mistune_plugins: vec![
                    "strikethrough".into(),
                    "subscript".into(),
                    "superscript".into(),
                ],
                mistune_parameters: vec!["escape".into(), "hard_wrap".into()],
            },
            hash: Hash {
                salt: DEFAULT_SALT.into(),
                algorithm: "pbkdf2".into(),
            },
            rss: Rss {
                base: String::new(),
                limit: 100,
            },
        }
    }
}

impl Config {
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Self::parse(&raw)
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let ini = Ini::load_from_str(s)?;
        Self::from_ini(ini)
    }

    fn from_ini(ini: Ini) -> anyhow::Result<Self> {
        // Expand $VAR / ${VAR} in every value before merging, matching Python's
        // `IssoParser.get` which runs os.path.expandvars on every lookup.
        let ini = expand_ini_env_vars(ini);
        let mut cfg = Self::default();
        cfg.merge_ini(&ini)?;
        Ok(cfg)
    }

    fn merge_ini(&mut self, ini: &Ini) -> anyhow::Result<()> {
        if let Some(s) = ini.section(Some("general")) {
            if let Some(v) = s.get("dbpath") {
                self.general.dbpath = v.into();
            }
            if let Some(v) = s.get("name") {
                self.general.name = v.into();
            }
            if let Some(v) = s.get("host") {
                self.general.hosts = v
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
            }
            if let Some(v) = s.get("max-age") {
                self.general.max_age = parse_timedelta(v)?;
            }
            if let Some(v) = s.get("notify") {
                self.general.notify = split_commas(v);
            }
            if let Some(v) = s.get("reply-notifications") {
                self.general.reply_notifications = parse_bool(v)?;
            }
            if let Some(v) = s.get("log-file") {
                self.general.log_file = v.into();
            }
            if let Some(v) = s.get("gravatar") {
                self.general.gravatar = parse_bool(v)?;
            }
            if let Some(v) = s.get("gravatar-url") {
                self.general.gravatar_url = v.into();
            }
            if let Some(v) = s.get("latest-enabled") {
                self.general.latest_enabled = parse_bool(v)?;
            }
        }
        if let Some(s) = ini.section(Some("admin")) {
            if let Some(v) = s.get("enabled") {
                self.admin.enabled = parse_bool(v)?;
            }
            if let Some(v) = s.get("password") {
                self.admin.password = v.into();
            }
        }
        if let Some(s) = ini.section(Some("moderation")) {
            if let Some(v) = s.get("enabled") {
                self.moderation.enabled = parse_bool(v)?;
            }
            if let Some(v) = s.get("approve-if-email-previously-approved") {
                self.moderation.approve_if_email_previously_approved = parse_bool(v)?;
            }
            if let Some(v) = s.get("purge-after") {
                self.moderation.purge_after = parse_timedelta(v)?;
            }
        }
        if let Some(s) = ini.section(Some("server")) {
            if let Some(v) = s.get("listen") {
                self.server.listen = v.into();
            }
            if let Some(v) = s.get("public-endpoint") {
                self.server.public_endpoint = v.into();
            }
            if let Some(v) = s.get("reload") {
                self.server.reload = parse_bool(v)?;
            }
            if let Some(v) = s.get("profile") {
                self.server.profile = parse_bool(v)?;
            }
            if let Some(v) = s.get("trusted-proxies") {
                self.server.trusted_proxies = v
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
            }
            if let Some(v) = s.get("samesite") {
                let v = v.trim();
                self.server.samesite = if v.is_empty() { None } else { Some(v.into()) };
            }
            if let Some(v) = s.get("static-dir") {
                self.server.static_dir = v.into();
            }
        }
        if let Some(s) = ini.section(Some("smtp")) {
            if let Some(v) = s.get("username") {
                self.smtp.username = v.into();
            }
            if let Some(v) = s.get("password") {
                self.smtp.password = v.into();
            }
            if let Some(v) = s.get("host") {
                self.smtp.host = v.into();
            }
            if let Some(v) = s.get("port") {
                self.smtp.port = v.parse()?;
            }
            if let Some(v) = s.get("security") {
                self.smtp.security = v.into();
            }
            if let Some(v) = s.get("to") {
                self.smtp.to = v.into();
            }
            if let Some(v) = s.get("from") {
                self.smtp.from = v.into();
            }
            if let Some(v) = s.get("timeout") {
                self.smtp.timeout = v.parse()?;
            }
        }
        if let Some(s) = ini.section(Some("guard")) {
            if let Some(v) = s.get("enabled") {
                self.guard.enabled = parse_bool(v)?;
            }
            if let Some(v) = s.get("ratelimit") {
                self.guard.ratelimit = v.parse()?;
            }
            if let Some(v) = s.get("direct-reply") {
                self.guard.direct_reply = v.parse()?;
            }
            if let Some(v) = s.get("reply-to-self") {
                self.guard.reply_to_self = parse_bool(v)?;
            }
            if let Some(v) = s.get("require-author") {
                self.guard.require_author = parse_bool(v)?;
            }
            if let Some(v) = s.get("require-email") {
                self.guard.require_email = parse_bool(v)?;
            }
        }
        if let Some(s) = ini.section(Some("markup")) {
            if let Some(v) = s.get("renderer") {
                self.markup.renderer = v.into();
            }
            if let Some(v) = s.get("allowed-elements") {
                self.markup.allowed_elements = split_commas(v);
            }
            if let Some(v) = s.get("allowed-attributes") {
                self.markup.allowed_attributes = split_commas(v);
            }
        }
        if let Some(s) = ini.section(Some("markup.mistune")) {
            if let Some(v) = s.get("plugins") {
                self.markup.mistune_plugins = split_commas(v);
            }
            if let Some(v) = s.get("parameters") {
                self.markup.mistune_parameters = split_commas(v);
            }
        }
        if let Some(s) = ini.section(Some("hash")) {
            if let Some(v) = s.get("salt") {
                self.hash.salt = v.into();
            }
            if let Some(v) = s.get("algorithm") {
                self.hash.algorithm = v.into();
            }
        }
        if let Some(s) = ini.section(Some("rss")) {
            if let Some(v) = s.get("base") {
                self.rss.base = v.into();
            }
            if let Some(v) = s.get("limit") {
                self.rss.limit = v.parse()?;
            }
        }
        Ok(())
    }
}

fn split_commas(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Walk every value in the Ini file and substitute env vars in place.
fn expand_ini_env_vars(mut ini: Ini) -> Ini {
    for section in ini.sections.values_mut() {
        for value in section.values_mut() {
            let expanded = expand_env_vars(value);
            if expanded != *value {
                *value = expanded;
            }
        }
    }
    ini
}

/// A tiny Python-`RawConfigParser`-compatible INI parser.
///
/// We rolled our own because the `rust-ini` and `configparser` crates both
/// fail on Python's indent-based multi-line values (the form Isso uses for
/// `host` and `trusted-proxies`). This covers exactly the dialect Isso's
/// config uses: `[section]` headers, `key = value` (or `key : value`) pairs,
/// `#`/`;` comments at line start only, and continuation lines that start
/// with whitespace and append to the previous option's value joined by `\n`
/// — the same shape CPython stores multi-line values in.
#[derive(Debug, Default)]
pub struct Ini {
    /// section name → (option name → value). `BTreeMap` keeps the API
    /// deterministic; insertion order doesn't matter for our consumers.
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl Ini {
    pub fn load_from_str(input: &str) -> anyhow::Result<Self> {
        let mut ini = Ini::default();
        let mut current_section: Option<String> = None;
        // (section, key) of the last option we emitted; new indented lines
        // append to this value.
        let mut last_option: Option<(String, String)> = None;

        for (lineno, line) in input.lines().enumerate() {
            let lineno = lineno + 1;
            let trimmed = line.trim_start();

            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                // Python's _read treats blank/comment lines as value terminators
                // too, so any following indented line must start a new option.
                last_option = None;
                continue;
            }

            // Continuation: indented line that isn't a section header or
            // comment. Append to the previous option's value.
            let is_indented = line.starts_with(|c: char| c.is_whitespace());
            if is_indented {
                if let Some((sec, key)) = &last_option {
                    let section = ini
                        .sections
                        .get_mut(sec)
                        .expect("last_option's section must exist");
                    let entry = section.get_mut(key).expect("last_option's key must exist");
                    entry.push('\n');
                    entry.push_str(trimmed);
                    continue;
                }
                anyhow::bail!("config line {lineno}: continuation line without a preceding option");
            }

            // Section header: [name]
            if let Some(rest) = trimmed.strip_prefix('[') {
                let name = rest
                    .strip_suffix(']')
                    .or_else(|| {
                        rest.split_once(']').map(|(n, trailing)| {
                            // Trailing text after ] is tolerated by ConfigParser only
                            // if it's whitespace; otherwise treat as malformed.
                            if trailing.trim().is_empty() {
                                n
                            } else {
                                ""
                            }
                        })
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("config line {lineno}: malformed section header")
                    })?;
                if name.is_empty() {
                    anyhow::bail!("config line {lineno}: malformed section header");
                }
                let name = name.trim().to_string();
                ini.sections.entry(name.clone()).or_default();
                current_section = Some(name);
                last_option = None;
                continue;
            }

            // key = value  or  key : value
            let Some(section_name) = current_section.as_deref() else {
                anyhow::bail!("config line {lineno}: option outside any section");
            };
            let sep_idx = trimmed.find(['=', ':']).ok_or_else(|| {
                anyhow::anyhow!("config line {lineno}: expected `key = value` or `key : value`")
            })?;
            let (raw_key, raw_val) = trimmed.split_at(sep_idx);
            // Skip the separator char itself.
            let raw_val = &raw_val[1..];
            let key = raw_key.trim().to_ascii_lowercase();
            if key.is_empty() {
                anyhow::bail!("config line {lineno}: empty option name");
            }
            let value = raw_val.trim().to_string();
            let section = ini.sections.entry(section_name.to_string()).or_default();
            section.insert(key.clone(), value);
            last_option = Some((section_name.to_string(), key));
        }

        Ok(ini)
    }

    pub fn section(&self, name: Option<&str>) -> Option<&BTreeMap<String, String>> {
        self.sections.get(name?)
    }
}

/// Reproduce Python's `os.path.expandvars`: substitute `$NAME` or `${NAME}`
/// from the process environment. Unknown names are left untouched.
/// Matches CPython's implementation: bare `$` with no valid identifier
/// following it is also left untouched.
pub fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // `${NAME}` form.
        if bytes.get(i + 1) == Some(&b'{') {
            if let Some(close) = input[i + 2..].find('}') {
                let name = &input[i + 2..i + 2 + close];
                match std::env::var(name) {
                    Ok(val) => out.push_str(&val),
                    Err(_) => out.push_str(&input[i..i + 2 + close + 1]),
                }
                i += 2 + close + 1;
                continue;
            }
            out.push('$');
            i += 1;
            continue;
        }
        // `$NAME` form — read an identifier [A-Za-z_][A-Za-z0-9_]*.
        let mut end = i + 1;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        if end == i + 1 {
            out.push('$');
            i += 1;
            continue;
        }
        let name = &input[i + 1..end];
        match std::env::var(name) {
            Ok(val) => out.push_str(&val),
            Err(_) => out.push_str(&input[i..end]),
        }
        i = end;
    }
    out
}

fn parse_bool(v: &str) -> anyhow::Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => anyhow::bail!("invalid boolean: {other}"),
    }
}

/// Parse Python's configparser timedelta syntax: bare seconds, or a
/// combination of the units Nw, Nd, Nh, Nm, Ns (e.g. "15m", "30d", "1h30m").
pub fn parse_timedelta(v: &str) -> anyhow::Result<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    for ch in v.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else if ch.is_whitespace() {
            continue;
        } else {
            let n: u64 = num
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid timedelta: {v}"))?;
            let mult = match ch {
                'w' | 'W' => 7 * 24 * 60 * 60,
                'd' | 'D' => 24 * 60 * 60,
                'h' | 'H' => 60 * 60,
                'm' | 'M' => 60,
                's' | 'S' => 1,
                other => anyhow::bail!("invalid timedelta unit '{other}' in '{v}'"),
            };
            total += n * mult;
            num.clear();
        }
    }
    if !num.is_empty() {
        anyhow::bail!("invalid timedelta: '{v}' has trailing digits without unit");
    }
    Ok(Duration::from_secs(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_python_defaults() {
        let c = Config::default();
        assert_eq!(c.general.max_age, Duration::from_secs(900));
        assert_eq!(c.guard.ratelimit, 2);
        assert_eq!(c.guard.direct_reply, 3);
        assert_eq!(c.hash.salt, "Eech7co8Ohloopo9Ol6baimi");
        assert_eq!(c.hash.algorithm, "pbkdf2");
        assert_eq!(c.rss.limit, 100);
    }

    #[test]
    fn parse_timedelta_covers_units() {
        assert_eq!(parse_timedelta("60").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_timedelta("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(
            parse_timedelta("30d").unwrap(),
            Duration::from_secs(30 * 86400)
        );
        assert_eq!(parse_timedelta("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(
            parse_timedelta("1w").unwrap(),
            Duration::from_secs(7 * 86400)
        );
    }

    #[test]
    fn env_var_expansion_applies_to_config_values() {
        // Use a test-scoped env var we set ourselves to keep the test
        // independent of the host environment.
        std::env::set_var("ISSO_TEST_DBPATH", "/mnt/volume/comments.db");
        let cfg = Config::parse("[general]\ndbpath = $ISSO_TEST_DBPATH\n").unwrap();
        assert_eq!(cfg.general.dbpath, "/mnt/volume/comments.db");

        // Braced form too.
        std::env::set_var("ISSO_TEST_SALT", "s3cret");
        let cfg = Config::parse("[hash]\nsalt = ${ISSO_TEST_SALT}\n").unwrap();
        assert_eq!(cfg.hash.salt, "s3cret");

        // Unknown names are left untouched (Python's expandvars behaviour).
        let cfg = Config::parse("[general]\ndbpath = $UNKNOWN_ABCXYZ\n").unwrap();
        assert_eq!(cfg.general.dbpath, "$UNKNOWN_ABCXYZ");
    }

    #[test]
    fn ini_overrides_defaults() {
        let cfg = Config::parse(
            "[general]\ndbpath = /var/lib/isso.db\nmax-age = 30m\n\n[guard]\nratelimit = 5\n",
        )
        .unwrap();
        assert_eq!(cfg.general.dbpath, "/var/lib/isso.db");
        assert_eq!(cfg.general.max_age, Duration::from_secs(1800));
        assert_eq!(cfg.guard.ratelimit, 5);
    }

    // ------------------------------------------------------------------- Ini

    #[test]
    fn ini_parses_multiline_host_like_python_configparser() {
        // The bug that motivated the custom parser: rust-ini and the
        // `configparser` crate both collapse indented continuation lines
        // into separate keys, so `host = \n    http://a/\n    http://b/`
        // came out as `host=""` + garbage siblings, leaving
        // `config.general.hosts` empty.
        let cfg =
            Config::parse("[general]\nhost =\n    http://a.example/\n    http://b.example/\n")
                .unwrap();
        assert_eq!(
            cfg.general.hosts,
            vec![
                "http://a.example/".to_string(),
                "http://b.example/".to_string(),
            ]
        );
    }

    #[test]
    fn ini_parses_multiline_trusted_proxies() {
        let cfg =
            Config::parse("[server]\ntrusted-proxies =\n    10.0.0.0/8\n    192.168.0.0/16\n")
                .unwrap();
        assert_eq!(
            cfg.server.trusted_proxies,
            vec!["10.0.0.0/8".to_string(), "192.168.0.0/16".to_string()]
        );
    }

    #[test]
    fn ini_ignores_comments_and_blank_lines() {
        let cfg = Config::parse(
            "\
# leading comment
; also a comment
[general]
# inside-section comment
dbpath = /tmp/x.db

; blank line above and comment here
name = Example
",
        )
        .unwrap();
        assert_eq!(cfg.general.dbpath, "/tmp/x.db");
        assert_eq!(cfg.general.name, "Example");
    }

    #[test]
    fn ini_accepts_colon_as_separator() {
        // Python's ConfigParser accepts both `key = value` and `key : value`.
        let cfg = Config::parse("[general]\ndbpath : /srv/comments.db\n").unwrap();
        assert_eq!(cfg.general.dbpath, "/srv/comments.db");
    }

    #[test]
    fn ini_option_keys_are_lowercased() {
        // Python ConfigParser lower-cases keys by default (optionxform).
        let ini = Ini::load_from_str("[general]\nDBPATH = /x.db\n").unwrap();
        let sec = ini.section(Some("general")).unwrap();
        assert_eq!(sec.get("dbpath"), Some(&"/x.db".to_string()));
        assert!(sec.get("DBPATH").is_none());
    }

    #[test]
    fn ini_continuation_without_preceding_option_is_error() {
        let err = Config::parse("[general]\n    orphan\n").unwrap_err();
        assert!(err.to_string().contains("continuation line"), "got: {err}");
    }

    #[test]
    fn ini_option_outside_section_is_error() {
        let err = Config::parse("dbpath = /x.db\n").unwrap_err();
        assert!(
            err.to_string().contains("outside any section"),
            "got: {err}"
        );
    }

    #[test]
    fn ini_blank_line_terminates_value_for_next_indented_line() {
        // A blank line between an option and the next indented line means
        // that line is NOT a continuation — it's an error (orphan
        // continuation).
        let err = Config::parse("[general]\ndbpath = /x.db\n\n    /y.db\n").unwrap_err();
        assert!(err.to_string().contains("continuation line"), "got: {err}");
    }
}
