use std::iter::Rev;
use std::ops::Range;
use std::path::Path;
use std::{fs, path};

use glob::Pattern;
use strsim::normalized_levenshtein;

use crate::db::{Database, Dir, Epoch};
use crate::util::{self, MONTH};

pub struct Stream<'a> {
    db: &'a mut Database,
    idxs: Rev<Range<usize>>,
    options: StreamOptions,
}

impl<'a> Stream<'a> {
    pub fn new(db: &'a mut Database, options: StreamOptions) -> Self {
        db.sort_by_score(options.now);
        let idxs = (0..db.dirs().len()).rev();
        Stream { db, idxs, options }
    }

    pub fn next(&mut self) -> Option<&Dir<'_>> {
        while let Some(idx) = self.idxs.next() {
            let dir = &self.db.dirs()[idx];

            if !self.filter_by_keywords(&dir.path) {
                continue;
            }

            if !self.filter_by_base_dir(&dir.path) {
                continue;
            }

            if !self.filter_by_exclude(&dir.path) {
                self.db.swap_remove(idx);
                continue;
            }

            // Exists queries are slow, this should always be checked last.
            if !self.filter_by_exists(&dir.path) {
                if dir.last_accessed < self.options.ttl {
                    self.db.swap_remove(idx);
                }
                continue;
            }

            let dir = &self.db.dirs()[idx];
            return Some(dir);
        }

        None
    }

    fn filter_by_base_dir(&self, path: &str) -> bool {
        match &self.options.base_dir {
            Some(base_dir) => Path::new(path).starts_with(base_dir),
            None => true,
        }
    }

    fn filter_by_exclude(&self, path: &str) -> bool {
        !self.options.exclude.iter().any(|pattern| pattern.matches(path))
    }

    fn filter_by_exists(&self, path: &str) -> bool {
        if !self.options.exists {
            return true;
        }

        // The logic here is reversed - if we resolve symlinks when adding entries to
        // the database, we should not return symlinks when querying back from
        // the database.
        let resolver =
            if self.options.resolve_symlinks { fs::symlink_metadata } else { fs::metadata };
        resolver(path).map(|metadata| metadata.is_dir()).unwrap_or_default()
    }

    fn filter_by_keywords(&self, path: &str) -> bool {
        let (keywords_last, keywords) = match self.options.keywords.split_last() {
            Some(split) => split,
            None => return true,
        };

        let path_lower = util::to_lowercase(path);

        // First, try exact substring matching (fast path)
        if self.filter_by_keywords_exact(&path_lower, keywords_last, keywords) {
            return true;
        }

        // Fall back to fuzzy matching
        self.filter_by_keywords_fuzzy(&path_lower, keywords_last, keywords)
    }

    /// Exact substring matching (original behavior)
    fn filter_by_keywords_exact(
        &self,
        path: &str,
        keywords_last: &str,
        keywords: &[String],
    ) -> bool {
        let mut path = path;
        match path.rfind(keywords_last) {
            Some(idx) => {
                if path[idx + keywords_last.len()..].contains(path::is_separator) {
                    return false;
                }
                path = &path[..idx];
            }
            None => return false,
        }

        for keyword in keywords.iter().rev() {
            match path.rfind(keyword) {
                Some(idx) => path = &path[..idx],
                None => return false,
            }
        }

        true
    }

    /// Fuzzy matching using Jaro-Winkler similarity
    fn filter_by_keywords_fuzzy(
        &self,
        path: &str,
        keywords_last: &str,
        keywords: &[String],
    ) -> bool {
        // Skip fuzzy matching if any keyword contains a path separator
        // (path separators have special semantic meaning)
        if keywords_last.contains(path::is_separator)
            || keywords.iter().any(|k| k.contains(path::is_separator))
        {
            return false;
        }

        const FUZZY_THRESHOLD: f64 = 0.7;

        // Split path into components for fuzzy matching
        let components: Vec<&str> = path
            .split(path::is_separator)
            .filter(|s| !s.is_empty())
            .collect();

        if components.is_empty() {
            return false;
        }

        // Last keyword must fuzzy-match the last component
        let last_component = components.last().unwrap();
        if !Self::fuzzy_matches(keywords_last, last_component, FUZZY_THRESHOLD) {
            return false;
        }

        // Remaining keywords must match remaining components (in order, from back)
        let mut comp_idx = components.len() - 1;
        for keyword in keywords.iter().rev() {
            let mut found = false;
            while comp_idx > 0 {
                comp_idx -= 1;
                if Self::fuzzy_matches(keyword, components[comp_idx], FUZZY_THRESHOLD) {
                    found = true;
                    break;
                }
            }
            if !found {
                return false;
            }
        }

        true
    }

    /// Check if keyword fuzzy-matches a component
    fn fuzzy_matches(keyword: &str, component: &str, threshold: f64) -> bool {
        // Substring match
        if component.contains(keyword) {
            return true;
        }
        // Normalized Levenshtein similarity (1.0 = identical, 0.0 = completely different)
        normalized_levenshtein(keyword, component) >= threshold
    }
}

pub struct StreamOptions {
    /// The current time.
    now: Epoch,

    /// Only directories matching these keywords will be returned.
    keywords: Vec<String>,

    /// Directories that match any of these globs will be lazily removed.
    exclude: Vec<Pattern>,

    /// Directories will only be returned if they exist on the filesystem.
    exists: bool,

    /// Whether to resolve symlinks when checking if a directory exists.
    resolve_symlinks: bool,

    /// Directories that do not exist and haven't been accessed since TTL will
    /// be lazily removed.
    ttl: Epoch,

    /// Only return directories within this parent directory
    /// Does not check if the path exists
    base_dir: Option<String>,
}

impl StreamOptions {
    pub fn new(now: Epoch) -> Self {
        StreamOptions {
            now,
            keywords: Vec::new(),
            exclude: Vec::new(),
            exists: false,
            resolve_symlinks: false,
            ttl: now.saturating_sub(3 * MONTH),
            base_dir: None,
        }
    }

    pub fn with_keywords<I>(mut self, keywords: I) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        self.keywords = keywords.into_iter().map(util::to_lowercase).collect();
        self
    }

    pub fn with_exclude(mut self, exclude: Vec<Pattern>) -> Self {
        self.exclude = exclude;
        self
    }

    pub fn with_exists(mut self, exists: bool) -> Self {
        self.exists = exists;
        self
    }

    pub fn with_resolve_symlinks(mut self, resolve_symlinks: bool) -> Self {
        self.resolve_symlinks = resolve_symlinks;
        self
    }

    pub fn with_base_dir(mut self, base_dir: Option<String>) -> Self {
        self.base_dir = base_dir;
        self
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rstest::rstest;

    use super::*;

    #[rstest]
    // Case normalization
    #[case(&["fOo", "bAr"], "/foo/bar", true)]
    // Last component
    #[case(&["ba"], "/foo/bar", true)]
    #[case(&["fo"], "/foo/bar", false)]
    // Slash as suffix
    #[case(&["foo/"], "/foo", false)]
    #[case(&["foo/"], "/foo/bar", true)]
    #[case(&["foo/"], "/foo/bar/baz", false)]
    #[case(&["foo", "/"], "/foo", false)]
    #[case(&["foo", "/"], "/foo/bar", true)]
    #[case(&["foo", "/"], "/foo/bar/baz", true)]
    // Split components
    #[case(&["/", "fo", "/", "ar"], "/foo/bar", true)]
    #[case(&["oo/ba"], "/foo/bar", true)]
    // Overlap
    #[case(&["foo", "o", "bar"], "/foo/bar", false)]
    #[case(&["/foo/", "/bar"], "/foo/bar", false)]
    #[case(&["/foo/", "/bar"], "/foo/baz/bar", true)]
    // Fuzzy matching - typos in last component
    #[case(&["opensourec"], "/Projects/opensource", true)]  // 1 typo
    #[case(&["opensoruce"], "/Projects/opensource", true)]  // transposition
    #[case(&["prjects"], "/home/Projects", true)]           // typo matches last component
    #[case(&["prjects", "opensourec"], "/Projects/opensource", true)] // typos in both
    // Fuzzy matching - too different (should not match)
    #[case(&["xyz"], "/Projects/opensource", false)]
    #[case(&["completely_different"], "/foo/bar", false)]
    fn query(#[case] keywords: &[&str], #[case] path: &str, #[case] is_match: bool) {
        let db = &mut Database::new(PathBuf::new(), Vec::new(), |_| Vec::new(), false);
        let options = StreamOptions::new(0).with_keywords(keywords.iter());
        let stream = Stream::new(db, options);
        assert_eq!(is_match, stream.filter_by_keywords(path));
    }
}
