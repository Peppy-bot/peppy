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

/// Pad `value` on the right to `width`, counting the wide `µ`/`Δ`/`✓` glyphs as
/// one column each (so columns stay aligned in a monospace terminal).
pub fn pad(value: &str, width: usize) -> String {
    let len = value.chars().count();
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
            widths[i] = widths[i].max(value.chars().count());
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
}
