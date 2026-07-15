use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

#[derive(Clone, Copy)]
pub enum LogStyle {
    Verbose,
    Compact,
}

struct LevelPrefixFormatter<E> {
    inner: E,
    colorize: bool,
}

impl<E> LevelPrefixFormatter<E> {
    fn new(inner: E, colorize: bool) -> Self {
        Self { inner, colorize }
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
        let level = *event.metadata().level();

        // Write colored level prefix for INFO, WARN, ERROR
        match level {
            Level::INFO => {
                if self.colorize {
                    writer.write_str("\x1b[32m[INFO]\x1b[0m ")?;
                } else {
                    writer.write_str("[INFO] ")?;
                }
            }
            Level::WARN => {
                if self.colorize {
                    writer.write_str("\x1b[33m[WARNING]\x1b[0m ")?;
                } else {
                    writer.write_str("[WARNING] ")?;
                }
            }
            Level::ERROR => {
                if self.colorize {
                    writer.write_str("\x1b[31m[ERROR]\x1b[0m ")?;
                } else {
                    writer.write_str("[ERROR] ")?;
                }
            }
            _ => {}
        }

        let mut buffer = String::new();
        let buffer_writer = tracing_subscriber::fmt::format::Writer::new(&mut buffer);
        self.inner.format_event(ctx, buffer_writer, event)?;
        writer.write_str(&buffer)?;

        Ok(())
    }
}

fn default_env_filter(default_directive: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive))
}

pub fn init_tracing(style: LogStyle) {
    let env_filter = match style {
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
    };

    match style {
        LogStyle::Verbose => tracing_subscriber::fmt().with_env_filter(env_filter).init(),
        LogStyle::Compact => {
            let colorize = peppy::colors_enabled();
            let format = tracing_subscriber::fmt::format::format()
                .without_time()
                .with_level(false)
                .with_target(false);
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_ansi(false)
                .event_format(LevelPrefixFormatter::new(format, colorize))
                .init();
        }
    }
}
