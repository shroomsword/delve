//! Parses Cisco version strings into a comparable ordinal, where possible.
//!
//! Cisco doesn't use one version scheme — this module handles the common
//! **IOS-XE-style dotted decimal** form (`"17.9.4a"`, `"16.12.5"`) that
//! modern platforms like the ISR4000 series use, since it has a real,
//! well-defined total order. It deliberately does *not* attempt to parse
//! **classic IOS's parenthetical train notation** (`"15.2(7)E3"`,
//! `"12.4(25e)T"`) into an ordinal, and this is a considered choice, not a
//! missing feature: the train letter (`T`/`E`/`M`/`S`/...) identifies a
//! distinct *feature train*, not a point on a single ordered timeline —
//! there's no principled way to say whether `"15.2(7)E3"` is newer or
//! older than `"15.1(4)M"` without knowing something about how those two
//! trains relate, which isn't recoverable from the version string alone.
//! Asserting an ordering there would be exactly the kind of guess the
//! project's `VersionKey`/`VersionDirection` model refuses to make (see
//! the README's "Data model and identity keys" section) — so classic IOS
//! versions fall back to `VersionScheme::Opaque` instead, which still
//! gives correct equality-based change detection, just no direction.

/// Attempts to parse `raw` as an IOS-XE-style version, returning a
/// comparable ordinal. Returns `None` for anything that doesn't match this
/// shape (including classic IOS's train notation) — callers should treat
/// `None` as "use `VersionScheme::Opaque`", not as a parse error to
/// propagate.
///
/// A single trailing lowercase ASCII letter is treated as a rebuild
/// indicator and encoded as an extra ordinal component (`'a'` → 1, `'b'` →
/// 2, ...; no letter → 0), so `"17.9.4"` < `"17.9.4a"` < `"17.9.4b"`,
/// matching Cisco's convention that a lettered rebuild supersedes the
/// unlettered release it's based on.
pub fn parse_ios_xe_version(raw: &str) -> Option<Vec<u64>> {
    let (numeric_part, letter) = split_trailing_rebuild_letter(raw);

    if numeric_part.is_empty() {
        return None;
    }

    let mut ordinal: Vec<u64> = Vec::new();
    for segment in numeric_part.split('.') {
        if segment.is_empty() {
            return None; // e.g. "17..9" or a leading/trailing dot
        }
        // Cisco version segments are plain unsigned decimal — reject
        // anything with a sign, hex prefix, etc. rather than let a
        // permissive parse silently accept something that isn't really a
        // dotted-decimal version.
        if !segment.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        ordinal.push(segment.parse().ok()?);
    }

    let letter_ordinal = match letter {
        None => 0,
        Some(c) => (c as u64) - (b'a' as u64) + 1,
    };
    ordinal.push(letter_ordinal);

    Some(ordinal)
}

/// Splits a single trailing lowercase ASCII letter off `raw`, but only when
/// what remains still looks like the tail of a dotted-decimal version
/// (ends in a digit) — so `"17.9.4a"` splits into `("17.9.4", Some('a'))`,
/// but something like `"beta"` (no digit before the trailing letter) is
/// left alone and will fail to parse as numeric segments anyway.
fn split_trailing_rebuild_letter(raw: &str) -> (&str, Option<char>) {
    if let Some(last) = raw.chars().last() {
        if last.is_ascii_lowercase() {
            let without_last = &raw[..raw.len() - last.len_utf8()];
            if without_last.chars().last().is_some_and(|c| c.is_ascii_digit()) {
                return (without_last, Some(last));
            }
        }
    }
    (raw, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_dotted_version_with_no_rebuild_letter() {
        assert_eq!(parse_ios_xe_version("16.12.5"), Some(vec![16, 12, 5, 0]));
    }

    #[test]
    fn parses_a_version_with_a_rebuild_letter() {
        assert_eq!(parse_ios_xe_version("17.9.4a"), Some(vec![17, 9, 4, 1]));
        assert_eq!(parse_ios_xe_version("17.9.4b"), Some(vec![17, 9, 4, 2]));
    }

    #[test]
    fn base_release_sorts_before_its_own_lettered_rebuild() {
        let base = parse_ios_xe_version("17.9.4").unwrap();
        let rebuild_a = parse_ios_xe_version("17.9.4a").unwrap();
        let rebuild_b = parse_ios_xe_version("17.9.4b").unwrap();
        assert!(base < rebuild_a, "unlettered base release must sort before its first rebuild");
        assert!(rebuild_a < rebuild_b, "rebuild 'a' must sort before rebuild 'b'");
    }

    #[test]
    fn compares_numerically_not_lexicographically() {
        // The entire point of a derived ordinal rather than string
        // comparison: "17.10.0" must sort after "17.9.4", even though
        // the string "17.10.0" sorts before "17.9.4" character-by-character.
        let v_9_4 = parse_ios_xe_version("17.9.4").unwrap();
        let v_10_0 = parse_ios_xe_version("17.10.0").unwrap();
        assert!(v_9_4 < v_10_0, "17.10.0 must be numerically newer than 17.9.4");
    }

    #[test]
    fn classic_ios_train_notation_is_deliberately_unparseable() {
        // Not a bug — see the module doc comment. These must return None
        // so the caller falls back to VersionScheme::Opaque rather than
        // asserting a fabricated ordering across feature trains.
        assert_eq!(parse_ios_xe_version("15.2(7)E3"), None);
        assert_eq!(parse_ios_xe_version("12.4(25e)T"), None);
        assert_eq!(parse_ios_xe_version("7.0(3)I7(9)"), None);
    }

    #[test]
    fn rejects_empty_and_malformed_input() {
        assert_eq!(parse_ios_xe_version(""), None);
        assert_eq!(parse_ios_xe_version("17..9"), None);
        assert_eq!(parse_ios_xe_version(".17.9"), None);
        assert_eq!(parse_ios_xe_version("17.9."), None);
        assert_eq!(parse_ios_xe_version("not-a-version"), None);
        assert_eq!(parse_ios_xe_version("17.9.-4"), None);
    }

    #[test]
    fn rejects_a_multi_character_or_uppercase_trailing_suffix() {
        // A single lowercase letter is a rebuild indicator; anything else
        // trailing the numeric part isn't something this parser
        // understands, and it must say so via None rather than silently
        // dropping the suffix and parsing a truncated, misleading ordinal.
        assert_eq!(parse_ios_xe_version("17.9.4ab"), None);
        assert_eq!(parse_ios_xe_version("17.9.4T"), None);
    }

    #[test]
    fn different_segment_counts_still_compare_sensibly() {
        // Less common, but some trains do publish a 4th numeric segment.
        // Both parse; comparison falls out of Vec's lexicographic Ord.
        let three_segment = parse_ios_xe_version("17.9.4").unwrap();
        let four_segment = parse_ios_xe_version("17.9.4.1").unwrap();
        assert!(three_segment < four_segment);
    }
}
