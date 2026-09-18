/// Wrap `s` in single quotes for safe inclusion in a shell command shown to the
/// user, escaping embedded single quotes.
#[must_use]
pub fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quotes_survive_spaces_and_quotes() {
        assert_eq!(
            sh_single_quote("/Users/a b/caffeinate2"),
            "'/Users/a b/caffeinate2'"
        );
        assert_eq!(sh_single_quote("it's"), r"'it'\''s'");
    }
}
