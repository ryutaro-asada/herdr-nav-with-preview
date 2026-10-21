//! Directory candidates for the "new workspace" picker.
//!
//! When the user adds a workspace (`N`), they can pick the directory to create
//! it in from a fuzzy list. Candidates come from (in order):
//!   1. the currently-selected pane's cwd (so "here" is always first),
//!   2. `zoxide query -l` if zoxide is installed,
//!   3. the immediate children of configured root directories (fallback when
//!      zoxide is absent, e.g. `~/repos/*`, `~/src/*`).
//!
//! Config lives at `<HERDR_PLUGIN_CONFIG_DIR>/config.toml`:
//! ```toml
//! [new_workspace]
//! roots = ["~/repos", "~/work"]   # optional; overrides the defaults
//! ```
//! The pure helpers (expand, dedup/order, filtering, roots parsing) are unit
//! tested; the filesystem/zoxide/config reads are thin wrappers.

use std::collections::HashSet;
use std::path::PathBuf;

/// Default roots probed when no config and no zoxide are available. Only those
/// that exist are scanned.
const DEFAULT_ROOTS: &[&str] = &[
    "~/repos", "~/src", "~/work", "~/dev", "~/projects", "~/go/src", "~/ghq",
];

/// Expand a leading `~` / `~/` to the user's home directory. Other paths are
/// returned unchanged. Pure given `home`.
pub fn expand_tilde_with(path: &str, home: Option<&str>) -> String {
    match (path, home) {
        ("~", Some(h)) => h.to_string(),
        (p, Some(h)) if p.starts_with("~/") => format!("{}/{}", h.trim_end_matches('/'), &p[2..]),
        _ => path.to_string(),
    }
}

/// Expand `~` using the real `$HOME`.
#[allow(dead_code)]
pub fn expand_tilde(path: &str) -> String {
    expand_tilde_with(path, std::env::var("HOME").ok().as_deref())
}

/// Parse the `roots` list from the plugin config TOML text. Returns the
/// configured roots (tilde-expanded) or `None` when absent/empty, so the
/// caller can fall back to defaults. Minimal parser: looks for a `roots = [..]`
/// array under any section, without pulling in a TOML dependency beyond what we
/// already have (serde/serde_json only), so we parse by hand.
pub fn parse_roots(config_toml: &str, home: Option<&str>) -> Option<Vec<String>> {
    // Find a line like: roots = ["a", "b"]  (possibly spanning is not handled;
    // we keep it single-line for simplicity and document that in the README).
    for line in config_toml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("roots") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                if let (Some(start), Some(end)) = (rest.find('['), rest.rfind(']')) {
                    let inner = &rest[start + 1..end];
                    let roots: Vec<String> = inner
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').trim_matches('\'').trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| expand_tilde_with(s, home))
                        .collect();
                    if roots.is_empty() {
                        return None;
                    }
                    return Some(roots);
                }
            }
        }
    }
    None
}

/// Order + de-duplicate candidate directories, preserving first-seen order.
/// `current` (if any) is placed first. Pure.
pub fn order_candidates(current: Option<&str>, others: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push = |s: &str, out: &mut Vec<String>, seen: &mut HashSet<String>| {
        let t = s.trim_end_matches('/');
        if t.is_empty() {
            return;
        }
        if seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    };
    if let Some(c) = current {
        push(c, &mut out, &mut seen);
    }
    for o in others {
        push(o, &mut out, &mut seen);
    }
    out
}

/// Case-insensitive subsequence fuzzy match (fzf-style): every char of `query`
/// appears in `hay` in order. Empty query matches everything. Pure.
pub fn fuzzy_match(hay: &str, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let mut qi = q.chars().peekable();
    for hc in hay.to_lowercase().chars() {
        if let Some(&qc) = qi.peek() {
            if hc == qc {
                qi.next();
            }
        } else {
            break;
        }
    }
    qi.peek().is_none()
}

/// Filter + keep order of candidates matching `query`. Pure.
pub fn filter_candidates(candidates: &[String], query: &str) -> Vec<String> {
    candidates
        .iter()
        .filter(|c| fuzzy_match(c, query))
        .cloned()
        .collect()
}

/// Scan the immediate children of `roots` that are directories. Only existing
/// roots are read; unreadable ones are skipped. Not pure (touches the fs).
pub fn scan_roots(roots: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for root in roots {
        let rp = PathBuf::from(root);
        let Ok(entries) = std::fs::read_dir(&rp) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(s) = path.to_str() {
                    out.push(s.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// Resolve the configured roots: config `roots` if present, else the existing
/// `DEFAULT_ROOTS`. Reads the plugin config file when available.
pub fn resolve_roots() -> Vec<String> {
    let home = std::env::var("HOME").ok();
    if let Ok(dir) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        let cfg = PathBuf::from(dir).join("config.toml");
        if let Ok(text) = std::fs::read_to_string(&cfg) {
            if let Some(roots) = parse_roots(&text, home.as_deref()) {
                return roots;
            }
        }
    }
    DEFAULT_ROOTS
        .iter()
        .map(|r| expand_tilde_with(r, home.as_deref()))
        .filter(|p| PathBuf::from(p).is_dir())
        .collect()
}

/// Build the full candidate list for the picker: current cwd first, then
/// zoxide entries, then scanned roots (used mainly when zoxide is absent).
/// Side-effectful (zoxide + fs); composition of the pure/thin helpers above.
pub fn collect_candidates(current_cwd: Option<&str>) -> Vec<String> {
    let mut others = crate::herdr_client::zoxide_dirs();
    others.extend(scan_roots(&resolve_roots()));
    order_candidates(current_cwd, &others)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde() {
        assert_eq!(expand_tilde_with("~", Some("/home/u")), "/home/u");
        assert_eq!(expand_tilde_with("~/a/b", Some("/home/u")), "/home/u/a/b");
        assert_eq!(expand_tilde_with("/abs", Some("/home/u")), "/abs");
        assert_eq!(expand_tilde_with("rel", Some("/home/u")), "rel");
        // No home: leave as-is.
        assert_eq!(expand_tilde_with("~/a", None), "~/a");
    }

    #[test]
    fn parses_roots_array() {
        let toml = "[new_workspace]\nroots = [\"~/repos\", \"/opt/src\"]\n";
        let r = parse_roots(toml, Some("/home/u")).unwrap();
        assert_eq!(r, vec!["/home/u/repos", "/opt/src"]);
    }

    #[test]
    fn parses_roots_single_quotes_and_spaces() {
        let toml = "roots=[ '~/a' , \"~/b\" ]";
        let r = parse_roots(toml, Some("/h")).unwrap();
        assert_eq!(r, vec!["/h/a", "/h/b"]);
    }

    #[test]
    fn missing_or_empty_roots_is_none() {
        assert!(parse_roots("name = \"x\"\n", Some("/h")).is_none());
        assert!(parse_roots("roots = []\n", Some("/h")).is_none());
    }

    #[test]
    fn orders_current_first_and_dedups() {
        let others = vec![
            "/a".to_string(),
            "/b/".to_string(),
            "/a".to_string(), // dup
            "/cwd".to_string(),
        ];
        let out = order_candidates(Some("/cwd/"), &others);
        assert_eq!(out, vec!["/cwd", "/a", "/b"]);
    }

    #[test]
    fn order_without_current() {
        let others = vec!["/a".to_string(), "/a".to_string(), "/b".to_string()];
        assert_eq!(order_candidates(None, &others), vec!["/a", "/b"]);
    }

    #[test]
    fn fuzzy_subsequence() {
        assert!(fuzzy_match("/home/u/repos/sptv-video", "sptv"));
        assert!(fuzzy_match("/home/u/repos/sptv-video", "svid")); // subsequence
        assert!(fuzzy_match("/home/u/repos/sptv-video", "SPTV")); // case-insensitive
        assert!(!fuzzy_match("/home/u/repos/sptv-video", "xyz"));
        assert!(fuzzy_match("anything", "")); // empty matches
    }

    #[test]
    fn filters_candidates_in_order() {
        let c = vec![
            "/x/sptv-video".to_string(),
            "/x/nhk".to_string(),
            "/x/sptv-other".to_string(),
        ];
        let got = filter_candidates(&c, "sptv");
        assert_eq!(got, vec!["/x/sptv-video", "/x/sptv-other"]);
    }
}
