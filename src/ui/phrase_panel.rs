use gpui::prelude::*;
use gpui::{div, px, App};
use gpui_component::{h_flex, theme::Theme, v_flex, ActiveTheme as _};

use crate::ui::lock_screen;

const GRID_COLUMNS: usize = 4;

/// The text written by "Save as text file": one numbered word per line, then
/// the whole phrase on a single line so it can be pasted into the recovery form.
pub(crate) fn phrase_file_text(phrase: &str) -> String {
    let words: Vec<&str> = phrase.split_whitespace().collect();
    let mut text = String::from("flint recovery phrase\n\n");
    for (index, word) in words.iter().enumerate() {
        text.push_str(&format!("{:>2}. {word}\n", index + 1));
    }
    text.push('\n');
    text.push_str(&words.join(" "));
    text.push('\n');
    text
}

fn word_cell(number: usize, word: &str, theme: &Theme) -> impl IntoElement {
    h_flex()
        .flex_1()
        .min_w_0()
        .gap(px(6.))
        .child(
            div()
                .w(px(18.))
                .flex_none()
                .text_xs()
                .text_right()
                .text_color(theme.muted_foreground)
                .child(number.to_string()),
        )
        .child(div().font_family("Cascadia Code").child(word.to_string()))
}

fn word_grid(phrase: &str, theme: &Theme) -> impl IntoElement {
    let words: Vec<&str> = phrase.split_whitespace().collect();
    let rows = words.chunks(GRID_COLUMNS).enumerate().map(|(row, chunk)| {
        let cells = chunk
            .iter()
            .enumerate()
            .map(|(column, word)| word_cell(row * GRID_COLUMNS + column + 1, word, theme));
        // Empty cells keep a short last row aligned with the columns above it.
        let padding = (chunk.len()..GRID_COLUMNS).map(|_| div().flex_1().min_w_0());
        h_flex().gap(px(8.)).children(cells).children(padding)
    });

    v_flex()
        .p(px(12.))
        .gap(px(6.))
        .rounded_md()
        .bg(theme.muted)
        .text_sm()
        .children(rows)
}

pub(crate) fn phrase_panel(
    phrase: &str,
    header: &'static str,
    subtitle: &'static str,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .gap(px(8.))
        .child(lock_screen::header(header, Some(subtitle), cx))
        .child(word_grid(phrase, cx.theme()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_text_numbers_every_word_and_ends_with_the_plain_phrase() {
        let phrase: String = (1..=24)
            .map(|n| format!("word{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let text = phrase_file_text(&phrase);
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines[0], "flint recovery phrase");
        assert_eq!(lines[2], " 1. word1");
        assert_eq!(lines[11], "10. word10");
        assert_eq!(lines[25], "24. word24");
        assert_eq!(lines[27], phrase);
        assert_eq!(lines.len(), 28);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn file_text_ignores_extra_whitespace() {
        let text = phrase_file_text("  alpha   beta\n");
        assert!(text.contains(" 1. alpha\n 2. beta\n"));
        assert!(text.ends_with("\nalpha beta\n"));
    }
}
