//! Finds the http/https link under a cell of the visible terminal text.
//!
//! Only plain text is read. A link the remote side sends as an OSC 8 escape is
//! ignored on purpose: it can show one address and open another.

use std::ops::Range;

use crate::shell::{GridPoint, GridSelectionRange, TermSnapshot};

/// Lowercase; the text is matched against them without regard to case.
const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Characters RFC 3986 allows in a URL, besides ASCII letters and digits.
/// Anything else (a space, a quote, `<`, `>`, a non-ASCII character) ends it.
const URL_PUNCTUATION: &str = "-._~:/?#[]@!$&'()*+,;=%";

/// Punctuation that is far more often the end of a sentence, or Markdown
/// around the link, than the end of the address.
const TRAILING_PUNCTUATION: &str = ".,;:!?'*";

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalLink {
    /// Cells the link covers, in the absolute coordinates a selection uses.
    pub range: GridSelectionRange,
    pub url: String,
}

/// The link covering the cell at `row` / `column` of the viewport.
///
/// A link the terminal wrapped onto several rows is read as one. A link that
/// runs on below the last visible row is not returned at all: opening the
/// visible half would open the wrong page.
pub fn link_at(snapshot: &TermSnapshot, row: usize, column: usize) -> Option<TerminalLink> {
    let columns = snapshot.cols as usize;
    if row >= snapshot.cells.len() || column >= columns {
        return None;
    }

    let rows = wrapped_line_rows(snapshot, row);
    let chars: Vec<char> = snapshot.cells[rows.clone()]
        .iter()
        .flat_map(|cells| cells.iter().map(|cell| cell.c))
        .collect();

    let index = (row - rows.start) * columns + column;
    let span = url_span_at(&chars, index)?;

    let is_line_cut_off =
        rows.end == snapshot.cells.len() && is_row_wrapped(snapshot, rows.end - 1);
    // Checked on what follows the span, not on its end: dropped trailing
    // punctuation ("example.com." + "evil.io" on the hidden row) is still
    // part of an address that runs on.
    if is_line_cut_off && chars[span.end..].iter().all(|c| is_url_char(*c)) {
        return None;
    }

    let to_point = |index: usize| GridPoint {
        line: (rows.start + index / columns) as i32 - snapshot.display_offset as i32,
        column: index % columns,
    };
    Some(TerminalLink {
        range: GridSelectionRange {
            start: to_point(span.start),
            end: to_point(span.end - 1),
        },
        url: chars[span].iter().collect(),
    })
}

/// The visible rows that make up the output line `row` belongs to.
fn wrapped_line_rows(snapshot: &TermSnapshot, row: usize) -> Range<usize> {
    let mut first = row;
    while first > 0 && is_row_wrapped(snapshot, first - 1) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < snapshot.cells.len() && is_row_wrapped(snapshot, last) {
        last += 1;
    }
    first..last + 1
}

fn is_row_wrapped(snapshot: &TermSnapshot, row: usize) -> bool {
    snapshot.is_row_wrapped.get(row).copied().unwrap_or(false)
}

/// The URL in `chars` that covers `index`.
fn url_span_at(chars: &[char], index: usize) -> Option<Range<usize>> {
    let mut start = 0;
    while start <= index {
        let Some(scheme_length) = scheme_length_at(chars, start) else {
            start += 1;
            continue;
        };
        let body_start = start + scheme_length;
        let end = url_end(chars, start, body_start);
        if end == body_start {
            // A bare "http://" with nothing after it.
            start = body_start;
            continue;
        }
        if index < end {
            return Some(start..end);
        }
        start = end;
    }
    None
}

fn scheme_length_at(chars: &[char], start: usize) -> Option<usize> {
    let rest = &chars[start..];
    SCHEMES
        .iter()
        .find(|scheme| {
            rest.len() >= scheme.len()
                && scheme
                    .chars()
                    .zip(rest)
                    .all(|(wanted, c)| c.to_ascii_lowercase() == wanted)
        })
        .map(|scheme| scheme.len())
}

/// Where the URL starting at `start` ends (exclusive). Returns `body_start`
/// when nothing usable follows the scheme.
fn url_end(chars: &[char], start: usize, body_start: usize) -> usize {
    let mut end = body_start;
    while end < chars.len() && is_url_char(chars[end]) {
        end += 1;
    }
    while end > body_start && is_trailing_noise(&chars[start..end]) {
        end -= 1;
    }
    end
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || URL_PUNCTUATION.contains(c)
}

/// True when the last character of `url` is not part of the address: sentence
/// punctuation, or a bracket closing one that was opened before the URL, as
/// in "(see https://example.com/page)". A bracket opened inside the URL keeps
/// its partner, so ".../Rust_(language)" stays whole.
fn is_trailing_noise(url: &[char]) -> bool {
    let Some(&last) = url.last() else {
        return false;
    };
    if TRAILING_PUNCTUATION.contains(last) {
        return true;
    }
    let opener = match last {
        ')' => '(',
        ']' => '[',
        _ => return false,
    };
    let count = |wanted: char| url.iter().filter(|c| **c == wanted).count();
    count(last) > count(opener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term_emulator::TermEmulator;

    /// A 6-row terminal of the given width holding `text`.
    fn snapshot_of(columns: u16, text: &str) -> TermSnapshot {
        let mut emulator = TermEmulator::new(6, columns);
        emulator.advance(text.as_bytes());
        emulator.snapshot()
    }

    /// The URL found by clicking the first cell of `needle` on the top row.
    fn url_at(text: &str, needle: &str) -> Option<String> {
        let column = text.find(needle).unwrap();
        assert!(
            text.is_ascii(),
            "byte offset is only a column for ASCII text"
        );
        link_at(&snapshot_of(80, text), 0, column).map(|link| link.url)
    }

    #[test]
    fn finds_the_url_under_the_pointer_and_its_cells() {
        let snapshot = snapshot_of(80, "see https://example.com/a?b=1#c now");
        let link = link_at(&snapshot, 0, 10).unwrap();
        assert_eq!(link.url, "https://example.com/a?b=1#c");
        assert_eq!(link.range.start, GridPoint { line: 0, column: 4 });
        assert_eq!(
            link.range.end,
            GridPoint {
                line: 0,
                column: 30
            }
        );
    }

    #[test]
    fn every_cell_of_the_url_hits_and_its_neighbours_do_not() {
        let snapshot = snapshot_of(80, "go http://a.io/x ok");
        assert!(link_at(&snapshot, 0, 2).is_none());
        assert!(link_at(&snapshot, 0, 3).is_some());
        assert!(link_at(&snapshot, 0, 15).is_some());
        assert!(link_at(&snapshot, 0, 16).is_none());
        assert!(link_at(&snapshot, 0, 17).is_none());
    }

    #[test]
    fn cells_outside_the_grid_have_no_link() {
        let snapshot = snapshot_of(20, "http://a.io");
        assert!(link_at(&snapshot, 6, 0).is_none());
        assert!(link_at(&snapshot, 0, 20).is_none());
        assert!(link_at(&snapshot, 3, 0).is_none());
    }

    #[test]
    fn only_http_and_https_count() {
        assert_eq!(url_at("ftp://example.com/file", "ftp"), None);
        assert_eq!(url_at("file:///etc/passwd", "file"), None);
        assert_eq!(url_at("example.com/path", "example"), None);
        assert_eq!(url_at("ssh://git@example.com", "git"), None);
        assert_eq!(url_at("javascript:alert(1)", "alert"), None);
    }

    #[test]
    fn scheme_matches_in_any_case() {
        assert_eq!(
            url_at("HTTPS://Example.com", "HTTPS"),
            Some("HTTPS://Example.com".to_string())
        );
    }

    #[test]
    fn a_scheme_with_nothing_after_it_is_not_a_link() {
        assert_eq!(url_at("use http:// or https://. ok", "http"), None);
        assert_eq!(url_at("use http:// or https://. ok", "https"), None);
    }

    #[test]
    fn sentence_punctuation_after_the_url_is_dropped() {
        assert_eq!(
            url_at("Open https://a.io/x.", "https"),
            Some("https://a.io/x".to_string())
        );
        assert_eq!(
            url_at("at https://a.io/x, then", "https"),
            Some("https://a.io/x".to_string())
        );
        assert_eq!(
            url_at("is it https://a.io/x?!", "https"),
            Some("https://a.io/x".to_string())
        );
        assert_eq!(
            url_at("**https://a.io/x**", "https"),
            Some("https://a.io/x".to_string())
        );
        // Inside the URL the same characters are kept.
        assert_eq!(
            url_at("https://a.io/v1.2/x?y=1,2 ", "https"),
            Some("https://a.io/v1.2/x?y=1,2".to_string())
        );
    }

    #[test]
    fn quotes_and_angle_brackets_around_the_url_are_dropped() {
        assert_eq!(
            url_at("\"https://a.io/x\"", "https"),
            Some("https://a.io/x".to_string())
        );
        assert_eq!(
            url_at("'https://a.io/x'", "https"),
            Some("https://a.io/x".to_string())
        );
        assert_eq!(
            url_at("<https://a.io/x>", "https"),
            Some("https://a.io/x".to_string())
        );
        assert_eq!(
            url_at("`https://a.io/x`", "https"),
            Some("https://a.io/x".to_string())
        );
    }

    #[test]
    fn a_bracket_opened_before_the_url_is_not_part_of_it() {
        assert_eq!(
            url_at("(see https://a.io/x)", "https"),
            Some("https://a.io/x".to_string())
        );
        assert_eq!(
            url_at("[docs](https://a.io/x).", "https"),
            Some("https://a.io/x".to_string())
        );
        assert_eq!(
            url_at("[https://a.io/x]", "https"),
            Some("https://a.io/x".to_string())
        );
    }

    #[test]
    fn brackets_opened_inside_the_url_are_kept() {
        assert_eq!(
            url_at("https://en.wikipedia.org/wiki/Rust_(language) ", "https"),
            Some("https://en.wikipedia.org/wiki/Rust_(language)".to_string())
        );
        assert_eq!(
            url_at("(https://en.wikipedia.org/wiki/Rust_(language))", "https"),
            Some("https://en.wikipedia.org/wiki/Rust_(language)".to_string())
        );
        assert_eq!(
            url_at("http://[::1]:8080/ ", "http"),
            Some("http://[::1]:8080/".to_string())
        );
    }

    #[test]
    fn two_urls_on_a_row_are_told_apart() {
        let text = "http://one.io http://two.io";
        assert_eq!(
            url_at(text, "http://one"),
            Some("http://one.io".to_string())
        );
        assert_eq!(url_at(text, "two"), Some("http://two.io".to_string()));
    }

    #[test]
    fn a_url_inside_a_query_string_stays_part_of_the_outer_url() {
        let text = "https://a.io/login?next=https://b.io/x";
        assert_eq!(url_at(text, "b.io"), Some(text.to_string()));
    }

    #[test]
    fn wide_characters_before_the_url_keep_the_columns_right() {
        // Two wide chars take four cells, then a space: the URL starts at 5.
        let snapshot = snapshot_of(80, "日本 http://a.io 語");
        assert!(link_at(&snapshot, 0, 4).is_none());
        let link = link_at(&snapshot, 0, 5).unwrap();
        assert_eq!(link.url, "http://a.io");
        assert_eq!(link.range.start.column, 5);
        assert_eq!(link.range.end.column, 15);
    }

    #[test]
    fn non_ascii_text_ends_the_url() {
        let snapshot = snapshot_of(80, "http://a.io/x。次");
        assert_eq!(link_at(&snapshot, 0, 0).unwrap().url, "http://a.io/x");
    }

    #[test]
    fn a_wrapped_url_is_read_as_one_from_any_of_its_rows() {
        let url = "https://example.com/a/very/long/path/that/wraps?token=abc";
        let snapshot = snapshot_of(20, &format!("go {url} ok"));
        let expected = GridSelectionRange {
            start: GridPoint { line: 0, column: 3 },
            end: GridPoint {
                line: 2,
                column: (3 + url.len() - 1) % 20,
            },
        };

        for (row, column) in [(0, 3), (0, 19), (1, 0), (1, 12), (2, 0)] {
            let link = link_at(&snapshot, row, column).unwrap();
            assert_eq!(link.url, url, "row {row}, column {column}");
            assert_eq!(link.range, expected);
        }
        assert!(link_at(&snapshot, 0, 0).is_none());
        assert!(link_at(&snapshot, 2, expected.end.column + 1).is_none());
    }

    #[test]
    fn rows_that_end_without_a_wrap_are_not_joined() {
        // A full first row followed by a real line break.
        let snapshot = snapshot_of(20, "see http://a.io/1234\r\n5678 next");
        assert_eq!(link_at(&snapshot, 0, 6).unwrap().url, "http://a.io/1234");
        assert!(link_at(&snapshot, 1, 0).is_none());
    }

    #[test]
    fn a_url_that_runs_on_below_the_viewport_is_not_a_link() {
        let mut emulator = TermEmulator::new(3, 20);
        emulator.advance(b"a\r\nb\r\nhttps://example.com/long/path/here");
        assert_eq!(
            link_at(&emulator.snapshot(), 1, 0).unwrap().url,
            "https://example.com/long/path/here"
        );

        // One row up: the second half of the URL is now below the last row.
        emulator.scroll_delta(1);
        let snapshot = emulator.snapshot();
        assert_eq!(snapshot.cells[2][0].c, 'h');
        assert!(link_at(&snapshot, 2, 0).is_none());
    }

    #[test]
    fn a_cut_off_url_whose_visible_part_ends_in_punctuation_is_not_a_link() {
        let mut emulator = TermEmulator::new(3, 20);
        // The first 20 cells are "https://example.com." and the rest wraps.
        emulator.advance(b"a\r\nb\r\nhttps://example.com.evil.io/x");
        emulator.scroll_delta(1);
        let snapshot = emulator.snapshot();
        assert_eq!(snapshot.cells[2][19].c, '.');
        assert!(link_at(&snapshot, 2, 0).is_none());
    }

    #[test]
    fn a_whole_url_on_a_cut_off_line_is_still_a_link() {
        let mut emulator = TermEmulator::new(3, 20);
        emulator.advance(b"a\r\nb\r\nhttp://a.io and a lot more text");
        emulator.scroll_delta(1);
        assert_eq!(
            link_at(&emulator.snapshot(), 2, 0).unwrap().url,
            "http://a.io"
        );
    }

    #[test]
    fn the_range_is_absolute_when_scrolled_back() {
        let mut emulator = TermEmulator::new(3, 20);
        emulator.advance(b"http://a.io\r\nb\r\nc\r\nd\r\ne");
        emulator.scroll_delta(2);
        let snapshot = emulator.snapshot();
        assert_eq!(snapshot.display_offset, 2);

        let link = link_at(&snapshot, 0, 0).unwrap();
        assert_eq!(
            link.range.start,
            GridPoint {
                line: -2,
                column: 0
            }
        );
        assert_eq!(
            link.range.end,
            GridPoint {
                line: -2,
                column: 10
            }
        );
    }
}
