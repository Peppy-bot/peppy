//! A menu on the terminal. It prints a title and one line for each option.
//! The person moves the cursor with the arrow keys (or `k` and `j`), jumps
//! with Home and End, and selects the option under the cursor with Enter.
//! Esc, `q` and Ctrl-C cancel the menu. When the person answers, one line that
//! names the answer replaces the menu.
//!
//! The menu draws on stderr, so stdout carries only what the command prints.
//! A list taller than the terminal scrolls with the cursor, and the title then
//! gives the position of the cursor in the list. A line wider than the
//! terminal is cut, and a control character prints as a space, so each option
//! takes exactly one row.
//!
//! What a key does, which options are on the screen and how a line fits the
//! width are pure functions, tested without a terminal.

use std::io::{self, IsTerminal, Write};

use crossterm::cursor::{Hide, MoveToPreviousLine, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::error::Result;

/// The keys of the menu, given on the title line.
const KEY_HINT: &str = "↑/↓ to move, enter to select, esc to cancel";

/// The columns before the text of an option: the cursor mark and a space.
const OPTION_PREFIX_WIDTH: usize = 2;

/// One question and the answers the person selects from.
pub(crate) struct Menu<'a> {
    /// What the person selects, as a noun: `Workspace`, `Project`.
    pub title: &'a str,
    pub options: &'a [String],
    /// The position of the option the cursor starts on.
    pub start: usize,
}

/// How the person answered a menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuAnswer {
    /// The option at this position of the list.
    Selected(usize),
    /// Esc, `q` or Ctrl-C: no option.
    Cancelled,
}

/// Whether there is a person at a terminal to show a menu to: stdin, where the
/// keys come from, and stderr, where the menu draws, are both terminals.
pub(crate) fn can_show_menu() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

impl Menu<'_> {
    /// Shows the menu on the terminal and returns the answer of the person.
    /// The caller checks [`can_show_menu`] first. A menu with no option has
    /// nothing to select, so it is cancelled at once.
    pub(crate) fn show(&self) -> Result<MenuAnswer> {
        let count = self.options.len();
        if count == 0 {
            return Ok(MenuAnswer::Cancelled);
        }
        let colors = crate::terminal::stderr_colors_enabled();
        let mut out = io::stderr();
        let raw_mode = RawMode::enter(&mut out)?;

        let mut screen = Screen::measure(count)?;
        let mut cursor = self.start.min(count - 1);
        let mut first = scroll(0, cursor, screen.rows, count);
        let mut drawn = 0;
        let answer = loop {
            drawn = self.draw(&mut out, colors, screen, first, cursor, drawn)?;
            match event::read()? {
                Event::Key(key) => match menu_key(&key).map(|key| step(cursor, count, key)) {
                    Some(Step::MoveTo(position)) => cursor = position,
                    Some(Step::Answer(answer)) => break answer,
                    None => continue,
                },
                Event::Resize(..) => screen = Screen::measure(count)?,
                _ => continue,
            }
            first = scroll(first, cursor, screen.rows, count);
        };

        queue!(
            out,
            MoveToPreviousLine(drawn),
            Clear(ClearType::FromCursorDown)
        )?;
        self.draw_answer(&mut out, colors, screen.width, answer)?;
        drop(raw_mode);
        Ok(answer)
    }

    /// Draws the title and the options from `first` that fit on the screen,
    /// over the `drawn` lines of the previous frame. Returns how many lines it
    /// drew.
    fn draw(
        &self,
        out: &mut impl Write,
        colors: bool,
        screen: Screen,
        first: usize,
        cursor: usize,
        drawn: u16,
    ) -> io::Result<u16> {
        if drawn > 0 {
            queue!(
                out,
                MoveToPreviousLine(drawn),
                Clear(ClearType::FromCursorDown)
            )?;
        }
        let count = self.options.len();
        let position = if screen.rows < count {
            format!(" ({}/{count})", cursor + 1)
        } else {
            String::new()
        };
        // The last column stays empty: some terminals wrap a full row.
        let text_width = screen.width.saturating_sub(OPTION_PREFIX_WIDTH + 1);
        // The position comes before the hint, so a narrow terminal cuts the
        // hint first.
        let line = fit(&format!("{}{position}  {KEY_HINT}", self.title), text_width);
        let (title, rest) = match line.strip_prefix(self.title) {
            Some(rest) => (self.title, rest),
            None => (line.as_str(), ""),
        };
        paint(out, colors, Paint::Accent, "? ")?;
        paint(out, colors, Paint::Strong, title)?;
        paint(out, colors, Paint::Faint, rest)?;
        queue!(out, Print("\r\n"))?;

        for (position, option) in self
            .options
            .iter()
            .enumerate()
            .skip(first)
            .take(screen.rows)
        {
            let text = fit(option, text_width);
            if position == cursor {
                paint(out, colors, Paint::Accent, "❯ ")?;
                paint(out, colors, Paint::Strong, &text)?;
            } else {
                queue!(out, Print("  "), Print(text))?;
            }
            queue!(out, Print("\r\n"))?;
        }
        out.flush()?;
        // The rows are bounded by the terminal height, a `u16`.
        Ok(1 + u16::try_from(screen.rows.min(count)).unwrap_or(u16::MAX))
    }

    /// The one line that stays on the screen in place of the menu.
    fn draw_answer(
        &self,
        out: &mut impl Write,
        colors: bool,
        width: usize,
        answer: MenuAnswer,
    ) -> io::Result<()> {
        let (mark, mark_paint, text) = match answer {
            MenuAnswer::Selected(position) => ("✔ ", Paint::Good, self.options[position].as_str()),
            MenuAnswer::Cancelled => ("✖ ", Paint::Bad, "cancelled"),
        };
        let line = fit(
            &format!("{}: {text}", self.title),
            width.saturating_sub(OPTION_PREFIX_WIDTH + 1),
        );
        paint(out, colors, mark_paint, mark)?;
        queue!(out, Print(line), Print("\r\n"))?;
        out.flush()
    }
}

/// The terminal in raw mode with the cursor hidden, for the life of the
/// value. Dropping it restores both, also when the menu fails or panics.
struct RawMode;

impl RawMode {
    fn enter(out: &mut impl Write) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let raw_mode = Self;
        execute!(out, Hide)?;
        Ok(raw_mode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Best effort: a terminal that refuses the restore has nothing more to
        // tell, and a drop cannot fail.
        let _ = execute!(io::stderr(), Show);
        let _ = terminal::disable_raw_mode();
    }
}

/// The room the menu has on the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Screen {
    /// Columns of a line.
    width: usize,
    /// Options on the screen at one time.
    rows: usize,
}

impl Screen {
    fn measure(count: usize) -> io::Result<Self> {
        let (columns, lines) = terminal::size()?;
        Ok(Self::of(count, columns, lines))
    }

    /// The room for `count` options on a terminal of `columns` by `lines`.
    /// The title takes one line and the cursor below the menu another, so the
    /// menu never scrolls the terminal. One option is always on the screen.
    fn of(count: usize, columns: u16, lines: u16) -> Self {
        Self {
            width: usize::from(columns),
            rows: usize::from(lines)
                .saturating_sub(2)
                .max(1)
                .min(count.max(1)),
        }
    }
}

/// What a key asks the menu to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuKey {
    Up,
    Down,
    First,
    Last,
    Select,
    Cancel,
}

fn menu_key(key: &KeyEvent) -> Option<MenuKey> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return (key.code == KeyCode::Char('c')).then_some(MenuKey::Cancel);
    }
    match key.code {
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => Some(MenuKey::Up),
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => Some(MenuKey::Down),
        KeyCode::Home => Some(MenuKey::First),
        KeyCode::End => Some(MenuKey::Last),
        KeyCode::Enter => Some(MenuKey::Select),
        KeyCode::Esc | KeyCode::Char('q') => Some(MenuKey::Cancel),
        _ => None,
    }
}

/// What a key does to a menu of `count` options with the cursor on `cursor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The cursor goes to this position.
    MoveTo(usize),
    /// The person answered.
    Answer(MenuAnswer),
}

/// The arrows wrap around: up from the first option goes to the last one.
fn step(cursor: usize, count: usize, key: MenuKey) -> Step {
    let last = count.saturating_sub(1);
    match key {
        MenuKey::Up if cursor == 0 => Step::MoveTo(last),
        MenuKey::Up => Step::MoveTo(cursor - 1),
        MenuKey::Down if cursor >= last => Step::MoveTo(0),
        MenuKey::Down => Step::MoveTo(cursor + 1),
        MenuKey::First => Step::MoveTo(0),
        MenuKey::Last => Step::MoveTo(last),
        MenuKey::Select => Step::Answer(MenuAnswer::Selected(cursor)),
        MenuKey::Cancel => Step::Answer(MenuAnswer::Cancelled),
    }
}

/// The first option on the screen once the cursor is on `cursor`: the window
/// of `rows` options moves as little as it can to show the cursor, and never
/// goes past the end of the `count` options.
fn scroll(first: usize, cursor: usize, rows: usize, count: usize) -> usize {
    let first = first.min(count.saturating_sub(rows));
    if cursor < first {
        cursor
    } else if cursor >= first + rows {
        cursor + 1 - rows
    } else {
        first
    }
}

/// `text` on one row of at most `width` columns: each control character
/// becomes a space, and a text that is wider ends in `…`.
fn fit(text: &str, width: usize) -> String {
    let printable: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if printable.width() <= width {
        return printable;
    }
    let Some(room) = width.checked_sub(1) else {
        return String::new();
    };
    let mut used = 0;
    let mut cut: String = printable
        .chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= room
        })
        .collect();
    cut.push('…');
    cut
}

/// How a part of a line looks.
#[derive(Debug, Clone, Copy)]
enum Paint {
    /// The question mark and the cursor.
    Accent,
    /// The title and the option under the cursor.
    Strong,
    /// The key hint.
    Faint,
    /// The mark of an answer.
    Good,
    /// The mark of a cancelled menu.
    Bad,
}

fn paint(out: &mut impl Write, colors: bool, how: Paint, text: &str) -> io::Result<()> {
    if !colors {
        return queue!(out, Print(text));
    }
    let (color, attribute) = match how {
        Paint::Accent => (Some(Color::Cyan), Attribute::Bold),
        Paint::Strong => (None, Attribute::Bold),
        Paint::Faint => (None, Attribute::Dim),
        Paint::Good => (Some(Color::Green), Attribute::Bold),
        Paint::Bad => (Some(Color::Yellow), Attribute::Bold),
    };
    if let Some(color) = color {
        queue!(out, SetForegroundColor(color))?;
    }
    queue!(
        out,
        SetAttribute(attribute),
        Print(text),
        SetAttribute(Attribute::Reset),
        ResetColor
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_arrows_and_the_vi_keys_move_and_enter_esc_answer() {
        for (code, expected) in [
            (KeyCode::Up, MenuKey::Up),
            (KeyCode::Char('k'), MenuKey::Up),
            (KeyCode::BackTab, MenuKey::Up),
            (KeyCode::Down, MenuKey::Down),
            (KeyCode::Char('j'), MenuKey::Down),
            (KeyCode::Tab, MenuKey::Down),
            (KeyCode::Home, MenuKey::First),
            (KeyCode::End, MenuKey::Last),
            (KeyCode::Enter, MenuKey::Select),
            (KeyCode::Esc, MenuKey::Cancel),
            (KeyCode::Char('q'), MenuKey::Cancel),
        ] {
            assert_eq!(menu_key(&key(code)), Some(expected), "{code:?}");
        }
        assert_eq!(
            menu_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(MenuKey::Cancel),
            "Ctrl-C cancels, as the terminal is in raw mode and sends no signal"
        );
        for ignored in [KeyCode::Char('c'), KeyCode::Char('x'), KeyCode::Left] {
            assert_eq!(menu_key(&key(ignored)), None, "{ignored:?}");
        }
        assert_eq!(
            menu_key(&KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            None,
            "a control chord other than Ctrl-C does nothing"
        );
        let mut release = key(KeyCode::Enter);
        release.kind = KeyEventKind::Release;
        assert_eq!(menu_key(&release), None, "a key counts when it goes down");
    }

    #[test]
    fn the_cursor_wraps_around_and_jumps_to_the_ends() {
        assert_eq!(step(0, 3, MenuKey::Down), Step::MoveTo(1));
        assert_eq!(step(2, 3, MenuKey::Down), Step::MoveTo(0));
        assert_eq!(step(1, 3, MenuKey::Up), Step::MoveTo(0));
        assert_eq!(step(0, 3, MenuKey::Up), Step::MoveTo(2));
        assert_eq!(step(1, 3, MenuKey::First), Step::MoveTo(0));
        assert_eq!(step(1, 3, MenuKey::Last), Step::MoveTo(2));
        assert_eq!(step(0, 1, MenuKey::Down), Step::MoveTo(0));
        assert_eq!(step(0, 1, MenuKey::Up), Step::MoveTo(0));
    }

    #[test]
    fn enter_selects_the_option_under_the_cursor_and_cancel_selects_none() {
        assert_eq!(
            step(2, 3, MenuKey::Select),
            Step::Answer(MenuAnswer::Selected(2))
        );
        assert_eq!(
            step(2, 3, MenuKey::Cancel),
            Step::Answer(MenuAnswer::Cancelled)
        );
    }

    #[test]
    fn the_window_moves_as_little_as_it_can_to_show_the_cursor() {
        // Ten options, four on the screen.
        assert_eq!(scroll(0, 3, 4, 10), 0, "the cursor is on the screen");
        assert_eq!(scroll(0, 4, 4, 10), 1, "one down past the bottom");
        assert_eq!(scroll(3, 2, 4, 10), 2, "one up past the top");
        assert_eq!(scroll(0, 9, 4, 10), 6, "a jump to the last option");
        assert_eq!(scroll(6, 0, 4, 10), 0, "a jump to the first option");
        assert_eq!(
            scroll(8, 9, 4, 10),
            6,
            "a taller screen pulls the window back from past the end"
        );
        assert_eq!(scroll(0, 2, 5, 3), 0, "all the options fit");
    }

    #[test]
    fn the_screen_keeps_a_line_for_the_title_and_one_below() {
        assert_eq!(Screen::of(3, 80, 24), Screen { width: 80, rows: 3 });
        assert_eq!(
            Screen::of(30, 80, 24),
            Screen {
                width: 80,
                rows: 22
            }
        );
        assert_eq!(
            Screen::of(30, 80, 2),
            Screen { width: 80, rows: 1 },
            "one option is always on the screen"
        );
    }

    #[test]
    fn a_line_is_cut_to_the_width_and_keeps_to_one_row() {
        assert_eq!(fit("Robotics lab", 20), "Robotics lab");
        assert_eq!(fit("Robotics lab", 12), "Robotics lab");
        assert_eq!(fit("Robotics lab", 9), "Robotics…");
        assert_eq!(fit("Robotics lab", 1), "…");
        assert_eq!(fit("Robotics lab", 0), "");
        assert_eq!(
            fit("ロボット研究室", 7),
            "ロボッ…",
            "a wide character takes two columns"
        );
        assert_eq!(
            fit("Lab\nField\u{1b}[31m", 40),
            "Lab Field [31m",
            "a control character cannot move the cursor or color the terminal"
        );
    }
}
