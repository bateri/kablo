//! Round trips: a stream is fed to a [`Screen`], written as an image, the image is fed to
//! a blank screen of the same size, and the two agree on everything a `Term` shows and on
//! everything the writer probes. Every scene of the corpus is a test of its own.

use alacritty_terminal::grid::Cursor;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell as TermCell, Flags};
use alacritty_terminal::vte::ansi::{
    CharsetIndex, Color, CursorShape, CursorStyle, KeyboardModes, Rgb,
};

use super::*;
use crate::image::{LAST_SETTABLE_COLOR, MODES, MODES_NOT_WRITTEN, Pen, Probed, encode, live_link};
use crate::kept::Kept;
use crate::tail::KEEP_LIMITS;

const HISTORY: usize = 1000;

fn screen(columns: u16, rows: u16) -> Screen {
    Screen::new(columns, rows, HISTORY)
}

fn fed(columns: u16, rows: u16, bytes: &[u8]) -> Screen {
    let mut screen = screen(columns, rows);
    screen.feed(bytes);
    screen
}

/// The writer's own output and what it probed; the screen is spent afterwards.
fn encoded(screen: &mut Screen) -> (Vec<u8>, Probed) {
    let heard = screen.heard.clone();
    encode(&mut screen.term, &move || heard.title())
}

/// What `Term` shows of the active screen without a probe.
fn readable(screen: &Screen) -> String {
    let term = &screen.term;
    let grid = term.grid();
    let cursor = |cursor: &Cursor<TermCell>| {
        format!(
            "{:?} wrap={} pen={:?} link={:?} sets={:?}",
            cursor.point,
            cursor.input_needs_wrap,
            Pen::of(&cursor.template),
            cursor.template.hyperlink().as_ref().map(live_link),
            cursor.charsets,
        )
    };
    // Only the colours the program changed.
    let colors: Vec<(usize, Rgb)> = (0..=LAST_SETTABLE_COLOR)
        .filter_map(|index| term.colors()[index].map(|rgb| (index, rgb)))
        .collect();
    format!(
        "mode={:?}\ncursor={}\nsaved={}\ncolors={colors:?}\nhistory={}\nstyle={:?}\ntitle={:?}",
        term.mode(),
        cursor(&grid.cursor),
        cursor(&grid.saved_cursor),
        grid.history_size(),
        term.cursor_style(),
        unset_is_empty(screen.title()),
    )
}

/// A title that was never set and an empty one: no sequence sets the first after the
/// second, and a window title reads the two alike (the writer's known limit).
fn unset_is_empty(title: Option<String>) -> Option<String> {
    title.filter(|title| !title.is_empty())
}

/// [`Probed`] with its titles read that way.
fn alike(mut probed: Probed) -> Probed {
    probed.title = unset_is_empty(probed.title);
    for entry in &mut probed.titles {
        *entry = unset_is_empty(entry.take());
    }
    probed
}

/// The canonical text of a blank cell on the default pen.
fn blank_cell() -> String {
    format!(
        "{:?}{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        ' ',
        <&[char]>::default(),
        Pen::DEFAULT.fg,
        Pen::DEFAULT.bg,
        None::<Color>,
        Flags::empty(),
        None::<String>,
    )
}

/// Every cell of the active screen, history included, as the fields an image promises to
/// keep. What it does not promise is left out: the second half of a wide character is the
/// character's (erasing under one leaves it without its spacer, and the image writes the
/// character whole); a wrap and the blank of a wide character that did not fit belong to a
/// row's last cell, elsewhere they are what a shift (`DCH`) left behind; and the blanks
/// that end a row, which a line break stands in for, are the default blank whatever pen
/// wrote them. Trailing blank rows are not told apart from missing ones.
fn cells(screen: &Screen) -> Vec<Vec<String>> {
    let grid = screen.term.grid();
    let invisible = |cell: &TermCell| {
        (cell.c == ' ' || cell.c == '\0')
            && cell.bg == Pen::DEFAULT.bg
            && !cell
                .flags
                .intersects(Flags::INVERSE | Flags::STRIKEOUT | Flags::ALL_UNDERLINES)
            && cell.zerowidth().is_none_or(<[char]>::is_empty)
    };
    // The cell a cursor waiting to wrap is written again, and that clears its wrap flag.
    let waiting: Vec<i32> = [&grid.cursor, &grid.saved_cursor]
        .into_iter()
        .filter(|cursor| cursor.input_needs_wrap)
        .map(|cursor| cursor.point.line.0)
        .collect();
    let mut rows: Vec<Vec<String>> = (grid.topmost_line().0..=grid.bottommost_line().0)
        .map(|line| {
            let row = &grid[Line(line)];
            let width = grid.columns();
            // A wrap means something where a row follows; on the screen's last row it
            // leads nowhere (the rows after it were scrolled away).
            let wraps = line < grid.bottommost_line().0 && !waiting.contains(&line);
            let ends = (0..width)
                .rposition(|col| !invisible(&row[Column(col)]))
                .map_or(0, |last| last + 1);
            (0..width)
                .map(|col| {
                    let cell = &row[Column(col)];
                    let first_half = col
                        .checked_sub(1)
                        .is_some_and(|before| row[Column(before)].flags.contains(Flags::WIDE_CHAR));
                    if first_half {
                        return "second half".to_owned();
                    }
                    // The blank at a row's end that a wide character left: what it shows
                    // is its background.
                    if wraps
                        && col + 1 == width
                        && cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        return format!("leading spacer|{:?}|{:?}", cell.bg, cell.flags);
                    }
                    if col >= ends
                        && !(wraps && col + 1 == width && cell.flags.contains(Flags::WRAPLINE))
                    {
                        return blank_cell();
                    }
                    let link = cell.hyperlink().map(|link| live_link(&link));
                    let flags = if wraps && col + 1 == width {
                        cell.flags
                    } else {
                        cell.flags - Flags::WRAPLINE - Flags::LEADING_WIDE_CHAR_SPACER
                    } - Flags::WIDE_CHAR_SPACER;
                    format!(
                        "{:?}{:?}|{:?}|{:?}|{:?}|{flags:?}|{link:?}",
                        cell.c,
                        cell.zerowidth().unwrap_or_default(),
                        cell.fg,
                        cell.bg,
                        cell.underline_color(),
                    )
                })
                .collect()
        })
        .collect();
    let blank = blank_cell();
    while rows
        .last()
        .is_some_and(|row| row.iter().all(|cell| *cell == blank))
    {
        rows.pop();
    }
    rows
}

/// Fails with the first cell that differs instead of both screens in full.
fn assert_same_cells(replayed: &[Vec<String>], original: &[Vec<String>], image: &[u8]) {
    for (row, (got, want)) in replayed.iter().zip(original).enumerate() {
        for (column, (got, want)) in got.iter().zip(want).enumerate() {
            assert_eq!(
                got,
                want,
                "row {row}, column {column}; the image: {:?}",
                lossy(image)
            );
        }
    }
    assert_eq!(replayed.len(), original.len(), "rows; {:?}", lossy(image));
}

/// Fails with the neighbourhood of the first byte that differs.
fn assert_same_bytes(got: &[u8], want: &[u8]) {
    let at = got
        .iter()
        .zip(want)
        .position(|(got, want)| got != want)
        .unwrap_or(got.len().min(want.len()));
    let around = |bytes: &[u8]| {
        lossy(
            bytes
                .get(at.saturating_sub(120)..(at + 60).min(bytes.len()))
                .unwrap_or_default(),
        )
    };
    assert!(
        got == want,
        "the bytes differ at {at}:\n got ...{:?}\nwant ...{:?}",
        around(got),
        around(want)
    );
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

struct Trip {
    probed: Probed,
    /// The title the blank screen ended up with.
    title: Option<String>,
}

/// Writes the image of a screen and checks it three ways: a blank screen of the same size
/// fed the image shows what the original showed (the active screen cell for cell, the
/// modes, cursors, palette, history and style); that screen's own image is the same bytes
/// (which compares every cell of **both** screens and every probed field); and
/// [`Screen::image`] — which leaves the screen live — gives the same bytes and a screen
/// that still shows the same.
fn trip_of(mut original: Screen) -> Trip {
    let (columns, rows) = original.size();
    let shown = readable(&original);
    let grid = cells(&original);

    let (first, probed) = encoded(&mut original);
    let mut replayed = fed(columns, rows, &first);
    assert_eq!(readable(&replayed), shown, "the image: {:?}", lossy(&first));
    assert_same_cells(&cells(&replayed), &grid, &first);
    let title = unset_is_empty(replayed.title());

    // `image` on a screen built from the image is the writer's output again (no open
    // sequence here), and the screen it leaves is the one it was.
    let mut live = fed(columns, rows, &first);
    let before = readable(&live);
    let cells_before = cells(&live);
    let image = live.image();
    assert_same_bytes(&image, &first);
    assert_eq!(readable(&live), before);
    assert_eq!(cells(&live), cells_before);

    let (second, again) = encoded(&mut replayed);
    assert_same_bytes(&second, &first);
    assert_eq!(alike(again), alike(probed.clone()));
    Trip { probed, title }
}

fn trip(columns: u16, rows: u16, bytes: &[u8]) -> Trip {
    trip_of(fed(columns, rows, bytes))
}

// ─── the corpus: one scene, one test ─────────────────────────────────────────────

#[test]
fn a_blank_screen_comes_back_blank() {
    let trip = trip(20, 6, b"");
    assert_eq!(trip.probed.tabs, [8, 16]);
    assert_eq!(trip.probed.region, (0, 5));
}

#[test]
fn colours_and_styles_come_back_cell_for_cell() {
    let bytes = "\x1b[31mred\x1b[0m \x1b[38;5;1mindexed\x1b[0m \
                 \x1b[38;2;10;20;30mrgb\x1b[0m \x1b[44mbg\x1b[0m\r\n\
                 \x1b[1mbold\x1b[2mdim\x1b[22mx \x1b[3mitalic\x1b[23m \
                 \x1b[7minverse\x1b[27m \x1b[8mhidden\x1b[28m \x1b[9mstrike\x1b[29m\r\n\
                 \x1b[4mu\x1b[4:2md\x1b[4:3mc\x1b[4:4mo\x1b[4:5ma\x1b[24m \
                 \x1b[4:3;58;2;200;100;0mcurl\x1b[59mplain\x1b[58;5;9mpal\x1b[0m\r\n\
                 \x1b[95;104mbright\x1b[39;49m end\r\n";
    trip(80, 6, bytes.as_bytes());
}

#[test]
fn a_named_colour_stays_apart_from_its_palette_slot() {
    let fg = |screen: &Screen, column: usize| screen.term.grid()[Line(0)][Column(column)].fg;
    let original = fed(20, 6, b"\x1b[31ma\x1b[38;5;1mb\r\n");
    assert_ne!(fg(&original, 0), fg(&original, 1));
    let mut blank = screen(20, 6);
    blank.feed(&fed(20, 6, b"\x1b[31ma\x1b[38;5;1mb").image());
    assert_ne!(fg(&blank, 0), fg(&blank, 1));
    trip_of(original);
}

#[test]
fn wide_characters_and_emoji_come_back() {
    trip(
        20,
        6,
        "漢字 a👍🏽b 🇹🇷 ❤️ é\r\n👨\u{200d}👩\u{200d}👧 x\u{0301}\r\n".as_bytes(),
    );
}

#[test]
fn a_wide_character_at_the_edge_wraps_and_comes_back() {
    trip(10, 6, "abcdefghi漢字\r\nnext".as_bytes());
}

#[test]
fn a_wrap_a_wide_character_made_keeps_its_rows_after_the_character_goes() {
    // A wide character that did not fit left its spacer and wrapped; the row it went to
    // is erased, or overwritten from its start.
    trip(10, 6, "abcdefghi漢\x1b[2K\r\nnext".as_bytes());
    trip(10, 6, "abcdefghi漢\r\x1b[K  x\r\nnext".as_bytes());
}

#[test]
fn a_long_line_wraps_across_rows_and_comes_back_wrapped() {
    let screen = fed(10, 6, "abcdefghij".repeat(3).as_bytes());
    let wrapped = |line: i32| {
        screen.term.grid()[Line(line)][Column(9)]
            .flags
            .contains(Flags::WRAPLINE)
    };
    assert!(wrapped(0) && wrapped(1));
    trip_of(screen);
}

#[test]
fn a_wrapped_row_keeps_its_erased_continuation() {
    // The first row wraps into the second, which is erased afterwards: the wrap flag
    // stays, the continuation is blank.
    trip(10, 6, b"abcdefghijXY\x1b[2K\r\nnext");
    trip(10, 4, b"one\r\ntwo\r\nabcdefghijXY\r\nz\r\nw\x1b[1J");
}

#[test]
fn scrollback_above_the_screen_comes_back() {
    let text: String = (0..40).map(|n| format!("line {n}\r\n")).collect();
    let original = fed(20, 6, text.as_bytes());
    assert!(original.term.grid().history_size() > 0);
    trip_of(original);
}

#[test]
fn a_scrolling_region_comes_back_with_the_text_inside_it() {
    let trip = trip(
        20,
        8,
        b"top\x1b[3;6r\x1b[3;1Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\x1b[8;1Hbottom",
    );
    assert_eq!(trip.probed.region, (2, 5));
}

#[test]
fn origin_mode_keeps_the_cursor_inside_the_region() {
    let trip = trip(20, 8, b"\x1b[3;6r\x1b[?6h\x1b[2;4Hz");
    assert_eq!(trip.probed.region, (2, 5));
}

#[test]
fn a_vim_like_alternate_screen_comes_back_with_both_cursors_and_the_region() {
    let mut bytes: String = (0..12).map(|n| format!("line {n}\r\n")).collect();
    // The primary's saved cursor (bold green, line drawing in G0), then the shell's
    // prompt.
    bytes.push_str("\x1b[3;5H\x1b[1;32m\x1b(0\x1b7\x1b(B\x1b[0m\x1b[8;1H$ vim");
    // vim: the alternate screen, a region, a status line, its own saved cursor and a
    // cursor with an open link.
    bytes.push_str("\x1b[?1049h\x1b[2;7r\x1b[H~\r\n~\x1b[8;1H\x1b[7m-- INSERT --\x1b[0m");
    bytes.push_str("\x1b[4;3H\x1b[4mx\x1b7\x1b[24m\x1b[5;10H\x1b]8;id=a;https://x.dev\x07li");
    let original = fed(20, 8, bytes.as_bytes());
    assert!(original.term.mode().contains(TermMode::ALT_SCREEN));
    let trip = trip_of(original);
    assert_eq!(trip.probed.region, (1, 6));
}

#[test]
fn the_primary_screen_is_what_it_was_after_leaving_the_alternate_one() {
    let bytes = b"one\r\ntwo\x1b[1;2H\x1b7\x1b[2;3H\x1b[?1049hvim\x1b[?1049l";
    let original = fed(20, 6, bytes);
    assert!(!original.term.mode().contains(TermMode::ALT_SCREEN));
    trip_of(original);
}

#[test]
fn the_primary_saved_cursor_survives_the_alternate_screen() {
    // `?1049h` overwrote the saved cursor with the cursor (2;3): the replay must carry
    // that, not the `ESC 7` before it.
    let bytes = "one\r\ntwo\x1b[1;2H\x1b7\x1b[2;3H\x1b[?1049hvim\x1b[?1049l";
    let mut original = fed(20, 6, bytes.as_bytes());
    let (image, _) = encoded(&mut original);
    let replayed = fed(20, 6, &image);
    assert_eq!(
        replayed.term.grid().saved_cursor.point,
        alacritty_terminal::index::Point::new(Line(1), Column(2))
    );
    trip(20, 6, bytes.as_bytes());
}

#[test]
fn application_cursor_keys_keypad_and_paste_modes_come_back() {
    trip(20, 6, b"\x1b[?1h\x1b=\x1b[?2004h\x1b[?1004h\x1b[?1007h");
}

#[test]
fn mouse_modes_and_encodings_come_back() {
    trip(20, 6, b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
    trip(20, 6, b"\x1b[?1000h\x1b[?1005h");
    trip(20, 6, b"\x1b[?1003h\x1b[?1003l\x1b[?1002h");
}

#[test]
fn wrap_insert_newline_and_hidden_cursor_modes_come_back() {
    trip(20, 6, b"\x1b[?7l\x1b[4h\x1b[20h\x1b[?25l");
}

#[test]
fn every_cursor_shape_and_blinking_comes_back() {
    for (code, shape, blinking) in [
        (1, CursorShape::Block, true),
        (2, CursorShape::Block, false),
        (3, CursorShape::Underline, true),
        (4, CursorShape::Underline, false),
        (5, CursorShape::Beam, true),
        (6, CursorShape::Beam, false),
    ] {
        let trip = trip(20, 6, format!("\x1b[{code} q").as_bytes());
        assert_eq!(
            trip.probed.cursor_style,
            Some(CursorStyle { shape, blinking }),
            "DECSCUSR {code}"
        );
    }
}

#[test]
fn a_default_cursor_style_and_default_tabs_write_nothing() {
    let mut screen = fed(20, 6, b"$ ");
    let (bytes, probed) = encoded(&mut screen);
    assert_eq!(probed.cursor_style, None);
    assert_eq!(probed.tabs, [8, 16]);
    let text = lossy(&bytes);
    assert!(
        !text.contains(" q") && !text.contains("\x1b[3g"),
        "{text:?}"
    );
}

#[test]
fn modes_palette_style_keyboard_titles_tabs_and_the_active_set_come_back() {
    let bytes = "\x1b]4;1;rgb:12/34/56\x07\x1b]11;rgb:01/02/03\x07\x1b]12;#ff0000\x07\
                 \x1b[6 q\
                 \x1b[>1u\x1b[>3u\x1b[=5;1u\
                 \x1b]2;first\x07\x1b[22t\x1b]2;second\x07\x1b[22t\x1b]2;now\x07\
                 \x1b[3g\x1b[5G\x1bH\x1b[13G\x1bH\
                 \x1b)0\x0e\x1b[2;3Habc\
                 \x1b[?2004h\x1b[?1004h\x1b[?1002h\x1b[?1006h\x1b[?1h\x1b=\x1b[4h\x1b[?7l\x1b[20h\x1b[?25l";
    let trip = trip(20, 6, bytes.as_bytes());
    let probed = &trip.probed;
    assert_eq!(probed.tabs, [4, 12]);
    assert_eq!(probed.charset, CharsetIndex::G1);
    assert_eq!(
        probed.keyboard,
        [
            vec![
                KeyboardModes::DISAMBIGUATE_ESC_CODES,
                KeyboardModes::from_bits_truncate(3)
            ],
            Vec::new()
        ]
    );
    assert_eq!(
        probed.titles,
        [Some("first".to_owned()), Some("second".to_owned())]
    );
    assert_eq!(probed.title.as_deref(), Some("now"));
    assert_eq!(trip.title.as_deref(), Some("now"));
    assert_eq!(
        probed.cursor_style,
        Some(CursorStyle {
            shape: CursorShape::Beam,
            blinking: false
        })
    );
}

#[test]
fn the_palette_comes_back_changed_and_only_changed() {
    let bytes = b"\x1b]4;5;rgb:aa/bb/cc\x07\x1b]10;rgb:11/22/33\x07x";
    let mut original = fed(20, 6, bytes);
    let (image, _) = encoded(&mut original);
    let text = lossy(&image);
    assert_eq!(text.matches("\x1b]4;").count(), 1, "{text:?}");
    assert!(text.contains("\x1b]4;5;rgb:aa/bb/cc\x07"), "{text:?}");
    assert!(text.contains("\x1b]10;rgb:11/22/33\x07"), "{text:?}");
    trip(20, 6, bytes);
}

#[test]
fn the_keyboard_stack_kept_off_the_alternate_screen_comes_back() {
    // alacritty keeps the alternate stack when the screen is left and gives it back on
    // the next entry.
    let trip_off = trip(20, 6, b"\x1b[>1u\x1b[?1049h\x1b[>2u\x1b[>4u\x1b[?1049l");
    assert_eq!(
        trip_off.probed.keyboard,
        [
            vec![KeyboardModes::DISAMBIGUATE_ESC_CODES],
            vec![
                KeyboardModes::REPORT_EVENT_TYPES,
                KeyboardModes::REPORT_ALTERNATE_KEYS
            ]
        ]
    );
    // And on the alternate screen, its stack is the active one.
    let trip_on = trip(20, 6, b"\x1b[>1u\x1b[?1049h\x1b[>2u");
    assert_eq!(
        trip_on.probed.keyboard,
        [
            vec![KeyboardModes::DISAMBIGUATE_ESC_CODES],
            vec![KeyboardModes::REPORT_EVENT_TYPES]
        ]
    );
}

#[test]
fn links_with_their_ids_clusters_wide_characters_and_wrapped_rows_come_back() {
    let bytes = format!(
        "\x1b]8;;https://a.dev/3\x07$ ls\x1b]8;;\x07\r\n\
         \x1b]8;id=k;https://x.dev\x07x\x1b]8;;\x07 \x1b]8;;file:///tmp\x07t\x1b]8;;\x07\r\n\
         漢字 a👍🏽b 🇹🇷 ❤️ é\r\n{}\r\n\
         \x1b]8;;https://a.dev/4\x07$ typ",
        "abcdefghij".repeat(3)
    );
    trip(10, 6, bytes.as_bytes());
}

#[test]
fn a_tab_comes_back_in_its_cell_and_the_text_after_it_in_place() {
    // The stops in force when the row was written are not the replay's: the default
    // ones, then custom ones.
    trip(20, 6, b"\tx\tu\r\nab\tc");
    trip(20, 6, b"\x1b[3g\x1b[5G\x1bH\x1b[12G\x1bH\r\n\tT\tU\r\n\tx");
}

#[test]
fn a_wrapped_row_with_a_background_does_not_colour_the_rest_of_the_row() {
    // The continuation starts with a coloured cell and ends short; the screen scrolls for
    // the wrap, and the new row would come filled with that colour.
    let text: String = (0..8).map(|n| format!("row {n}\r\n")).collect();
    trip(
        10,
        4,
        format!("{text}\x1b[42mabcdefghijkl\x1b[0m\r\nz").as_bytes(),
    );
}

#[test]
fn a_tab_cell_keeps_the_background_it_had() {
    // A tab only turns a blank cell into a tab: the cell keeps its green.
    let original = fed(20, 6, b"\x1b[42m\x1b[K\x1b[0m\x1b[1G\tx");
    let first = original.term.grid()[Line(0)][Column(0)].clone();
    assert_eq!(first.c, '\t');
    assert_ne!(
        first.bg,
        Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background)
    );
    trip_of(original);
}

#[test]
fn a_tab_in_a_wrapping_rows_last_cell_keeps_the_wrap() {
    trip(10, 6, b"abcdefghi KL\x1b[1;10H\t\x1b[3;1Hz");
}

#[test]
fn the_last_columns_tab_stop_comes_back() {
    // A stop set there, and a fresh grid's stop there cleared: a tab cannot tell either
    // at this width, a wider grid can.
    let trip_set = trip(12, 6, b"\x1b[3g\x1b[5G\x1bH\x1b[12G\x1bH");
    assert_eq!(
        (trip_set.probed.tabs, trip_set.probed.last_tab),
        (vec![4], true)
    );
    let trip_clear = trip(17, 6, b"\x1b[3g\x1b[5G\x1bH");
    assert_eq!(
        (trip_clear.probed.tabs, trip_clear.probed.last_tab),
        (vec![4], false)
    );
    // The defaults write nothing.
    assert!(trip(17, 6, b"x").probed.last_tab);
}

#[test]
fn the_line_drawing_set_and_its_cells_come_back() {
    trip(20, 6, b"\x1b(0lqqk\x1b(B ok\r\n\x1b(0x");
}

#[test]
fn a_cursor_waiting_to_wrap_comes_back_waiting() {
    // The saved cursor at the last column, then the cursor on a wide character's
    // spacer.
    let bytes = "abcdefghij\x1b7\r\n漢字漢字漢";
    let original = fed(10, 6, bytes.as_bytes());
    assert!(original.term.grid().cursor.input_needs_wrap);
    assert!(original.term.grid().saved_cursor.input_needs_wrap);
    trip_of(original);
}

#[test]
fn a_cursor_waiting_to_wrap_over_a_wrapped_row_keeps_the_text_in_place() {
    // The cursor is restored to the last column with a wrap pending, after the row wrapped
    // under it: writing its cell again for the pending wrap clears the wrap, and the rows
    // after it must not move.
    trip(10, 4, b"abcdefghij\x1b7k\x1b8\r\n\n\nlast");
    trip(10, 4, b"abcdefghij\x1b7k\x1b8");
}

#[test]
fn titles_and_their_stack_come_back() {
    let trip = trip(
        20,
        6,
        b"\x1b]2;one\x07\x1b[22t\x1b]2;two\x07\x1b[22t\x1b]2;three\x07",
    );
    assert_eq!(trip.title.as_deref(), Some("three"));
    assert_eq!(
        trip.probed.titles,
        [Some("one".to_owned()), Some("two".to_owned())]
    );
}

#[test]
fn a_synchronized_update_is_applied_as_it_comes() {
    // The begin sequence without its end: a parser that held the bytes back would show a
    // blank screen, and nothing here would ever end the wait.
    let screen = fed(20, 6, b"\x1b[?2026hhello\r\nworld");
    assert_eq!(cells(&screen).len(), 2);
    trip_of(screen);
}

#[test]
fn a_resized_screen_comes_back_at_its_new_size() {
    let text: String = (0..12)
        .map(|n| format!("line number {n} is long\r\n"))
        .collect();
    let mut original = fed(30, 8, text.as_bytes());
    original.resize(12, 5);
    assert_eq!(original.size(), (12, 5));
    trip_of(original);
}

#[test]
fn a_size_of_zero_is_one() {
    let mut screen = screen(0, 0);
    assert_eq!(screen.size(), (1, 1));
    screen.feed(b"abc\r\ndef");
    screen.resize(0, 3);
    assert_eq!(screen.size(), (1, 3));
    let _ = screen.image();
}

#[test]
fn every_mode_bit_is_written_or_named_as_not_written() {
    let mut seen = 0;
    for (name, bit) in TermMode::all().iter_names() {
        if bit.bits().count_ones() != 1 {
            continue;
        }
        seen += 1;
        let written = MODES.iter().any(|(mode, _, _)| *mode == bit);
        assert!(
            written != MODES_NOT_WRITTEN.contains(bit),
            "{name} is written {written}"
        );
    }
    assert_eq!(seen, 23, "a new mode bit needs a place");
}

// ─── random streams ──────────────────────────────────────────────────────────────

/// A small deterministic generator: the streams are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// (No tab: a tab character in the cell a cursor waits after cannot be written as a tab
/// and as the wait at once, a corner of its own. The scenes cover tabs.)
const FRAGMENTS: &[&str] = &[
    "hello",
    "a",
    "é",
    "\r\n",
    "\n",
    "\r",
    "\x08",
    "\x1b[1m",
    "\x1b[2m",
    "\x1b[3m",
    "\x1b[4m",
    "\x1b[4:3m",
    "\x1b[7m",
    "\x1b[9m",
    "\x1b[0m",
    "\x1b[22m",
    "\x1b[31m",
    "\x1b[42m",
    "\x1b[38;5;200m",
    "\x1b[48;2;10;200;30m",
    "\x1b[58;2;1;2;3m",
    "\x1b[39;49m",
    "\x1b[H",
    "\x1b[3;4H",
    "\x1b[2;9H",
    "\x1b[A",
    "\x1b[2B",
    "\x1b[3C",
    "\x1b[2D",
    "\x1b[J",
    "\x1b[1J",
    "\x1b[2J",
    "\x1b[K",
    "\x1b[1K",
    "\x1b[2K",
    "\x1b[2L",
    "\x1b[M",
    "\x1b[3X",
    "\x1b[S",
    "\x1b[T",
    "\x1b[2;5r",
    "\x1b[r",
    // Origin mode is left out: restoring a cursor saved before the region (`ESC 8`)
    // can leave the cursor outside the region with the mode on, and no sequence puts it
    // there (see the writer's limits). The scenes cover the mode.
    "\x1b[?6l",
    "\x1b[?7l",
    "\x1b[?7h",
    "\x1b[4l",
    "\x1b[?25l",
    "\x1b[?25h",
    "\x1b[?1h",
    "\x1b[?1000h",
    "\x1b[?1006h",
    "\x1b[?2004h",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b7",
    "\x1b8",
    "\x1b(0",
    "\x1b(B",
    "\x1b[3g",
    "\x1bH",
    "\x1b[5G",
    "\x1b]8;id=q;https://x.dev\x07",
    "\x1b]8;;\x07",
    "\x1b]2;a title\x07",
    "\x1b[22t",
    "\x1b[23t",
    "\x1b[3 q",
    "\x1b[>1u",
    "\x1b[<u",
    "\x1b]4;3;rgb:10/20/30\x07",
];

/// Characters of two columns and combining ones. Kept apart from [`SHIFTS`]: moving cells
/// sideways under a wide character (or inserting in front of one) leaves its halves apart
/// or overlapping, a grid no sequence can build, so a stream has one or the other.
const WIDE: &[&str] = &["漢字", "👍", "x\u{0301}", "é"];

/// Sequences that move the cells of a row sideways.
const SHIFTS: &[&str] = &["\x1b[2@", "\x1b[2P", "\x1b[4h", "\x1b[4l"];

/// Streams drawn from the fragments above, with a fixed seed. They found what the scenes
/// did not think of (a blank scrolled in with the wrap's colour, a tab cell's background,
/// a wide character whose second half was erased); a longer run finds states no sequence
/// can build — wide characters overlapping after erases — which the writer does not try to
/// reproduce.
#[test]
fn random_streams_round_trip() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for run in 0..600 {
        let columns = 4 + rng.below(20) as u16;
        let rows = 3 + rng.below(8) as u16;
        let extra = if rng.below(2) == 0 { WIDE } else { SHIFTS };
        let mut stream = String::new();
        for _ in 0..(10 + rng.below(70)) {
            let pick = rng.below(FRAGMENTS.len() + extra.len());
            let fragment = FRAGMENTS
                .get(pick)
                .or_else(|| extra.get(pick - FRAGMENTS.len()));
            if let Some(fragment) = fragment {
                stream.push_str(fragment);
            }
        }
        let original = fed(columns, rows, stream.as_bytes());
        let result = std::panic::catch_unwind(|| trip_of(original));
        assert!(result.is_ok(), "run {run}, {columns}x{rows}: {stream:?}");
    }
}

// ─── the screen, live ────────────────────────────────────────────────────────────

const STREAM: &str = "ab\x1b[1;31mred\x1b[0m 👍 é\x1b]2;title\x07\x1b[2;3Hz\x1bP1$qm\x1b\\\r\nnext\
                      \x1b_Gq=2;AAAA\x1b\\ \x1b[?1049hvim\x1b[?1049l tail";

/// The bytes the writer gives for a screen (which spends it), for comparing two screens.
fn written(mut screen: Screen) -> String {
    lossy(&encoded(&mut screen).0)
}

#[test]
fn a_stream_cut_anywhere_comes_back_whole_with_its_tail() {
    let stream = STREAM.as_bytes();
    let expected = written(fed(20, 6, stream));
    for cut in 0..=stream.len() {
        // The reader's reads are any split of the bytes it got.
        let mut old = screen(20, 6);
        old.feed(stream.get(..cut / 2).unwrap_or_default());
        old.feed(stream.get(cut / 2..cut).unwrap_or_default());
        let rest = stream.get(cut..).unwrap_or_default();

        // The client that gets the image, then the bytes after the cut.
        let image = old.image();
        let tail = old.tail.bytes().to_vec();
        assert!(image.ends_with(&tail), "cut at {cut}");
        let mut client = screen(20, 6);
        client.feed(&image);
        client.feed(rest);
        assert_eq!(
            written(client),
            expected,
            "cut at {cut}, tail {:?}",
            lossy(&tail)
        );

        // And the screen the image was taken of goes on as if nothing happened.
        old.feed(rest);
        assert_eq!(written(old), expected, "cut at {cut} on the live screen");
    }
}

#[test]
fn an_image_taken_twice_is_the_same_bytes() {
    let mut screen = fed(20, 6, STREAM.as_bytes());
    let first = screen.image();
    let second = screen.image();
    assert_eq!(lossy(&first), lossy(&second));
}

#[test]
fn feeding_after_an_image_is_feeding_without_it() {
    let (head, rest) = STREAM.as_bytes().split_at(30);
    let mut imaged = fed(20, 6, head);
    let _ = imaged.image();
    imaged.feed(rest);
    let plain = fed(20, 6, STREAM.as_bytes());
    assert_eq!(readable(&imaged), readable(&plain));
    assert_eq!(cells(&imaged), cells(&plain));
    assert_eq!(imaged.title(), plain.title());
}

#[test]
fn a_program_that_asks_gets_the_terminals_answer() {
    let mut screen = screen(20, 6);
    screen.feed(b"x\x1b[5;3H\x1b[6n");
    assert_eq!(lossy(&screen.take_replies()), "\x1b[5;3R");
    assert!(screen.take_replies().is_empty());
    screen.feed(b"\x1b[c");
    assert!(lossy(&screen.take_replies()).starts_with("\x1b[?"));
}

#[test]
fn the_questions_a_program_asks_a_terminal_are_answered() {
    let answer = |question: &str| {
        let mut screen = screen(20, 6);
        screen.feed(question.as_bytes());
        lossy(&screen.take_replies())
    };
    // Device attributes (both spellings), status, size in characters, mode reports and the
    // kitty keyboard query.
    assert_eq!(answer("\x1b[c"), "\x1b[?6c");
    assert_eq!(answer("\x1bZ"), "\x1b[?6c");
    assert!(answer("\x1b[>c").starts_with("\x1b[>"));
    assert_eq!(answer("\x1b[5n"), "\x1b[0n");
    assert_eq!(answer("\x1b[18t"), "\x1b[8;6;20t");
    assert_eq!(answer("\x1b[?7$p"), "\x1b[?7;1$y");
    assert_eq!(answer("\x1b[4$p"), "\x1b[4;2$y");
    assert_eq!(answer("\x1b[?u"), "\x1b[?0u");
    // The size in pixels is the owner's to give; nobody here knows the cell.
    assert_eq!(answer("\x1b[14t"), "");
}

#[test]
fn an_image_leaves_the_answers_waiting_and_adds_none() {
    let mut screen = fed(20, 6, b"\x1b[4;2H\x1b[6n\x1b[>1u\x1b[?1049hq");
    let _ = screen.image();
    assert_eq!(lossy(&screen.take_replies()), "\x1b[4;2R");
    let _ = screen.image();
    assert!(screen.take_replies().is_empty());
}

#[test]
fn questions_that_need_the_owner_get_no_answer() {
    let mut screen = screen(20, 6);
    // The foreground colour, the clipboard.
    screen.feed(b"\x1b]10;?\x07\x1b]52;c;?\x07");
    assert!(screen.take_replies().is_empty());
}

#[test]
fn answers_nobody_takes_are_bounded() {
    let mut screen = screen(20, 6);
    let questions = "\x1b[6n".repeat(40_000);
    screen.feed(questions.as_bytes());
    assert_eq!(screen.take_replies().len(), super::REPLY_LIMIT);
}

#[test]
fn the_title_is_the_programs_last() {
    let mut screen = screen(20, 6);
    assert_eq!(screen.title(), None);
    screen.feed(b"\x1b]2;vim\x07");
    assert_eq!(screen.title().as_deref(), Some("vim"));
    let _ = screen.image();
    assert_eq!(screen.title().as_deref(), Some("vim"));
}

// ─── the cut sequence ────────────────────────────────────────────────────────────

fn tail_of(bytes: &[u8]) -> Vec<u8> {
    let mut tail = Tail::default();
    tail.feed(bytes);
    tail.bytes().to_vec()
}

#[test]
fn the_tail_holds_only_what_has_not_taken_effect() {
    // A line feed vte executed inside the CSI is not replayed.
    assert_eq!(tail_of(b"x\x1b[3\n1"), b"\x1b[31");
    // An ESC ends the OSC and starts a new sequence.
    assert_eq!(tail_of(b"\x1b]2;t\x1b"), b"\x1b");
    assert_eq!(tail_of("a\u{1F44D}".as_bytes()), b"");
    assert_eq!(tail_of(&"a\u{1F44D}".as_bytes()[..3]), &[0xf0, 0x9f]);
    assert_eq!(tail_of(b"\x1b[0m"), b"");
    assert_eq!(tail_of(b"\x1b]8;;x\x07"), b"");
    assert_eq!(tail_of(b"\x1b\x1b["), b"\x1b[");
    // `0x9c` ends a DCS passthrough, not an APC: there it is a UTF-8 continuation byte
    // (`Ŝ`).
    assert_eq!(tail_of(b"\x1bPq\x9c"), b"");
    assert_eq!(tail_of("\x1b_Ŝ".as_bytes()), "\x1b_Ŝ".as_bytes());
    assert_eq!(tail_of(b"\x1bP1<q\x9c"), b"\x1bP1<q\x9c");
}

/// Counts the application program commands a parser completes.
#[derive(Default)]
struct Ends(u32);

impl alacritty_terminal::vte::ansi::Handler for Ends {
    fn apc_end(&mut self, cancelled: bool) {
        if !cancelled {
            self.0 += 1;
        }
    }
}

#[test]
fn a_command_cut_anywhere_is_completed_once_by_the_parser_that_gets_its_tail() {
    // Two commands, a cancelled one and one whose `ESC` is cut from its `\`.
    let stream: &[u8] =
        b"a\x1b_Ga=t,i=1;AAAA\x1b\\b\x1b_Gq=2;BB\x18c\x1b_Gm=0;CC\x1b\\\x1b_G;D\x1b[1mx";
    let ends = |bytes: &[u8]| {
        let mut parser: Processor<NoSync> = Processor::new();
        let mut ends = Ends::default();
        parser.advance(&mut ends, bytes);
        ends.0
    };
    assert_eq!(ends(stream), 2);
    for cut in 0..=stream.len() {
        let head = stream.get(..cut).unwrap_or_default();
        let carried = [
            tail_of(head).as_slice(),
            stream.get(cut..).unwrap_or_default(),
        ]
        .concat();
        assert_eq!(
            ends(head) + ends(&carried),
            ends(stream),
            "cut at {cut}, tail {:?}",
            lossy(&tail_of(head))
        );
    }
}

#[test]
fn the_tail_keeps_a_command_open_past_its_esc_until_the_next_byte() {
    assert_eq!(tail_of(b"\x1b_Ga=t;AA\x1b"), b"\x1b_Ga=t;AA\x1b");
    assert_eq!(tail_of(b"\x1b_Ga=t;AA\x1b\\"), b"");
    // Cancelled: the byte after the `ESC` begins a sequence of its own.
    assert_eq!(tail_of(b"\x1b_Ga=t;AA\x1b[1"), b"\x1b[1");
    assert_eq!(tail_of(b"\x1b_Ga=t;AA\x1b_Gb"), b"\x1b_Gb");
    assert_eq!(tail_of(b"\x1b_Ga=t;AA\x1b\x18"), b"");
    // A command that is not ours is not held open past its `ESC`.
    assert_eq!(tail_of(b"\x1bXjunk\x1b"), b"\x1b");
}

#[test]
fn a_command_that_never_ends_does_not_grow_the_tail_without_end() {
    const SMALL: usize = 1 << 20;
    let mut tail = Tail::with_apc_keep(SMALL);
    tail.feed(b"\x1b_Ga=t;");
    let chunk = vec![b'A'; 1 << 18];
    for _ in 0..(SMALL >> 18) + 2 {
        tail.feed(&chunk);
    }
    assert_eq!(tail.bytes().len(), SMALL);
    tail.feed(b"\x1b\\");
    assert!(tail.bytes().is_empty());
}

#[test]
fn a_command_the_receiver_takes_reaches_it_whole() {
    // The largest graphics command bateri accepts is 64 MiB of payload; the limit leaves
    // that and a header, so a command that terminal takes is never cut short.
    let (_, apc_keep) = KEEP_LIMITS;
    assert!(apc_keep >= (64 << 20) + b"\x1b_Ga=T,f=100,m=0;".len());
}

#[test]
fn a_large_osc_does_not_pin_the_tails_capacity() {
    let (tail_keep, _) = KEEP_LIMITS;
    let mut tail = Tail::default();
    let mut osc = b"\x1b]52;c;".to_vec();
    osc.extend(std::iter::repeat_n(b'A', 100_000));
    tail.feed(&osc);
    assert!(tail.bytes().len() > 100_000);
    tail.feed(b"\x07");
    assert!(tail.capacity() <= tail_keep);
}

// ─── the modes the emulator drops, and forgetting the scrollback ─────────────────

const NONE: Kept = Kept {
    pixel_mouse: false,
    resize_reports: false,
};
const PIXEL: Kept = Kept {
    pixel_mouse: true,
    resize_reports: false,
};
const REPORTS: Kept = Kept {
    pixel_mouse: false,
    resize_reports: true,
};
const BOTH: Kept = Kept {
    pixel_mouse: true,
    resize_reports: true,
};

fn kept_after(bytes: &[u8]) -> Kept {
    let mut tail = Tail::default();
    tail.feed(bytes);
    tail.kept()
}

fn has(image: &[u8], needle: &[u8]) -> bool {
    image.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn the_stream_turns_the_modes_the_emulator_drops_on_and_off() {
    assert_eq!(kept_after(b""), NONE);
    assert_eq!(kept_after(b"\x1b[?1016h"), PIXEL);
    assert_eq!(kept_after(b"\x1b[?2048h"), REPORTS);
    // A list names every mode in it, the others among them.
    assert_eq!(kept_after(b"\x1b[?1000;1016;1006;2048h"), BOTH);
    assert_eq!(kept_after(b"\x1b[?1016h\x1b[?2048h\x1b[?1016l"), REPORTS);
    assert_eq!(kept_after(b"\x1b[?1016;2048h\x1b[?2048;1016l"), NONE);
    // The parser reads a number as it is: leading zeros, and the first of a subparameter.
    assert_eq!(kept_after(b"\x1b[?01016h"), PIXEL);
    assert_eq!(kept_after(b"\x1b[?1016:1h"), PIXEL);
    assert_eq!(kept_after(b"\x1b[?5:2048h"), NONE);
    // The text between is whatever else happened: text, controls the parser executes.
    assert_eq!(kept_after(b"x\x1b[?10\n16\x07hy"), PIXEL);
}

#[test]
fn what_the_parser_would_not_dispatch_is_not_followed() {
    for ignored in [
        // Not a private mode, or not set or reset.
        &b"\x1b[1016h"[..],
        b"\x1b[?1016$p",
        b"\x1b[?1016p",
        b"\x1b[?1016m",
        // A marker after the parameters, an intermediate before them, a byte that is none.
        b"\x1b[?1?1016h",
        b"\x1b[1;?1016h",
        b"\x1b[?$1016h",
        b"\x1b[>1016h",
        b"\x1b[?1016 h",
        b"\x1b[?1016x6h",
        // A number that is not 1016 or 2048 after saturating, and one that is empty.
        b"\x1b[?71016h",
        b"\x1b[?;h",
        // Another string with the bytes in it.
        b"\x1b]0;[?1016h\x07",
        b"\x1b_G[?1016h\x1b\\",
        b"\x1bP1$r[?1016h\x1b\\",
        // A cancelled one.
        b"\x1b[?10\x1816h",
        b"\x1b[?10\x1a16h",
    ] {
        assert_eq!(kept_after(ignored), NONE, "{:?}", lossy(ignored));
    }
    // The parser keeps 32 parameters, the last of a list one of them.
    let list = |separators: usize| {
        let mut sequence = b"\x1b[?1016".to_vec();
        sequence.extend(b";0".repeat(separators));
        sequence.push(b'h');
        sequence
    };
    assert_eq!(kept_after(&list(31)), PIXEL);
    assert_eq!(kept_after(&list(32)), NONE);
}

#[test]
fn a_full_reset_turns_them_off_and_nothing_else_does() {
    assert_eq!(kept_after(b"\x1b[?1016h\x1b[?2048h\x1bc"), NONE);
    assert_eq!(kept_after(b"\x1bc\x1b[?1016h"), PIXEL);
    // `ESC ( c` designates a character set; the soft reset and the screen switches leave
    // the modes where they are.
    assert_eq!(kept_after(b"\x1b[?1016h\x1b(c"), PIXEL);
    assert_eq!(
        kept_after(b"\x1b[?1016h\x1b[!p\x1b[?1049h\x1b[?1049l\x1b[2J"),
        PIXEL
    );
}

#[test]
fn the_modes_are_followed_however_the_stream_is_cut() {
    let stream =
        b"ab\x1b[?1000;10\x1b[?1016h16;2048h\x1b[?2048l\x1b[?1\x1b[?1016l\x1b[?1016;2048hc";
    let whole = kept_after(stream);
    assert_eq!(whole, BOTH);
    for piece in [1, 2, 3, 7] {
        let mut tail = Tail::default();
        for chunk in stream.chunks(piece) {
            tail.feed(chunk);
        }
        assert_eq!(tail.kept(), whole, "pieces of {piece}");
    }
}

#[test]
fn the_image_turns_on_what_the_stream_turned_on() {
    let rich = b"\x1b[1;31mred\x1b[0m text\r\n\x1b]0;a title\x07\x1b[?1000h\x1b[?1006h";
    for (modes, written) in [
        (&b""[..], &[][..]),
        (b"\x1b[?1016h", &[&b"\x1b[?1016h"[..]][..]),
        (b"\x1b[?2048h", &[&b"\x1b[?2048h"[..]][..]),
        (
            b"\x1b[?2048;1016h",
            &[&b"\x1b[?1016h"[..], &b"\x1b[?2048h"[..]][..],
        ),
        (b"\x1b[?1016h\x1b[?2048h\x1b[?1016;2048l", &[][..]),
        (b"\x1b[?1016h\x1b[?2048h\x1bc", &[][..]),
    ] {
        let mut stream = rich.to_vec();
        stream.extend_from_slice(modes);
        let mut original = fed(30, 6, &stream);
        let image = original.image();
        for needle in [&b"\x1b[?1016h"[..], b"\x1b[?2048h"] {
            assert_eq!(
                has(&image, needle),
                written.contains(&needle),
                "{:?}: {:?}",
                lossy(modes),
                lossy(&image)
            );
        }
        // A blank screen fed the image shows what the original showed, has the modes the
        // same, and writes the image again byte for byte; the screen that wrote it still
        // has them, which is the rebuild from its own image reading them back.
        let mut replayed = fed(30, 6, &image);
        assert_eq!(replayed.tail.kept(), original.tail.kept());
        assert_eq!(readable(&replayed), readable(&original));
        assert_same_bytes(&replayed.image(), &image);
        assert_same_bytes(&original.image(), &image);
    }
}

#[test]
fn the_image_ends_with_the_open_sequence_and_the_modes_come_before_it() {
    let mut screen = fed(30, 6, b"hi\x1b[?1016h\x1b[?20");
    let image = screen.image();
    assert!(image.ends_with(b"\x1b[?20"), "{:?}", lossy(&image));
    assert!(has(&image, b"\x1b[?1016h"));
    // The sequence the stream stopped in goes on from where it stood.
    screen.feed(b"48h");
    assert_eq!(screen.tail.kept(), BOTH);
    let mut replayed = fed(30, 6, &image);
    replayed.feed(b"48h");
    assert_eq!(replayed.tail.kept(), BOTH);
}

fn numbered(count: usize) -> Vec<u8> {
    (1..=count)
        .flat_map(|n| format!("row{n:02}\r\n").into_bytes())
        .collect()
}

#[test]
fn forgetting_the_history_keeps_the_screen_and_what_scrolls_off_after() {
    let mut screen = fed(10, 3, &numbered(20));
    let before = screen.text(HISTORY, false);
    assert!(before.len() > 3, "there is history: {before:?}");
    let shown = screen.text(0, false);
    assert!(lossy(&screen.image()).contains("row01"));

    screen.forget_history();
    assert_eq!(
        screen.text(HISTORY, false),
        shown,
        "only the screen is left"
    );
    let image = lossy(&screen.image());
    assert!(
        !image.contains("row01") && !image.contains("row16"),
        "{image:?}"
    );
    assert!(image.contains("row20"), "the screen stayed: {image:?}");

    // What scrolls off from now on is kept, and the stream goes on as it did.
    screen.feed(b"new1\r\nnew2\r\n");
    let after = screen.text(HISTORY, false);
    assert_eq!(after.len(), shown.len() + 2, "{after:?}");
    assert_eq!(after.first().map(String::as_str), Some("row19"));

    // Forgetting nothing is nothing.
    let mut blank = fed(10, 3, b"");
    blank.forget_history();
    assert_eq!(
        blank.text(HISTORY, false),
        fed(10, 3, b"").text(HISTORY, false)
    );
}

#[test]
fn a_forget_while_a_program_holds_the_alternate_screen_waits_for_it_to_give_it_back() {
    let mut screen = fed(10, 3, &numbered(20));
    screen.feed(b"\x1b[?1049halt\r\n");
    screen.forget_history();
    // The primary screen is out of reach: nothing is done, nothing is lost.
    screen.feed(b"more alt\r\n");
    assert!(screen.forgetting);
    screen.feed(b"\x1b[?1049l");
    assert!(!screen.forgetting);
    let text = screen.text(HISTORY, false);
    assert_eq!(
        text.len(),
        3,
        "the history is gone, the screen is back: {text:?}"
    );
    assert_eq!(text.last().map(String::as_str), Some(""));
    assert!(text.iter().any(|row| row == "row20"), "{text:?}");

    // A program that leaves without a forget asked leaves the history alone.
    let mut other = fed(10, 3, &numbered(20));
    other.feed(b"\x1b[?1049halt\r\n\x1b[?1049l");
    assert!(other.text(HISTORY, false).len() > 3);
}
