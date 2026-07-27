//! Hangul jamo composition helpers.
//!
//! NOTE: this module is currently NOT wired into the live key-forwarding path.
//! crossterm's default (legacy) terminal encoding does not reliably report IME
//! composition state (crossterm 0.28's `KeyEventState` exposes only `KEYPAD`,
//! `CAPS_LOCK`, `NUM_LOCK`, `NONE` — there is no `COMPOSING` flag), so buffering
//! and composing jamos in-app risks double-emit or input lag. True inline
//! preedit (showing the in-progress syllable at the cursor without desyncing the
//! agent) requires a kitty / OSC 51-style preedit protocol between the
//! multiplexer and the agent, which is out of scope here. This module exists as
//! a tested building block if/when that protocol lands.

/// Compose a sequence of Hangul compatibility jamos (U+3131..=U+318E) and/or
/// precomposed syllables into a single precomposed Hangul syllable
/// (U+AC00..=U+D7A3) when the jamos form a valid (L, V, LV, LVT) combination
/// per the Unicode Hangul Composition Algorithm (UAX #15 / chapter 3.12).
///
/// Returns `Some(syllable)` if exactly one syllable composes from the input;
/// `None` otherwise (empty input, non-Hangul chars, or multi-syllable input).
///
/// # Examples
///
/// ```
/// # use orcatui::hangul::compose_syllable;
/// // ㅇ (U+3147) + ㅏ (U+314F) + ㄴ (U+3134) -> 안 (U+C548).
/// assert_eq!(compose_syllable("\u{3147}\u{314F}\u{3134}"), Some('\u{C548}'));
/// ```
#[must_use]
pub fn compose_syllable(jamos: &str) -> Option<char> {
    const S_BASE: u32 = 0xAC00;
    const S_LAST: u32 = 0xD7A3;
    const N_V: u32 = 21;
    const N_T: u32 = 28;

    let mut chars = jamos.chars();

    let first = chars.next()?;
    let first_cp = u32::from(first);

    // A lone precomposed syllable is already "one syllable" — return it as-is.
    // A precomposed syllable followed by anything else is ambiguous -> None.
    if (S_BASE..=S_LAST).contains(&first_cp) {
        return if chars.next().is_none() {
            Some(first)
        } else {
            None
        };
    }

    // L — leading consonant (required).
    let l = lead_index(first)?;
    // V — vowel (required for any syllable).
    let v = vowel_index(chars.next()?)?;
    let lv = S_BASE + (l as u32 * N_V + v as u32) * N_T; // T = 0 (fill)

    // Optional T — trailing consonant.
    let syllable = match chars.next() {
        None => lv, // LV syllable, no trail.
        Some(t_char) => {
            let t = trail_value(t_char)?;
            let s = lv + u32::from(t);
            // Exactly one syllable: anything after L V T is multi-syllable.
            if chars.next().is_some() {
                return None;
            }
            s
        }
    };

    debug_assert!((S_BASE..=S_LAST).contains(&syllable));
    char::from_u32(syllable)
}

/// Leading-consonant (L) index 0..=18 for a Hangul compatibility jamo, per the
/// 19 modern initial consonants ㄱ..ㅎ. Returns `None` for non-lead chars.
#[must_use]
fn lead_index(c: char) -> Option<u8> {
    match c {
        'ㄱ' => Some(0),
        'ㄲ' => Some(1),
        'ㄴ' => Some(2),
        'ㄷ' => Some(3),
        'ㄸ' => Some(4),
        'ㄹ' => Some(5),
        'ㅁ' => Some(6),
        'ㅂ' => Some(7),
        'ㅃ' => Some(8),
        'ㅅ' => Some(9),
        'ㅆ' => Some(10),
        'ㅇ' => Some(11),
        'ㅈ' => Some(12),
        'ㅉ' => Some(13),
        'ㅊ' => Some(14),
        'ㅋ' => Some(15),
        'ㅌ' => Some(16),
        'ㅍ' => Some(17),
        'ㅎ' => Some(18),
        _ => None,
    }
}

/// Vowel (V) index 0..=20 for a Hangul compatibility jamo, per the 21 modern
/// vowels ㅏ..ㅣ. Returns `None` for non-vowel chars.
#[must_use]
fn vowel_index(c: char) -> Option<u8> {
    match c {
        'ㅏ' => Some(0),
        'ㅐ' => Some(1),
        'ㅑ' => Some(2),
        'ㅒ' => Some(3),
        'ㅓ' => Some(4),
        'ㅔ' => Some(5),
        'ㅕ' => Some(6),
        'ㅖ' => Some(7),
        'ㅗ' => Some(8),
        'ㅘ' => Some(9),
        'ㅙ' => Some(10),
        'ㅚ' => Some(11),
        'ㅛ' => Some(12),
        'ㅜ' => Some(13),
        'ㅝ' => Some(14),
        'ㅞ' => Some(15),
        'ㅟ' => Some(16),
        'ㅠ' => Some(17),
        'ㅡ' => Some(18),
        'ㅢ' => Some(19),
        'ㅣ' => Some(20),
        _ => None,
    }
}

/// Trailing-consonant (T) value 1..=27 for a Hangul compatibility jamo, per the
/// 27 modern final consonants. Returns `None` for non-trail chars. (T = 0 is
/// reserved for "no trailing consonant" and is never returned here.)
#[must_use]
fn trail_value(c: char) -> Option<u8> {
    match c {
        'ㄱ' => Some(1),
        'ㄲ' => Some(2),
        'ㄳ' => Some(3),
        'ㄴ' => Some(4),
        'ㄵ' => Some(5),
        'ㄶ' => Some(6),
        'ㄷ' => Some(7),
        'ㄹ' => Some(8),
        'ㄺ' => Some(9),
        'ㄻ' => Some(10),
        'ㄼ' => Some(11),
        'ㄽ' => Some(12),
        'ㄾ' => Some(13),
        'ㄿ' => Some(14),
        'ㅀ' => Some(15),
        'ㅁ' => Some(16),
        'ㅂ' => Some(17),
        'ㅄ' => Some(18),
        'ㅅ' => Some(19),
        'ㅆ' => Some(20),
        'ㅇ' => Some(21),
        'ㅈ' => Some(22),
        'ㅊ' => Some(23),
        'ㅋ' => Some(24),
        'ㅌ' => Some(25),
        'ㅍ' => Some(26),
        'ㅎ' => Some(27),
        _ => None,
    }
}

/// True if `c` is a Hangul compatibility jamo (the precomposed jamo block
/// U+3131..=U+318E, covering ㅏ..ㆎ). Returns `false` for ASCII, precomposed
/// Hangul syllables (U+AC00..), and all other characters.
#[must_use]
pub fn is_compat_jamo(c: char) -> bool {
    ('\u{3131}'..='\u{318E}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_syllable_an() {
        // ㅇ (U+3147, L=11) + ㅏ (U+314F, V=0) + ㄴ (U+3134, T=4) -> 안 (U+C548).
        // Math: 0xAC00 + (11*21 + 0)*28 + 4 = 0xAC00 + 6468 + 4 = 0xC548.
        assert_eq!(
            compose_syllable("\u{3147}\u{314F}\u{3134}"),
            Some('\u{C548}')
        );
        assert_eq!(compose_syllable("ㅇㅏㄴ"), Some('안'));
    }

    #[test]
    fn compose_syllable_lv_without_trail() {
        // ㅈ (L=12) + ㅜ (V=13), no trail -> 주 (U+C8FC).
        // Math: 0xAC00 + (12*21 + 13)*28 + 0 = 0xAC00 + 265*28 = 0xAC00 + 7420 = 0xC8FC.
        assert_eq!(compose_syllable("ㅈㅜ"), Some('\u{C8FC}'));
        assert_eq!(compose_syllable("ㅈㅜ"), Some('주'));
    }

    #[test]
    fn compose_syllable_lone_precomposed_returns_itself() {
        assert_eq!(compose_syllable("안"), Some('안'));
        assert_eq!(compose_syllable("한"), Some('한'));
    }

    #[test]
    fn compose_syllable_returns_none_for_empty() {
        assert_eq!(compose_syllable(""), None);
    }

    #[test]
    fn compose_syllable_returns_none_for_ascii() {
        assert_eq!(compose_syllable("a"), None);
        assert_eq!(compose_syllable("abc"), None);
        assert_eq!(compose_syllable("ㅇa"), None);
    }

    #[test]
    fn compose_syllable_returns_none_for_lone_consonant_or_vowel() {
        // A valid syllable needs at least L + V.
        assert_eq!(compose_syllable("ㄴ"), None);
        assert_eq!(compose_syllable("ㅏ"), None);
    }

    #[test]
    fn compose_syllable_returns_none_for_multi_syllable() {
        // ㅇㅏㄴㄱㅏ is 안 + 가 — more than one syllable.
        assert_eq!(compose_syllable("ㅇㅏㄴㄱㅏ"), None);
        // Two precomposed syllables.
        assert_eq!(compose_syllable("안안"), None);
    }

    #[test]
    fn is_compat_jamo_true_for_jamo_false_for_ascii() {
        assert!(is_compat_jamo('ㄱ'));
        assert!(is_compat_jamo('ㅏ'));
        assert!(is_compat_jamo('ㅇ'));
        assert!(is_compat_jamo('\u{318E}')); // last char of the block (ㆎ)
        assert!(!is_compat_jamo('a'));
        assert!(!is_compat_jamo('Z'));
        assert!(!is_compat_jamo('1'));
        assert!(!is_compat_jamo(' '));
        assert!(!is_compat_jamo('안')); // precomposed syllable, not a compat jamo
        assert!(!is_compat_jamo('\u{3130}')); // just before the block
        assert!(!is_compat_jamo('\u{318F}')); // just after the block
    }
}
