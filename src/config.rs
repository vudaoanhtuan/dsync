//! `.dsync/` layout, `config.yaml` (de)serialization, named remotes, remote-string parsing,
//! and the `init` / `remote add|remove|list` behaviors. See specs/config.md.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{DsyncError, Result};

pub const DSYNC_DIR: &str = ".dsync";
pub const CONFIG_FILE: &str = "config.yaml";
pub const GITIGNORE_FILE: &str = ".gitignore";

fn default_true() -> bool {
    true
}
fn default_level() -> i32 {
    3
}
fn default_delta_cap() -> u64 {
    536_870_912 // 512 MiB
}

/// The per-directory configuration, persisted as `.dsync/config.yaml`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    /// name -> raw path string (parsed lazily via `Remote::parse`). BTreeMap keeps a stable,
    /// sorted order for listing and serialization.
    pub remote: BTreeMap<String, String>,
    #[serde(default)]
    pub ignore: String,
    #[serde(default = "default_true")]
    pub compression: bool,
    #[serde(default = "default_level")]
    pub compression_level: i32,
    #[serde(default)]
    pub threads: usize,
    #[serde(default = "default_delta_cap")]
    pub delta_size_cap: u64,
}

impl Config {
    /// A fresh config seeded with a single `default` remote.
    pub(crate) fn seeded(default_path: String) -> Config {
        let mut remote = BTreeMap::new();
        remote.insert("default".to_string(), default_path);
        Config {
            remote,
            ignore: String::new(),
            compression: true,
            compression_level: 3,
            threads: 0,
            delta_size_cap: default_delta_cap(),
        }
    }

    fn validate(&self) -> Result<()> {
        if !(1..=19).contains(&self.compression_level) {
            return Err(DsyncError::Config(format!(
                "compression_level must be between 1 and 19, got {}",
                self.compression_level
            )));
        }
        for (name, raw) in &self.remote {
            if raw.trim().is_empty() {
                return Err(DsyncError::Config(format!(
                    "remote `{name}` has an empty path"
                )));
            }
        }
        Ok(())
    }

    /// Walk up from `cwd` to find the directory containing `.dsync/config.yaml` (so commands
    /// work from subdirectories of the source root, like git). Returns the loaded config and
    /// the **source root** (the directory containing `.dsync/`).
    pub fn load() -> Result<(Config, PathBuf)> {
        let cwd = std::env::current_dir().map_err(|e| DsyncError::io(".", e))?;
        let root = find_root(&cwd).ok_or(DsyncError::NotInitialized)?;
        let cfg = Config::load_from_root(&root)?;
        Ok((cfg, root))
    }

    pub fn load_from_root(root: &Path) -> Result<Config> {
        let path = config_path(root);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DsyncError::NotInitialized
            } else {
                DsyncError::io(&path, e)
            }
        })?;
        let cfg: Config = serde_yaml::from_str(&text)
            .map_err(|e| DsyncError::Config(format!("malformed {}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        self.validate()?;
        let path = config_path(root);
        let text = serde_yaml::to_string(self)
            .map_err(|e| DsyncError::Config(format!("failed to serialize config: {e}")))?;
        std::fs::write(&path, text).map_err(|e| DsyncError::io(&path, e))?;
        Ok(())
    }

    /// Resolve the selected remote name into a parsed `Remote`. `None` => the `default` entry.
    pub fn select_remote(&self, name: Option<&str>) -> Result<Remote> {
        match name {
            Some(n) => {
                let raw = self.remote.get(n).ok_or_else(|| {
                    DsyncError::Config(format!("no such remote: {n}"))
                })?;
                Remote::parse(raw)
            }
            None => {
                let raw = self.remote.get("default").ok_or_else(|| {
                    DsyncError::Config(
                        "no default remote; pass a name or run `dsync remote add <name> <path>`"
                            .to_string(),
                    )
                })?;
                Remote::parse(raw)
            }
        }
    }
}

pub fn config_path(root: &Path) -> PathBuf {
    root.join(DSYNC_DIR).join(CONFIG_FILE)
}

/// Walk up from `start` looking for an existing `.dsync/config.yaml`.
fn find_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if config_path(dir).is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// A sync target: either a local path or a remote SSH endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    Local {
        path: PathBuf,
    },
    Ssh {
        user: Option<String>,
        host: String,
        port: u16,
        path: PathBuf,
    },
}

impl Remote {
    /// Parse a remote string into a structured target. See specs/config.md, "Remote parsing".
    pub fn parse(s: &str) -> Result<Remote> {
        let s = s.trim();
        if s.is_empty() {
            return Err(DsyncError::Config("empty remote path".to_string()));
        }

        // ssh:// URL form (allows a custom port): ssh://[user@]host[:port]/path
        if let Some(rest) = s.strip_prefix("ssh://") {
            return parse_ssh_url(rest);
        }

        // Bracketed IPv6 host: [2001:db8::1]:/path  (optionally user@ prefix)
        if let Some(remote) = parse_bracketed_ipv6(s)? {
            return Ok(remote);
        }

        // `[user@]host:path` — only if the part before the first ':' is not an existing local
        // path and not a Windows drive letter.
        if let Some(idx) = s.find(':') {
            let before = &s[..idx];
            let after = &s[idx + 1..];
            let looks_like_host = !before.is_empty()
                && !is_windows_drive(before)
                && !Path::new(before).exists()
                && !before.contains('/');
            if looks_like_host {
                let (user, host) = split_user_host(before);
                return Ok(Remote::Ssh {
                    user,
                    host: host.to_string(),
                    port: 22,
                    path: PathBuf::from(after),
                });
            }
        }

        Ok(Remote::Local {
            path: PathBuf::from(s),
        })
    }

    #[allow(dead_code)]
    pub fn is_local(&self) -> bool {
        matches!(self, Remote::Local { .. })
    }
}

fn split_user_host(before: &str) -> (Option<String>, &str) {
    match before.split_once('@') {
        Some((u, h)) => (Some(u.to_string()), h),
        None => (None, before),
    }
}

fn is_windows_drive(before: &str) -> bool {
    // A single letter, e.g. "C" in "C:\path".
    before.len() == 1 && before.chars().next().unwrap().is_ascii_alphabetic()
}

fn parse_bracketed_ipv6(s: &str) -> Result<Option<Remote>> {
    // [user@] then '[host]:path'
    let (user, rest) = match s.split_once('@') {
        Some((u, r)) if r.starts_with('[') => (Some(u.to_string()), r),
        _ if s.starts_with('[') => (None, s),
        _ => return Ok(None),
    };
    let close = rest
        .find(']')
        .ok_or_else(|| DsyncError::Config(format!("unterminated IPv6 host in `{s}`")))?;
    let host = &rest[1..close];
    let after = &rest[close + 1..];
    let path = after.strip_prefix(':').unwrap_or(after);
    Ok(Some(Remote::Ssh {
        user,
        host: host.to_string(),
        port: 22,
        path: PathBuf::from(path),
    }))
}

fn parse_ssh_url(rest: &str) -> Result<Remote> {
    // [user@]host[:port]/path
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (user, hostport) = split_user_host(authority);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p
                .parse()
                .map_err(|_| DsyncError::Config(format!("invalid ssh port in `{rest}`")))?;
            (h.to_string(), port)
        }
        None => (hostport.to_string(), 22),
    };
    Ok(Remote::Ssh {
        user,
        host,
        port,
        path: PathBuf::from(path),
    })
}

/// `dsync init <path>`: create `.dsync/` in cwd, seeded with the `default` remote.
pub fn init(path: &str) -> Result<()> {
    let cwd = std::env::current_dir().map_err(|e| DsyncError::io(".", e))?;
    let dsync_dir = cwd.join(DSYNC_DIR);
    if config_path(&cwd).exists() {
        return Err(DsyncError::AlreadyInitialized);
    }
    let remote = Remote::parse(path)?;
    validate_not_self_sync(&cwd, &remote)?;

    std::fs::create_dir_all(&dsync_dir).map_err(|e| DsyncError::io(&dsync_dir, e))?;
    set_dir_mode(&dsync_dir, 0o755);

    let gitignore = dsync_dir.join(GITIGNORE_FILE);
    std::fs::write(&gitignore, "*\n").map_err(|e| DsyncError::io(&gitignore, e))?;

    let cfg = Config::seeded(path.trim().to_string());
    cfg.save(&cwd)?;

    println!("Initialized dsync in {}", config_path(&cwd).display());
    println!("  default -> {}", path.trim());
    Ok(())
}

/// Reject a local remote that resolves inside the source root (or vice versa).
pub fn validate_not_self_sync(src_root: &Path, remote: &Remote) -> Result<()> {
    if let Remote::Local { path } = remote {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            src_root.join(path)
        };
        let src = src_root;
        // Compare with best-effort canonicalization; fall back to the raw join for paths that
        // do not yet exist.
        let abs_c = abs.canonicalize().unwrap_or(abs.clone());
        let src_c = src.canonicalize().unwrap_or_else(|_| src.to_path_buf());
        if abs_c.starts_with(&src_c) || src_c.starts_with(&abs_c) {
            return Err(DsyncError::Config(format!(
                "remote path `{}` is inside the source directory; a directory cannot sync into itself",
                path.display()
            )));
        }
    }
    Ok(())
}

/// `dsync remote add <name> <path>`.
pub fn remote_add(name: &str, path: &str) -> Result<()> {
    let (mut cfg, root) = Config::load()?;
    if cfg.remote.contains_key(name) {
        return Err(DsyncError::Config(format!(
            "remote `{name}` already exists (remove it first to change its path)"
        )));
    }
    let remote = Remote::parse(path)?;
    validate_not_self_sync(&root, &remote)?;
    cfg.remote.insert(name.to_string(), path.trim().to_string());
    cfg.save(&root)?;
    println!("Added remote `{name}` -> {}", path.trim());
    Ok(())
}

/// `dsync remote remove <name>`.
pub fn remote_remove(name: &str) -> Result<()> {
    let (mut cfg, root) = Config::load()?;
    if cfg.remote.remove(name).is_none() {
        return Err(DsyncError::Config(format!("no such remote: {name}")));
    }
    cfg.save(&root)?;
    println!("Removed remote `{name}`");
    Ok(())
}

/// `dsync remote list`.
pub fn remote_list() -> Result<()> {
    let (cfg, _root) = Config::load()?;
    if cfg.remote.is_empty() {
        println!("(no remotes configured)");
        return Ok(());
    }
    for (name, path) in &cfg.remote {
        let marker = if name == "default" { " (default)" } else { "" };
        println!("{name}{marker} -> {path}");
    }
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn set_dir_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_local() {
        assert_eq!(
            Remote::parse("/backup/myproject").unwrap(),
            Remote::Local {
                path: PathBuf::from("/backup/myproject")
            }
        );
        assert_eq!(
            Remote::parse("relative/path").unwrap(),
            Remote::Local {
                path: PathBuf::from("relative/path")
            }
        );
    }

    #[test]
    fn parse_ssh_user_host() {
        assert_eq!(
            Remote::parse("tuan@server.com:/srv/app").unwrap(),
            Remote::Ssh {
                user: Some("tuan".into()),
                host: "server.com".into(),
                port: 22,
                path: PathBuf::from("/srv/app"),
            }
        );
    }

    #[test]
    fn parse_ssh_no_user_relative() {
        assert_eq!(
            Remote::parse("server.com:relative/path").unwrap(),
            Remote::Ssh {
                user: None,
                host: "server.com".into(),
                port: 22,
                path: PathBuf::from("relative/path"),
            }
        );
    }

    #[test]
    fn parse_ipv6() {
        assert_eq!(
            Remote::parse("[2001:db8::1]:/path").unwrap(),
            Remote::Ssh {
                user: None,
                host: "2001:db8::1".into(),
                port: 22,
                path: PathBuf::from("/path"),
            }
        );
    }

    #[test]
    fn parse_ssh_url_with_port() {
        assert_eq!(
            Remote::parse("ssh://user@host:2222/path").unwrap(),
            Remote::Ssh {
                user: Some("user".into()),
                host: "host".into(),
                port: 2222,
                path: PathBuf::from("/path"),
            }
        );
    }

    #[test]
    fn config_yaml_roundtrip() {
        let cfg = Config::seeded("/tmp/dest".into());
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let back: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.remote.get("default").unwrap(), "/tmp/dest");
        assert!(back.compression);
        assert_eq!(back.compression_level, 3);
        assert_eq!(back.delta_size_cap, 536_870_912);
    }

    #[test]
    fn defaults_applied_when_absent() {
        let yaml = "remote:\n  default: /tmp/x\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.compression);
        assert_eq!(cfg.compression_level, 3);
        assert_eq!(cfg.threads, 0);
        assert_eq!(cfg.ignore, "");
    }

    #[test]
    fn select_default_and_unknown() {
        let cfg = Config::seeded("/tmp/dest".into());
        assert!(cfg.select_remote(None).is_ok());
        assert!(cfg.select_remote(Some("nope")).is_err());
    }

    #[test]
    fn select_missing_default_errors() {
        let mut cfg = Config::seeded("/tmp/dest".into());
        cfg.remote.clear();
        assert!(cfg.select_remote(None).is_err());
    }

    #[test]
    fn compression_level_validation() {
        let mut cfg = Config::seeded("/tmp/dest".into());
        cfg.compression_level = 25;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn windows_drive_is_local() {
        let r = Remote::parse("C:/Users/foo").unwrap();
        assert!(matches!(r, Remote::Local { .. }));
    }
}
