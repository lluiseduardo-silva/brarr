//! Matching a typed query against a title, forgivingly.
//!
//! The library is a Portuguese-language catalogue of mostly foreign
//! titles, which makes exact matching useless in three separate ways.
//!
//! ## The original title is not a typo, it is the other name
//!
//! "tensei shitara" finds nothing in "That Time I Got Reincarnated as a
//! Slime" at any edit distance — they share no letters in the right
//! order. But `library_items.original_title` holds *Tensei Shitara Slime
//! Datta Ken*, and the query is a clean prefix of it. **Searching both
//! fields is what actually solves the anime case**; edit distance would
//! have been the wrong tool, applied harder.
//!
//! ## Accents are optional when typing and mandatory in the data
//!
//! Nobody types "Pokémon" with the acute. Normalisation folds the Latin
//! diacritics to ASCII so the stored form and the typed form meet.
//!
//! ## Word order and punctuation are noise
//!
//! "boys the" should find "The Boys", and "spider man" should find
//! "Spider-Man". Every run of non-alphanumeric characters collapses to
//! a single space, and the token pass is order-free.
//!
//! Typo tolerance is the *last* tier rather than the first, and it is
//! bounded per token: one edit for a short word, two for a long one.
//! Unbounded distance turns a catalogue of 360 titles into a list where
//! everything matches everything a little.

/// Fold one character to its unaccented ASCII form.
///
/// Hand-rolled rather than pulled from a crate: the range that matters
/// here is Latin-1 plus the handful of Latin Extended-A letters that
/// show up in European titles, and romanised Japanese is already ASCII.
fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'í' | 'ì' | 'î' | 'ï' | 'ī' | 'ĭ' | 'į' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ú' | 'ù' | 'û' | 'ü' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => 'c',
        'ñ' | 'ń' | 'ň' | 'ņ' => 'n',
        'ý' | 'ÿ' => 'y',
        'š' | 'ś' | 'ş' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        'ł' => 'l',
        'ğ' => 'g',
        'ð' => 'd',
        'þ' => 'p',
        other => other,
    }
}

/// Lowercase, unaccented, and punctuation-free.
///
/// Every run of non-alphanumeric characters becomes a single space, and
/// the result is trimmed — so `"Spider-Man: No Way Home"` and
/// `"spider man no way home"` normalise to the same string.
#[must_use]
pub fn normalise(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for c in raw.chars().flat_map(char::to_lowercase).map(fold) {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        } else {
            pending_space = true;
        }
    }
    out
}

/// Levenshtein distance, abandoned as soon as it exceeds `max`.
///
/// The bound is the point: an unbounded distance over a whole catalogue
/// makes every title match every query a little, and the ranking stops
/// meaning anything.
fn within(a: &str, b: &str, max: usize) -> Option<usize> {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut row_best = cur[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            row_best = row_best.min(cur[j + 1]);
        }
        if row_best > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    (d <= max).then_some(d)
}

/// How many edits a token of this length is allowed to be wrong by.
///
/// Zero for very short words: at three characters, one edit reaches a
/// large share of the dictionary and "ovo" would match "avo", "ova" and
/// "oxo" equally.
const fn budget(len: usize) -> usize {
    match len {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// Best match of `query` against any of `fields`, or `None`.
///
/// The number is a rank, not a percentage — higher is a better match,
/// and only the ordering is meaningful. Tiers rather than a continuous
/// score, because "the title starts with what you typed" should always
/// beat "two of your words appear somewhere", regardless of how many
/// words there were.
#[must_use]
pub fn score(query: &str, fields: &[&str]) -> Option<u32> {
    let q = normalise(query);
    if q.is_empty() {
        return None;
    }
    fields
        .iter()
        .filter_map(|f| score_one(&q, &normalise(f)))
        .max()
}

fn score_one(q: &str, hay: &str) -> Option<u32> {
    if hay.is_empty() {
        return None;
    }
    if hay == q {
        return Some(1000);
    }
    if hay.starts_with(q) {
        return Some(900);
    }
    if hay.contains(q) {
        return Some(800);
    }

    let q_tokens: Vec<&str> = q.split(' ').filter(|t| !t.is_empty()).collect();
    let h_tokens: Vec<&str> = hay.split(' ').filter(|t| !t.is_empty()).collect();
    if q_tokens.is_empty() {
        return None;
    }

    // Order-free: every typed word has to land somewhere, but not in the
    // order it was typed. "boys the" finds "The Boys".
    if q_tokens
        .iter()
        .all(|qt| h_tokens.iter().any(|ht| ht.contains(*qt)))
    {
        return Some(700);
    }

    // Last resort: allow each word to be misspelt, and pay for it. A
    // query that needed more edits ranks below one that needed fewer.
    let mut total = 0usize;
    for qt in &q_tokens {
        let best = h_tokens
            .iter()
            .filter_map(|ht| within(qt, ht, budget(qt.len())))
            .min()?;
        total += best;
    }
    Some(600u32.saturating_sub(u32::try_from(total).unwrap_or(u32::MAX)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert on happy paths")]
mod tests {
    use super::{normalise, score};

    #[test]
    fn punctuation_and_case_and_accents_all_fold_away() {
        assert_eq!(
            normalise("Spider-Man: No Way Home"),
            "spider man no way home"
        );
        assert_eq!(normalise("Pokémon"), "pokemon");
        assert_eq!(normalise("  A Casa do Dragão  "), "a casa do dragao");
        assert_eq!(normalise("!!!"), "");
        assert_eq!(normalise("Yu-Gi-Oh!"), "yu gi oh");
    }

    /// The case the operator named. It is **not** a fuzzy match: the two
    /// names share no letters in the right order, and no edit distance
    /// would ever connect them. It works because the original title is a
    /// field we already store.
    #[test]
    fn the_original_title_finds_the_anime_by_its_japanese_name() {
        let fields = [
            "That Time I Got Reincarnated as a Slime",
            "Tensei Shitara Slime Datta Ken",
        ];
        assert!(score("tensei shitara", &fields).is_some());
        assert!(score("slime datta", &fields).is_some());
        // And the localised name still works, obviously.
        assert!(score("reincarnated", &fields).is_some());

        // Against the localised name alone it is unfindable, which is
        // exactly why searching one field was not enough.
        assert!(score("tensei shitara", &fields[..1]).is_none());
    }

    #[test]
    fn a_prefix_outranks_a_substring_outranks_scattered_words() {
        let hay = ["The Boys"];
        let prefix = score("the b", &hay).unwrap();
        let substring = score("he bo", &hay).unwrap();
        let scattered = score("boys the", &hay).unwrap();
        assert!(prefix > substring, "{prefix} vs {substring}");
        assert!(substring > scattered, "{substring} vs {scattered}");
    }

    #[test]
    fn an_exact_title_beats_everything() {
        let hay = ["Dexter"];
        assert_eq!(score("dexter", &hay), Some(1000));
        assert_eq!(score("DEXTER", &hay), Some(1000));
    }

    #[test]
    fn a_typo_still_finds_the_title_and_ranks_below_a_clean_match() {
        let hay = ["Reincarnated"];
        let clean = score("reincarnated", &hay).unwrap();
        let typo = score("reincarnted", &hay).unwrap();
        assert!(typo > 0, "one dropped letter must still match");
        assert!(clean > typo, "{clean} vs {typo}");
    }

    #[test]
    fn fewer_edits_rank_higher() {
        let hay = ["Reincarnated"];
        let one = score("reincarnted", &hay).unwrap();
        let two = score("reincrnted", &hay).unwrap();
        assert!(one > two, "one edit must outrank two: {one} vs {two}");
    }

    /// The budget is read off the **typed** word, the way a search
    /// engine does it — so a short query cannot fan out across the
    /// catalogue just because some catalogue entry happens to be long.
    #[test]
    fn the_budget_follows_the_typed_word_not_the_title() {
        // Six letters, so one edit. Two deletions is out of reach, even
        // though the target is long enough to absorb them.
        assert!(score("futrma", &["Futurama"]).is_none());
        assert!(score("futurma", &["Futurama"]).is_some());
    }

    /// A three-letter word gets no budget: at that length one edit
    /// reaches a large share of the dictionary and the filter stops
    /// filtering.
    #[test]
    fn very_short_words_are_not_allowed_to_be_wrong() {
        assert!(score("dex", &["Dexter"]).is_some(), "prefix, not a typo");
        assert!(score("zzz", &["Dexter"]).is_none());
        assert!(score("dxt", &["Dexter"]).is_none());
    }

    #[test]
    fn an_unrelated_query_does_not_match() {
        let hay = ["The Office", "The Office"];
        assert!(score("breaking bad", &hay).is_none());
        assert!(score("xylophone", &hay).is_none());
    }

    #[test]
    fn an_empty_query_matches_nothing_rather_than_everything() {
        assert!(score("", &["The Office"]).is_none());
        assert!(score("   ", &["The Office"]).is_none());
        assert!(score("!!!", &["The Office"]).is_none());
    }

    #[test]
    fn an_absent_original_title_is_simply_skipped() {
        // The view passes `""` for a movie with no distinct original
        // title; it must not match every query by being empty.
        assert!(score("office", &["The Office", ""]).is_some());
        assert!(score("zzzzz", &["The Office", ""]).is_none());
    }
}
