//! The screen's text, read from the grid without touching the terminal.

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::{Cell, Flags};

/// The text of the active screen: up to `history` rows of scrollback above it, then every
/// row of the screen, trailing spaces taken off. With `unwrap`, rows the terminal wrapped
/// are one line again (a wrapped row ends where the wrap is, its trailing spaces are the
/// text's own and stay).
///
/// Only reads: the terminal is the one that `Screen::image` would write.
pub(crate) fn lines(grid: &Grid<Cell>, history: usize, unwrap: bool) -> Vec<String> {
    let above = i32::try_from(history.min(grid.history_size())).unwrap_or(i32::MAX);
    let mut lines: Vec<String> = Vec::new();
    // The row before wrapped into this one.
    let mut continues = false;
    for line in -above..=grid.bottommost_line().0 {
        let row = &grid[Line(line)];
        let wraps = unwrap
            && row
                .into_iter()
                .last()
                .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
        let mut text = String::new();
        for cell in row {
            push_cell(&mut text, cell);
        }
        if !wraps {
            let end = text.trim_end_matches(' ').len();
            text.truncate(end);
        }
        match lines.last_mut() {
            Some(last) if continues => last.push_str(&text),
            _ => lines.push(text),
        }
        continues = wraps;
    }
    lines
}

/// A cell's character and the zero-width ones behind it. The halves a wide character
/// leaves in the cells beside it are not text, and a tab alacritty keeps in the cell it
/// started in is the blank the cursor moved over.
fn push_cell(text: &mut String, cell: &Cell) {
    if cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        return;
    }
    text.push(match cell.c {
        '\0' | '\t' => ' ',
        other => other,
    });
    text.extend(cell.zerowidth().unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use crate::Screen;

    fn fed(columns: u16, rows: u16, history: usize, bytes: &str) -> Screen {
        let mut screen = Screen::new(columns, rows, history);
        screen.feed(bytes.as_bytes());
        screen
    }

    #[test]
    fn the_visible_rows_come_back_with_trailing_spaces_off() {
        let screen = fed(10, 4, 100, "one\r\ntwo  \r\n a  b ");
        assert_eq!(screen.text(0, false), ["one", "two", " a  b", ""]);
    }

    #[test]
    fn cursor_addressing_puts_text_in_its_row() {
        let screen = fed(10, 4, 100, "\x1b[3;2Hx\x1b[1;1Hy");
        assert_eq!(screen.text(0, false), ["y", "", " x", ""]);
    }

    #[test]
    fn scrollback_comes_when_asked_and_no_further_than_there_is() {
        let screen = fed(10, 3, 100, "1\r\n2\r\n3\r\n4\r\n5\r\n6");
        assert_eq!(screen.text(0, false), ["4", "5", "6"]);
        assert_eq!(screen.text(2, false), ["2", "3", "4", "5", "6"]);
        assert_eq!(screen.text(100, false), ["1", "2", "3", "4", "5", "6"]);
    }

    #[test]
    fn a_screen_that_keeps_no_scrollback_has_none_to_give() {
        let screen = fed(10, 2, 0, "1\r\n2\r\n3");
        assert_eq!(screen.text(50, false), ["2", "3"]);
    }

    #[test]
    fn wrapped_rows_are_rows_unless_they_are_joined() {
        let screen = fed(5, 4, 100, "abcdefghij\r\nxyz");
        assert_eq!(screen.text(0, false), ["abcde", "fghij", "xyz", ""]);
        assert_eq!(screen.text(0, true), ["abcdefghij", "xyz", ""]);
    }

    #[test]
    fn a_blank_at_the_end_of_a_wrapped_row_is_the_texts_own() {
        let screen = fed(3, 4, 100, "ab cd ef");
        assert_eq!(screen.text(0, false), ["ab", "cd", "ef", ""]);
        assert_eq!(screen.text(0, true), ["ab cd ef", ""]);
    }

    #[test]
    fn a_line_that_is_broken_with_a_newline_is_not_joined() {
        let screen = fed(5, 4, 100, "abcde\r\nfgh");
        assert_eq!(screen.text(0, true), ["abcde", "fgh", "", ""]);
    }

    #[test]
    fn joining_reaches_into_the_scrollback() {
        let screen = fed(5, 2, 100, "abcdefghijklmno");
        assert_eq!(screen.text(0, false), ["fghij", "klmno"]);
        assert_eq!(screen.text(1, true), ["abcdefghijklmno"]);
        assert_eq!(screen.text(1, false), ["abcde", "fghij", "klmno"]);
    }

    #[test]
    fn a_wide_character_is_written_once() {
        let screen = fed(6, 3, 100, "\u{65e5}\u{672c}x");
        assert_eq!(screen.text(0, false), ["\u{65e5}\u{672c}x", "", ""]);
    }

    #[test]
    fn a_wide_character_that_did_not_fit_leaves_no_trace_at_the_row_end() {
        let screen = fed(5, 3, 100, "abcd\u{65e5}");
        assert_eq!(screen.text(0, false), ["abcd", "\u{65e5}", ""]);
        assert_eq!(screen.text(0, true), ["abcd\u{65e5}", ""]);
    }

    #[test]
    fn a_zero_width_character_stays_behind_its_base() {
        let screen = fed(6, 2, 100, "e\u{301}x");
        assert_eq!(screen.text(0, false), ["e\u{301}x", ""]);
    }

    #[test]
    fn a_tab_is_the_blank_the_cursor_moved_over() {
        let screen = fed(20, 2, 100, "a\tb");
        assert_eq!(screen.text(0, false), ["a       b", ""]);
    }

    #[test]
    fn the_alternate_screen_is_the_text_when_it_is_the_one_shown() {
        let screen = fed(10, 3, 100, "1\r\n2\r\n3\r\n4\r\n\x1b[?1049h\x1b[Hvim");
        assert_eq!(screen.text(5, false), ["vim", "", ""]);
    }

    #[test]
    fn reading_leaves_the_image_as_it_was() {
        let bytes = "1\r\n2\r\n3\r\n\x1b[1mabcdefghijklmnopqrstuvwxyz\x1b[0m\r\n\x1b[?1h\x1b[>1u";
        let mut read = fed(8, 3, 100, bytes);
        let mut left = fed(8, 3, 100, bytes);
        let _ = (read.text(100, true), read.text(2, false), read.modes());
        assert_eq!(read.image(), left.image());
        // And twice: reading after an image is reading the same screen.
        assert_eq!(read.text(100, false), left.text(100, false));
    }
}
