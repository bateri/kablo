//! The writer: a `Term` as VT bytes.
//!
//! **Bytes, not a grid dump.** The image goes through a parser like any program's
//! output, so colours, styles, wide characters and clusters come back by the path the
//! parser already knows, and a terminal that is not ours (a plain emulator behind
//! `kablo connect`) can take it too. alacritty's types do not reach the format and the
//! format does not follow alacritty's internal layout.
//!
//! **Destructive.** alacritty keeps the tab stops, the scrolling region, the active
//! character set, the keyboard and title stacks and the cursor style private, with no
//! getter; this writer reads them by driving the `Term` itself (popping the stacks
//! empty, tabbing across a row, writing a cell). The `Term` is not to be fed or drawn
//! after it: [`crate::Screen::image`] builds a new one from the image, which is also
//! what the round-trip tests rely on.
//!
//! The order is the contract (alacritty 0.26.0's `swap_alt`: entering the alternate
//! screen copies the primary cursor into it, **overwrites the primary's saved cursor**
//! with it and swaps the keyboard stacks; leaving restores no cursor):
//!
//! 1. the primary rows, links as they are (`id` included);
//! 2. off the alternate screen, its kept keyboard stack: `?1049h`, pushes, `?1049l` —
//!    before the saved cursor, which `?1049h` overwrites;
//! 3. the primary keyboard stack (`CSI > n u`);
//! 4. off the alternate screen the primary saved cursor and `ESC 7`; on it the primary
//!    cursor (equal to its saved one — entering overwrote it), then `?1049h` and an erase
//!    (entering fills the grid with the cursor's background), the alternate rows, its
//!    keyboard stack, its saved cursor and `ESC 7`. Each cursor is position (the cell
//!    written again when `input_needs_wrap` is set), pen, open link and designations, and
//!    the pen goes back to the default before more rows: rows are written with ASCII in
//!    all four sets — the cells hold mapped characters;
//! 5. the scrolling region, the tab stops, the title stack (`CSI 22 t`) and the title;
//! 6. the changed palette (OSC 4/10/11/12) and the application's cursor style
//!    (DECSCUSR);
//! 7. every mode bit outside `MODES_NOT_WRITTEN`, all resets before all sets (the mouse
//!    modes and encodings exclude one another), and the active kitty bits when they
//!    differ from the stack's top (`CSI = n u`);
//! 8. the active screen's cursor — **after** the modes: `?6h` homes the cursor and under
//!    origin mode `CUP` is region-relative — and SI/SO last, since it maps what the
//!    cursor's rewritten cell writes.
//!
//! **Known limits:** a title of `None` in the stack, or after one, comes back as `""` (no
//! sequence resets the title to none; a window title reads the two alike); G2/G3 as the
//! active set comes back as G0 (no sequence invokes them); off the alternate screen its
//! saved cursor is not carried (the next `?1049h` keeps it, invisible until a `DECRC`
//! without a `DECSC`); a cursor outside the scrolling region while origin mode is on
//! (`DECRC` of a cursor saved before the region was set) comes back on the region's first
//! row — setting the mode homes the cursor and `CUP` cannot leave the region; a row whose
//! last cell the cursor waits after loses its wrap (the cell is written again to make the
//! cursor wait, which clears the flag), and a tab character in that cell comes back as a
//! tab that does not wait; and grids no sequence can build — a wide character whose second
//! half was erased or overwritten by another wide character — come back with the character
//! whole and the cell after it dropped.
//!
//! **Rows are written the way the replay will scroll.** A row that wraps into the next
//! one is written without a break, the next row's cells are written from the wrap, and
//! when that wrap makes the screen scroll the new row comes filled with the background of
//! the cell that caused it — so a continuation that starts coloured and ends short is
//! erased after its content. A tab is written as a blank with the cell's pen and then
//! tabbed over (the tab only turns a blank into a tab and keeps what the cell had).

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Charsets, Cursor, Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell as TermCell, Flags, Hyperlink};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    CharsetIndex, ClearMode, Color, CursorShape, CursorStyle, Handler, KeyboardModes, NamedColor,
    NamedPrivateMode, PrivateMode, Rgb, StandardCharset,
};

/// A grid's size, the way [`Term::new`] and `Term::resize` ask for it.
pub(crate) struct Size {
    pub(crate) columns: usize,
    pub(crate) lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// The flags the pen carries; the rest (wrap, wide, spacer) are geometry.
const STYLE: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT)
    .union(Flags::ALL_UNDERLINES);

/// The flags that make a blank cell visible; a trailing blank without them (and on the
/// default background) is dropped.
const VISIBLE_BLANK: Flags = Flags::INVERSE
    .union(Flags::STRIKEOUT)
    .union(Flags::ALL_UNDERLINES);

/// The cell attributes SGR can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pen {
    pub(crate) fg: Color,
    pub(crate) bg: Color,
    pub(crate) underline_color: Option<Color>,
    pub(crate) flags: Flags,
}

impl Pen {
    pub(crate) const DEFAULT: Self = Self {
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        underline_color: None,
        flags: Flags::empty(),
    };

    pub(crate) fn of(cell: &TermCell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            underline_color: cell.underline_color(),
            flags: cell.flags & STYLE,
        }
    }
}

/// Writes `rows` as VT bytes, for a fresh `Term` of the same size.
///
/// `link_of` answers a cell's OSC 8 body (what follows `8;`: the parameters, `;` and the
/// URI); `None` → no link. Every row that does not wrap into the next ends with the link
/// closed, the pen back at the default and `\r\n` — except the last one: a live screen's
/// last row must not scroll the screen one row into the history.
///
/// A tab in a cell (alacritty keeps one where a tab started) comes back as that tab:
/// replayed, a tab moves by the stops in force, and the cursor is put back to where the
/// row goes on.
///
/// `unwrapped` lists rows whose wrap is not to be written: the cell of a cursor waiting to
/// wrap is written again, and that clears the flag on it.
fn write_rows(
    out: &mut Vec<u8>,
    rows: &[Vec<&TermCell>],
    unwrapped: &[usize],
    link_of: &mut dyn FnMut(&TermCell) -> Option<String>,
) {
    let last = rows.len().saturating_sub(1);
    let mut pen = Pen::DEFAULT;
    let mut open: Option<String> = None;
    // How the row before ended: `None` with a break (or there was none), `Some(spacer)`
    // wrapped with no break — `spacer` the pen of its last cell when that is the blank a
    // wide character that did not fit left there.
    let mut continues: Option<Option<Pen>> = None;
    for (index, cells) in rows.iter().enumerate() {
        let wrap = wrapped(cells) && index < last && !unwrapped.contains(&index);
        let mut end = if wrap {
            cells.len()
        } else {
            content_end(cells)
        };
        let starts_wide = cells
            .first()
            .is_some_and(|cell| cell.flags.contains(Flags::WIDE_CHAR));
        // The wrap into this row opens it with the background of the first cell written,
        // when the screen has to scroll for it: the cells after the row's content must be
        // the default ones, and only an erase says so.
        let continuation = continues.is_some();
        let mut filled = false;
        match continues {
            // The wrap would come from this row's wide first character, and it is not
            // one (erased or overwritten since): a wide placeholder makes it, and the
            // row it lands on is erased again.
            Some(Some(spacer)) if !starts_wide => {
                transition(out, pen, spacer);
                out.extend_from_slice("\u{3000}\x1b[0m\r\x1b[2K".as_bytes());
                pen = Pen::DEFAULT;
            }
            // The row before carried no break: its wrap happens when this row's first
            // cell is written. A continuation erased since (`ED 1`, `EL 2`) is blank and
            // would write nothing — the two rows would replay as one.
            Some(_) => end = end.max(1).min(cells.len()),
            None => {}
        }
        continues = wrap.then(|| {
            cells
                .last()
                .filter(|cell| cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER))
                .map(|cell| Pen::of(cell))
        });
        let width = cells.len();
        // The previous cell was a wide character: this column is its second half, which
        // the character itself writes. Whatever the cell says — erasing under a wide
        // character leaves it without its spacer, and writing the cell as well would push
        // the row one column past its end.
        let mut covered = false;
        for (column, cell) in cells.iter().take(end).enumerate() {
            // The blank a wide character that did not fit leaves is the last cell of a row
            // that wraps; the flag on another cell, or on a row that leads nowhere, is a
            // leftover (of a shift, of the rows below being scrolled away) and the cell
            // is an ordinary blank.
            let leading =
                wrap && column + 1 == width && cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);
            if std::mem::take(&mut covered) || leading {
                continue;
            }
            covered = cell.flags.contains(Flags::WIDE_CHAR);
            let anchor = link_of(cell);
            if anchor != open {
                out.extend_from_slice(b"\x1b]8;");
                out.extend_from_slice(anchor.as_deref().unwrap_or(";").as_bytes());
                out.push(0x07);
                open = anchor;
            }
            let next = Pen::of(cell);
            if continuation && column == 0 {
                filled = next.bg != Pen::DEFAULT.bg;
            }
            transition(out, pen, next);
            pen = next;
            if cell.c == '\t' && wrap && column + 1 == width {
                // A tab in a wrapping row's last cell: a tab there stays put and would
                // clear the pending wrap. A blank makes the wrap pending, a tab past the
                // last column is the wrap, and the cursor goes back up to write the tab
                // into the cell and down to where the next row starts.
                out.extend_from_slice(format!(" \t\x1b[A\x1b[{width}G\t\x1b[B\r").as_bytes());
            } else if cell.c == '\t' {
                // alacritty keeps a tab in the cell it started from and the cell keeps
                // what it had: a tab only turns a blank cell into a tab, so the blank is
                // written first with the pen and the link. Replayed, the tab moves by the
                // stops in force — the defaults, the stops come later — so the cursor
                // goes back to the cell and then to the next column.
                out.extend_from_slice(
                    format!(" \x1b[{}G\t\x1b[{}G", column + 1, column + 2).as_bytes(),
                );
            } else {
                push_cell_text(out, cell);
            }
        }
        if !wrap {
            // The link closes before the break like the pen: the rows a line feed opens
            // must not inherit it.
            if open.take().is_some() {
                out.extend_from_slice(b"\x1b]8;;\x07");
            }
            if pen != Pen::DEFAULT {
                out.extend_from_slice(b"\x1b[0m");
                pen = Pen::DEFAULT;
            }
            if filled && end < width {
                out.extend_from_slice(b"\x1b[K");
            }
            if index < last {
                out.extend_from_slice(b"\r\n");
            }
        }
    }
}

/// A cell's base character and its zero-width characters behind it.
fn push_cell_text(out: &mut Vec<u8>, cell: &TermCell) {
    push_char(out, cell.c);
    for &extra in cell.zerowidth().unwrap_or_default() {
        push_char(out, extra);
    }
}

/// Whether the row continues on the next one (alacritty puts the flag on the row's last
/// cell).
fn wrapped(cells: &[&TermCell]) -> bool {
    cells
        .last()
        .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE))
}

/// The number of cells up to the last one that is not a default blank.
fn content_end(cells: &[&TermCell]) -> usize {
    cells
        .iter()
        .rposition(|cell| !default_blank(cell))
        .map_or(0, |index| index + 1)
}

/// A blank a line break can stand in for: a space (or a spacer) on the default
/// background, with nothing drawn over it. The foreground is invisible on a space.
fn default_blank(cell: &TermCell) -> bool {
    (cell.c == ' ' || cell.c == '\0')
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell.flags.intersects(VISIBLE_BLANK)
        && cell.zerowidth().is_none_or(<[char]>::is_empty)
}

fn push_char(out: &mut Vec<u8>, ch: char) {
    // A NUL cell is an untouched cell; it reads as a space.
    let ch = if ch == '\0' { ' ' } else { ch };
    let mut buf = [0; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

/// Writes the SGR that turns `from` into `to`; nothing if they are equal.
///
/// `22` cancels bold **and** dim, so when one of the two goes the survivor is set again.
/// The underline styles exclude one another (alacritty clears all five before setting
/// one), so a change is a single `4:n` or `24`.
pub(crate) fn transition(out: &mut Vec<u8>, from: Pen, to: Pen) {
    if from == to {
        return;
    }
    let mut codes: Vec<String> = Vec::new();
    let removed = from.flags - to.flags;
    let mut added = to.flags - from.flags;
    if removed.intersects(Flags::BOLD | Flags::DIM) {
        codes.push("22".to_owned());
        added |= to.flags & (Flags::BOLD | Flags::DIM);
    }
    for (flag, code) in [
        (Flags::ITALIC, "23"),
        (Flags::INVERSE, "27"),
        (Flags::HIDDEN, "28"),
        (Flags::STRIKEOUT, "29"),
    ] {
        if removed.contains(flag) {
            codes.push(code.to_owned());
        }
    }
    let underline = to.flags & Flags::ALL_UNDERLINES;
    if from.flags & Flags::ALL_UNDERLINES != underline {
        codes.push(underline_code(underline).to_owned());
    }
    for (flag, code) in [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::INVERSE, "7"),
        (Flags::HIDDEN, "8"),
        (Flags::STRIKEOUT, "9"),
    ] {
        if added.contains(flag) {
            codes.push(code.to_owned());
        }
    }
    if from.fg != to.fg {
        codes.push(color_code(to.fg, Layer::Foreground));
    }
    if from.bg != to.bg {
        codes.push(color_code(to.bg, Layer::Background));
    }
    if from.underline_color != to.underline_color {
        codes.push(match to.underline_color {
            Some(color) => color_code(color, Layer::Underline),
            None => "59".to_owned(),
        });
    }
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(codes.join(";").as_bytes());
    out.push(b'm');
}

fn underline_code(underline: Flags) -> &'static str {
    if underline.contains(Flags::DOUBLE_UNDERLINE) {
        "4:2"
    } else if underline.contains(Flags::UNDERCURL) {
        "4:3"
    } else if underline.contains(Flags::DOTTED_UNDERLINE) {
        "4:4"
    } else if underline.contains(Flags::DASHED_UNDERLINE) {
        "4:5"
    } else if underline.contains(Flags::UNDERLINE) {
        "4"
    } else {
        "24"
    }
}

/// Which colour an SGR sets.
#[derive(Clone, Copy)]
enum Layer {
    Foreground,
    Background,
    Underline,
}

/// The SGR of a colour. A named colour stays named (`31`, not `38;5;1`), because the
/// parser keeps the two apart and a theme draws them apart. The named colours SGR cannot
/// produce (the cursor's, the dim and bright foregrounds) fall to the default — they
/// never come out of the parser.
fn color_code(color: Color, layer: Layer) -> String {
    let (base, bright, default, extended) = match layer {
        Layer::Foreground => (30, 90, "39", "38"),
        Layer::Background => (40, 100, "49", "48"),
        Layer::Underline => (0, 0, "59", "58"),
    };
    match color {
        Color::Spec(rgb) => format!("{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => format!("{extended};5;{index}"),
        Color::Named(named) => {
            let index = named as usize;
            match (layer, index) {
                // `58` has no named form; the palette slot is the same colour.
                (Layer::Underline, 0..=15) => format!("58;5;{index}"),
                (_, 0..=7) => (base + index).to_string(),
                (_, 8..=15) => (bright + index - 8).to_string(),
                _ => default.to_owned(),
            }
        }
    }
}

/// alacritty's keyboard-mode stack depth (`KEYBOARD_MODE_STACK_MAX_DEPTH`, private in
/// 0.26.0); [`probe_keyboard`] pops one past it.
const KEYBOARD_DEPTH: usize = 4096;

/// alacritty's title stack depth (`TITLE_STACK_MAX_DEPTH`, private).
const TITLE_DEPTH: usize = 4096;

/// What [`probe_titles`] writes before each pop: a pop from an empty stack leaves it in
/// place. The parser never produces it — an OSC cannot carry a control character.
const TITLE_SENTINEL: &str = "\u{1}kablo-title-probe";

/// The mode bits [`encode`] writes, each with its set and reset sequence. The rest of
/// `TermMode` is [`MODES_NOT_WRITTEN`]; a test pins that the two cover every bit.
pub(crate) const MODES: [(TermMode, &str, &str); 15] = [
    (TermMode::SHOW_CURSOR, "\x1b[?25h", "\x1b[?25l"),
    (TermMode::APP_CURSOR, "\x1b[?1h", "\x1b[?1l"),
    (TermMode::APP_KEYPAD, "\x1b=", "\x1b>"),
    (TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h", "\x1b[?1000l"),
    (TermMode::MOUSE_DRAG, "\x1b[?1002h", "\x1b[?1002l"),
    (TermMode::MOUSE_MOTION, "\x1b[?1003h", "\x1b[?1003l"),
    (TermMode::BRACKETED_PASTE, "\x1b[?2004h", "\x1b[?2004l"),
    (TermMode::SGR_MOUSE, "\x1b[?1006h", "\x1b[?1006l"),
    (TermMode::UTF8_MOUSE, "\x1b[?1005h", "\x1b[?1005l"),
    (TermMode::LINE_WRAP, "\x1b[?7h", "\x1b[?7l"),
    (TermMode::LINE_FEED_NEW_LINE, "\x1b[20h", "\x1b[20l"),
    (TermMode::ORIGIN, "\x1b[?6h", "\x1b[?6l"),
    (TermMode::INSERT, "\x1b[4h", "\x1b[4l"),
    (TermMode::FOCUS_IN_OUT, "\x1b[?1004h", "\x1b[?1004l"),
    (TermMode::ALTERNATE_SCROLL, "\x1b[?1007h", "\x1b[?1007l"),
];

/// The mode bits that are not a mode sequence: the alternate screen is the content's
/// (`?1049h`), the kitty bits the keyboard stacks' (`CSI > u`, `CSI = u`), and `VI` and
/// `URGENCY_HINTS` are the terminal's own — no application turns vi mode on, and the
/// urgency hint is a window setting.
#[cfg(test)]
pub(crate) const MODES_NOT_WRITTEN: TermMode = TermMode::ALT_SCREEN
    .union(TermMode::KITTY_KEYBOARD_PROTOCOL)
    .union(TermMode::VI)
    .union(TermMode::URGENCY_HINTS);

/// The kitty bits of `TermMode` and the keyboard mode each one is.
const KITTY: [(TermMode, KeyboardModes); 5] = [
    (
        TermMode::DISAMBIGUATE_ESC_CODES,
        KeyboardModes::DISAMBIGUATE_ESC_CODES,
    ),
    (
        TermMode::REPORT_EVENT_TYPES,
        KeyboardModes::REPORT_EVENT_TYPES,
    ),
    (
        TermMode::REPORT_ALTERNATE_KEYS,
        KeyboardModes::REPORT_ALTERNATE_KEYS,
    ),
    (
        TermMode::REPORT_ALL_KEYS_AS_ESC,
        KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
    ),
    (
        TermMode::REPORT_ASSOCIATED_TEXT,
        KeyboardModes::REPORT_ASSOCIATED_TEXT,
    ),
];

/// The four designations, in index order: `ESC ( 0` designates G0 and so on.
const DESIGNATORS: [(CharsetIndex, u8); 4] = [
    (CharsetIndex::G0, b'('),
    (CharsetIndex::G1, b')'),
    (CharsetIndex::G2, b'*'),
    (CharsetIndex::G3, b'+'),
];

/// The highest palette index an OSC sets: 0–255 with OSC 4, then the foreground,
/// background and cursor colours with OSC 10/11/12.
pub(crate) const LAST_SETTABLE_COLOR: usize = NamedColor::Cursor as usize;

/// What only a **destructive probe** reads from `Term`: alacritty keeps these fields
/// private and no getter exists. [`encode`] writes them; the round-trip tests compare
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Probed {
    /// The tab stops a forward tab lands on, ascending; the last column (where a tab
    /// always stops) and column 0 (where none goes) are not observable and not listed.
    pub(crate) tabs: Vec<usize>,
    /// The scrolling region's first and last row, inclusive.
    pub(crate) region: (usize, usize),
    /// The active character set (SI/SO).
    pub(crate) charset: CharsetIndex,
    /// The primary and the alternate screen's keyboard-mode stacks, bottom to top. A
    /// `NO_MODE` entry at the bottom is not observable and is dropped — popping down to
    /// it or past it reads `NO_MODE` alike.
    pub(crate) keyboard: [Vec<KeyboardModes>; 2],
    /// The title stack, bottom to top (`CSI 22 t` pushes, `CSI 23 t` pops).
    pub(crate) titles: Vec<Option<String>>,
    /// The current title as the listener knows it.
    pub(crate) title: Option<String>,
    /// The application's cursor style; `None` → the terminal's default.
    pub(crate) cursor_style: Option<CursorStyle>,
    /// Whether the last column is a tab stop. A tab always stops there, so `tabs` cannot
    /// list it — but a wider grid later tabs onto it.
    pub(crate) last_tab: bool,
}

/// A cursor as written: where it is, the cell to write again when `input_needs_wrap` is
/// set, its pen, its open link and its four designations.
struct CursorShot {
    line: i32,
    column: usize,
    /// `input_needs_wrap` is not settable by a sequence: only writing the last column
    /// sets it. The cell is written again — the real one, with its pen and link — and the
    /// cursor lands where it was (a wide character's spacer: the character itself, one
    /// column back).
    rewrite: Option<(usize, TermCell)>,
    pen: Pen,
    /// The template's link: characters typed after the image must carry it.
    link: Option<String>,
    charsets: Charsets,
}

impl CursorShot {
    fn of(grid: &Grid<TermCell>, cursor: &Cursor<TermCell>) -> Self {
        let point = cursor.point;
        let rewrite = cursor
            .input_needs_wrap
            .then(|| {
                let in_grid = point.line >= grid.topmost_line()
                    && point.line <= grid.bottommost_line()
                    && point.column.0 < grid.columns();
                in_grid.then_some(())?;
                let row = &grid[point.line];
                let mut column = point.column.0;
                if column > 0 && row[point.column].flags.contains(Flags::WIDE_CHAR_SPACER) {
                    column -= 1;
                }
                Some((column, row[Column(column)].clone()))
            })
            .flatten();
        Self {
            line: point.line.0,
            column: point.column.0,
            rewrite,
            pen: Pen::of(&cursor.template),
            link: cursor.template.hyperlink().as_ref().map(live_link),
            charsets: cursor.charsets,
        }
    }

    /// Writes the cursor; `origin` is the scrolling region's first row when origin mode
    /// is on (`CUP` is then relative to it). Expects the pen at the default, no link open
    /// and ASCII in all four sets.
    fn write(&self, out: &mut Vec<u8>, origin: Option<usize>) {
        let row = (self.line - origin.map_or(0, |top| top as i32)).max(0);
        match &self.rewrite {
            Some((column, cell)) => {
                cup(out, row, *column);
                if let Some(link) = cell.hyperlink().as_ref().map(live_link) {
                    osc8(out, &link);
                }
                transition(out, Pen::DEFAULT, Pen::of(cell));
                push_cell_text(out, cell);
                out.extend_from_slice(b"\x1b]8;;\x07\x1b[0m");
            }
            None => cup(out, row, self.column),
        }
        transition(out, Pen::DEFAULT, self.pen);
        if let Some(link) = &self.link {
            osc8(out, link);
        }
        for (index, intro) in DESIGNATORS {
            if self.charsets[index] == StandardCharset::SpecialCharacterAndLineDrawing {
                out.extend_from_slice(&[0x1b, intro, b'0']);
            }
        }
    }
}

/// Back to the state [`CursorShot::write`] expects: no link, the default pen, ASCII in
/// all four sets.
fn reset_pen(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b]8;;\x07\x1b[0m\x1b(B\x1b)B\x1b*B\x1b+B");
}

fn cup(out: &mut Vec<u8>, row: i32, column: usize) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, column + 1).as_bytes());
}

fn osc8(out: &mut Vec<u8>, body: &str) {
    out.extend_from_slice(b"\x1b]8;");
    out.extend_from_slice(body.as_bytes());
    out.push(0x07);
}

/// A link's OSC 8 body with its `id` — alacritty's own ids (`N_alacritty`) included, so
/// the replay gives every cell the same link back.
pub(crate) fn live_link(link: &Hyperlink) -> String {
    format!("id={};{}", link.id(), link.uri())
}

/// The rows from `top` through the bottom of the screen, with links as they are and no
/// break after the last row. `waiting` are the lines of the cursors that will be written
/// waiting to wrap.
fn live_rows(grid: &Grid<TermCell>, top: i32, waiting: &[i32]) -> Vec<u8> {
    let rows: Vec<Vec<&TermCell>> = (top..=grid.bottommost_line().0)
        .map(|line| grid[Line(line)].into_iter().collect())
        .collect();
    let unwrapped: Vec<usize> = waiting
        .iter()
        .filter_map(|&line| usize::try_from(line - top).ok())
        .collect();
    let mut out = Vec::new();
    let mut link_of = |cell: &TermCell| cell.hyperlink().as_ref().map(live_link);
    write_rows(&mut out, &rows, &unwrapped, &mut link_of);
    out
}

/// The kitty bits of `mode` as a keyboard mode.
pub(crate) fn keyboard_bits(mode: TermMode) -> KeyboardModes {
    KITTY
        .iter()
        .filter(|(bit, _)| mode.contains(*bit))
        .fold(KeyboardModes::NO_MODE, |all, (_, keys)| all | *keys)
}

/// The active screen's keyboard-mode stack, bottom to top — **destructive**, it empties
/// the stack.
///
/// The top entry is not observable by popping (`pop_keyboard_modes` shows the entry
/// **under** the popped one, and `mode()` may differ from the top after `CSI = u`), so a
/// dummy is pushed first. A no-op while `kitty_keyboard` is off in the config: push and
/// pop return at once and the stack reads empty.
fn probe_keyboard<T: EventListener>(term: &mut Term<T>) -> Vec<KeyboardModes> {
    term.push_keyboard_mode(KeyboardModes::NO_MODE);
    let mut stack: Vec<KeyboardModes> = (0..=KEYBOARD_DEPTH)
        .map(|_| {
            term.pop_keyboard_modes(1);
            keyboard_bits(*term.mode())
        })
        .collect();
    while stack.last() == Some(&KeyboardModes::NO_MODE) {
        stack.pop();
    }
    stack.reverse();
    stack
}

/// The current title and the title stack, bottom to top — **destructive**, it empties the
/// stack; the listener's title is put back.
///
/// `title` reads the listener's title, which `Title`/`ResetTitle` write. A pop from an
/// empty stack sends no event, so the sentinel written before each pop stays — that is
/// the stack's bottom.
fn probe_titles<T: EventListener>(
    term: &mut Term<T>,
    title: &dyn Fn() -> Option<String>,
) -> (Option<String>, Vec<Option<String>>) {
    let current = title();
    let mut stack = Vec::new();
    for _ in 0..TITLE_DEPTH {
        term.set_title(Some(TITLE_SENTINEL.to_owned()));
        term.pop_title();
        let popped = title();
        if popped.as_deref() == Some(TITLE_SENTINEL) {
            break;
        }
        stack.push(popped);
    }
    stack.reverse();
    term.set_title(current.clone());
    (current, stack)
}

/// The tab stops — **destructive**: a tab writes into a blank cell it passes
/// (`put_tab`).
fn probe_tabs<T: EventListener>(term: &mut Term<T>) -> Vec<usize> {
    let last = term.columns().saturating_sub(1);
    let cursor = &mut term.grid_mut().cursor;
    cursor.point = Point::new(Line(0), Column(0));
    cursor.input_needs_wrap = false;
    let mut stops = Vec::new();
    loop {
        term.put_tab(1);
        let column = term.grid().cursor.point.column.0;
        if column >= last {
            return stops;
        }
        stops.push(column);
    }
}

/// Whether the last column is a tab stop — **destructive**: the grid grows one column
/// (the stops keep their flags) and a tab from the column before the old last lands on it
/// only if it is a stop. Last of the probes: a resize resets the scrolling region.
/// Nothing is read after it, so both histories go first — the resize reflows only the
/// screens, not the scrollback.
fn probe_last_tab<T: EventListener>(term: &mut Term<T>) -> bool {
    let columns = term.columns();
    if columns < 2 {
        return false;
    }
    term.clear_screen(ClearMode::Saved);
    term.swap_alt();
    term.clear_screen(ClearMode::Saved);
    term.resize(Size {
        columns: columns + 1,
        lines: term.screen_lines(),
    });
    let cursor = &mut term.grid_mut().cursor;
    cursor.point = Point::new(Line(0), Column(columns - 2));
    cursor.input_needs_wrap = false;
    term.put_tab(1);
    term.grid().cursor.point.column.0 == columns - 1
}

/// The scrolling region — **destructive**: origin mode is left on and the cursor moved.
/// Under origin mode `goto` clamps into the region, so the home position is its first
/// row and a goto far below its last one; no row scrolls.
fn probe_region<T: EventListener>(term: &mut Term<T>) -> (usize, usize) {
    let lines = i32::try_from(term.screen_lines()).unwrap_or(i32::MAX);
    term.set_private_mode(PrivateMode::Named(NamedPrivateMode::Origin));
    let top = term.grid().cursor.point.line.0;
    term.goto(lines, 0);
    let bottom = term.grid().cursor.point.line.0;
    (
        usize::try_from(top).unwrap_or(0),
        usize::try_from(bottom).unwrap_or(0),
    )
}

/// The active character set — **destructive**: it writes into the top-left cell. One set
/// at a time maps `q` to the line-drawing `─`; the active one is the one that does.
fn probe_charset<T: EventListener>(term: &mut Term<T>) -> CharsetIndex {
    for (index, _) in DESIGNATORS {
        let cursor = &mut term.grid_mut().cursor;
        cursor.charsets = Charsets::default();
        cursor.charsets[index] = StandardCharset::SpecialCharacterAndLineDrawing;
        cursor.point = Point::new(Line(0), Column(0));
        cursor.input_needs_wrap = false;
        term.input('q');
        if term.grid()[Line(0)][Column(0)].c == '─' {
            return index;
        }
    }
    CharsetIndex::G0
}

/// Whether the application set the cursor style — **destructive**: the config is
/// replaced. `cursor_style()` answers the application's style or the config's default;
/// with a different default only the former stays.
fn probe_cursor_style<T: EventListener>(term: &mut Term<T>) -> Option<CursorStyle> {
    let before = term.cursor_style();
    let shape = if before.shape == CursorShape::Block {
        CursorShape::Beam
    } else {
        CursorShape::Block
    };
    let flipped = CursorStyle {
        shape,
        blinking: !before.blinking,
    };
    term.set_options(Config {
        default_cursor_style: flipped,
        ..Config::default()
    });
    (term.cursor_style() == before).then_some(before)
}

/// One screen as read before the probes: its rows and its two cursors.
struct ScreenShot {
    rows: Vec<u8>,
    cursor: CursorShot,
    saved: CursorShot,
    keyboard: Vec<KeyboardModes>,
}

impl ScreenShot {
    /// Reads the active grid from `top` (the history's top on the primary screen, `0` on
    /// the alternate one) and then probes its keyboard stack. `saved_written` is whether
    /// the saved cursor is written at all: not for the primary screen when the alternate
    /// one is active, whose entry overwrote it.
    fn read<T: EventListener>(term: &mut Term<T>, primary: bool, saved_written: bool) -> Self {
        let grid = term.grid();
        let top = if primary { grid.topmost_line().0 } else { 0 };
        let cursor = CursorShot::of(grid, &grid.cursor);
        let saved = CursorShot::of(grid, &grid.saved_cursor);
        let waiting: Vec<i32> = [Some(&cursor), saved_written.then_some(&saved)]
            .into_iter()
            .flatten()
            .filter(|cursor| cursor.rewrite.is_some())
            .map(|cursor| cursor.line)
            .collect();
        let rows = live_rows(grid, top, &waiting);
        let keyboard = probe_keyboard(term);
        Self {
            rows,
            cursor,
            saved,
            keyboard,
        }
    }
}

/// `CSI > n u` for each entry, bottom first.
fn push_keyboard(out: &mut Vec<u8>, stack: &[KeyboardModes]) {
    for modes in stack {
        out.extend_from_slice(format!("\x1b[>{}u", modes.bits()).as_bytes());
    }
}

/// OSC 2: the title.
fn osc2(out: &mut Vec<u8>, title: &str) {
    out.extend_from_slice(b"\x1b]2;");
    out.extend_from_slice(title.as_bytes());
    out.push(0x07);
}

/// The `Term`'s **whole** state as VT bytes: a fresh `Term` of the **same size** with at
/// least as much `scrolling_history` (and `kitty_keyboard` on, or the keyboard stacks
/// read empty) that parses them reads back every field `Term` shows and every field
/// [`Probed`] lists. Resizing is the caller's, after the replay. The module header holds
/// the order and says why the `Term` is spent afterwards.
///
/// `title` reads the listener's title (`Title`/`ResetTitle`).
pub(crate) fn encode<T: EventListener>(
    term: &mut Term<T>,
    title: &dyn Fn() -> Option<String>,
) -> (Vec<u8>, Probed) {
    let mode = *term.mode();
    let colors: Vec<Option<Rgb>> = (0..=LAST_SETTABLE_COLOR)
        .map(|index| term.colors()[index])
        .collect();
    let alt = mode.contains(TermMode::ALT_SCREEN);
    let (title_now, titles) = probe_titles(term, title);
    // The active screen first: the other one is behind `swap_alt`.
    let (primary, alternate, kept) = if alt {
        let alternate = ScreenShot::read(term, false, true);
        term.swap_alt();
        (
            ScreenShot::read(term, true, false),
            Some(alternate),
            Vec::new(),
        )
    } else {
        let primary = ScreenShot::read(term, true, true);
        // Entering resets the alternate grid; only its kept stack is read.
        term.swap_alt();
        (primary, None, probe_keyboard(term))
    };
    let lines = term.screen_lines();
    let columns = term.columns();
    let probed = Probed {
        tabs: probe_tabs(term),
        region: probe_region(term),
        charset: probe_charset(term),
        keyboard: [
            primary.keyboard.clone(),
            alternate
                .as_ref()
                .map_or_else(|| kept.clone(), |screen| screen.keyboard.clone()),
        ],
        titles,
        title: title_now,
        // After the stacks: it replaces the config, and a different `kitty_keyboard`
        // would empty them.
        cursor_style: probe_cursor_style(term),
        // Last: it resizes, which resets the scrolling region.
        last_tab: probe_last_tab(term),
    };

    // 1–4: the screens.
    let mut out = primary.rows;
    if !kept.is_empty() {
        out.extend_from_slice(b"\x1b[?1049h");
        push_keyboard(&mut out, &kept);
        out.extend_from_slice(b"\x1b[?1049l");
    }
    push_keyboard(&mut out, &primary.keyboard);
    match &alternate {
        None => {
            primary.saved.write(&mut out, None);
            out.extend_from_slice(b"\x1b7");
            reset_pen(&mut out);
        }
        Some(screen) => {
            primary.cursor.write(&mut out, None);
            out.extend_from_slice(b"\x1b[?1049h");
            reset_pen(&mut out);
            // Entering fills the alternate grid with the cursor's background (the
            // primary cursor's pen, copied in); the screen being rebuilt may have been
            // cleared since. An erase in the alternate screen only resets its cells.
            out.extend_from_slice(b"\x1b[2J\x1b[H");
            out.extend_from_slice(&screen.rows);
            push_keyboard(&mut out, &screen.keyboard);
            screen.saved.write(&mut out, None);
            out.extend_from_slice(b"\x1b7");
            reset_pen(&mut out);
        }
    }

    // 5: the region, the tab stops, the titles.
    let (top, bottom) = probed.region;
    if (top, bottom) != (0, lines.saturating_sub(1)) {
        out.extend_from_slice(format!("\x1b[{};{}r", top + 1, bottom + 1).as_bytes());
    }
    let last = columns.saturating_sub(1);
    let default_tabs: Vec<usize> = (8..last).step_by(8).collect();
    // A fresh grid's stops are every eighth column, the last one included when it falls
    // there.
    let default_last = last % 8 == 0;
    if probed.tabs != default_tabs || probed.last_tab != default_last {
        out.extend_from_slice(b"\x1b[3g");
        let last_stop = probed.last_tab.then_some(last);
        for stop in probed.tabs.iter().chain(&last_stop) {
            out.extend_from_slice(format!("\x1b[{}G\x1bH", stop + 1).as_bytes());
        }
    }
    for entry in &probed.titles {
        osc2(&mut out, entry.as_deref().unwrap_or_default());
        out.extend_from_slice(b"\x1b[22t");
    }
    match &probed.title {
        Some(title) => osc2(&mut out, title),
        None if !probed.titles.is_empty() => osc2(&mut out, ""),
        None => {}
    }

    // 6: the palette and the cursor style.
    for (index, color) in colors.iter().enumerate() {
        let Some(rgb) = color else {
            continue;
        };
        let spec = format!("rgb:{:02x}/{:02x}/{:02x}", rgb.r, rgb.g, rgb.b);
        let osc = if index < 256 {
            format!("\x1b]4;{index};{spec}\x07")
        } else {
            format!("\x1b]{};{spec}\x07", 10 + index - 256)
        };
        out.extend_from_slice(osc.as_bytes());
    }
    if let Some(style) = probed.cursor_style {
        let base = match style.shape {
            CursorShape::Underline => 3,
            CursorShape::Beam => 5,
            // A hollow block and a hidden cursor are the terminal's, no sequence asks
            // for them.
            CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => 1,
        };
        let code = if style.blinking { base } else { base + 1 };
        out.extend_from_slice(format!("\x1b[{code} q").as_bytes());
    }

    // 7: the modes.
    for (bit, _, reset) in MODES {
        if !mode.contains(bit) {
            out.extend_from_slice(reset.as_bytes());
        }
    }
    for (bit, set, _) in MODES {
        if mode.contains(bit) {
            out.extend_from_slice(set.as_bytes());
        }
    }
    let active_keys = alternate
        .as_ref()
        .map_or(&primary.keyboard, |screen| &screen.keyboard);
    let keys = keyboard_bits(mode);
    if keys
        != active_keys
            .last()
            .copied()
            .unwrap_or(KeyboardModes::NO_MODE)
    {
        out.extend_from_slice(format!("\x1b[={};1u", keys.bits()).as_bytes());
    }

    // 8: the active cursor, then the active set.
    let origin = mode.contains(TermMode::ORIGIN).then_some(top);
    let active = alternate
        .as_ref()
        .map_or(&primary.cursor, |screen| &screen.cursor);
    active.write(&mut out, origin);
    if probed.charset == CharsetIndex::G1 {
        out.push(0x0e);
    }
    (out, probed)
}
