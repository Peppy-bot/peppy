//! Human-readable duration / delta formatting and a generic aligned table.

use std::time::Duration;

/// Format a duration as `ns` / `µs` / `ms`, picking the unit by magnitude.
pub fn fmt_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos < 1_000 {
        format!("{nanos}ns")
    } else if nanos < 1_000_000 {
        format!("{:.0}µs", nanos as f64 / 1_000.0)
    } else {
        format!("{:.2}ms", nanos as f64 / 1_000_000.0)
    }
}

/// Format the percentage change of `now_ns` versus `prev_ns` (e.g. `+12.3%`).
/// Returns `—` when there is no usable baseline.
pub fn fmt_delta(now_ns: u64, prev_ns: u64) -> String {
    if prev_ns == 0 {
        return "—".to_string();
    }
    let pct = (now_ns as f64 - prev_ns as f64) / prev_ns as f64 * 100.0;
    format!("{pct:+.1}%")
}

/// Visible length of `value` in columns: the `µ`/`Δ`/`✓` glyphs count as one
/// column each, and ANSI SGR color escapes count as zero (they occupy no display
/// columns). Keeps a colored cell measuring the same as its plain text so the
/// table stays aligned whether or not the caller tints cells.
fn visible_len(value: &str) -> usize {
    if !value.as_bytes().contains(&0x1b) {
        return value.chars().count();
    }
    let mut len = 0;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip a CSI escape: an optional `[`, then bytes up to and including
            // a final byte in `@`..=`~` (the `[` itself is in that range, so it
            // must be consumed first).
            if chars.clone().next() == Some('[') {
                chars.next();
            }
            for f in chars.by_ref() {
                if ('@'..='~').contains(&f) {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

/// Pad `value` on the right to `width`, counting the wide `µ`/`Δ`/`✓` glyphs as
/// one column each and ANSI color escapes as zero (so columns stay aligned in a
/// monospace terminal whether or not the cell is colored). Internal helper for
/// [`render_table`].
fn pad(value: &str, width: usize) -> String {
    let len = visible_len(value);
    if len >= width {
        value.to_string()
    } else {
        format!("{value}{}", " ".repeat(width - len))
    }
}

/// Render a header row, a dashed separator, and the data rows as a single string,
/// with every column padded to the widest cell in it. Columns are joined by two
/// spaces. The caller is responsible for any surrounding title / footnotes.
///
/// `rows` whose length differs from `headers` are still rendered; missing cells
/// are treated as empty.
pub fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths = vec![0usize; cols];
    for (i, header) in headers.iter().enumerate() {
        widths[i] = header.chars().count();
    }
    for row in rows {
        for (i, value) in row.iter().take(cols).enumerate() {
            widths[i] = widths[i].max(visible_len(value));
        }
    }

    let render_row = |cells: &[String]| {
        (0..cols)
            .map(|i| {
                let value = cells.get(i).map(String::as_str).unwrap_or("");
                pad(value, widths[i])
            })
            .collect::<Vec<_>>()
            .join("  ")
    };

    let mut out = String::new();
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    out.push_str(&render_row(&header_cells));
    out.push('\n');
    let separator_len = widths.iter().sum::<usize>() + 2 * cols.saturating_sub(1);
    out.push_str(&"-".repeat(separator_len));
    for row in rows {
        out.push('\n');
        out.push_str(&render_row(row));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_units() {
        assert_eq!(fmt_duration(Duration::from_nanos(500)), "500ns");
        assert_eq!(fmt_duration(Duration::from_nanos(2_600)), "3µs");
        assert_eq!(fmt_duration(Duration::from_micros(1_500)), "1.50ms");
    }

    #[test]
    fn fmt_duration_unit_boundaries() {
        // The branch cutoffs are `< 1_000` (ns) and `< 1_000_000` (µs).
        assert_eq!(fmt_duration(Duration::from_nanos(999)), "999ns");
        assert_eq!(fmt_duration(Duration::from_nanos(1_000)), "1µs");
        assert_eq!(fmt_duration(Duration::from_nanos(999_999)), "1000µs");
        assert_eq!(fmt_duration(Duration::from_nanos(1_000_000)), "1.00ms");
    }

    #[test]
    fn fmt_delta_handles_zero_baseline() {
        assert_eq!(fmt_delta(100, 0), "—");
        assert_eq!(fmt_delta(110, 100), "+10.0%");
        assert_eq!(fmt_delta(90, 100), "-10.0%");
    }

    #[test]
    fn render_table_aligns_columns() {
        let headers = ["name", "p50"];
        let rows = vec![
            vec!["a".to_string(), "1ms".to_string()],
            vec!["longer".to_string(), "12ms".to_string()],
        ];
        let table = render_table(&headers, &rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines[0], "name    p50 ");
        // separator spans both columns plus the 2-space gap
        assert_eq!(lines[1].len(), "name    p50 ".len());
        assert_eq!(lines[2], "a       1ms ");
        assert_eq!(lines[3], "longer  12ms");
    }

    #[test]
    fn pad_counts_visible_columns_only() {
        // A cyan-painted "a" occupies one display column; the ANSI escapes are
        // zero-width, so it pads to the same width as a plain "a".
        let colored = "\x1b[36ma\x1b[0m";
        assert_eq!(pad(colored, 4), format!("{colored}   "));
        assert_eq!(visible_len(colored), 1);
    }

    #[test]
    fn visible_len_tolerates_truncated_and_bare_escapes() {
        // A complete CSI color sequence is zero-width: only "a" is a column.
        assert_eq!(visible_len("\x1b[36ma"), 1);
        // An escape whose final byte never arrives before end-of-string is
        // consumed without panicking and contributes no columns.
        assert_eq!(visible_len("a\x1b[36"), 1);
        // An escape not followed by `[` still skips up to its final byte (here
        // `m`), so only "a" and "b" count.
        assert_eq!(visible_len("a\x1bmb"), 2);
    }

    #[test]
    fn render_table_pads_short_rows_and_ignores_extra_cells() {
        let headers = ["a", "b", "c"];
        let rows = vec![
            // One cell short: the missing third column renders as empty.
            vec!["x".to_string(), "y".to_string()],
            // One cell long: the fourth ("extra") is dropped.
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "extra".to_string(),
            ],
        ];
        let table = render_table(&headers, &rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines[0], "a  b  c");
        assert_eq!(lines[2], "x  y   ");
        assert_eq!(lines[3], "1  2  3");
        assert!(!table.contains("extra"), "the extra cell must be dropped");
    }

    #[test]
    fn render_table_alignment_is_unaffected_by_color_codes() {
        let headers = ["name", "p50"];
        // The wider cell is colored; the column width must track its visible
        // text ("longer"), not the byte length inflated by escape codes.
        let plain = vec![
            vec!["a".to_string(), "1ms".to_string()],
            vec!["longer".to_string(), "12ms".to_string()],
        ];
        let colored = vec![
            vec!["a".to_string(), "1ms".to_string()],
            vec!["\x1b[36mlonger\x1b[0m".to_string(), "12ms".to_string()],
        ];
        // Stripping the escapes from the colored render reproduces the plain one.
        let strip = |s: String| s.replace("\x1b[36m", "").replace("\x1b[0m", "");
        assert_eq!(
            strip(render_table(&headers, &colored)),
            render_table(&headers, &plain)
        );
    }
}
