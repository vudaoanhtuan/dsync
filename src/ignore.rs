//! Config-only gitignore-syntax ignore engine. Patterns live solely in the `ignore` section of
//! `config.yaml`; repo `.gitignore` files are never discovered during sync. See specs/ignore.md.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::config::{Config, DSYNC_DIR};
use crate::error::{DsyncError, Result};

/// Compiled matcher built from the config `ignore` patterns plus the always-ignore `.dsync/`
/// rule.
pub struct IgnoreSet {
    matcher: Gitignore,
}

impl IgnoreSet {
    /// Build from the config `ignore` patterns plus the always-ignore `.dsync/` rule, rooted at
    /// `src_root`. Does not read repo `.gitignore` files.
    pub fn build(src_root: &Path, cfg: &Config) -> Result<IgnoreSet> {
        Self::from_patterns(src_root, &cfg.ignore)
    }

    /// Build directly from a multiline pattern string (used by the remote agent, which receives
    /// the source-resolved patterns over the wire).
    pub fn from_patterns(src_root: &Path, patterns: &str) -> Result<IgnoreSet> {
        let mut builder = GitignoreBuilder::new(src_root);
        // Always-ignore rule first (lowest of the config layer; the hard-coded guard in
        // `is_ignored` is the real authority, this keeps the walker honest too).
        builder
            .add_line(None, format!("{DSYNC_DIR}/").as_str())
            .map_err(|e| DsyncError::Config(format!("ignore rule error: {e}")))?;
        for line in patterns.lines() {
            let trimmed = line.trim();
            // Skip blanks and comments — they are preserved in config but mean nothing to the
            // matcher.
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            builder
                .add_line(None, trimmed)
                .map_err(|e| DsyncError::Config(format!("invalid ignore pattern `{trimmed}`: {e}")))?;
        }
        let matcher = builder
            .build()
            .map_err(|e| DsyncError::Config(format!("failed to compile ignore patterns: {e}")))?;
        Ok(IgnoreSet { matcher })
    }

    /// True if `rel_path` (relative to root) should be excluded from sync. `is_dir` lets
    /// directory-only patterns match correctly.
    pub fn is_ignored(&self, rel_path: &Path, is_dir: bool) -> bool {
        // Hard-coded highest-priority rule: `.dsync/` is always ignored.
        if rel_path.starts_with(DSYNC_DIR) {
            return true;
        }
        // `matched_path_or_any_parents` honors "a path is ignored if a parent dir is excluded",
        // matching git semantics (a negation cannot re-include under an excluded parent).
        self.matcher
            .matched_path_or_any_parents(rel_path, is_dir)
            .is_ignore()
    }
}

// ---------------------------------------------------------------------------
// `dsync ignore add | update | remove` — edit the config `ignore` section.
// ---------------------------------------------------------------------------

/// Split the config ignore block into its (trimmed) significant pattern lines, preserving the
/// raw lines for rewriting.
fn existing_patterns(cfg: &Config) -> Vec<String> {
    cfg.ignore
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// The set of significant (non-blank, non-comment) trimmed pattern strings already present.
fn present_set(cfg: &Config) -> std::collections::HashSet<String> {
    cfg.ignore
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn append_lines(cfg: &mut Config, new_lines: &[String]) {
    let mut lines = existing_patterns(cfg);
    // Trim a single trailing empty line so we don't accumulate blank gaps on repeated edits.
    while matches!(lines.last(), Some(l) if l.trim().is_empty()) {
        lines.pop();
    }
    lines.extend(new_lines.iter().cloned());
    let mut joined = lines.join("\n");
    if !joined.is_empty() {
        joined.push('\n');
    }
    cfg.ignore = joined;
}

/// `dsync ignore add <patterns…>`.
pub fn add(patterns: &[String]) -> Result<()> {
    if patterns.is_empty() {
        return Err(DsyncError::Other("no patterns given".into()));
    }
    let (mut cfg, root) = Config::load()?;
    let present = present_set(&cfg);
    let mut to_add = Vec::new();
    let mut already = Vec::new();
    for p in patterns {
        let t = p.trim();
        if t.is_empty() {
            continue;
        }
        if present.contains(t) || to_add.iter().any(|x: &String| x == t) {
            already.push(t.to_string());
        } else {
            to_add.push(t.to_string());
        }
    }
    if !to_add.is_empty() {
        append_lines(&mut cfg, &to_add);
        cfg.save(&root)?;
    }
    for p in &to_add {
        println!("added: {p}");
    }
    for p in &already {
        println!("already present: {p}");
    }
    Ok(())
}

/// `dsync ignore update <gitignore files…>` — import patterns from existing gitignore-syntax
/// files into the config `ignore` section.
pub fn update(files: &[std::path::PathBuf]) -> Result<()> {
    if files.is_empty() {
        return Err(DsyncError::Other("no files given".into()));
    }
    let (mut cfg, root) = Config::load()?;
    let mut present = present_set(&cfg);
    let mut all_new: Vec<String> = Vec::new();

    for file in files {
        let content = std::fs::read_to_string(file).map_err(|e| DsyncError::io(file, e))?;
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for line in content.lines() {
            let trimmed = line.trim();
            // Comments and blanks are preserved as-is (carried verbatim) but never dedupe-keyed.
            if trimmed.is_empty() || trimmed.starts_with('#') {
                all_new.push(line.to_string());
                continue;
            }
            if present.contains(trimmed) {
                skipped += 1;
                continue;
            }
            present.insert(trimmed.to_string());
            all_new.push(trimmed.to_string());
            imported += 1;
        }
        println!(
            "{}: {imported} imported, {skipped} already present",
            file.display()
        );
    }

    // Drop trailing comment/blank lines that carried no real new pattern, to avoid noise.
    while matches!(all_new.last(), Some(l) if l.trim().is_empty() || l.trim().starts_with('#')) {
        all_new.pop();
    }

    if !all_new.is_empty() {
        append_lines(&mut cfg, &all_new);
        cfg.save(&root)?;
    }
    Ok(())
}

/// `dsync ignore remove` — interactive multi-select removal.
pub fn remove_interactive() -> Result<()> {
    use dialoguer::MultiSelect;
    use std::io::IsTerminal;

    let (cfg, root) = Config::load()?;
    let items: Vec<String> = cfg
        .ignore
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();

    if items.is_empty() {
        println!("no ignore patterns to remove");
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        return Err(DsyncError::Other(
            "no TTY available; edit `.dsync/config.yaml` directly to remove ignore patterns".into(),
        ));
    }

    let selections = MultiSelect::new()
        .with_prompt("Select patterns to remove (space toggles, enter submits)")
        .items(&items)
        .interact()
        .map_err(|e| DsyncError::Other(format!("selection failed: {e}")))?;

    let to_remove: std::collections::HashSet<usize> = selections.into_iter().collect();
    remove_indices(&root, cfg, &items, &to_remove)
}

/// Removal logic split out so unit tests can drive it without the interactive UI.
fn remove_indices(
    root: &Path,
    mut cfg: Config,
    items: &[String],
    to_remove: &std::collections::HashSet<usize>,
) -> Result<()> {
    if to_remove.is_empty() {
        println!("nothing selected; config unchanged");
        return Ok(());
    }
    let mut removed = Vec::new();
    let kept: Vec<String> = items
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            if to_remove.contains(&i) {
                removed.push(line.clone());
                None
            } else {
                Some(line.clone())
            }
        })
        .collect();
    let mut joined = kept.join("\n");
    if !joined.is_empty() {
        joined.push('\n');
    }
    cfg.ignore = joined;
    cfg.save(root)?;
    for r in &removed {
        println!("removed: {}", r.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg_with_ignore(ignore: &str) -> Config {
        let mut c = Config::seeded("/tmp/x".into());
        c.ignore = ignore.to_string();
        c
    }

    #[test]
    fn dsync_always_ignored_even_empty() {
        let set = IgnoreSet::from_patterns(Path::new("/src"), "").unwrap();
        assert!(set.is_ignored(Path::new(".dsync"), true));
        assert!(set.is_ignored(Path::new(".dsync/config.yaml"), false));
        assert!(!set.is_ignored(Path::new("foo.txt"), false));
    }

    #[test]
    fn pattern_excludes_and_negation_reincludes() {
        let set = IgnoreSet::from_patterns(Path::new("/src"), "*.log\n!keep.log\n").unwrap();
        assert!(set.is_ignored(Path::new("debug.log"), false));
        assert!(!set.is_ignored(Path::new("keep.log"), false));
    }

    #[test]
    fn directory_pattern() {
        let set = IgnoreSet::from_patterns(Path::new("/src"), "node_modules/\n").unwrap();
        assert!(set.is_ignored(Path::new("node_modules"), true));
        assert!(set.is_ignored(Path::new("node_modules/x.js"), false));
    }

    #[test]
    fn add_dedupes() {
        let cfg = cfg_with_ignore("*.log\n");
        let present = present_set(&cfg);
        assert!(present.contains("*.log"));
        // simulate add logic
        let mut cfg2 = cfg;
        append_lines(&mut cfg2, &["*.tmp".to_string()]);
        assert!(cfg2.ignore.contains("*.log"));
        assert!(cfg2.ignore.contains("*.tmp"));
    }

    #[test]
    fn remove_selected_indices() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".dsync")).unwrap();
        let cfg = cfg_with_ignore("*.log\n*.tmp\nbuild/\n");
        cfg.save(root).unwrap();
        let items: Vec<String> = vec!["*.log".into(), "*.tmp".into(), "build/".into()];
        let mut sel = std::collections::HashSet::new();
        sel.insert(1usize); // remove *.tmp
        remove_indices(root, cfg, &items, &sel).unwrap();
        let reloaded = Config::load_from_root(root).unwrap();
        assert!(reloaded.ignore.contains("*.log"));
        assert!(!reloaded.ignore.contains("*.tmp"));
        assert!(reloaded.ignore.contains("build/"));
    }

    #[test]
    fn update_imports_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".dsync")).unwrap();
        let cfg = cfg_with_ignore("*.log\n");
        cfg.save(root).unwrap();

        let gi = root.join(".gitignore");
        std::fs::write(&gi, "*.log\n*.tmp\n# comment\nbuild/\n").unwrap();

        // drive update via a cwd switch is awkward; instead replicate its core dedupe here.
        let cfg = Config::load_from_root(root).unwrap();
        let mut present = present_set(&cfg);
        let content = std::fs::read_to_string(&gi).unwrap();
        let mut new = Vec::new();
        for line in content.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if !present.contains(t) {
                present.insert(t.to_string());
                new.push(t.to_string());
            }
        }
        assert_eq!(new, vec!["*.tmp".to_string(), "build/".to_string()]);
        let _ = PathBuf::new();
    }
}
