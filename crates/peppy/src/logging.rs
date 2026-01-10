use std::io::IsTerminal;

use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

#[derive(Clone, Copy)]
pub enum LogStyle {
    Verbose,
    Compact,
}

struct RedErrorFormatter<E> {
    inner: E,
    color_errors: bool,
}

impl<E> RedErrorFormatter<E> {
    fn new(inner: E, color_errors: bool) -> Self {
        Self {
            inner,
            color_errors,
        }
    }
}

impl<S, N, E> FormatEvent<S, N> for RedErrorFormatter<E>
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
        let mut buffer = String::new();
        let buffer_writer = tracing_subscriber::fmt::format::Writer::new(&mut buffer);
        self.inner.format_event(ctx, buffer_writer, event)?;

        if self.color_errors && *event.metadata().level() == Level::ERROR {
            writer.write_str("\x1b[31m")?;
            writer.write_str(&buffer)?;
            writer.write_str("\x1b[0m")?;
        } else {
            writer.write_str(&buffer)?;
        }

        Ok(())
    }
}

fn should_colorize_stdout() -> bool {
    std::env::var("NO_COLOR").map_or(true, |v| v.is_empty()) && std::io::stdout().is_terminal()
}

fn default_env_filter(default_directive: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive))
}

pub fn init_tracing(style: LogStyle) {
    let env_filter = match style {
        LogStyle::Verbose => default_env_filter("info"),
        LogStyle::Compact => default_env_filter("peppy=info"),
    };

    match style {
        LogStyle::Verbose => tracing_subscriber::fmt().with_env_filter(env_filter).init(),
        LogStyle::Compact => {
            let color_errors = should_colorize_stdout();
            let format = tracing_subscriber::fmt::format::format()
                .without_time()
                .with_level(false)
                .with_target(false);
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_ansi(false)
                .event_format(RedErrorFormatter::new(format, color_errors))
                .init();
        }
    }
}
