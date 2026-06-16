//! Minimal `~/.ssh/config` resolver. Looks up a `Host` alias and returns the
//! `HostName`/`User`/`Port`/`IdentityFile` dsync needs at connect time (see specs/transport.md).
//!
//! This is intentionally a small subset of OpenSSH's config grammar: line-based `keyword value`
//! (or `keyword=value`) directives grouped under `Host <patterns>` blocks, with `*`/`?` globs and
//! `!` negation. Per OpenSSH semantics the **first** obtained value for each keyword wins, so a
//! more specific block placed before a wildcard block takes precedence. Keywords other than the
//! four we consume are ignored, as are `Match`/`Include` (treated as no-ops).

use std::path::PathBuf;

/// Values resolved for a host alias. All fields are `None` when the file is absent/unreadable or
/// no block matches — callers fall back to the literal token and built-in defaults.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
}

/// Read `~/.ssh/config` and resolve `alias`. Never errors — a missing or malformed file yields
/// `HostConfig::default()`.
pub fn resolve(alias: &str) -> HostConfig {
    let path = match dirs::home_dir() {
        Some(h) => h.join(".ssh").join("config"),
        None => return HostConfig::default(),
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_and_resolve(&text, alias),
        Err(_) => HostConfig::default(),
    }
}

/// Core parser, split out from filesystem access so it is unit-testable.
fn parse_and_resolve(text: &str, alias: &str) -> HostConfig {
    let mut result = HostConfig::default();
    // Directives before any `Host` line apply to all hosts (OpenSSH treats the implicit leading
    // scope as `Host *`).
    let mut in_match = true;

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match split_kv(line) {
            Some(kv) => kv,
            None => continue,
        };
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                in_match = host_block_matches(value, alias);
            }
            "match" => {
                // Unsupported; never matches so its directives are skipped.
                in_match = false;
            }
            _ if !in_match => {}
            "hostname" => set_first(&mut result.hostname, value),
            "user" => set_first(&mut result.user, value),
            "port" if result.port.is_none() => {
                if let Ok(p) = value.parse::<u16>() {
                    result.port = Some(p);
                }
            }
            "identityfile" if result.identity_file.is_none() => {
                result.identity_file = Some(expand_tilde(value));
            }
            _ => {}
        }
    }
    result
}

/// True if `alias` matches the whitespace-separated `Host` patterns: at least one positive
/// pattern matches and no negated (`!`) pattern matches.
fn host_block_matches(patterns: &str, alias: &str) -> bool {
    let mut matched = false;
    for pat in patterns.split_whitespace() {
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_match(neg, alias) {
                return false;
            }
        } else if glob_match(pat, alias) {
            matched = true;
        }
    }
    matched
}

/// Shell-style wildcard match supporting `*` (any run) and `?` (single char).
fn glob_match(pattern: &str, s: &str) -> bool {
    fn helper(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => helper(&p[1..], s) || (!s.is_empty() && helper(p, &s[1..])),
            Some(b'?') => !s.is_empty() && helper(&p[1..], &s[1..]),
            Some(&c) => !s.is_empty() && s[0] == c && helper(&p[1..], &s[1..]),
        }
    }
    helper(pattern.as_bytes(), s.as_bytes())
}

/// Split a directive into `(keyword, value)`. OpenSSH accepts whitespace or `=` (optionally
/// surrounded by whitespace) as the separator; the value may be double-quoted.
fn split_kv(line: &str) -> Option<(&str, &str)> {
    let sep = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let key = &line[..sep];
    if key.is_empty() {
        return None;
    }
    let value = line[sep..]
        .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
        .trim();
    let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
    if value.is_empty() {
        return None;
    }
    Some((key, value))
}

/// Drop an unquoted trailing `#` comment. (Quoted `#` is rare in practice; we keep this simple.)
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

fn set_first(slot: &mut Option<String>, value: &str) {
    if slot.is_none() {
        *slot = Some(value.to_string());
    }
}

/// Expand a leading `~` or `~/` to the user's home directory.
fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# global default
User globaluser

Host myvm
    HostName 10.0.0.5
    User deploy
    Port 2222
    IdentityFile ~/.ssh/myvm_key

Host *.internal
    User intern

Host prod !secret.prod
    HostName prod.example.com
";

    #[test]
    fn resolves_exact_alias() {
        let c = parse_and_resolve(SAMPLE, "myvm");
        assert_eq!(c.hostname.as_deref(), Some("10.0.0.5"));
        // OpenSSH "first obtained value wins": the global `User` at the top of the file is seen
        // before the block's `User`, so it wins (the classic reason `Host *` defaults go last).
        assert_eq!(c.user.as_deref(), Some("globaluser"));
        assert_eq!(c.port, Some(2222));
        assert!(c.identity_file.unwrap().ends_with(".ssh/myvm_key"));
    }

    #[test]
    fn block_user_wins_when_no_earlier_global() {
        let cfg = "Host myvm\n  User deploy\n  HostName 10.0.0.5\nHost *\n  User fallback\n";
        let c = parse_and_resolve(cfg, "myvm");
        assert_eq!(c.user.as_deref(), Some("deploy"));
    }

    #[test]
    fn unknown_alias_inherits_only_global() {
        let c = parse_and_resolve(SAMPLE, "nope");
        assert_eq!(c.hostname, None);
        assert_eq!(c.user.as_deref(), Some("globaluser"));
        assert_eq!(c.port, None);
    }

    #[test]
    fn wildcard_block_matches() {
        let c = parse_and_resolve(SAMPLE, "db.internal");
        assert_eq!(c.user.as_deref(), Some("globaluser")); // global User obtained first
        assert_eq!(c.hostname, None);
    }

    #[test]
    fn negation_excludes_host() {
        assert!(host_block_matches("prod !secret.prod", "prod"));
        assert!(!host_block_matches("prod !secret.prod", "secret.prod"));
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*.internal", "db.internal"));
        assert!(!glob_match("*.internal", "db.external"));
        assert!(glob_match("h?st", "host"));
    }

    #[test]
    fn equals_separator_and_quotes() {
        let c = parse_and_resolve("Host x\nHostName=\"example.com\"\n", "x");
        assert_eq!(c.hostname.as_deref(), Some("example.com"));
    }
}
