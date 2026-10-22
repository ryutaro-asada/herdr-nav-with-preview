//! Directory candidates and fuzzy ranking for the "new workspace" picker.
//!
//! When the user adds a workspace (`N`), they pick the directory to create it
//! in from a fuzzy list. Candidates come from (in order, de-duplicated):
//!   1. the currently-selected pane's cwd (so "here" is always first),
//!   2. the cwds of every open pane (projects you already work in),
//!   3. `zoxide query -l` if zoxide is installed,
//!   4. sibling directories of the open cwds (e.g. other projects next to an
//!      open one),
//!   5. directories under configured roots, scanned up to `depth` levels
//!      (descending stops at git repositories).
//!
//! Config lives at `<HERDR_PLUGIN_CONFIG_DIR>/config.toml`:
//! ```toml
//! [new_workspace]
//! roots = ["~/repos", "~/job"]   # optional; overrides the default roots
//! depth = 2                      # optional; scan depth under each root
//! ```
//!
//! Matching is fzf-style: every query character must appear in order
//! (case-insensitive), and results are ranked by a score that rewards
//! consecutive characters, word boundaries, and matches in the last path
//! segment. Space-separated terms must all match. Paths are matched in their
//! `~`-abbreviated form so the home prefix doesn't produce noise matches.
//!
//! The pure helpers are unit tested; filesystem/zoxide/config reads are thin
//! wrappers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Default roots probed when no config is present. Only existing ones are
/// scanned.
const DEFAULT_ROOTS: &[&str] = &[
    "~/repos", "~/src", "~/work", "~/dev", "~/projects", "~/code", "~/go/src", "~/ghq",
];

/// Default scan depth under each root (children = 1, grandchildren = 2).
const DEFAULT_DEPTH: usize = 2;

/// Upper bound on scanned directories, to keep the picker responsive.
const MAX_SCANNED: usize = 5000;

/// Directory names never descended into or listed.
const SKIP_NAMES: &[&str] = &["node_modules", "target", "Library", "vendor", "dist", "build"];

/// Expand a leading `~` / `~/` to the user's home directory. Other paths are
/// returned unchanged. Pure given `home`.
pub fn expand_tilde_with(path: &str, home: Option<&str>) -> String {
    match (path, home) {
        ("~", Some(h)) => h.to_string(),
        (p, Some(h)) if p.starts_with("~/") => format!("{}/{}", h.trim_end_matches('/'), &p[2..]),
        _ => path.to_string(),
    }
}

/// Abbreviate the home prefix to `~` for display and matching. Pure.
pub fn abbreviate_home(path: &str, home: Option<&str>) -> String {
    if let Some(h) = home {
        let h = h.trim_end_matches('/');
        if !h.is_empty() {
            if path == h {
                return "~".to_string();
            }
            if let Some(rest) = path.strip_prefix(&format!("{h}/")) {
                return format!("~/{rest}");
            }
        }
    }
    path.to_string()
}

/// Find the value of `key = ...` (first occurrence) in simple TOML text.
fn toml_value<'a>(config_toml: &'a str, key: &str) -> Option<&'a str> {
    for line in config_toml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return Some(rest.trim());
            }
        }
    }
    None
}

/// Parse the `roots` list from the plugin config TOML text. Returns the
/// configured roots (tilde-expanded) or `None` when absent/empty. Minimal
/// single-line parser (no TOML dependency).
pub fn parse_roots(config_toml: &str, home: Option<&str>) -> Option<Vec<String>> {
    let rest = toml_value(config_toml, "roots")?;
    let (start, end) = (rest.find('[')?, rest.rfind(']')?);
    let inner = &rest[start + 1..end];
    let roots: Vec<String> = inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').trim())
        .filter(|s| !s.is_empty())
        .map(|s| expand_tilde_with(s, home))
        .collect();
    if roots.is_empty() {
        None
    } else {
        Some(roots)
    }
}

/// Parse the `depth` integer from the plugin config TOML text.
pub fn parse_depth(config_toml: &str) -> Option<usize> {
    let v = toml_value(config_toml, "depth")?;
    v.split_whitespace().next()?.parse().ok()
}

/// Order + de-duplicate candidate directories, preserving first-seen order.
/// `current` (if any) is placed first. Pure.
pub fn order_candidates(current: Option<&str>, others: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |s: &str| {
        let t = s.trim_end_matches('/');
        if !t.is_empty() && seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    };
    if let Some(c) = current {
        push(c);
    }
    for o in others {
        push(o);
    }
    out
}

// --- fuzzy scoring ---------------------------------------------------------

const SCORE_MATCH: i64 = 16;
const BONUS_CONSECUTIVE: i64 = 18;
const BONUS_BOUNDARY: i64 = 12;
const BONUS_BASENAME: i64 = 10;
const BONUS_BASENAME_START: i64 = 15;
const PENALTY_GAP_MAX: i64 = 8;

fn is_boundary(prev: char) -> bool {
    matches!(prev, '/' | '-' | '_' | '.' | ' ')
}

/// Score a set of matched char positions within `hay`.
fn score_positions(hay: &[char], pos: &[usize], base_start: usize) -> i64 {
    let mut score = 0;
    for (k, &i) in pos.iter().enumerate() {
        score += SCORE_MATCH;
        if i == 0 || is_boundary(hay[i - 1]) {
            score += BONUS_BOUNDARY;
        } else if hay[i - 1].is_lowercase() && hay[i].is_uppercase() {
            score += BONUS_BOUNDARY / 2;
        }
        if i >= base_start {
            score += BONUS_BASENAME;
        }
        if k == 0 && i == base_start {
            score += BONUS_BASENAME_START;
        }
        if k > 0 {
            let gap = (i - pos[k - 1] - 1) as i64;
            if gap == 0 {
                score += BONUS_CONSECUTIVE;
            } else {
                score -= gap.min(PENALTY_GAP_MAX);
            }
        }
    }
    score
}

/// Best-scoring match of one query term in `hay`, as (score, positions).
fn score_term(hay: &[char], lower: &[char], term: &[char], base_start: usize) -> Option<(i64, Vec<usize>)> {
    if term.is_empty() {
        return Some((0, Vec::new()));
    }
    let n = lower.len();
    let m = term.len();
    let mut best: Option<(i64, Vec<usize>)> = None;
    let consider = |pos: Vec<usize>, best: &mut Option<(i64, Vec<usize>)>| {
        let s = score_positions(hay, &pos, base_start);
        if best.as_ref().map_or(true, |(b, _)| s > *b) {
            *best = Some((s, pos));
        }
    };
    // Contiguous occurrences (strongest signal).
    if m <= n {
        for start in 0..=n - m {
            if lower[start..start + m] == *term {
                consider((start..start + m).collect(), &mut best);
            }
        }
    }
    // Subsequence matches, greedy forward from every possible start.
    for start in 0..n {
        if lower[start] != term[0] {
            continue;
        }
        let mut pos = vec![start];
        let mut j = 1;
        let mut i = start + 1;
        while j < m && i < n {
            if lower[i] == term[j] {
                pos.push(i);
                j += 1;
            }
            i += 1;
        }
        if j < m {
            // A later start can only match a subset; stop.
            break;
        }
        consider(pos, &mut best);
    }
    best
}

/// fzf-style fuzzy score of `hay` against `query`. Space-separated terms must
/// all match (case-insensitive subsequence). Returns `(score, matched char
/// positions)`, or `None` when it doesn't match. Empty query matches with
/// score 0. Pure.
pub fn fuzzy_score(hay: &str, query: &str) -> Option<(i64, Vec<usize>)> {
    let terms: Vec<Vec<char>> = query
        .split_whitespace()
        .map(|t| t.to_lowercase().chars().collect())
        .collect();
    if terms.is_empty() {
        return Some((0, Vec::new()));
    }
    let chars: Vec<char> = hay.chars().collect();
    let lower: Vec<char> = hay.to_lowercase().chars().collect();
    if lower.len() != chars.len() {
        // Lowercasing changed the length (rare Unicode); fall back to a simple
        // check on the lowered string so positions stay consistent.
        return fuzzy_score(&hay.to_lowercase(), query);
    }
    let trimmed_len = chars.iter().rposition(|&c| c != '/').map_or(0, |i| i + 1);
    let base_start = chars[..trimmed_len]
        .iter()
        .rposition(|&c| c == '/')
        .map_or(0, |i| i + 1);
    let mut total = 0;
    let mut positions: Vec<usize> = Vec::new();
    for term in &terms {
        let (s, p) = score_term(&chars, &lower, term, base_start)?;
        total += s;
        positions.extend(p);
    }
    positions.sort_unstable();
    positions.dedup();
    Some((total, positions))
}

/// Does `hay` fuzzy-match `query`? Pure.
#[cfg(test)]
pub fn fuzzy_match(hay: &str, query: &str) -> bool {
    fuzzy_score(hay, query).is_some()
}

/// A ranked candidate: index into the input list, score, and matched char
/// positions (for highlighting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub index: usize,
    pub score: i64,
    pub positions: Vec<usize>,
}

/// Rank `items` against `query`: matches only, best score first; ties keep
/// shorter strings first, then original order. Empty query keeps the original
/// order. Pure.
pub fn rank(items: &[String], query: &str) -> Vec<Ranked> {
    let mut out: Vec<Ranked> = items
        .iter()
        .enumerate()
        .filter_map(|(index, s)| {
            fuzzy_score(s, query).map(|(score, positions)| Ranked {
                index,
                score,
                positions,
            })
        })
        .collect();
    if !query.trim().is_empty() {
        out.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(items[a.index].len().cmp(&items[b.index].len()))
                .then(a.index.cmp(&b.index))
        });
    }
    out
}

/// Filter + rank candidates matching `query` (best first). Pure.
#[cfg(test)]
pub fn filter_candidates(candidates: &[String], query: &str) -> Vec<String> {
    rank(candidates, query)
        .into_iter()
        .map(|r| candidates[r.index].clone())
        .collect()
}

// --- filesystem collection --------------------------------------------------

fn skip_dir(path: &Path) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.starts_with('.') || SKIP_NAMES.contains(&name),
        None => true,
    }
}

/// Scan directories under `roots` up to `depth` levels (1 = direct children).
/// Hidden and build/dependency dirs are skipped; descending stops at git
/// repositories (a dir containing `.git` is listed but not descended into).
/// Output order: per root, breadth-first, sorted within each level.
pub fn scan_roots(roots: &[String], depth: usize) -> Vec<String> {
    let mut out = Vec::new();
    for root in roots {
        let mut level: Vec<PathBuf> = vec![PathBuf::from(root)];
        for _ in 0..depth {
            let mut next = Vec::new();
            for dir in &level {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                let mut children: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir() && !skip_dir(p))
                    .collect();
                children.sort();
                for child in children {
                    if out.len() >= MAX_SCANNED {
                        return out;
                    }
                    if let Some(s) = child.to_str() {
                        out.push(s.to_string());
                    }
                    if !child.join(".git").exists() {
                        next.push(child);
                    }
                }
            }
            level = next;
            if level.is_empty() {
                break;
            }
        }
    }
    out
}

/// Parent directories of `cwds` that are worth scanning for siblings: skips
/// the filesystem root, the home directory, and ancestors of home (too broad).
/// Pure given `home`.
pub fn sibling_roots(cwds: &[String], home: Option<&str>) -> Vec<String> {
    let home = home.map(|h| h.trim_end_matches('/').to_string());
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for c in cwds {
        let Some(parent) = Path::new(c.trim_end_matches('/')).parent() else {
            continue;
        };
        let Some(p) = parent.to_str() else {
            continue;
        };
        let p = p.trim_end_matches('/');
        let too_broad = match home.as_deref() {
            Some(h) => h == p || h.starts_with(&format!("{p}/")),
            None => false,
        };
        if p.is_empty() || too_broad {
            continue;
        }
        if seen.insert(p.to_string()) {
            out.push(p.to_string());
        }
    }
    out
}

/// Resolved scan settings: roots and depth (config overrides defaults).
pub fn resolve_scan_settings() -> (Vec<String>, usize) {
    let home = std::env::var("HOME").ok();
    let mut roots: Option<Vec<String>> = None;
    let mut depth = DEFAULT_DEPTH;
    if let Ok(dir) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        if let Ok(text) = std::fs::read_to_string(PathBuf::from(dir).join("config.toml")) {
            roots = parse_roots(&text, home.as_deref());
            if let Some(d) = parse_depth(&text) {
                depth = d;
            }
        }
    }
    let roots = roots.unwrap_or_else(|| {
        DEFAULT_ROOTS
            .iter()
            .map(|r| expand_tilde_with(r, home.as_deref()))
            .filter(|p| PathBuf::from(p).is_dir())
            .collect()
    });
    (roots, depth)
}

/// Build the full candidate list for the picker (absolute paths). See the
/// module docs for the order. Side-effectful (zoxide + fs).
pub fn collect_candidates(current_cwd: Option<&str>, open_cwds: &[String]) -> Vec<String> {
    let home = std::env::var("HOME").ok();
    let mut others: Vec<String> = open_cwds.to_vec();
    others.extend(crate::herdr_client::zoxide_dirs());
    others.extend(scan_roots(&sibling_roots(open_cwds, home.as_deref()), 1));
    let (roots, depth) = resolve_scan_settings();
    others.extend(scan_roots(&roots, depth));
    order_candidates(current_cwd, &others)
        .into_iter()
        .filter(|p| Path::new(p).is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn expands_tilde() {
        assert_eq!(expand_tilde_with("~", Some("/home/u")), "/home/u");
        assert_eq!(expand_tilde_with("~/a/b", Some("/home/u")), "/home/u/a/b");
        assert_eq!(expand_tilde_with("/abs", Some("/home/u")), "/abs");
        assert_eq!(expand_tilde_with("rel", Some("/home/u")), "rel");
        assert_eq!(expand_tilde_with("~/a", None), "~/a");
    }

    #[test]
    fn abbreviates_home() {
        assert_eq!(abbreviate_home("/home/u/repos/x", Some("/home/u")), "~/repos/x");
        assert_eq!(abbreviate_home("/home/u", Some("/home/u/")), "~");
        assert_eq!(abbreviate_home("/home/user2/x", Some("/home/u")), "/home/user2/x");
        assert_eq!(abbreviate_home("/opt/x", None), "/opt/x");
    }

    #[test]
    fn parses_roots_and_depth() {
        let toml = "[new_workspace]\nroots = [\"~/repos\", \"/opt/src\"]\ndepth = 3\n";
        assert_eq!(parse_roots(toml, Some("/home/u")).unwrap(), s(&["/home/u/repos", "/opt/src"]));
        assert_eq!(parse_depth(toml), Some(3));
        assert_eq!(parse_depth("roots = []"), None);
    }

    #[test]
    fn parses_roots_single_quotes_and_spaces() {
        let toml = "roots=[ '~/a' , \"~/b\" ]";
        assert_eq!(parse_roots(toml, Some("/h")).unwrap(), s(&["/h/a", "/h/b"]));
    }

    #[test]
    fn missing_or_empty_roots_is_none() {
        assert!(parse_roots("name = \"x\"\n", Some("/h")).is_none());
        assert!(parse_roots("roots = []\n", Some("/h")).is_none());
    }

    #[test]
    fn orders_current_first_and_dedups() {
        let out = order_candidates(Some("/cwd/"), &s(&["/a", "/b/", "/a", "/cwd"]));
        assert_eq!(out, s(&["/cwd", "/a", "/b"]));
        assert_eq!(order_candidates(None, &s(&["/a", "/a", "/b"])), s(&["/a", "/b"]));
    }

    #[test]
    fn fuzzy_subsequence_basics() {
        assert!(fuzzy_match("~/repos/sptv-video", "sptv"));
        assert!(fuzzy_match("~/repos/sptv-video", "svid"));
        assert!(fuzzy_match("~/repos/sptv-video", "SPTV"));
        assert!(!fuzzy_match("~/repos/sptv-video", "xyz"));
        assert!(fuzzy_match("anything", ""));
    }

    #[test]
    fn multi_term_requires_all_terms() {
        assert!(fuzzy_match("~/job/tmp1/sptv-video", "job sptv"));
        assert!(!fuzzy_match("~/repos/sptv-video", "job sptv"));
    }

    #[test]
    fn ranks_basename_contiguous_match_first() {
        // The bug: "sptv" used to match these junk entries via the home
        // prefix and alphabetical order put them first.
        let items = s(&[
            "~/repos/jumpstart-vite",
            "~/repos/tview",
            "~/repos/partial-diff.nvim",
            "~/job/tmp1/sptv-video",
        ]);
        let ranked = filter_candidates(&items, "sptv");
        assert_eq!(ranked.first().map(String::as_str), Some("~/job/tmp1/sptv-video"));
        // Junk entries without the chars in order are filtered out.
        assert!(!ranked.contains(&"~/repos/tview".to_string()));
    }

    #[test]
    fn ranks_prefix_of_basename_highly() {
        let items = s(&[
            "~/repos/my-lazy-notes-archive",
            "~/repos/lazygit",
            "~/repos/lazydocker",
        ]);
        let ranked = filter_candidates(&items, "lazy");
        // Basename-prefix matches beat a mid-name match; shorter wins ties.
        assert_eq!(ranked[0], "~/repos/lazygit");
        assert_eq!(ranked[1], "~/repos/lazydocker");
        assert_eq!(ranked[2], "~/repos/my-lazy-notes-archive");
    }

    #[test]
    fn rank_prefers_word_boundary_initials() {
        let items = s(&["~/repos/herdr-plugin-examples", "~/repos/herder-nav-with-preview"]);
        let ranked = filter_candidates(&items, "hnwp");
        assert_eq!(ranked[0], "~/repos/herder-nav-with-preview");
    }

    #[test]
    fn empty_query_keeps_order() {
        let items = s(&["~/b", "~/a", "~/c"]);
        assert_eq!(filter_candidates(&items, ""), items);
    }

    #[test]
    fn positions_point_at_matched_chars() {
        let (_, pos) = fuzzy_score("~/repos/lazygit", "lazy").unwrap();
        let chars: Vec<char> = "~/repos/lazygit".chars().collect();
        let got: String = pos.iter().map(|&i| chars[i]).collect();
        assert_eq!(got, "lazy");
        // Contiguous in the basename, not scattered.
        assert_eq!(pos, vec![8, 9, 10, 11]);
    }

    #[test]
    fn sibling_roots_skip_home_and_root() {
        let cwds = s(&["/home/u/job/tmp1/sptv-video", "/home/u/memo", "/home/u", "/home/u/job/tmp1/nhk"]);
        // /home/u (home) and /home (ancestor of home) are too broad.
        assert_eq!(sibling_roots(&cwds, Some("/home/u")), s(&["/home/u/job/tmp1"]));
    }

    #[test]
    fn scan_respects_depth_hidden_and_git_repos() {
        let base = std::env::temp_dir().join(format!("hnp-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in [
            "root/proj-a/.git",
            "root/proj-a/inner",
            "root/group/proj-b",
            "root/group/proj-b/deep",
            "root/.hidden/x",
            "root/node_modules/y",
        ] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let root = base.join("root").to_str().unwrap().to_string();
        let got = scan_roots(&[root.clone()], 2);
        let rel: Vec<String> = got
            .iter()
            .map(|p| p.strip_prefix(&format!("{root}/")).unwrap().to_string())
            .collect();
        // depth 1: group, proj-a; depth 2: group/proj-b (proj-a is a git repo,
        // so not descended). Hidden/node_modules skipped; depth 3 not reached.
        assert_eq!(rel, s(&["group", "proj-a", "group/proj-b"]));
        let _ = std::fs::remove_dir_all(&base);
    }
}
