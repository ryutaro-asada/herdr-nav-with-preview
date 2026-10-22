//! Directory candidates and fuzzy ranking for the "new workspace" picker.
//!
//! When the user adds a workspace (`N`), they pick the directory to create it
//! in from a fuzzy list. Candidates come from (in order, de-duplicated):
//!   1. the currently-selected pane's cwd (so "here" is always first),
//!   2. the cwds of every open pane (projects you already work in),
//!   3. `zoxide query -l` if zoxide is installed,
//!   4. sibling directories of the open cwds,
//!   5. every directory under the scan roots (default: `$HOME`), walked up to
//!      `depth` levels with the `ignore` crate (hidden, `.gitignore`d and
//!      build/dependency dirs skipped) — like fzf's `Alt-C` / `fd -t d`.
//!
//! Config lives at `<HERDR_PLUGIN_CONFIG_DIR>/config.toml`:
//! ```toml
//! roots = ["~/repos", "~/job"]   # optional; overrides the default (~)
//! depth = 5                      # optional; walk depth under each root
//! ```
//!
//! Matching and scoring use `nucleo-matcher` (fzf's algorithm, as used by the
//! Helix editor) with path-aware bonuses and fzf query syntax. Paths are
//! matched in their `~`-abbreviated form so the home prefix doesn't produce
//! noise matches.
//!
//! The pure helpers are unit tested; filesystem/zoxide/config reads are thin
//! wrappers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Default roots probed when no config is present. Only existing ones are
/// scanned.
const DEFAULT_ROOTS: &[&str] = &["~"];

/// Default scan depth under each root (children = 1, grandchildren = 2).
const DEFAULT_DEPTH: usize = 5;

/// Upper bound on scanned directories, to keep the picker responsive.
const MAX_SCANNED: usize = 50_000;

/// Directory names never descended into or listed.
const SKIP_NAMES: &[&str] = &["node_modules", "target", "vendor", "dist", "build", "__pycache__"];

/// Directory names skipped only directly under `$HOME` (system/media folders
/// that are never project roots, and that macOS guards with privacy prompts).
const SKIP_HOME_CHILDREN: &[&str] = &[
    "Library", "Applications", "Movies", "Music", "Pictures", "Public",
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

// --- fuzzy ranking (nucleo-matcher) ----------------------------------------

/// Highlight positions are computed only for this many top results (the
/// visible list is never taller), keeping each keystroke cheap on large lists.
const HIGHLIGHT_LIMIT: usize = 500;

fn new_matcher() -> nucleo_matcher::Matcher {
    nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths())
}

fn parse_pattern(query: &str) -> nucleo_matcher::pattern::Pattern {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)
}

/// fzf-style fuzzy score of `hay` against `query`, using nucleo-matcher (the
/// fzf algorithm port used by the Helix editor). Supports fzf query syntax:
/// space-separated terms (all must match), `'exact`, `^prefix`, `suffix$`,
/// `!negation`; smart case. Returns `(score, sorted matched char positions)`,
/// or `None` when it doesn't match. Empty query matches with score 0. Pure.
#[cfg(test)]
pub fn fuzzy_score(hay: &str, query: &str) -> Option<(i64, Vec<usize>)> {
    let pattern = parse_pattern(query);
    let mut matcher = new_matcher();
    let mut buf = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    let score = pattern.indices(
        nucleo_matcher::Utf32Str::new(hay, &mut buf),
        &mut matcher,
        &mut idx,
    )?;
    idx.sort_unstable();
    idx.dedup();
    Some((score as i64, idx.into_iter().map(|i| i as usize).collect()))
}

/// Does `hay` fuzzy-match `query`? Pure.
#[cfg(test)]
pub fn fuzzy_match(hay: &str, query: &str) -> bool {
    fuzzy_score(hay, query).is_some()
}

/// A ranked candidate: index into the input list, score, and matched char
/// positions (for highlighting; empty beyond the top `HIGHLIGHT_LIMIT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub index: usize,
    pub score: i64,
    pub positions: Vec<usize>,
}

/// Rank `items` against `query`: matches only, best score first; ties keep
/// shorter strings first, then original order (like fzf's `--tiebreak`
/// length,index). Empty query keeps the original order. Pure.
pub fn rank(items: &[String], query: &str) -> Vec<Ranked> {
    let pattern = parse_pattern(query);
    let mut matcher = new_matcher();
    let mut buf = Vec::new();
    let mut out: Vec<Ranked> = items
        .iter()
        .enumerate()
        .filter_map(|(index, s)| {
            pattern
                .score(nucleo_matcher::Utf32Str::new(s, &mut buf), &mut matcher)
                .map(|score| Ranked {
                    index,
                    score: score as i64,
                    positions: Vec::new(),
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
        let mut idx: Vec<u32> = Vec::new();
        for r in out.iter_mut().take(HIGHLIGHT_LIMIT) {
            idx.clear();
            pattern.indices(
                nucleo_matcher::Utf32Str::new(&items[r.index], &mut buf),
                &mut matcher,
                &mut idx,
            );
            idx.sort_unstable();
            idx.dedup();
            r.positions = idx.iter().map(|&i| i as usize).collect();
        }
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

/// Should the walker skip (not list, not descend into) this directory?
/// `home` enables skipping macOS-style media/system folders directly under it.
fn skip_dir(path: &Path, home: Option<&Path>) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    if name.starts_with('.') || SKIP_NAMES.contains(&name) {
        return true;
    }
    match (home, path.parent()) {
        (Some(h), Some(parent)) => parent == h && SKIP_HOME_CHILDREN.contains(&name),
        _ => false,
    }
}

/// Walk directories under `roots` up to `depth` levels (1 = direct children)
/// with the `ignore` crate (ripgrep's parallel walker). Hidden dirs,
/// `.gitignore`d dirs and build/dependency dirs are skipped. Output is sorted
/// shallow-first, then by path, and capped at `MAX_SCANNED`.
pub fn scan_roots(roots: &[String], depth: usize) -> Vec<String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let roots: Vec<&String> = roots.iter().filter(|r| Path::new(r).is_dir()).collect();
    let Some((first, rest)) = roots.split_first() else {
        return Vec::new();
    };
    if depth == 0 {
        return Vec::new();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut builder = ignore::WalkBuilder::new(first);
    for r in rest {
        builder.add(r);
    }
    builder
        .max_depth(Some(depth))
        .hidden(true)
        .parents(false)
        .follow_links(false)
        .filter_entry(move |e| {
            !(e.file_type().is_some_and(|t| t.is_dir()) && e.depth() > 0 && skip_dir(e.path(), home.as_deref()))
        });

    let found: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());
    let count = AtomicUsize::new(0);
    builder.build_parallel().run(|| {
        Box::new(|entry| {
            let Ok(e) = entry else {
                return ignore::WalkState::Continue;
            };
            if e.depth() == 0 || !e.file_type().is_some_and(|t| t.is_dir()) {
                return ignore::WalkState::Continue;
            }
            if count.fetch_add(1, Ordering::Relaxed) >= MAX_SCANNED {
                return ignore::WalkState::Quit;
            }
            if let Some(s) = e.path().to_str() {
                found.lock().unwrap().push((e.depth(), s.to_string()));
            }
            ignore::WalkState::Continue
        })
    });
    let mut found = found.into_inner().unwrap();
    found.sort();
    found.into_iter().map(|(_, p)| p).collect()
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
        // Smart case (like fzf): uppercase in the query makes it case-sensitive.
        assert!(!fuzzy_match("~/repos/sptv-video", "SPTV"));
        assert!(fuzzy_match("~/repos/nvim/LazyVim", "LazyV"));
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
    fn fzf_query_syntax() {
        let items = s(&["~/job/tmp1/sptv-video", "~/job/tmp1/sptv-video/cms", "~/repos/sptv"]);
        // `$` anchors to the end, `^` to the start, `!` negates.
        assert_eq!(filter_candidates(&items, "video$"), s(&["~/job/tmp1/sptv-video"]));
        assert_eq!(filter_candidates(&items, "^~/repos"), s(&["~/repos/sptv"]));
        assert_eq!(
            filter_candidates(&items, "sptv !job"),
            s(&["~/repos/sptv"])
        );
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
    fn scan_respects_depth_and_skips_hidden_and_deps() {
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
        // Shallow first, then by path. Hidden/node_modules skipped (also
        // `.git`); depth 3 (`group/proj-b/deep`) not reached.
        assert_eq!(rel, s(&["group", "proj-a", "group/proj-b", "proj-a/inner"]));
        let _ = std::fs::remove_dir_all(&base);
    }
}
