//! Changes here are what the cache-pool end-to-end test edits.

pub const GREETING: &str = "greeting v1";

pub fn count_words(text: &str) -> usize {
    regex::Regex::new(r"\w+").unwrap().find_iter(text).count()
}
