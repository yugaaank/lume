//! Launcher search over the application index.
//!
//! ## Why nucleo
//!
//! Scoring a fuzzy match means deciding how much better `firefox` is than
//! `frefx` is from `f`, and getting that ordering wrong is the whole game.
//! Rolling a scorer means inventing an answer and then being surprised by it.
//! `nucleo-matcher` is the matcher behind skim and fzf's successor line, so
//! results order the way muscle memory already expects.
//!
//! `nucleo` — the wrapper with a parallel matcher — is the other option. It is
//! built for ranking hundreds of thousands of files in a background thread.
//! A launcher is searching a few hundred applications in well under a
//! millisecond, so the simpler matcher is the right size and keeps the
//! dependency smaller.
//!
//! ## Always-insensitive, deliberately not smart-case
//!
//! fzf, ripgrep and ripgrep's fzf-alikes use *smart case*: a lowercase query
//! matches anything, but a query containing an uppercase letter is matched
//! case-sensitively. It is a good rule for searching a large file list, where
//! lowercasing narrows hundreds of thousands of hits.
//!
//! Here it is actively harmful. The corpus is a few hundred applications, so
//! there is never too much to narrow — the only case smart case buys us is
//! "type it in caps because you did not know the capitalisation", and that
//! user is one of the two this shell is built for. Measured: `firefox` scores
//! `Firefox` at 192, while `FIREFOX` matches nothing at all.
//!
//! So matching ignores case outright. `Normalization::Smart` is kept: matching
//! accented text without the accent is a genuine convenience, and unlike case
//! it cannot surprise anyone.
//!
//! ## Two fields, one query
//!
//! Both the display name and the desktop-file id are matched. "kitty" finds
//! `kitty.desktop`; "zen.desktop" and "firefox firefox" also work, because a
//! user who knows the file name types the file name.
//!
//! ## An empty query lists everything
//!
//! That is what a launcher opened with no typing should show, and it is what
//! a naive user expects from clicking the clock.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

use crate::apps::AppEntry;

/// One search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub entry: AppEntry,
    pub score: u32,
}

/// The most hits returned. Enough to scroll, few enough that the list is not
/// an index dump.
pub const LIMIT: usize = 50;

/// Search the index.
///
/// `query` may be empty, in which case everything is returned in index order.
/// An empty query is not a special case that needs its own sort: the index is
/// already sorted by name, which is the right order for "show me all".
pub fn search(entries: &[AppEntry], query: &str, limit: usize) -> Vec<Hit> {
    if query.trim().is_empty() {
        return entries
            .iter()
            .take(limit)
            .map(|e| Hit { entry: e.clone(), score: 0 })
            .collect();
    }

    Searcher::new(query).run(entries, limit)
}

/// Score one haystack. `Utf32Str::new` needs a scratch buffer for non-ASCII
/// text, which is why this is a function rather than a call chain: the
/// buffer has to outlive the `Utf32Str` borrowing it, so it cannot be a
/// temporary inside the expression.
fn score_text(pattern: &Pattern, text: &str, scratch: &mut Vec<char>, matcher: &mut Matcher) -> Option<u32> {
    pattern.score(Utf32Str::new(text, scratch), matcher)
}

/// A ready-made matcher, for callers that search repeatedly with one pattern.
///
/// Built once per keystroke by the view; this exists so the construction and
/// the pattern parse are not re-done per candidate.
pub struct Searcher {
    pattern: Pattern,
    /// Scratch for `Utf32Str::new`, which cannot take a temporary. Reused
    /// across candidates so a search does not allocate per entry; only
    /// non-ASCII names ever touch it.
    scratch: Vec<char>,
}

impl Searcher {
    pub fn new(query: &str) -> Self {
        Self {
            pattern: Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart),
            scratch: Vec::new(),
        }
    }

    pub fn run(&mut self, entries: &[AppEntry], limit: usize) -> Vec<Hit> {
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut hits: Vec<Hit> = entries
            .iter()
            .filter_map(|entry| {
                // Name first: it is what the user sees and what they type. The
                // id is a fallback, not an equal partner — matching it as a
                // co-equal field lets "firefox" score `firefox-esr.desktop`
                // above `Firefox`.
                let score = score_text(&self.pattern, &entry.name, &mut self.scratch, &mut matcher)
                    .max(score_text(&self.pattern, &entry.id, &mut self.scratch, &mut matcher));
                score
                    .filter(|s| *s > 0)
                    .map(|score| Hit { entry: entry.clone(), score })
            })
            .collect();

        // Score descending, then name ascending. The tie-break matters:
        // without it, two apps scoring equally come back in filesystem order,
        // which is the order they were installed — arbitrary to the user and
        // unstable across reboots.
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.entry.name.to_lowercase().cmp(&b.entry.name.to_lowercase()))
        });
        hits.truncate(limit);
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, id: &str) -> AppEntry {
        AppEntry {
            id: id.into(),
            name: name.into(),
            generic_name: String::new(),
            exec: name.into(),
            icon: String::new(),
            terminal: false,
            path: std::path::PathBuf::from(id),
        }
    }

    fn corpus() -> Vec<AppEntry> {
        vec![
            app("Firefox", "firefox_firefox.desktop"),
            app("Zen Browser", "zen.desktop"),
            app("kitty", "kitty.desktop"),
            app("Text Editor", "org.gnome.TextEditor.desktop"),
            app("7zip", "7zip.desktop"),
        ]
    }

    fn names(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.entry.name.as_str()).collect()
    }

    #[test]
    fn an_empty_query_lists_everything() {
        let hits = search(&corpus(), "", 10);
        assert_eq!(hits.len(), 5);
        // Score is meaningless here and must not be used to reorder.
        assert!(hits.iter().all(|h| h.score == 0));
    }

    #[test]
    fn a_limit_truncates() {
        assert_eq!(search(&corpus(), "", 2).len(), 2);
        assert_eq!(search(&corpus(), "e", 1).len(), 1);
    }

    /// The core behaviour: a subsequence matches, in order, not necessarily
    /// adjacent.
    #[test]
    fn fuzzy_subsequences_match() {
        assert!(names(&search(&corpus(), "ffx", 10)).contains(&"Firefox"));
        // z, b, r — not "ztn": "Zen Browser" contains no 't', and a
        // subsequence matcher must say so rather than helpfully match anyway.
        assert!(names(&search(&corpus(), "zbr", 10)).contains(&"Zen Browser"));
        assert!(search(&corpus(), "ztn", 10).is_empty());
        assert!(names(&search(&corpus(), "kit", 10)).contains(&"kitty"));
    }

    /// Not smart-case: see the module comment. `FIREFOX` finding `Firefox` is
    /// the whole point of ignoring case, and it is the query a naive user
    /// types when they are unsure of the capitalisation.
    #[test]
    fn matching_ignores_case() {
        let hits = search(&corpus(), "FIREFOX", 10);
        assert!(names(&hits).contains(&"Firefox"));
        assert_eq!(hits[0].entry.name, "Firefox");
    }

    /// A digit-leading name is a real application, not a failed calculation.
    #[test]
    fn digit_leading_names_are_findable() {
        assert!(names(&search(&corpus(), "7zip", 10)).contains(&"7zip"));
        assert!(names(&search(&corpus(), "7z", 10)).contains(&"7zip"));
    }

    /// Accents are optional in the query but not in the name.
    #[test]
    fn matching_ignores_case_and_accents() {
        let unicode = vec![app("Café", "cafe.desktop")];
        for q in ["CAFE", "cafe", "café", "CafÉ"] {
            assert_eq!(search(&unicode, q, 10).len(), 1, "{q:?} did not match Café");
        }
    }

    /// Typing the desktop-file name is a legitimate thing to do.
    #[test]
    fn the_desktop_file_id_is_searchable_too() {
        let hits = search(&corpus(), "TextEditor", 10);
        assert!(names(&hits).contains(&"Text Editor"));
    }

    #[test]
    fn nothing_matches_nothing() {
        assert!(search(&corpus(), "qqqqzzz", 10).is_empty());
    }

    /// Without a name tie-break, equal scores come back in index order, which
    /// is filesystem order — arbitrary, and unstable across reboots.
    #[test]
    fn equal_scores_are_broken_by_name() {
        let hits = search(&corpus(), "e", 10);
        let scores: Vec<u32> = hits.iter().map(|h| h.score).collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(scores, sorted, "results are not in descending score order");

        // Within one score, names ascend.
        for pair in hits.windows(2) {
            if pair[0].score == pair[1].score {
                assert!(
                    pair[0].entry.name.to_lowercase() <= pair[1].entry.name.to_lowercase(),
                    "{} came before {} at the same score",
                    pair[0].entry.name,
                    pair[1].entry.name
                );
            }
        }
    }

    /// A run of the same letter must not blow up on a large corpus.
    #[test]
    fn a_degenerate_query_is_still_fast() {
        let many: Vec<AppEntry> = (0..2000)
            .map(|i| app(&format!("Application Number {i}"), &format!("app{i}.desktop")))
            .collect();
        let hits = search(&many, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 50);
        assert!(hits.len() <= 50);
    }

    /// The view builds one of these per keystroke rather than re-parsing the
    /// pattern for every candidate.
    #[test]
    fn the_searcher_agrees_with_the_convenience_function() {
        let corpus = corpus();
        let mut s = Searcher::new("kit");
        let via_searcher: Vec<String> = s.run(&corpus, 10).iter().map(|h| h.entry.name.clone()).collect();
        let via_fn: Vec<String> = search(&corpus, "kit", 10).iter().map(|h| h.entry.name.clone()).collect();
        assert_eq!(via_searcher, via_fn);
    }
}
