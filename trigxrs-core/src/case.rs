//! Unicode case folding for trigram search
//!
//! Generates case-variant trigrams to support case-insensitive search.
//! Mirrors zoekt's `generateCaseNgrams` approach: for each rune in the trigram,
//! collect all case variants (via simple fold cycling), then produce all combinations.

use crate::ngram::{Ngram, ngram_to_runes, runes_to_ngram};

/// Collect all case variants of a single character.
///
/// Returns a vector containing the character and all its case-folded variants
/// (lowercase + uppercase). For characters without case variants, returns just
/// the original character.
fn case_variants(c: char) -> Vec<char> {
    let mut variants = vec![c];

    // Add lowercase variants
    for lc in c.to_lowercase() {
        if !variants.contains(&lc) {
            variants.push(lc);
        }
    }

    // Add uppercase variants
    for uc in c.to_uppercase() {
        if !variants.contains(&uc) {
            variants.push(uc);
        }
    }

    variants
}

/// Generate all case-variant trigrams for a given ngram.
///
/// For each of the 3 runes in the trigram, collects all case variants,
/// then returns the cartesian product of all variant combinations as
/// unique trigrams.
///
/// # Example
/// ```
/// use trigxrs_core::ngram::runes_to_ngram;
/// use trigxrs_core::case::generate_case_ngrams;
///
/// let ng = runes_to_ngram(['a', 'b', 'c']);
/// let variants = generate_case_ngrams(ng);
/// // Should include 'abc', 'Abc', 'aBc', 'abC', 'ABC', etc.
/// assert!(variants.len() > 1);
/// assert!(variants.contains(&ng)); // original is always included
/// ```
pub fn generate_case_ngrams(ng: Ngram) -> Vec<Ngram> {
    let runes = ngram_to_runes(ng);
    let v0 = case_variants(runes[0]);
    let v1 = case_variants(runes[1]);
    let v2 = case_variants(runes[2]);

    let mut result = Vec::with_capacity(v0.len() * v1.len() * v2.len());

    for &r0 in &v0 {
        for &r1 in &v1 {
            for &r2 in &v2 {
                let variant = runes_to_ngram([r0, r1, r2]);
                if !result.contains(&variant) {
                    result.push(variant);
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ngram::runes_to_ngram;

    #[test]
    fn test_case_variants_ascii_lower() {
        let v = case_variants('a');
        assert!(v.contains(&'a'));
        assert!(v.contains(&'A'));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn test_case_variants_ascii_upper() {
        let v = case_variants('A');
        assert!(v.contains(&'A'));
        assert!(v.contains(&'a'));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn test_case_variants_digit() {
        let v = case_variants('5');
        assert_eq!(v, vec!['5']);
    }

    #[test]
    fn test_case_variants_non_casing() {
        let v = case_variants('.');
        assert_eq!(v, vec!['.']);
    }

    #[test]
    fn test_generate_case_ngrams_all_ascii() {
        let ng = runes_to_ngram(['a', 'b', 'c']);
        let variants = generate_case_ngrams(ng);
        // 2 * 2 * 2 = 8 variants
        assert_eq!(variants.len(), 8);
        assert!(variants.contains(&runes_to_ngram(['a', 'b', 'c'])));
        assert!(variants.contains(&runes_to_ngram(['A', 'B', 'C'])));
        assert!(variants.contains(&runes_to_ngram(['A', 'b', 'c'])));
        assert!(variants.contains(&runes_to_ngram(['a', 'B', 'c'])));
        assert!(variants.contains(&runes_to_ngram(['a', 'b', 'C'])));
    }

    #[test]
    fn test_generate_case_ngrams_mixed_casing() {
        // "a1b" — only 'a' and 'b' have case variants
        let ng = runes_to_ngram(['a', '1', 'b']);
        let variants = generate_case_ngrams(ng);
        // 2 * 1 * 2 = 4 variants
        assert_eq!(variants.len(), 4);
        assert!(variants.contains(&runes_to_ngram(['a', '1', 'b'])));
        assert!(variants.contains(&runes_to_ngram(['A', '1', 'b'])));
        assert!(variants.contains(&runes_to_ngram(['a', '1', 'B'])));
        assert!(variants.contains(&runes_to_ngram(['A', '1', 'B'])));
    }

    #[test]
    fn test_generate_case_ngrams_no_casing() {
        let ng = runes_to_ngram(['1', '2', '3']);
        let variants = generate_case_ngrams(ng);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0], ng);
    }

    #[test]
    fn test_generate_case_ngrams_unicode() {
        // Greek: 'α' (U+03B1) has uppercase 'Α' (U+0391)
        let ng = runes_to_ngram(['α', 'β', 'γ']);
        let variants = generate_case_ngrams(ng);
        assert!(variants.len() > 1);
        assert!(variants.contains(&ng));
        assert!(variants.contains(&runes_to_ngram(['Α', 'Β', 'Γ'])));
    }

    #[test]
    fn test_generate_case_ngrams_contains_original() {
        let ng = runes_to_ngram(['H', 'e', 'l']);
        let variants = generate_case_ngrams(ng);
        assert!(variants.contains(&ng));
    }
}
