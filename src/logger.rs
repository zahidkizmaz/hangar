//! Diagnostics on stderr through the `log` facade; stdout is for command
//! output only.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Stderr;

static LOGGER: Stderr = Stderr;
static COLOR: AtomicBool = AtomicBool::new(false);

pub(crate) fn init(level: LevelFilter, color: bool) {
    COLOR.store(color, Ordering::Relaxed);
    // Only fails when a logger is already set, which can't happen here.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}

/// `-q` and `-v`/`-vv` win over `HANGAR_LOG`; info by default.
pub(crate) fn level(
    quiet: bool,
    verbose: usize,
    env: Option<&str>,
) -> LevelFilter {
    match (quiet, verbose) {
        (true, _) => LevelFilter::Error,
        (false, 1) => LevelFilter::Debug,
        (false, 2..) => LevelFilter::Trace,
        (false, 0) => env
            .and_then(|value| value.parse().ok())
            .unwrap_or(LevelFilter::Info),
    }
}

fn line(level: Level, message: &str, color: bool) -> String {
    let (prefix, code) = match level {
        Level::Error => ("error:", "31"),
        Level::Warn => ("warning:", "33"),
        Level::Info => ("==>", "1"),
        Level::Debug => ("debug:", "2"),
        Level::Trace => ("trace:", "2"),
    };
    if color {
        format!("\x1b[{code}m{prefix}\x1b[0m {message}")
    } else {
        format!("{prefix} {message}")
    }
}

/// Trap: ureq and rustls log too, and their lines may name a request's
/// headers; only hangar's own reach stderr.
fn ours(metadata: &Metadata) -> bool {
    let target = metadata.target();
    target == "hangar" || target.starts_with("hangar::")
}

impl Log for Stderr {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level() && ours(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let color = COLOR.load(Ordering::Relaxed);
        let line = line(record.level(), &record.args().to_string(), color);
        let _ = writeln!(io::stderr().lock(), "{line}");
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::{level, line, ours};
    use log::{Level, LevelFilter, Metadata};

    #[test]
    fn flags_win_over_the_environment() {
        assert_eq!(level(false, 0, None), LevelFilter::Info);
        assert_eq!(level(false, 0, Some("debug")), LevelFilter::Debug);
        assert_eq!(level(false, 0, Some("nonsense")), LevelFilter::Info);
        assert_eq!(level(true, 2, Some("trace")), LevelFilter::Error);
        assert_eq!(level(false, 1, Some("error")), LevelFilter::Debug);
        assert_eq!(level(false, 3, None), LevelFilter::Trace);
    }

    #[test]
    fn only_hangar_logs() {
        let target = |target| ours(&Metadata::builder().target(target).build());
        assert!(target("hangar") && target("hangar::http"));
        assert!(
            !target("ureq::run") && !target("rustls") && !target("hangarx")
        );
    }

    #[test]
    fn lines_carry_a_level_prefix_and_color_only_on_request() {
        assert_eq!(line(Level::Info, "up", false), "==> up");
        assert_eq!(line(Level::Warn, "x", false), "warning: x");
        assert_eq!(line(Level::Error, "x", false), "error: x");
        assert_eq!(line(Level::Debug, "x", false), "debug: x");
        assert_eq!(line(Level::Trace, "x", false), "trace: x");
        assert_eq!(line(Level::Error, "x", true), "\x1b[31merror:\x1b[0m x");
    }
}
