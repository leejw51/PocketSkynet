//! Tokenisation and hashtag extraction (docs/SEARCH.md).
//!
//! Everything here is deterministic string work with no allocation beyond the
//! outputs — it runs once per message written and once per query, never in a
//! loop over the corpus.
//!
//! # What counts as a hashtag
//!
//! `#` at the start of the text or after whitespace, followed by letters,
//! digits, `_` or `-`, containing at least one letter. That last clause keeps
//! `#1` and `#2026` out of the tag space (they are list markers and years),
//! and the "after whitespace" clause keeps URL fragments
//! (`https://a.example/page#section`) from becoming tags. Tags are stored
//! lowercased so `#Rust` and `#rust` are one tag; Hangul, kana and han have
//! no case and pass through unchanged.

/// A word for feature purposes: a maximal run of Unicode alphanumerics,
/// underscores excluded, lowercased. CJK runs are handled separately by the
/// embedder because they are not space-delimited.
pub fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Han, Hangul, Hiragana or Katakana — scripts where a "word" is not
/// space-delimited and character n-grams are the useful unit.
pub fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{4E00}'..='\u{9FFF}'     // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}'   // CJK Extension A
        | '\u{AC00}'..='\u{D7AF}'   // Hangul syllables
        | '\u{1100}'..='\u{11FF}'   // Hangul jamo
        | '\u{3040}'..='\u{309F}'   // Hiragana
        | '\u{30A0}'..='\u{30FF}'   // Katakana
    )
}

const MAX_TAG_CHARS: usize = 64;

/// Extract hashtags: lowercased, deduplicated, in order of first appearance.
pub fn hashtags(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let at_boundary = prev.is_none_or(char::is_whitespace);
        prev = Some(ch);
        if ch != '#' || !at_boundary {
            continue;
        }
        let mut tag = String::new();
        let mut has_letter = false;
        while let Some(&next) = chars.peek() {
            if next.is_alphanumeric() || next == '_' || next == '-' {
                has_letter |= next.is_alphabetic();
                tag.extend(next.to_lowercase());
                prev = Some(next);
                chars.next();
            } else {
                break;
            }
        }
        if has_letter
            && !tag.is_empty()
            && tag.chars().count() <= MAX_TAG_CHARS
            && !out.contains(&tag)
        {
            out.push(tag);
        }
    }
    out
}

/// Split a query into free text and the hashtags it filters by. `#rust bm25`
/// means "documents tagged #rust, ranked against 'bm25'" — the tag tokens do
/// not also compete as search terms.
pub fn split_query(query: &str) -> (String, Vec<String>) {
    let tags = hashtags(query);
    if tags.is_empty() {
        return (query.trim().to_owned(), tags);
    }
    let text = query
        .split_whitespace()
        .filter(|token| !token.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ");
    (text, tags)
}

/// Quote tokens into an FTS5 MATCH expression: every token becomes a quoted
/// phrase joined by OR, so user text can never be parsed as FTS5 syntax
/// (`AND`, `NEAR`, `*`, unbalanced quotes) and multi-word queries rank by
/// best term rather than demanding every term.
pub fn fts_query(text: &str) -> Option<String> {
    let words = words(text);
    if words.is_empty() {
        return None;
    }
    Some(
        words
            .iter()
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_lowercase_and_split_on_everything_else() {
        assert_eq!(words("Hello, World! x2"), ["hello", "world", "x2"]);
    }

    #[test]
    fn korean_text_is_tokenised_by_spaces() {
        assert_eq!(words("김치 만드는 법"), ["김치", "만드는", "법"]);
    }

    #[test]
    fn a_tag_is_found_at_start_middle_and_end() {
        assert_eq!(
            hashtags("#recipe kimchi #Cooking notes #food"),
            ["recipe", "cooking", "food"]
        );
    }

    #[test]
    fn a_url_fragment_is_not_a_tag() {
        assert_eq!(
            hashtags("see https://a.example/page#section for more"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn numeric_only_tags_are_list_markers_not_tags() {
        assert_eq!(hashtags("#1 first #2026 plan #b2b"), ["b2b"]);
    }

    #[test]
    fn korean_tags_survive_unchanged() {
        assert_eq!(hashtags("#김치 담그기 #요리"), ["김치", "요리"]);
    }

    #[test]
    fn duplicate_tags_collapse_case_insensitively() {
        assert_eq!(hashtags("#Rust and #rust and #RUST"), ["rust"]);
    }

    #[test]
    fn a_bare_hash_is_nothing() {
        assert_eq!(hashtags("# not a tag, ## neither"), Vec::<String>::new());
    }

    #[test]
    fn an_absurdly_long_tag_is_ignored() {
        let long = format!("#{}", "a".repeat(65));
        assert_eq!(hashtags(&long), Vec::<String>::new());
    }

    #[test]
    fn split_query_separates_tags_from_text() {
        let (text, tags) = split_query("#recipe how to make kimchi");
        assert_eq!(text, "how to make kimchi");
        assert_eq!(tags, ["recipe"]);
    }

    #[test]
    fn split_query_with_only_tags_leaves_empty_text() {
        let (text, tags) = split_query("#recipe #food");
        assert_eq!(text, "");
        assert_eq!(tags, ["recipe", "food"]);
    }

    #[test]
    fn fts_query_quotes_every_token() {
        assert_eq!(
            fts_query("kimchi AND \"quotes* NEAR/2").as_deref(),
            Some("\"kimchi\" OR \"and\" OR \"quotes\" OR \"near\" OR \"2\"")
        );
    }

    #[test]
    fn fts_query_of_nothing_is_none() {
        assert_eq!(fts_query("  ,,, "), None);
        assert_eq!(fts_query(""), None);
    }

    #[test]
    fn cjk_detection_covers_the_three_scripts() {
        assert!(is_cjk('김'));
        assert!(is_cjk('漢'));
        assert!(is_cjk('か'));
        assert!(is_cjk('カ'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('1'));
    }
}
