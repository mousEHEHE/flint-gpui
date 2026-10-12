//! Shortcuts on the Russian keyboard layout.
//!
//! gpui names a key by the character the active layout prints on it, not by the
//! physical key. With ЙЦУКЕН active, Ctrl+C therefore arrives as `ctrl-С`,
//! which matches neither our key bindings nor the terminal's control-code
//! table, so the shell never sees SIGINT and copy/paste stop working.
//!
//! Both are recovered here by going back to the character a US layout puts on
//! the same physical key. Only Russian is covered: a general fix needs the
//! physical key, which only the platform layer knows.

use gpui::{Action, App, KeyBinding};

/// US-layout character, then the Russian characters sharing its physical key,
/// lower case and upper case.
#[rustfmt::skip]
const RUSSIAN_KEYS: &[(char, char, char)] = &[
    ('`', 'ё', 'Ё'),
    ('q', 'й', 'Й'), ('w', 'ц', 'Ц'), ('e', 'у', 'У'), ('r', 'к', 'К'),
    ('t', 'е', 'Е'), ('y', 'н', 'Н'), ('u', 'г', 'Г'), ('i', 'ш', 'Ш'),
    ('o', 'щ', 'Щ'), ('p', 'з', 'З'), ('[', 'х', 'Х'), (']', 'ъ', 'Ъ'),
    ('a', 'ф', 'Ф'), ('s', 'ы', 'Ы'), ('d', 'в', 'В'), ('f', 'а', 'А'),
    ('g', 'п', 'П'), ('h', 'р', 'Р'), ('j', 'о', 'О'), ('k', 'л', 'Л'),
    ('l', 'д', 'Д'), (';', 'ж', 'Ж'), ('\'', 'э', 'Э'),
    ('z', 'я', 'Я'), ('x', 'ч', 'Ч'), ('c', 'с', 'С'), ('v', 'м', 'М'),
    ('b', 'и', 'И'), ('n', 'т', 'Т'), ('m', 'ь', 'Ь'), (',', 'б', 'Б'),
    ('.', 'ю', 'Ю'),
];

/// The character a US layout puts on the same physical key as `key`, or `None`
/// when `key` is not a single Russian character.
pub fn us_layout_key(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let typed = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    RUSSIAN_KEYS
        .iter()
        .find(|(_, lower, upper)| *lower == typed || *upper == typed)
        .map(|(us, _, _)| *us)
}

/// Bind `chord` to `action`, plus the spellings the same physical keys produce
/// on the Russian layout.
///
/// Both cases are registered: gpui reports the key upper case on Windows
/// (`MapVirtualKeyW` hands back the unshifted capital) and lower case on macOS.
/// A binding that never fires costs nothing, and guessing wrong would leave the
/// shortcut dead on one of the two.
pub fn bind_chord(cx: &mut App, chord: &str, action: impl Action + Clone, context: Option<&str>) {
    for variant in chord_variants(chord) {
        cx.bind_keys([KeyBinding::new(&variant, action.clone(), context)]);
    }
}

fn chord_variants(chord: &str) -> Vec<String> {
    let mut variants = vec![chord.to_string()];

    // Everything before the last '-' is the modifier list. A chord whose key is
    // '-' itself ("ctrl--") leaves an empty tail and falls through untouched.
    let Some((modifiers, key)) = chord.rsplit_once('-') else {
        return variants;
    };
    let mut chars = key.chars();
    let Some(us) = chars.next() else {
        return variants;
    };
    if chars.next().is_some() {
        return variants;
    }

    let Some((_, lower, upper)) = RUSSIAN_KEYS
        .iter()
        .find(|(candidate, _, _)| *candidate == us)
    else {
        return variants;
    };
    variants.push(format!("{modifiers}-{lower}"));
    variants.push(format!("{modifiers}-{upper}"));
    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_keys_resolve_to_their_us_position() {
        // The case gpui reports on Windows (upper) and on macOS (lower).
        assert_eq!(us_layout_key("С"), Some('c'));
        assert_eq!(us_layout_key("с"), Some('c'));
        assert_eq!(us_layout_key("м"), Some('v'));
        assert_eq!(us_layout_key("Ф"), Some('a'));
        assert_eq!(us_layout_key("х"), Some('['));
        assert_eq!(us_layout_key("б"), Some(','));
    }

    #[test]
    fn latin_and_named_keys_are_left_alone() {
        assert_eq!(us_layout_key("c"), None);
        assert_eq!(us_layout_key("tab"), None);
        assert_eq!(us_layout_key("f1"), None);
        assert_eq!(us_layout_key(""), None);
    }

    #[test]
    fn chord_gains_both_russian_spellings() {
        assert_eq!(
            chord_variants("ctrl-shift-c"),
            ["ctrl-shift-c", "ctrl-shift-с", "ctrl-shift-С"]
        );
        assert_eq!(chord_variants("cmd-,"), ["cmd-,", "cmd-б", "cmd-Б"]);
    }

    // `KeyBinding::new` panics on a chord it cannot parse, and the Windows and
    // Linux chord sets are not compiled on macOS, so check here that the
    // generated spellings get through gpui's parser.
    #[test]
    fn generated_spellings_parse_as_keystrokes() {
        for chord in [
            "cmd-c",
            "ctrl-shift-c",
            "cmd-shift-]",
            "ctrl-,",
            "ctrl-shift-tab",
        ] {
            for variant in chord_variants(chord) {
                let parsed = gpui::Keystroke::parse(&variant)
                    .unwrap_or_else(|e| panic!("{variant} does not parse: {e:?}"));
                // gpui only lowercases ASCII, so the two Cyrillic cases stay
                // distinct and both are registered.
                assert!(!parsed.key.is_empty(), "{variant} parsed to an empty key");
            }
        }
    }

    #[test]
    fn chords_without_a_russian_key_stay_single() {
        assert_eq!(chord_variants("ctrl-1"), ["ctrl-1"]);
        assert_eq!(chord_variants("ctrl-tab"), ["ctrl-tab"]);
        assert_eq!(chord_variants("shift-tab"), ["shift-tab"]);
        // The key is '-' itself: nothing to swap, and the tail is empty.
        assert_eq!(chord_variants("ctrl--"), ["ctrl--"]);
        assert_eq!(chord_variants("cmd-shift-="), ["cmd-shift-="]);
    }
}
