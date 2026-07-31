//! Local embeddings: hashed n-gram feature vectors (docs/SEARCH.md).
//!
//! # Why not a neural model
//!
//! The design constraint is "works with no cloud and no downloads, on a fresh
//! checkout, forever". An ONNX sentence-transformer would give truer synonym
//! matching at the cost of a ~100 MB model fetched at first run and a native
//! runtime dependency — both directly against what this server is. Feature
//! hashing gives the part of "semantic" that matters most in practice —
//! robustness to word forms, typos, and CJK segmentation, none of which BM25
//! survives — deterministically, in a few hundred lines of arithmetic.
//!
//! # Construction
//!
//! A document becomes a bag of features:
//!
//! * **words** — lowercased alphanumeric runs (`kimchi`, `노트북`)
//! * **word bigrams** — adjacent word pairs, for phrase identity
//! * **character trigrams** of each non-CJK word, `^`/`$` padded — this is
//!   what makes `kubernets` still find `kubernetes`
//! * **CJK unigrams and bigrams** of each CJK run — Korean compounds and
//!   Japanese text match on shared characters without a segmenter
//!
//! Each feature is FNV-1a hashed; the hash picks one of [`DIMS`] buckets and
//! a sign, and the feature's weight is added there. The vector is then
//! L2-normalised, so similarity is a dot product.

/// Vector width. 384 keeps a document at 1.5 KB — a million messages of
/// embeddings is 1.5 GB, far beyond any personal server, and collisions at
/// this width are noise well below the ranking's discrimination.
pub const DIMS: usize = 384;

const W_WORD: f32 = 1.0;
const W_WORD_BIGRAM: f32 = 0.8;
const W_CHAR_TRIGRAM: f32 = 0.4;
const W_CJK_UNIGRAM: f32 = 0.6;
const W_CJK_BIGRAM: f32 = 1.0;

/// FNV-1a, inlined rather than imported: eight lines, and the whole scheme
/// depends on this function never changing between versions of the binary —
/// a dependency bump must not silently re-embed the world.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn add_feature(vector: &mut [f32; DIMS], feature: &str, weight: f32) {
    let hash = fnv1a(feature.as_bytes());
    let index = (hash % DIMS as u64) as usize;
    let sign = if hash >> 63 == 0 { 1.0 } else { -1.0 };
    vector[index] += sign * weight;
}

/// Embed a text. Empty or featureless text embeds to the zero vector, which
/// is cosine-0 against everything — absent, not similar.
pub fn embed(text: &str) -> [f32; DIMS] {
    let mut vector = [0.0f32; DIMS];

    // Split into runs, keeping CJK runs separate from latin/digit runs even
    // when adjacent ("김치recipe" is a CJK run then a word).
    let mut words: Vec<String> = Vec::new();
    let mut cjk_runs: Vec<Vec<char>> = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();
    let flush_word = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word));
        }
    };
    let flush_cjk = |cjk: &mut Vec<char>, runs: &mut Vec<Vec<char>>| {
        if !cjk.is_empty() {
            runs.push(std::mem::take(cjk));
        }
    };
    for ch in text.chars() {
        if super::text::is_cjk(ch) {
            flush_word(&mut word, &mut words);
            cjk.push(ch);
        } else if ch.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut cjk_runs);
            word.extend(ch.to_lowercase());
        } else {
            flush_word(&mut word, &mut words);
            flush_cjk(&mut cjk, &mut cjk_runs);
        }
    }
    flush_word(&mut word, &mut words);
    flush_cjk(&mut cjk, &mut cjk_runs);

    for w in &words {
        add_feature(&mut vector, w, W_WORD);
        // `^word$` padded trigrams: boundaries matter, "art" ≠ tail of "cart".
        let padded: Vec<char> = std::iter::once('^')
            .chain(w.chars())
            .chain(std::iter::once('$'))
            .collect();
        for tri in padded.windows(3) {
            let mut feature = String::from("t:");
            feature.extend(tri);
            add_feature(&mut vector, &feature, W_CHAR_TRIGRAM);
        }
    }
    for pair in words.windows(2) {
        add_feature(
            &mut vector,
            &format!("b:{} {}", pair[0], pair[1]),
            W_WORD_BIGRAM,
        );
    }
    for run in &cjk_runs {
        for &ch in run {
            add_feature(&mut vector, &format!("c:{ch}"), W_CJK_UNIGRAM);
        }
        for pair in run.windows(2) {
            add_feature(
                &mut vector,
                &format!("cb:{}{}", pair[0], pair[1]),
                W_CJK_BIGRAM,
            );
        }
    }

    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut vector {
            *v /= norm;
        }
    }
    vector
}

/// Dot product — cosine, because [`embed`] normalises.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// f32 little-endian, the storage form in `search_docs.embedding`.
pub fn to_blob(vector: &[f32; DIMS]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(DIMS * 4);
    for v in vector {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    blob
}

/// Read a stored embedding. A blob of the wrong width (from some future
/// re-dimensioning) decodes to `None` and the document simply does not score
/// on the semantic side — degraded, never wrong.
pub fn from_blob(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.len() != DIMS * 4 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim(a: &str, b: &str) -> f32 {
        cosine(&embed(a), &embed(b))
    }

    #[test]
    fn embedding_is_deterministic() {
        assert_eq!(embed("hello world"), embed("hello world"));
    }

    #[test]
    fn embedding_is_normalised() {
        let norm = embed("some ordinary text")
            .iter()
            .map(|v| v * v)
            .sum::<f32>();
        assert!((norm - 1.0).abs() < 1e-5, "norm² = {norm}");
    }

    #[test]
    fn empty_text_is_the_zero_vector() {
        assert!(embed("").iter().all(|&v| v == 0.0));
        assert!(embed("  ...  ").iter().all(|&v| v == 0.0));
    }

    #[test]
    fn related_text_beats_unrelated_text() {
        let related = sim("how to make kimchi at home", "kimchi recipe for beginners");
        let unrelated = sim("how to make kimchi at home", "gpu driver segfault on boot");
        // What matters for ranking is separation, not absolute magnitude —
        // one shared content word in short texts lands near 0.2.
        assert!(
            related > 3.0 * unrelated.max(0.01),
            "related {related} vs unrelated {unrelated}"
        );
    }

    #[test]
    fn a_typo_still_finds_the_word() {
        // No shared full word — only trigrams connect these.
        let typo = sim("kubernets", "kubernetes");
        let noise = sim("kubernets", "breakfast");
        assert!(typo > 0.3, "typo similarity too low: {typo}");
        assert!(typo > noise + 0.25, "typo {typo} vs noise {noise}");
    }

    #[test]
    fn korean_shares_characters_without_a_segmenter() {
        // "김치찌개" (kimchi stew) never appears space-delimited next to
        // "김치" — character bigrams are what connect them.
        let related = sim("김치찌개 끓이는 법", "김치 요리");
        let unrelated = sim("김치찌개 끓이는 법", "자동차 보험 갱신");
        assert!(
            related > unrelated + 0.1,
            "related {related} vs unrelated {unrelated}"
        );
    }

    #[test]
    fn word_order_matters_a_little_but_not_everything() {
        let reordered = sim("rotate the room key", "the room key rotate");
        assert!(
            reordered > 0.8,
            "reordering collapsed similarity: {reordered}"
        );
    }

    #[test]
    fn blob_roundtrips_exactly() {
        let vector = embed("roundtrip me #tags and 한국어 too");
        let back = from_blob(&to_blob(&vector)).expect("valid blob");
        assert_eq!(vector.to_vec(), back);
    }

    #[test]
    fn a_wrong_width_blob_is_none_not_garbage() {
        assert!(from_blob(&[1, 2, 3]).is_none());
        assert!(from_blob(&[]).is_none());
    }

    #[test]
    fn identical_text_is_perfect_similarity() {
        let s = sim("the exact same sentence", "the exact same sentence");
        assert!((s - 1.0).abs() < 1e-5, "{s}");
    }
}
