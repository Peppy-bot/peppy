//! Terminal output utilities for displaying scrolling, fixed-height output regions.
//!
//! This module provides a `ScrollingOutput` struct that displays output in a fixed
//! number of terminal lines, similar to how `docker buildx` displays build progress.

use crossterm::{
    ExecutableCommand, QueueableCommand,
    cursor::{MoveToColumn, MoveUp},
    style::{Attribute, SetAttribute},
    terminal::{Clear, ClearType},
};
use std::collections::VecDeque;
use std::io::{IsTerminal, Stdout, Write};

/// Whether stdout should carry ANSI color: only when it is an interactive
/// terminal and `NO_COLOR` is unset or empty. Single source of truth for the
/// color gate so every command, and the binary's log formatter, stay
/// consistent. Re-exported from the crate root as `peppy::colors_enabled`.
pub fn colors_enabled() -> bool {
    std::io::stdout().is_terminal() && !no_color_requested()
}

/// Whether stderr should carry ANSI color: the gate of [`colors_enabled`] for
/// what draws on stderr, such as a menu.
pub(crate) fn stderr_colors_enabled() -> bool {
    std::io::stderr().is_terminal() && !no_color_requested()
}

/// The stdout column budget for width-fitted output: the terminal's width
/// when stdout is one, `None` when stdout is piped, so redirected output
/// keeps every row on one line.
pub(crate) fn stdout_width() -> Option<usize> {
    std::io::stdout().is_terminal().then(terminal_width)
}

/// The width of the terminal, or 80 columns when the size cannot be read.
fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(width, _)| width as usize)
        .unwrap_or(80)
}

/// Whether the `NO_COLOR` convention asks for plain output: the variable set to
/// a non-empty value. An empty `NO_COLOR` is treated as unset, per the
/// convention at https://no-color.org.
fn no_color_requested() -> bool {
    no_color_from_env(std::env::var("NO_COLOR").ok())
}

/// Pure decision behind [`no_color_requested`]: `NO_COLOR` asks for plain output
/// only when it is present and non-empty. Split out so the convention can be
/// tested without mutating the process environment.
fn no_color_from_env(value: Option<String>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// A steady-tick spinner for a user-visible wait (browser approval, the daemon
/// establishing federation, …). The single source of truth for the spinner look
/// so every wait shows the *same* loader: default braille frames, a `120 ms`
/// tick, and `{spinner} {msg}` rendered to stderr.
///
/// Returns `None` when stderr is not a real terminal, so piped/CI output stays
/// clean (the caller just gets no spinner). [`while_spinning`] ends it with
/// [`indicatif::ProgressBar::finish_and_clear`] once the wait is over.
fn spinner(message: &'static str) -> Option<indicatif::ProgressBar> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_message(message);
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    Some(pb)
}

/// Runs `work` under a [`spinner`] that shows `message`, and clears the
/// spinner when `work` returns, whatever it returns. The one owner of the
/// spinner's start and end, so every wait of the CLI shows and removes it the
/// same way.
pub(crate) fn while_spinning<T>(message: &'static str, work: impl FnOnce() -> T) -> T {
    let spinner = spinner(message);
    let result = work();
    if let Some(spinner) = spinner {
        spinner.finish_and_clear();
    }
    result
}

/// A fixed-height scrolling output region for the terminal.
///
/// This struct manages displaying a fixed number of lines in the terminal,
/// with new lines pushing old ones up. The text is displayed in a dimmed/greyed
/// style to visually distinguish it from normal output.
pub struct ScrollingOutput {
    /// The lines currently being displayed.
    lines: VecDeque<String>,
    /// Maximum number of lines to display.
    max_lines: usize,
    /// Number of lines currently rendered on screen.
    rendered_lines: usize,
    /// Whether we're outputting to a real terminal (enables ANSI codes).
    is_terminal: bool,
    /// Stdout handle.
    stdout: Stdout,
}

impl ScrollingOutput {
    /// Creates a new `ScrollingOutput` with the specified maximum number of lines.
    ///
    /// # Arguments
    /// * `max_lines` - The maximum number of lines to display at once.
    pub fn new(max_lines: usize) -> Self {
        let stdout = std::io::stdout();
        let is_terminal = colors_enabled();

        Self {
            lines: VecDeque::with_capacity(max_lines),
            max_lines,
            rendered_lines: 0,
            is_terminal,
            stdout,
        }
    }

    /// Adds a line to the output and refreshes the display.
    ///
    /// If the buffer is full, the oldest line will be removed.
    ///
    /// # Arguments
    /// * `line` - The line to add.
    pub fn add_line(&mut self, line: &str) {
        // Trim the line and add it to the buffer
        let line = line.trim_end().to_string();

        if self.lines.len() >= self.max_lines {
            self.lines.pop_front();
        }
        self.lines.push_back(line);

        self.render();
    }

    /// Renders the current lines to the terminal.
    fn render(&mut self) {
        if self.is_terminal {
            self.render_terminal();
        } else {
            self.render_plain();
        }
    }

    /// Renders using terminal escape codes for the scrolling effect.
    fn render_terminal(&mut self) {
        // Leave some margin
        let max_width = terminal_width().saturating_sub(2);
        draw_frame(
            &mut self.stdout.lock(),
            &self.lines,
            self.max_lines,
            self.rendered_lines,
            max_width,
        );
        self.rendered_lines = self.max_lines;
    }

    /// Renders without escape codes (for non-terminal output).
    fn render_plain(&mut self) {
        // In non-terminal mode, just print the latest line
        if let Some(line) = self.lines.back() {
            println!("{}", line);
        }
    }

    /// Truncates `line` to at most `max_width` bytes, never splitting a UTF-8
    /// character (it backs up to the nearest char boundary). Pure in its inputs
    /// so the boundary math can be tested without a real terminal.
    fn truncate_to_width(line: &str, max_width: usize) -> &str {
        if line.len() <= max_width {
            return line;
        }
        let mut end = max_width;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        &line[..end]
    }

    /// Clears the scrolling output region and moves the cursor back up.
    ///
    /// Call this when you're done with the scrolling output to clean up
    /// the display area.
    pub fn clear(&mut self) {
        if !self.is_terminal || self.rendered_lines == 0 {
            return;
        }

        // Move cursor up to the start of our region
        let _ = self.stdout.execute(MoveUp(self.rendered_lines as u16));

        // Clear all the lines we rendered
        for _ in 0..self.rendered_lines {
            let _ = self.stdout.execute(MoveToColumn(0));
            let _ = self.stdout.execute(Clear(ClearType::CurrentLine));
            let _ = writeln!(self.stdout);
        }

        // Move back up again so subsequent output starts at the right place
        let _ = self.stdout.execute(MoveUp(self.rendered_lines as u16));
        let _ = self.stdout.flush();

        self.rendered_lines = 0;
        self.lines.clear();
    }
}

/// Draws one frame of a [`ScrollingOutput`] region into `out`: moves up over
/// the `rendered_lines` rows the previous frame drew, writes each line dimmed
/// and cut to `max_width` bytes, pads the region to `max_lines` rows, and
/// flushes.
///
/// The escapes are queued rather than executed, which would flush after each
/// one: on line-buffered stdout a redraw then costs about one write per row,
/// and the clear of a row reaches the terminal together with its text.
fn draw_frame(
    out: &mut impl Write,
    lines: &VecDeque<String>,
    max_lines: usize,
    rendered_lines: usize,
    max_width: usize,
) {
    // Move cursor up to overwrite previous output
    if rendered_lines > 0 {
        let _ = out.queue(MoveUp(rendered_lines as u16));
    }

    // Render each line with dim styling
    for line in lines {
        let _ = out.queue(MoveToColumn(0));
        let _ = out.queue(Clear(ClearType::CurrentLine));
        let _ = out.queue(SetAttribute(Attribute::Dim));

        // Truncate line if it's too long for the terminal
        let truncated = ScrollingOutput::truncate_to_width(line, max_width);
        let _ = write!(out, "{}", truncated);

        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = writeln!(out);
    }

    // Fill remaining lines with blanks if we have fewer lines than max
    for _ in lines.len()..max_lines {
        let _ = out.queue(MoveToColumn(0));
        let _ = out.queue(Clear(ClearType::CurrentLine));
        let _ = writeln!(out);
    }

    let _ = out.flush();
}

impl Drop for ScrollingOutput {
    fn drop(&mut self) {
        // Ensure terminal attributes are reset
        if self.is_terminal {
            let _ = self.stdout.execute(SetAttribute(Attribute::Reset));
            let _ = self.stdout.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_decision_follows_the_convention() {
        assert!(!no_color_from_env(None), "unset means colors allowed");
        assert!(
            !no_color_from_env(Some(String::new())),
            "empty is treated as unset"
        );
        assert!(
            no_color_from_env(Some("1".to_string())),
            "any non-empty value disables color"
        );
        assert!(no_color_from_env(Some("anything".to_string())));
    }

    #[test]
    fn truncate_keeps_short_lines_intact() {
        assert_eq!(ScrollingOutput::truncate_to_width("hello", 80), "hello");
        assert_eq!(ScrollingOutput::truncate_to_width("hello", 5), "hello");
    }

    #[test]
    fn truncate_never_splits_a_multibyte_char() {
        // "é" is 2 bytes (0xC3 0xA9). Cutting at an odd byte must back up to the
        // char boundary rather than slice mid-character (which would panic).
        let line = "aéb"; // bytes: a(1) é(2) b(1) = 4 bytes
        // max_width 2 lands inside "é"; result must be just "a".
        assert_eq!(ScrollingOutput::truncate_to_width(line, 2), "a");
        // max_width 3 covers "a" + full "é".
        assert_eq!(ScrollingOutput::truncate_to_width(line, 3), "aé");
    }

    #[test]
    fn truncate_to_zero_width_is_empty() {
        assert_eq!(ScrollingOutput::truncate_to_width("abc", 0), "");
    }

    /// The bytes of two frames of a three-row region: the first draws two
    /// lines and pads the third row, the second moves back up over all three
    /// rows and cuts the line wider than the region.
    #[test]
    fn draw_frame_writes_the_exact_escape_sequence_of_each_frame() {
        const DIM_ROW: &str = "\x1b[1G\x1b[2K\x1b[2m";
        const RESET: &str = "\x1b[0m\n";
        const BLANK_ROW: &str = "\x1b[1G\x1b[2K\n";

        let mut lines = VecDeque::from(["one".to_string(), "two".to_string()]);
        let mut first = Vec::new();
        draw_frame(&mut first, &lines, 3, 0, 4);
        assert_eq!(
            String::from_utf8(first).expect("utf-8 frame"),
            format!("{DIM_ROW}one{RESET}{DIM_ROW}two{RESET}{BLANK_ROW}")
        );

        lines.push_back("three".to_string());
        let mut second = Vec::new();
        draw_frame(&mut second, &lines, 3, 3, 4);
        assert_eq!(
            String::from_utf8(second).expect("utf-8 frame"),
            format!("\x1b[3A{DIM_ROW}one{RESET}{DIM_ROW}two{RESET}{DIM_ROW}thre{RESET}")
        );
    }

    #[test]
    fn add_line_evicts_oldest_beyond_max() {
        let mut output = ScrollingOutput::new(3);
        for i in 0..5 {
            output.add_line(&format!("line {i}"));
        }
        assert_eq!(output.lines.len(), 3, "buffer holds at most max_lines");
        assert_eq!(output.lines.front().map(String::as_str), Some("line 2"));
        assert_eq!(output.lines.back().map(String::as_str), Some("line 4"));
    }
}
