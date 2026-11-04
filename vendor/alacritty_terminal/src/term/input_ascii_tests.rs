// Added to alacritty_terminal 0.26.0 (Apache License 2.0, section 4(b)): this file is not part
// of the release. It tests `Term`'s `Handler::input_ascii` against `input`; FORK.md lists the
// changes of the whole package.

//! `Term::input_ascii` must leave exactly what `input` called for each character leaves.
//!
//! Every test runs the same bytes through two terminals: one takes the runs the parser hands
//! over (`input_ascii`), the other is driven through [`PerChar`], which forwards everything
//! but runs, so the default turns each run into `input` calls. Then it compares what a
//! dependent can see or tell from the outside: every cell, the cursor and its pending wrap,
//! the rows' `occ`, the damage and the modes.

use super::*;

use crate::event::VoidListener;
use crate::grid::Row;
use crate::index::{Column, Line};
use crate::term::cell::{Cell, Flags};
use crate::term::test::TermSize;
use crate::vte::ansi::{self, Handler, Processor};

type Screen = Term<VoidListener>;

/// A handler that is a `Term` in all but `input_ascii`, which keeps its default.
struct PerChar<'a>(&'a mut Screen);

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(fn $name(&mut self $(, $arg: $ty)*) { Handler::$name(&mut *self.0 $(, $arg)*) })*
    };
}

impl Handler for PerChar<'_> {
    forward! {
        set_title(a: Option<String>);
        set_cursor_style(a: Option<ansi::CursorStyle>);
        set_cursor_shape(a: ansi::CursorShape);
        input(a: char);
        goto(a: i32, b: usize);
        goto_line(a: i32);
        goto_col(a: usize);
        insert_blank(a: usize);
        move_up(a: usize);
        move_down(a: usize);
        identify_terminal(a: Option<char>);
        device_status(a: usize);
        move_forward(a: usize);
        move_backward(a: usize);
        move_down_and_cr(a: usize);
        move_up_and_cr(a: usize);
        put_tab(a: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        set_horizontal_tabstop();
        scroll_up(a: usize);
        scroll_down(a: usize);
        insert_blank_lines(a: usize);
        delete_lines(a: usize);
        erase_chars(a: usize);
        delete_chars(a: usize);
        move_backward_tabs(a: u16);
        move_forward_tabs(a: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(a: ansi::LineClearMode);
        clear_screen(a: ansi::ClearMode);
        clear_tabs(a: ansi::TabulationClearMode);
        set_tabs(a: u16);
        reset_state();
        reverse_index();
        terminal_attribute(a: ansi::Attr);
        set_mode(a: ansi::Mode);
        unset_mode(a: ansi::Mode);
        report_mode(a: ansi::Mode);
        set_private_mode(a: ansi::PrivateMode);
        unset_private_mode(a: ansi::PrivateMode);
        report_private_mode(a: ansi::PrivateMode);
        set_scrolling_region(a: usize, b: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(a: ansi::CharsetIndex);
        configure_charset(a: ansi::CharsetIndex, b: ansi::StandardCharset);
        set_color(a: usize, b: ansi::Rgb);
        dynamic_color_sequence(a: String, b: usize, c: &str);
        reset_color(a: usize);
        clipboard_store(a: u8, b: &[u8]);
        clipboard_load(a: u8, b: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(a: Option<ansi::Hyperlink>);
        set_mouse_cursor_icon(a: ansi::cursor_icon::CursorIcon);
        set_pointer_shape(a: &[u8], b: &str);
        report_keyboard_mode();
        push_keyboard_mode(a: ansi::KeyboardModes);
        pop_keyboard_modes(a: u16);
        set_keyboard_mode(a: ansi::KeyboardModes, b: ansi::KeyboardModesApplyBehavior);
        set_modify_other_keys(a: ansi::ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(a: ansi::ScpCharPath, b: ansi::ScpUpdateMode);
        apc_start();
        apc_put(a: &[u8]);
        apc_end(a: bool);
    }

    fn halt_requested(&self) -> bool {
        Handler::halt_requested(&*self.0)
    }
}

/// Everything the two terminals could differ in, as far as a dependent can tell.
fn assert_same(fast: &Screen, slow: &Screen, ctx: &str) {
    assert_same_grid(fast.grid(), slow.grid(), "grid", ctx);
    assert_same_grid(&fast.inactive_grid, &slow.inactive_grid, "inactive grid", ctx);

    assert_eq!(fast.mode(), slow.mode(), "{ctx}: modes");
    assert_eq!(fast.active_charset, slow.active_charset, "{ctx}: active charset");
    assert_eq!(fast.scroll_region, slow.scroll_region, "{ctx}: scroll region");
    assert_eq!(fast.damage.full, slow.damage.full, "{ctx}: full damage");
    assert_eq!(fast.damage.lines, slow.damage.lines, "{ctx}: damage");
    assert_eq!(fast.damage.last_cursor, slow.damage.last_cursor, "{ctx}: last cursor");
}

fn assert_same_grid(fast: &Grid<Cell>, slow: &Grid<Cell>, which: &str, ctx: &str) {
    assert_eq!(fast.cursor, slow.cursor, "{ctx}: {which}: cursor and its pending wrap");
    assert_eq!(fast.display_offset(), slow.display_offset(), "{ctx}: {which}: display offset");
    assert_eq!(fast.total_lines(), slow.total_lines(), "{ctx}: {which}: lines");

    let first = -(fast.history_size() as i32);
    for line in first..fast.screen_lines() as i32 {
        let (fast, slow): (&Row<Cell>, &Row<Cell>) = (&fast[Line(line)], &slow[Line(line)]);
        assert_eq!(fast.occ, slow.occ, "{ctx}: {which}: occupied cells of line {line}");
        for column in 0..fast.len() {
            let (f, s) = (&fast[Column(column)], &slow[Column(column)]);
            // The fields one by one, so a failure says which one.
            assert_eq!(f.c, s.c, "{ctx}: {which}: character at {line}:{column}");
            assert_eq!(f.fg, s.fg, "{ctx}: {which}: foreground at {line}:{column}");
            assert_eq!(f.bg, s.bg, "{ctx}: {which}: background at {line}:{column}");
            assert_eq!(f.flags, s.flags, "{ctx}: {which}: flags at {line}:{column}");
            assert_eq!(f.extra, s.extra, "{ctx}: {which}: extra at {line}:{column}");
        }
    }
}

/// The damage `Term::damage` reports, the way a renderer reads it.
fn damage_of(term: &mut Screen) -> Option<Vec<LineDamageBounds>> {
    match term.damage() {
        TermDamage::Full => None,
        TermDamage::Partial(lines) => Some(lines.collect()),
    }
}

/// Two terminals of one size, one taking runs of ASCII whole and one a character at a time.
struct Pair {
    fast: Screen,
    slow: Screen,
    fast_parser: Processor,
    slow_parser: Processor,
}

impl Pair {
    fn new(columns: usize, lines: usize) -> Pair {
        let size = TermSize::new(columns, lines);
        Pair {
            fast: Term::new(Config::default(), &size, VoidListener),
            slow: Term::new(Config::default(), &size, VoidListener),
            fast_parser: Processor::new(),
            slow_parser: Processor::new(),
        }
    }

    /// One read of the program's output, then the comparison.
    fn feed(&mut self, bytes: &[u8], ctx: &str) {
        self.fast_parser.advance(&mut self.fast, bytes);
        self.slow_parser.advance(&mut PerChar(&mut self.slow), bytes);
        assert_same(&self.fast, &self.slow, ctx);
    }

    /// What a renderer does between reads: ask for the damage, maybe take it.
    fn render(&mut self, reset: bool, ctx: &str) {
        assert_eq!(damage_of(&mut self.fast), damage_of(&mut self.slow), "{ctx}: reported damage");
        if reset {
            self.fast.reset_damage();
            self.slow.reset_damage();
        }
        assert_same(&self.fast, &self.slow, ctx);
    }

    /// Ends a synchronized update the stream left open, so its bytes arrive too.
    fn finish(&mut self, ctx: &str) {
        self.fast_parser.stop_sync(&mut self.fast);
        self.slow_parser.stop_sync(&mut PerChar(&mut self.slow));
        assert_same(&self.fast, &self.slow, ctx);
    }
}

/// Reads `text` as one stream and compares, in a terminal of the given size.
fn same_as_per_character(columns: usize, lines: usize, text: &str) -> Pair {
    let mut pair = Pair::new(columns, lines);
    pair.feed(text.as_bytes(), text);
    pair.render(false, text);
    pair.finish(text);
    pair
}

fn letters(term: &Screen, line: i32) -> String {
    term.grid()[Line(line)].into_iter().map(|cell| cell.c).collect::<String>()
}

#[test]
fn a_run_fills_and_wraps_lines_like_its_characters() {
    // Shorter than the line, exactly the line, a line and a bit, several lines and a bit.
    for length in [1, 4, 5, 6, 9, 10, 11, 23, 60] {
        let text = "0123456789".repeat(7)[..length].to_owned();
        let pair = same_as_per_character(5, 4, &text);
        assert_eq!(pair.fast.grid().cursor.input_needs_wrap, length % 5 == 0, "{length}");
    }

    let pair = same_as_per_character(5, 4, "abcdefghijkl");
    assert_eq!(letters(&pair.fast, 0), "abcde");
    assert_eq!(letters(&pair.fast, 1), "fghij");
    assert_eq!(letters(&pair.fast, 2), "kl   ");
}

#[test]
fn a_run_overwrites_a_fullwidth_character_from_either_half() {
    // The wide character sits on columns 2 and 3 of the first line.
    let wide = "ab漢cd";
    for goto in ["\x1b[1;3H", "\x1b[1;4H", "\x1b[1;2H", "\x1b[1;5H"] {
        for run in ["x", "xy", "xyz", "xyzuvw"] {
            same_as_per_character(8, 3, &format!("{wide}{goto}{run}"));
        }
    }

    // Both halves at once, and a run reaching past the character.
    let pair = same_as_per_character(8, 3, "ab漢cd\x1b[1;3Hwxyz");
    assert_eq!(letters(&pair.fast, 0), "abwxyz  ");
    let pair = same_as_per_character(8, 3, "ab漢cd\x1b[1;4Hw");
    assert_eq!(letters(&pair.fast, 0), "ab wcd  ");
}

#[test]
fn a_run_clears_the_leading_spacer_a_wide_character_left_at_the_line_end() {
    // 漢 does not fit on the last column of the first line: a spacer stays there and the
    // character starts the second line. Overwriting it from column 0 or 1 takes the spacer
    // away, and `write_at_cursor` is what does so.
    for goto in ["\x1b[2;1H", "\x1b[2;2H"] {
        let text = format!("abcd漢{goto}xyz");
        let pair = same_as_per_character(5, 3, &text);
        let first_line_end = &pair.fast.grid()[Line(0)][Column(4)];
        assert!(!first_line_end.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER), "{text:?}");
    }

    // Without the overwrite it stays.
    let pair = same_as_per_character(5, 3, "abcd漢\x1b[2;3Hxyz");
    let first_line_end = &pair.fast.grid()[Line(0)][Column(4)];
    assert!(first_line_end.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));
}

#[test]
fn a_run_with_line_wrap_off_overwrites_the_last_column() {
    let pair = same_as_per_character(5, 3, "\x1b[?7labcdefgh\x1b[?7hij");
    assert_eq!(letters(&pair.fast, 0), "abcdh");
    assert_eq!(letters(&pair.fast, 1), "ij   ");
}

#[test]
fn a_run_takes_the_cells_zero_width_marks_and_links_away_like_its_characters() {
    // (Links carry an id: one without gets a number from a counter the two terminals share.)
    // A cell with a mark or a link of its own gets the template's `extra` (none) when it is
    // written over.
    same_as_per_character(8, 3, "e\u{301}x\u{200d}😀\u{fe0f}z\x1b[1;1Habcdefgh");
    same_as_per_character(8, 3, "\x1b]8;id=1;http://a\x1b\\link\x1b]8;;\x1b\\text\x1b[1;1Hab");
    same_as_per_character(8, 3, "\x1b[4:3m\x1b[58;5;9mabc\x1b[0m\x1b[1;1Hxyzxyzxyz");

    // And a template that has `extra` of its own is written character by character.
    same_as_per_character(8, 3, "\x1b]8;id=1;http://a\x07abcdefghijkl\x1b]8;;\x07mn");
    same_as_per_character(8, 3, "\x1b[58;2;1;2;3m\x1b[4mabcdefghijkl\x1b[24mmn");
}

#[test]
fn a_run_follows_the_insert_mode_and_the_charsets_like_its_characters() {
    same_as_per_character(10, 3, "0123456789\x1b[1;3H\x1b[4hxyz\x1b[4lw");
    same_as_per_character(10, 3, "\x1b(0qqxx\x1b(Bqqxx\x0e\x1b)0qqxx\x0fqqxx");
    same_as_per_character(10, 3, "abc\x1b(0abc\x1b(Babc");
}

#[test]
fn repeating_right_after_a_run_repeats_its_last_character() {
    let pair = same_as_per_character(10, 3, "abc\x1b[4b");
    assert_eq!(letters(&pair.fast, 0), "abccccc   ");
    let pair = same_as_per_character(10, 3, "abc\x1b[2Cde\x1b[3b");
    assert_eq!(letters(&pair.fast, 0), "abc  deeee");
}

#[test]
fn damage_after_runs_is_what_the_characters_report() {
    let mut pair = Pair::new(6, 4);
    for read in ["abc", "def", "ghijklmnopq", "\x1b[3;2Hxy", "\r\n\r\n\r\nz"] {
        pair.feed(read.as_bytes(), read);
        pair.render(true, read);
    }
}

// A small deterministic generator: the cases must come back the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<'a, T: ?Sized>(&mut self, options: &[&'a T]) -> &'a T {
        options[self.below(options.len())]
    }
}

/// A run of printable ASCII, mostly short and now and then several lines long.
fn ascii_run(rng: &mut Rng) -> String {
    let length = match rng.below(20) {
        0..=9 => 1 + rng.below(8),
        10..=16 => 9 + rng.below(52),
        _ => 61 + rng.below(340),
    };
    (0..length).map(|_| (0x20 + rng.below(0x5f) as u8) as char).collect()
}

/// A piece of a program's output: text, or something that changes what the text lands on.
fn piece(rng: &mut Rng, columns: usize, lines: usize) -> String {
    let (row, column) = (1 + rng.below(lines), 1 + rng.below(columns));
    let count = 1 + rng.below(columns + 2);
    match rng.below(100) {
        0..=34 => ascii_run(rng),
        // Wide and zero-width characters, whole or about to be overwritten from either half.
        35..=41 => rng.pick(&["漢", "日本語", "😀", "👨\u{200d}👩", "ｱ", "ａｂ"]).to_owned(),
        42..=44 => rng.pick(&["e\u{301}", "\u{200d}", "\u{fe0f}", "a\u{300}\u{301}", "\u{301}"]).to_owned(),
        45..=47 => rng.pick(&["é", "€", "·", "\u{7ff}", "\u{ff}"]).to_owned(),
        48..=51 => rng.pick(&["\r", "\n", "\r\n", "\x08", "\t", "\x7f", "\x0b", "\x0c"]).to_owned(),
        // Attributes.
        52..=58 => rng
            .pick(&[
                "\x1b[0m", "\x1b[1m", "\x1b[31m", "\x1b[44m", "\x1b[7m", "\x1b[4m", "\x1b[9m",
                "\x1b[48;5;12m", "\x1b[38;2;1;2;3m", "\x1b[22m", "\x1b[39;49m", "\x1b[58;5;3m",
                "\x1b[4:3m", "\x1b[4:0m", "\x1b[5m", "\x1b[2m",
            ])
            .to_owned(),
        59..=61 => rng
            .pick(&[
                "\x1b]8;id=1;http://a/\x1b\\", "\x1b]8;id=7;http://b/\x07", "\x1b]8;;\x07",
                "\x1b]8;id=7;http://b/\x1b\\",
            ])
            .to_owned(),
        // Modes: insert, line wrap, origin, alternate screen, synchronized updates.
        62..=64 => rng.pick(&["\x1b[4h", "\x1b[4l", "\x1b[4h", "\x1b[4l"]).to_owned(),
        65..=67 => rng.pick(&["\x1b[?7l", "\x1b[?7h", "\x1b[?7l", "\x1b[?7h"]).to_owned(),
        68 => rng.pick(&["\x1b[?6h", "\x1b[?6l"]).to_owned(),
        69..=70 => rng.pick(&["\x1b[?1049h", "\x1b[?1049l", "\x1b[?47h", "\x1b[?47l"]).to_owned(),
        71 => rng.pick(&["\x1b[?2026h", "\x1b[?2026l"]).to_owned(),
        // Charsets.
        72..=74 => rng
            .pick(&["\x1b(0", "\x1b(B", "\x1b)0", "\x1b)B", "\x0e", "\x0f", "\x1b*0", "\x1bn"])
            .to_owned(),
        // Scrolling regions and scrolling.
        75..=76 => {
            let top = 1 + rng.below(lines);
            format!("\x1b[{};{}r", top, top + rng.below(lines - top + 1))
        },
        77 => rng.pick(&["\x1b[r", "\x1bM", "\x1bD", "\x1bE"]).to_owned(),
        78..=79 => format!("\x1b[{count}{}", rng.pick(&["S", "T", "L", "M"])),
        // Cursor moves and edits.
        80..=84 => format!("\x1b[{row};{column}H"),
        85..=86 => format!("\x1b[{count}{}", rng.pick(&["A", "B", "C", "D", "G", "d"])),
        87 => rng.pick(&["\x1b7", "\x1b8", "\x1b[s", "\x1b[u"]).to_owned(),
        88..=90 => format!("\x1b[{count}{}", rng.pick(&["X", "P", "@"])),
        91 => rng.pick(&["\x1b[K", "\x1b[1K", "\x1b[2K", "\x1b[J", "\x1b[1J", "\x1b[2J"]).to_owned(),
        // Repeating the character before.
        92..=95 => format!("\x1b[{count}b"),
        // Bytes that are no text at all.
        96 => rng.pick(&["\u{fffd}", "\x1b[?25l", "\x1bc"]).to_owned(),
        _ => ascii_run(rng),
    }
}

#[test]
fn a_random_mix_ends_where_its_characters_one_by_one_end() {
    for seed in 1..=600u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let columns = 2 + rng.below(40);
        let lines = 2 + rng.below(10);

        let mut stream = Vec::new();
        for _ in 0..(20 + rng.below(80)) {
            stream.extend_from_slice(piece(&mut rng, columns, lines).as_bytes());
        }
        // Bytes that are no valid UTF-8 now and then.
        if rng.below(4) == 0 {
            let at = rng.below(stream.len());
            stream.insert(at, *rng.pick(&[&0xffu8, &0xc3, &0xe6]));
        }

        // The reads are cut at random, so a run is cut at random too.
        let mut pair = Pair::new(columns, lines);
        let mut rest = &stream[..];
        let mut reads = 0;
        while !rest.is_empty() {
            let cut = match rng.below(3) {
                0 => 1 + rng.below(8),
                1 => 1 + rng.below(64),
                _ => 1 + rng.below(1024),
            }
            .min(rest.len());
            let (read, tail) = rest.split_at(cut);
            rest = tail;

            let ctx = format!("seed {seed}, {columns}x{lines}, read {reads}");
            pair.feed(read, &ctx);
            if rng.below(3) == 0 {
                pair.render(rng.below(2) == 0, &ctx);
            }
            reads += 1;
        }
        pair.finish(&format!("seed {seed}, {columns}x{lines}, the end"));
        pair.render(true, &format!("seed {seed}, {columns}x{lines}, the damage at the end"));
    }
}
