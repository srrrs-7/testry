//! Glob patterns over repository-relative paths.
//!
//! Semantics (fixed by the tests below, which act as the contract):
//! - patterns are matched against the whole normalized [`RepoPath`];
//! - `*` and `?` never cross a `/`;
//! - `**/x` matches `x` at any depth including the root, `x/**` matches
//!   everything below `x` but not `x` itself;
//! - matching is case-sensitive and dot files are not special;
//! - `{a,b}` alternation and `\` escaping are supported.

use std::fmt;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Serialize, Serializer};

use crate::paths::RepoPath;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    #[error("pattern is empty")]
    Empty,
    #[error("pattern {pattern:?} must be repository-relative: no leading `/`, no `..`, no NUL")]
    NotRepositoryRelative { pattern: String },
    #[error("pattern {pattern:?} is not a valid glob: {reason}")]
    InvalidGlob { pattern: String, reason: String },
}

/// An immutable set of glob patterns. The empty set matches nothing.
#[derive(Clone)]
pub struct PatternSet {
    sources: Vec<String>,
    set: GlobSet,
}

impl PatternSet {
    pub fn empty() -> Self {
        Self {
            sources: Vec::new(),
            set: GlobSet::empty(),
        }
    }

    /// Compiles patterns. A leading `./` is dropped; duplicates are kept once.
    pub fn try_new<I, S>(patterns: I) -> Result<Self, PatternError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut sources: Vec<String> = Vec::new();
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let source = normalize_pattern(pattern.as_ref())?;
            if sources.contains(&source) {
                continue;
            }
            let glob = GlobBuilder::new(&source)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .map_err(|error| PatternError::InvalidGlob {
                    pattern: source.clone(),
                    reason: error.kind().to_string(),
                })?;
            builder.add(glob);
            sources.push(source);
        }
        let set = builder.build().map_err(|error| PatternError::InvalidGlob {
            pattern: sources.join(", "),
            reason: error.kind().to_string(),
        })?;
        Ok(Self { sources, set })
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    pub fn is_match(&self, path: &RepoPath) -> bool {
        self.set.is_match(path.as_str())
    }

    /// The first configured pattern that matches, for explanations.
    pub fn first_match(&self, path: &RepoPath) -> Option<&str> {
        let index = self.set.matches(path.as_str()).into_iter().min()?;
        self.sources.get(index).map(String::as_str)
    }
}

impl Default for PatternSet {
    fn default() -> Self {
        Self::empty()
    }
}

impl PartialEq for PatternSet {
    fn eq(&self, other: &Self) -> bool {
        self.sources == other.sources
    }
}

impl Eq for PatternSet {}

impl fmt::Debug for PatternSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PatternSet").field(&self.sources).finish()
    }
}

impl Serialize for PatternSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.sources.serialize(serializer)
    }
}

fn normalize_pattern(raw: &str) -> Result<String, PatternError> {
    let mut pattern = raw;
    while let Some(rest) = pattern.strip_prefix("./") {
        pattern = rest;
    }
    if pattern.is_empty() {
        return Err(PatternError::Empty);
    }
    let escapes = pattern.starts_with('/')
        || pattern.contains('\0')
        || pattern.split('/').any(|segment| segment == "..");
    if escapes {
        return Err(PatternError::NotRepositoryRelative {
            pattern: raw.to_owned(),
        });
    }
    Ok(pattern.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(patterns: &[&str]) -> PatternSet {
        PatternSet::try_new(patterns).unwrap()
    }

    fn matches(patterns: &[&str], path: &str) -> bool {
        set(patterns).is_match(&RepoPath::try_new(path).unwrap())
    }

    #[test]
    fn double_star_prefix_matches_root_and_nested() {
        assert!(matches(&["**/package.json"], "package.json"));
        assert!(matches(&["**/package.json"], "apps/api/package.json"));
        assert!(!matches(&["**/package.json"], "apps/api/package.json.bak"));
        assert!(matches(&["**/.env"], ".env"));
        assert!(matches(&["**/.env.*"], "apps/web/.env.local"));
        assert!(!matches(&["**/.env.*"], "apps/web/.env"));
    }

    #[test]
    fn double_star_suffix_matches_descendants_only() {
        assert!(matches(&[".storybook/**"], ".storybook/main.ts"));
        assert!(matches(&[".storybook/**"], ".storybook/a/b/c.ts"));
        assert!(!matches(&[".storybook/**"], ".storybook"));
        assert!(!matches(&[".storybook/**"], "apps/web/.storybook/main.ts"));
        assert!(matches(
            &["**/.storybook/**"],
            "apps/web/.storybook/main.ts"
        ));
        assert!(matches(&["**/.storybook/**"], ".storybook/main.ts"));
        assert!(matches(
            &["**/node_modules/**"],
            "node_modules/react/index.js"
        ));
        assert!(matches(
            &["**/node_modules/**"],
            "apps/web/node_modules/x/y.js"
        ));
    }

    #[test]
    fn double_star_in_the_middle_matches_zero_or_more_directories() {
        let patterns = ["apps/api/**/*.test.ts"];
        assert!(matches(&patterns, "apps/api/user.test.ts"));
        assert!(matches(&patterns, "apps/api/src/deep/user.test.ts"));
        assert!(!matches(&patterns, "apps/web/user.test.ts"));
        assert!(!matches(&patterns, "apps/api/user.test.tsx"));
    }

    #[test]
    fn single_star_does_not_cross_directories() {
        assert!(matches(&["apps/*.ts"], "apps/a.ts"));
        assert!(!matches(&["apps/*.ts"], "apps/x/a.ts"));
        assert!(matches(
            &["**/tsconfig*.json"],
            "apps/web/tsconfig.node.json"
        ));
        assert!(!matches(&["tsconfig*.json"], "apps/web/tsconfig.json"));
        assert!(matches(&["**/Dockerfile*"], "docker/Dockerfile.dev"));
    }

    #[test]
    fn matching_is_case_sensitive_and_supports_alternation() {
        assert!(!matches(&["**/README.md"], "readme.md"));
        assert!(matches(
            &["**/vite.config.{ts,js}"],
            "apps/web/vite.config.ts"
        ));
        assert!(!matches(
            &["**/vite.config.{ts,js}"],
            "apps/web/vite.config.mjs"
        ));
    }

    #[test]
    fn special_characters_in_paths_are_literal() {
        assert!(matches(&["apps/**"], "apps/日本語 の/[id] page.tsx"));
        assert!(matches(&["apps/\\[id\\]/*.ts"], "apps/[id]/a.ts"));
        assert!(!matches(&["apps/\\[id\\]/*.ts"], "apps/i/a.ts"));
    }

    #[test]
    fn empty_set_matches_nothing() {
        assert!(PatternSet::empty().is_empty());
        assert!(!PatternSet::empty().is_match(&RepoPath::try_new("a").unwrap()));
    }

    #[test]
    fn rejects_escaping_and_invalid_patterns() {
        assert_eq!(PatternSet::try_new([""]).unwrap_err(), PatternError::Empty);
        assert_eq!(
            PatternSet::try_new(["./"]).unwrap_err(),
            PatternError::Empty
        );
        assert!(matches!(
            PatternSet::try_new(["/etc/**"]),
            Err(PatternError::NotRepositoryRelative { .. })
        ));
        assert!(matches!(
            PatternSet::try_new(["../other/**"]),
            Err(PatternError::NotRepositoryRelative { .. })
        ));
        assert!(matches!(
            PatternSet::try_new(["a/../../b"]),
            Err(PatternError::NotRepositoryRelative { .. })
        ));
        assert!(matches!(
            PatternSet::try_new(["a/[z"]),
            Err(PatternError::InvalidGlob { .. })
        ));
    }

    #[test]
    fn first_match_reports_the_earliest_configured_pattern() {
        let patterns = set(&["**/*.json", "**/package.json"]);
        let path = RepoPath::try_new("apps/package.json").unwrap();
        assert_eq!(patterns.first_match(&path), Some("**/*.json"));
        assert_eq!(
            patterns.first_match(&RepoPath::try_new("a.ts").unwrap()),
            None
        );
    }

    #[test]
    fn leading_dot_slash_and_duplicates_are_normalized() {
        let patterns = set(&["./apps/**", "apps/**"]);
        assert_eq!(patterns.sources(), ["apps/**"]);
        assert_eq!(serde_json::to_string(&patterns).unwrap(), "[\"apps/**\"]");
    }
}
