//! Diagnostics through the `log` facade: on stderr, and every `info` and
//! above in the persistent log file, each line with the local time.
//! Stdout is for command output only.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use jiff::Zoned;
use log::{Level, LevelFilter, Log, Metadata, Record};

struct Logger {
    stderr: LevelFilter,
    file: Option<PathBuf>,
}

/// Target of what a command reports on stdout: the file records it, stderr
/// would only repeat it.
pub(crate) const REPORT: &str = "hangar::report";

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Set once a [`heading`] is on stderr: the lines after it are indented,
/// until [`end`].
static UNDER_HEADING: AtomicBool = AtomicBool::new(false);

/// A line naming what a command is about to do, unindented on stderr; the
/// progress lines after it are indented under it.
pub(crate) fn heading(text: &str) {
    if HEADINGS.load(Ordering::Relaxed) {
        log::info!(target: HEADING, "{text}");
    }
}

/// Off for `--json`: machine-read stderr ends with the JSON error alone.
static HEADINGS: AtomicBool = AtomicBool::new(true);

pub(crate) fn without_headings() {
    HEADINGS.store(false, Ordering::Relaxed);
}

/// Closes a [`heading`] with an empty line, so a result follows apart.
pub(crate) fn end() {
    if UNDER_HEADING.swap(false, Ordering::Relaxed) {
        let _ = writeln!(io::stderr().lock());
    }
}

/// Target of a [`heading`].
const HEADING: &str = "hangar::heading";

/// `file` is created on the first line written to it.
pub(crate) fn init(level: LevelFilter, file: Option<PathBuf>) {
    let logger = LOGGER.get_or_init(|| Logger::new(level, file));
    // Only fails when a logger is already set, which can't happen here.
    let _ = log::set_logger(logger);
    log::set_max_level(logger.max_level());
}

impl Logger {
    fn new(stderr: LevelFilter, file: Option<PathBuf>) -> Self {
        Self { stderr, file }
    }

    /// The file keeps `info` even under `-q`: it's the record of what
    /// happened.
    fn file_level(&self) -> LevelFilter {
        self.stderr.max(LevelFilter::Info)
    }

    fn max_level(&self) -> LevelFilter {
        if self.file.is_some() {
            self.file_level()
        } else {
            self.stderr
        }
    }

    fn write_stderr(&self, record: &Record) {
        if record.level() > self.stderr || record.target() == REPORT {
            return;
        }
        let message = record.args().to_string();
        let mut stderr = io::stderr().lock();
        if record.target() == HEADING {
            UNDER_HEADING.store(true, Ordering::Relaxed);
            let _ = writeln!(stderr, "{message}");
            return;
        }
        let line = line(&Zoned::now(), record.level(), &message);
        // clap's indent, so progress lines up with `--help`.
        let indent = if UNDER_HEADING.load(Ordering::Relaxed) {
            "  "
        } else {
            ""
        };
        let _ = writeln!(stderr, "{indent}{line}");
    }

    fn write_file(&self, record: &Record) {
        let Some(path) = &self.file else { return };
        if record.level() > self.file_level() {
            return;
        }
        let line = file_line(
            &Zoned::now(),
            record.level(),
            &record.args().to_string(),
        );
        // A log that can't be written must not break the command.
        let _ = append(path, &line);
    }
}

fn append(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    // Trap: never follow a symlink planted at the log's path.
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(format!("{line}\n").as_bytes())
}

/// One line per event, even when the message spans several, stamped with
/// the date, the local time and its offset from UTC.
fn file_line(now: &Zoned, level: Level, message: &str) -> String {
    let level = level.as_str().to_lowercase();
    format!(
        "{} {level} {}",
        now.strftime("%Y-%m-%dT%H:%M:%S%:z"),
        message.replace('\n', " ")
    )
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

/// The local time and the level, in columns: plain, like clap's output.
fn line(now: &Zoned, level: Level, message: &str) -> String {
    let level = level.as_str().to_lowercase();
    format!("{}  {level:<5}  {message}", now.strftime("%H:%M:%S"))
}

/// Trap: ureq and rustls log too, and their lines may name a request's
/// headers; only hangar's own reach stderr.
fn ours(metadata: &Metadata) -> bool {
    let target = metadata.target();
    target == "hangar" || target.starts_with("hangar::")
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.max_level() && ours(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.write_stderr(record);
        self.write_file(record);
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::{Logger, REPORT, file_line, level, line, ours};
    use log::{Level, LevelFilter, Metadata, Record};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use jiff::Timestamp;
    use jiff::tz::{TimeZone, offset};

    /// `secs` after the epoch, in a zone `hours` ahead of UTC.
    fn at(secs: i64, hours: i8) -> jiff::Zoned {
        Timestamp::from_second(secs)
            .unwrap()
            .to_zoned(TimeZone::fixed(offset(hours)))
    }

    #[test]
    fn file_lines_carry_the_local_time_and_its_offset_on_one_line() {
        assert_eq!(
            file_line(&at(0, 0), Level::Info, "x"),
            "1970-01-01T00:00:00+00:00 info x"
        );
        assert_eq!(
            file_line(&at(1_793_880_000, 1), Level::Warn, "m"),
            "2026-11-05T13:00:00+01:00 warn m"
        );
        assert_eq!(
            file_line(&at(0, -5), Level::Error, "a\nb"),
            "1969-12-31T19:00:00-05:00 error a b"
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("hangar-logger-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("state/hangar.log")
    }

    fn write(logger: &Logger, level: Level, target: &str, message: &str) {
        logger.write_file(
            &Record::builder()
                .level(level)
                .target(target)
                .args(format_args!("{message}"))
                .build(),
        );
    }

    #[test]
    fn quiet_still_writes_info_to_file() {
        let path = scratch("quiet");
        let logger = Logger::new(level(true, 0, None), Some(path.clone()));
        assert!(!path.exists());
        write(&logger, Level::Info, "hangar", "default: vm");
        write(&logger, Level::Debug, "hangar", "run: msb");
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.ends_with(" info default: vm\n"), "{text}");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn reports_reach_the_file() {
        let path = scratch("report");
        let logger = Logger::new(LevelFilter::Info, Some(path.clone()));
        write(&logger, Level::Info, REPORT, "default: created");
        assert!(fs::read_to_string(&path).unwrap().contains("created"));
    }

    #[test]
    fn a_planted_symlink_is_not_followed_and_a_loose_file_is_tightened() {
        let path = scratch("symlink");
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        let target = dir.join("victim");
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let logger = Logger::new(LevelFilter::Info, Some(path.clone()));
        write(&logger, Level::Info, "hangar", "x");
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");

        fs::remove_file(&path).unwrap();
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write(&logger, Level::Info, "hangar", "x");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = fs::metadata(dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o755, "pre-existing dir is left alone");
    }

    #[test]
    fn verbose_adds_debug_to_the_file_and_appends() {
        let path = scratch("verbose");
        let logger = Logger::new(LevelFilter::Debug, Some(path.clone()));
        write(&logger, Level::Debug, "hangar::process", "run: msb");
        write(&logger, Level::Trace, "hangar", "too much");
        write(&logger, Level::Warn, "hangar", "again");
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains(" debug run: msb\n"), "{text}");
    }

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
    fn terminal_lines_carry_the_local_time_and_a_plain_prefix() {
        let now = at(1_793_880_000, 1);
        assert_eq!(line(&now, Level::Info, "up"), "13:00:00  info   up");
        assert_eq!(line(&now, Level::Warn, "x"), "13:00:00  warn   x");
        assert_eq!(line(&now, Level::Error, "x"), "13:00:00  error  x");
        assert_eq!(line(&now, Level::Debug, "x"), "13:00:00  debug  x");
        assert_eq!(line(&now, Level::Trace, "x"), "13:00:00  trace  x");
    }
}
