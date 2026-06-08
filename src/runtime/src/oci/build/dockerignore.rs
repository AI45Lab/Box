//! `.dockerignore` support for the build context.
//!
//! Parses a context-root `.dockerignore` and decides which context paths are
//! excluded from `COPY`/`ADD`, matching Docker's behaviour closely enough for
//! real-world ignore files: `#` comments, blank lines, `!` negation
//! (last match wins), and glob patterns where `?` matches one non-`/`
//! character, `*` matches a run within a path segment, and `**` matches across
//! segments.
//!
//! Patterns are relative to the context root. A matched directory prunes its
//! whole subtree (the caller does not descend into it). Re-including a path
//! under a pruned directory via `!` is a known limitation (as it is in Docker).

use std::path::Path;

/// A single parsed ignore rule.
struct Rule {
    /// Pattern split into `/`-separated segments.
    segments: Vec<String>,
    /// `!`-prefixed rule that re-includes a previously excluded path.
    negated: bool,
}

/// Compiled `.dockerignore` matcher.
pub(crate) struct DockerIgnore {
    rules: Vec<Rule>,
}

impl DockerIgnore {
    /// Load `<context_dir>/.dockerignore`. Returns an empty (matches-nothing)
    /// matcher when the file is absent or unreadable.
    pub(crate) fn load(context_dir: &Path) -> Self {
        let contents =
            std::fs::read_to_string(context_dir.join(".dockerignore")).unwrap_or_default();
        Self::parse(&contents)
    }

    fn parse(contents: &str) -> Self {
        let mut rules = Vec::new();
        for raw in contents.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, body) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            // Normalise: drop a leading "./" and leading/trailing "/".
            let cleaned = body.strip_prefix("./").unwrap_or(body).trim_matches('/');
            if cleaned.is_empty() {
                continue;
            }
            let segments = cleaned.split('/').map(|s| s.to_string()).collect();
            rules.push(Rule { segments, negated });
        }
        Self { rules }
    }

    /// True when no rules were parsed (the common no-`.dockerignore` case).
    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `rel` (a path relative to the context root) is excluded.
    /// Last matching rule wins, so a later `!` rule can re-include.
    pub(crate) fn is_excluded(&self, rel: &Path) -> bool {
        let path_segs: Vec<&str> = rel
            .to_str()
            .unwrap_or_default()
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let mut excluded = false;
        for rule in &self.rules {
            if segments_match(&rule.segments, &path_segs) {
                excluded = !rule.negated;
            }
        }
        excluded
    }
}

/// Match `/`-separated pattern segments against path segments, with `**`
/// spanning zero or more segments.
fn segments_match(pattern: &[String], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((head, rest)) => {
            if head == "**" {
                // `**` consumes zero or more path segments.
                (0..=path.len()).any(|i| segments_match(rest, &path[i..]))
            } else {
                match path.split_first() {
                    Some((ph, ptail)) if wildcard_match(head, ph) => segments_match(rest, ptail),
                    _ => false,
                }
            }
        }
    }
}

/// Glob-match a single path segment: `*` matches any run of characters (not
/// `/`, which cannot occur in a segment), `?` matches exactly one character.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    // Classic two-pointer wildcard match with backtracking on `*`.
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < txt.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ign(s: &str) -> DockerIgnore {
        DockerIgnore::parse(s)
    }
    fn ex(d: &DockerIgnore, p: &str) -> bool {
        d.is_excluded(&PathBuf::from(p))
    }

    #[test]
    fn test_exact_and_subtree() {
        let d = ign(".git\nnode_modules\n.env\n");
        assert!(ex(&d, ".git"));
        assert!(ex(&d, "node_modules"));
        assert!(ex(&d, ".env"));
        // The directory itself matches; the caller prunes its subtree.
        assert!(!ex(&d, "src"));
        assert!(!ex(&d, "README.md"));
    }

    #[test]
    fn test_star_within_segment() {
        let d = ign("*.log\n");
        assert!(ex(&d, "app.log"));
        assert!(ex(&d, "x.log"));
        assert!(!ex(&d, "app.txt"));
        // `*` does not cross a path separator.
        assert!(!ex(&d, "logs/app.log"));
    }

    #[test]
    fn test_doublestar_crosses_segments() {
        let d = ign("**/__pycache__\n");
        assert!(ex(&d, "__pycache__"));
        assert!(ex(&d, "a/__pycache__"));
        assert!(ex(&d, "a/b/__pycache__"));
        assert!(!ex(&d, "a/cache"));
    }

    #[test]
    fn test_negation_last_match_wins() {
        let d = ign("*.log\n!keep.log\n");
        assert!(ex(&d, "app.log"));
        assert!(!ex(&d, "keep.log"));
    }

    #[test]
    fn test_comments_blanks_and_slashes() {
        let d = ign("# a comment\n\n/build/\n./tmp\n");
        assert!(ex(&d, "build"));
        assert!(ex(&d, "tmp"));
        assert!(d.rules.len() == 2);
    }

    #[test]
    fn test_question_mark() {
        let d = ign("file?.txt\n");
        assert!(ex(&d, "file1.txt"));
        assert!(ex(&d, "fileA.txt"));
        assert!(!ex(&d, "file10.txt"));
        assert!(!ex(&d, "file.txt"));
    }
}
