use tracing::{Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Clone, Copy)]
pub enum LogStyle {
    Verbose,
    Compact,
}

/// Heads every line with its level, coloring the tag itself rather than
/// letting the inner format color the whole line, so a compact run reads as
/// `[INFO] <message>`.
struct LevelPrefixFormatter<E> {
    inner: E,
    colorize: bool,
}

impl<E> LevelPrefixFormatter<E> {
    fn new(inner: E, colorize: bool) -> Self {
        Self { inner, colorize }
    }
}

/// The tag a compact line opens with for `level`, and the color the tag
/// takes when the run paints. DEBUG and TRACE lines carry no tag.
fn level_tag(level: Level) -> Option<(&'static str, &'static str)> {
    match level {
        Level::INFO => Some(("[INFO]", "\x1b[32m")),
        Level::WARN => Some(("[WARNING]", "\x1b[33m")),
        Level::ERROR => Some(("[ERROR]", "\x1b[31m")),
        _ => None,
    }
}

impl<S, N, E> FormatEvent<S, N> for LevelPrefixFormatter<E>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    E: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        if let Some((tag, color)) = level_tag(*event.metadata().level()) {
            if self.colorize {
                write!(writer, "{color}{tag}\x1b[0m ")?;
            } else {
                write!(writer, "{tag} ")?;
            }
        }

        // `by_ref` hands the inner format the subscriber's own writer, and
        // with it the ANSI decisions the builder made. A writer built here
        // over a local buffer would carry the defaults instead, and the
        // default escapes the control characters out of a message: a painted
        // run would print its endpoint blocks as literal `\x1b[35m…` text.
        self.inner.format_event(ctx, writer.by_ref(), event)
    }
}

fn default_env_filter(default_directive: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive))
}

/// What each style lets through, `RUST_LOG` overriding it.
fn env_filter(style: LogStyle) -> EnvFilter {
    match style {
        // Demote Zenoh's routine session-lifecycle chatter (`Using ZID`,
        // `close session zid=…`) to WARN. The router watchdog probes liveness by
        // opening and closing a throwaway Zenoh session every couple of seconds,
        // and at INFO those two lines per probe bury the daemon's own logs. The
        // daemon's useful messaging logs come from the `pmi`/`peppy` targets, not
        // `zenoh`, and genuine Zenoh warnings/errors still surface. Override with
        // `RUST_LOG=info` to see the full Zenoh output when debugging the router.
        LogStyle::Verbose => default_env_filter("info,zenoh=warn"),
        // `daemon_config` is included because the auth commands also load (and
        // complete) peppy_config.json5: without it, the one-time "added
        // settings" line after an upgrade, and even this crate's warnings,
        // would be invisible in release CLI runs.
        LogStyle::Compact => {
            default_env_filter("peppy=info,daemon=info,auth=info,daemon_config=info")
        }
    }
}

/// The subscriber [`init_tracing`] installs, over an explicit filter, color
/// decision and sink so a test can read back exactly what a style writes
/// without depending on the ambient terminal or on `RUST_LOG`.
fn subscriber<W>(
    style: LogStyle,
    filter: EnvFilter,
    colorize: bool,
    writer: W,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    // Whether to escape the ANSI control characters out of a message before
    // writing it, which is the reverse of the CLI's color decision.
    //
    // A painting run tints its own output (the endpoint blocks a launch
    // prints, `stack list`, `repo list`, …) and those control characters
    // travel to the terminal in the message, so they have to go through
    // whole. A plain run writes none of its own: the only control characters
    // that can reach its log come from the data in it, a node name or a
    // repository label or a remote daemon's error text, and a pipe, a log
    // file or a CI transcript is exactly where an operator wants to read the
    // escape rather than have a terminal obey it later.
    let sanitize_ansi = !colorize;

    match style {
        LogStyle::Verbose => Box::new(
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi_sanitization(sanitize_ansi)
                .with_writer(writer)
                .finish(),
        ),
        LogStyle::Compact => {
            let format = tracing_subscriber::fmt::format::format()
                .without_time()
                .with_level(false)
                .with_target(false);
            Box::new(
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_ansi(false)
                    .with_ansi_sanitization(sanitize_ansi)
                    .with_writer(writer)
                    .event_format(LevelPrefixFormatter::new(format, colorize))
                    .finish(),
            )
        }
    }
}

pub fn init_tracing(style: LogStyle) {
    subscriber(
        style,
        env_filter(style),
        peppy::colors_enabled(),
        std::io::stdout,
    )
    .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io;
    use std::sync::{Arc, Mutex};

    /// Keeps every byte a subscriber writes, so a test can read the rendered
    /// line back instead of watching it go to stdout.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn written(&self) -> String {
            String::from_utf8(self.0.lock().expect("capture is not poisoned").clone())
                .expect("the subscriber writes utf-8")
        }
    }

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture is not poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs `log` under the subscriber `style` installs over `filter`, and
    /// returns what it wrote.
    fn written_by(style: LogStyle, filter: &str, colorize: bool, log: impl FnOnce()) -> String {
        let capture = Capture::default();
        let subscriber = subscriber(style, EnvFilter::new(filter), colorize, capture.clone());
        tracing::subscriber::with_default(subscriber, log);
        capture.written()
    }

    /// Logs `message` at INFO through the subscriber `style` installs, and
    /// returns what it wrote.
    fn logged(style: LogStyle, colorize: bool, message: &str) -> String {
        written_by(style, "info", colorize, || tracing::info!("{message}"))
    }

    /// A line of the endpoint block a launch prints: the CLI paints the fields
    /// itself, so the control characters are already in the message by the
    /// time a style renders it.
    const PAINTED: &str = "  \x1b[35msimulation_inst\x1b[0m (\x1b[36mwaldo:v1\x1b[0m) @\x1b[34mcn-innovative-grouper\x1b[0m";

    /// The tints the CLI painted survive to the terminal whole, in both
    /// styles: this is what a colored `stack launch` block is made of, and an
    /// escaped one reads as literal `\x1b[35m…` noise.
    #[test]
    fn a_painted_message_reaches_the_terminal_with_its_control_characters() {
        for style in [LogStyle::Verbose, LogStyle::Compact] {
            let written = logged(style, true, PAINTED);
            assert!(
                written.contains(PAINTED),
                "the painted line goes through untouched, got:\n{written:?}"
            );
        }
    }

    /// A plain run paints nothing, so the control characters it is handed came
    /// from the data and are escaped rather than obeyed.
    #[test]
    fn a_plain_message_carries_its_control_characters_escaped() {
        for style in [LogStyle::Verbose, LogStyle::Compact] {
            let written = logged(style, false, PAINTED);
            assert!(
                written.contains("\\x1b[35msimulation_inst\\x1b[0m"),
                "the control characters are escaped into the text, got:\n{written:?}"
            );
            assert!(
                !written.contains(PAINTED),
                "no unescaped run of the painted line survives, got:\n{written:?}"
            );
        }
    }

    /// The compact style renders a line as its level tag and the message,
    /// once: the level is the formatter's own, the message the inner format's.
    #[test]
    fn the_compact_style_writes_the_level_tag_and_the_message_once() {
        assert_eq!(
            logged(LogStyle::Compact, true, "Launch complete"),
            "\x1b[32m[INFO]\x1b[0m Launch complete\n"
        );
        assert_eq!(
            logged(LogStyle::Compact, false, "Launch complete"),
            "[INFO] Launch complete\n"
        );
    }

    /// The compact style tags a warning and an error with their own level, in
    /// their own color when the run paints, and a debug line with no tag.
    #[test]
    fn the_compact_style_tags_each_level_it_names() {
        let warned = |colorize| {
            written_by(LogStyle::Compact, "info", colorize, || {
                tracing::warn!("Disk almost full")
            })
        };
        let failed = |colorize| {
            written_by(LogStyle::Compact, "info", colorize, || {
                tracing::error!("Build failed")
            })
        };
        let debugged = |colorize| {
            written_by(LogStyle::Compact, "debug", colorize, || {
                tracing::debug!("Resolved 3 nodes")
            })
        };
        assert_eq!(warned(true), "\x1b[33m[WARNING]\x1b[0m Disk almost full\n");
        assert_eq!(warned(false), "[WARNING] Disk almost full\n");
        assert_eq!(failed(true), "\x1b[31m[ERROR]\x1b[0m Build failed\n");
        assert_eq!(failed(false), "[ERROR] Build failed\n");
        assert_eq!(debugged(true), "Resolved 3 nodes\n");
        assert_eq!(debugged(false), "Resolved 3 nodes\n");
    }
}
