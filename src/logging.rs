//! Pretty and precise log formatting.
//!
//! Output layout (one line per event):
//!
//! ```text
//! 2026-08-26T04:00:00.123456Z  INFO handle{service=ssh}: Registered, exposed at 0.0.0.0:6022
//! ```
//!
//! * timestamp — RFC 3339, uncolored
//! * level — padded to 5 chars and color-coded: ERROR red (bold), WARN
//!   yellow, INFO green, DEBUG cyan, TRACE purple
//! * scope — the chain of active spans with their fields, in cyan; this is
//!   what makes `service=ssh` visible on every line of a forwarded service
//! * message + event fields — plain
//!
//! The event target is appended in dim gray at `debug` and `trace` levels,
//! where pinpointing the source matters more than brevity.
//!
//! Colors are enabled only when the output is a terminal and `NO_COLOR` is
//! unset or empty, so redirected/journald logs stay free of escape codes.

use std::fmt;
use std::io::IsTerminal;

use nu_ansi_term as ansi;
use tracing::{Event, Level};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt as ts_fmt};

/// Decide whether ANSI colors should be used.
fn colors_enabled() -> bool {
    if !std::io::stdout().is_terminal() {
        return false;
    }
    match std::env::var_os("NO_COLOR") {
        Some(v) => v.is_empty(),
        None => true,
    }
}

/// Color for a given level, matching the conventional palette.
fn level_style(level: &Level) -> ansi::Style {
    match *level {
        Level::ERROR => ansi::Color::Red.bold(),
        Level::WARN => ansi::Color::Yellow.normal(),
        Level::INFO => ansi::Color::Green.normal(),
        Level::DEBUG => ansi::Color::Cyan.normal(),
        Level::TRACE => ansi::Color::Purple.normal(),
    }
}

/// Install the global default subscriber.
///
/// `RUST_LOG` wins when set; otherwise `default_level` applies. The `console`
/// feature replaces the subscriber entirely in the binary entrypoint.
pub fn init(default_level: &str) {
    let ansi = colors_enabled();
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            ts_fmt::layer()
                .with_ansi(ansi)
                .event_format(PrettyFormat)
                .with_writer(std::io::stdout),
        )
        .init();
}

/// A compact, level-colored [`FormatEvent`] with visible span context.
///
/// Whether colors are emitted follows [`Writer::has_ansi_escapes`], which the
/// installed `fmt` layer derives from its own `with_ansi` setting.
#[derive(Clone, Copy, Default)]
pub struct PrettyFormat;

impl<S, N> FormatEvent<S, N> for PrettyFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let color = writer.has_ansi_escapes();
        let dim = ansi::Style::new().dimmed();

        // Timestamp (uncolored)
        SystemTime.format_time(&mut writer)?;

        // Level, fixed width so columns line up
        let style = level_style(meta.level());
        if color {
            write!(
                writer,
                " {}{:<5}{} ",
                style.prefix(),
                meta.level(),
                style.suffix()
            )?;
        } else {
            write!(writer, " {:<5} ", meta.level())?;
        }

        // Span scope with fields: `handle{service=ssh}:`
        let mut in_scope = false;
        ctx.visit_spans(|span| {
            if color {
                write!(writer, "{}", ansi::Color::Cyan.prefix())?;
            }
            write!(writer, "{}", span.metadata().name())?;
            if let Some(fields) = span.extensions().get::<ts_fmt::FormattedFields<N>>() {
                write!(writer, "{{{}}}", fields)?;
            }
            if color {
                write!(writer, "{}", ansi::Color::Cyan.suffix())?;
            }
            write!(writer, ":")?;
            in_scope = true;
            Ok::<(), fmt::Error>(())
        })?;
        if in_scope {
            write!(writer, " ")?;
        }

        // Message and event-level fields
        ctx.format_fields(writer.by_ref(), event)?;

        // Target at verbose levels only (`Level` orders ERROR < .. < TRACE)
        if *meta.level() >= Level::DEBUG {
            if color {
                write!(writer, " {}{}{}", dim.prefix(), meta.target(), dim.suffix())?;
            } else {
                write!(writer, " {}", meta.target())?;
            }
        }

        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::fmt::Debug;
    use std::sync::{Arc, Mutex};
    use tracing::instrument;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Debug for SharedBuf {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("SharedBuf")
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuf {
        type Writer = SharedBufGuard;

        fn make_writer(&'a self) -> Self::Writer {
            SharedBufGuard(self.0.clone())
        }
    }

    struct SharedBufGuard(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBufGuard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[instrument(fields(service = "ssh"))]
    fn do_work() {
        tracing::info!("hello {}", "world");
        tracing::debug!("detail");
    }

    fn capture(ansi: bool, max_level: &str) -> String {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new(max_level))
            .with(
                ts_fmt::layer()
                    .with_ansi(ansi)
                    .event_format(PrettyFormat)
                    .with_writer(buf.clone()),
            );
        tracing::subscriber::with_default(subscriber, do_work);
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn plain_output_has_level_span_and_message() {
        let out = capture(false, "info");
        let line = out.lines().next().unwrap();
        assert!(line.contains(" INFO "), "level: {line}");
        assert!(line.contains("do_work{service="), "span context: {line}");
        assert!(line.ends_with("hello world"), "message: {line}");
        // target hidden at info level
        assert!(!line.contains("molehill_rathole"), "target leaked: {line}");
    }

    #[test]
    fn verbose_output_includes_target() {
        let out = capture(false, "debug");
        assert!(out.contains(" DEBUG "), "{out}");
        assert!(out.contains("molehill_rathole::logging"), "{out}");
    }

    #[test]
    fn colored_levels_use_distinct_colors() {
        let styles = [
            level_style(&Level::ERROR),
            level_style(&Level::WARN),
            level_style(&Level::INFO),
            level_style(&Level::DEBUG),
            level_style(&Level::TRACE),
        ];
        for (i, a) in styles.iter().enumerate() {
            for b in styles.iter().skip(i + 1) {
                assert_ne!(
                    a.prefix().to_string(),
                    b.prefix().to_string(),
                    "two levels share a color"
                );
            }
        }
        assert_ne!(
            level_style(&Level::ERROR).prefix().to_string(),
            ansi::Style::new().prefix().to_string()
        );
    }

    #[test]
    fn colored_output_contains_escape_codes() {
        let out = capture(true, "info");
        assert!(out.contains("\x1b["), "ansi expected: {out}");
        let plain = capture(false, "info");
        assert!(!plain.contains("\x1b["), "no ansi expected: {plain}");
    }
}
