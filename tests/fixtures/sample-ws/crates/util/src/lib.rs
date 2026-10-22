pub fn describe(text: &str) -> String {
    format!("{} ({} words)", sample_core::GREETING, sample_core::count_words(text))
}
