//! The application's `log.txt`: a UTF-8 file emptied at every start. Each line is the local time as
//! `MM-DD HH:MM:SS.mmm`, the logger name padded to 12 columns, the level name padded to 8 columns and the
//! message. The logger name is the event's target.

use std::fmt;
use std::fs::File;
use std::path::Path;

use anyhow::Context;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Creates the folder if needed and opens `path` emptied.
pub fn open(path: &Path) -> anyhow::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    File::create(path).with_context(|| format!("opening {}", path.display()))
}

/// The level names written to the log; TRACE, which is below DEBUG, has no name and prints as `Level 5`.
fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARNING",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "Level 5",
    }
}

/// The time stamp, the padded logger name and the padded level name: the part before the message.
fn prefix(stamp: &str, name: &str, level: Level) -> String {
    format!("{stamp} {name:<12} {:<8} ", level_name(level))
}

/// The line format of [`prefix`] followed by the message.
pub struct LineFormat;

impl<S, N> FormatEvent<S, N> for LineFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        let meta = event.metadata();
        writer.write_str(&prefix(&openrigcue_daemon::log_time_stamp(), meta.target(), *meta.level()))?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Name (12 columns) and level (8 columns) are left-aligned and padded; longer names are kept whole.
    #[test]
    fn line_prefix() {
        assert_eq!(prefix("11-10 14:05:09.007", "App.main", Level::WARN), "11-10 14:05:09.007 App.main     WARNING  ");
        assert_eq!(
            prefix("01-01 00:00:00.000", "openrigcue_daemon::games", Level::INFO),
            "01-01 00:00:00.000 openrigcue_daemon::games INFO     "
        );
        assert_eq!(level_name(Level::TRACE), "Level 5");
    }

    /// `MM-DD HH:MM:SS.mmm`.
    #[test]
    fn stamp_format() {
        let stamp = openrigcue_daemon::log_time_stamp();
        let bytes = stamp.as_bytes();
        assert_eq!(stamp.len(), 18, "{stamp}");
        assert_eq!((bytes[2], bytes[5], bytes[8], bytes[11], bytes[14]), (b'-', b' ', b':', b':', b'.'));
    }

    /// An existing log is emptied; the folder is created.
    #[test]
    fn open_truncates() {
        let root = std::env::temp_dir().join(format!("openrigcue-log-test-{}", std::process::id()));
        let path = root.join("cfg/log.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old").unwrap();
        drop(open(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
