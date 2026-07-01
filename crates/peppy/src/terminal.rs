//! Terminal output utilities for displaying scrolling, fixed-height output regions.
//!
//! This module provides a `ScrollingOutput` struct that displays output in a fixed
//! number of terminal lines, similar to how `docker buildx` displays build progress.

use crossterm::{
    ExecutableCommand,
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
/// clean (the caller just gets no spinner). The caller is responsible for ending
/// it with [`indicatif::ProgressBar::finish_and_clear`] once the wait is over.
pub(crate) fn spinner(
    message: impl Into<std::borrow::Cow<'static, str>>,
) -> Option<indicatif::ProgressBar> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_message(message);
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    Some(pb)
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
        // Move cursor up to overwrite previous output
        if self.rendered_lines > 0 {
            let _ = self.stdout.execute(MoveUp(self.rendered_lines as u16));
        }

        // Render each line with dim styling
        for line in &self.lines {
            let _ = self.stdout.execute(MoveToColumn(0));
            let _ = self.stdout.execute(Clear(ClearType::CurrentLine));
            let _ = self.stdout.execute(SetAttribute(Attribute::Dim));

            // Truncate line if it's too long for the terminal
            let truncated = Self::truncate_line(line);
            let _ = write!(self.stdout, "{}", truncated);

            let _ = self.stdout.execute(SetAttribute(Attribute::Reset));
            let _ = writeln!(self.stdout);
        }

        // Fill remaining lines with blanks if we have fewer lines than max
        for _ in self.lines.len()..self.max_lines {
            let _ = self.stdout.execute(MoveToColumn(0));
            let _ = self.stdout.execute(Clear(ClearType::CurrentLine));
            let _ = writeln!(self.stdout);
        }

        let _ = self.stdout.flush();
        self.rendered_lines = self.max_lines;
    }

    /// Renders without escape codes (for non-terminal output).
    fn render_plain(&mut self) {
        // In non-terminal mode, just print the latest line
        if let Some(line) = self.lines.back() {
            println!("{}", line);
        }
    }

    /// Truncates a line to fit the terminal width, leaving a small margin.
    fn truncate_line(line: &str) -> &str {
        let term_width = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80);
        // Leave some margin
        Self::truncate_to_width(line, term_width.saturating_sub(2))
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
