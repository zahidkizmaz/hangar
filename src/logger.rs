//! Diagnostics through the `log` facade: on stderr, and every `info` and
//! above in the persistent, timestamped log file. Stdout is for command
//! output only.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Logger {
    stderr: LevelFilter,
    color: bool,
    file: Option<PathBuf>,
}

/// Target of what a command reports on stdout: the file records it, stderr
/// would only repeat it.
pub(crate) const REPORT: &str = "hangar::report";

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// `file` is created on the first line written to it.
pub(crate) fn init(level: LevelFilter, color: bool, file: Option<PathBuf>) {
    let logger = LOGGER.get_or_init(|| Logger::new(level, color, file));
    // Only fails when a logger is already set, which can't happen here.
    let _ = log::set_logger(logger);
    log::set_max_level(logger.max_level());
}

impl Logger {
    fn new(stderr: LevelFilter, color: bool, file: Option<PathBuf>) -> Self {
        Self {
            stderr,
            color,
            file,
        }
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
        let line = line(record.level(), &record.args().to_string(), self.color);
        let _ = writeln!(io::stderr().lock(), "{line}");
    }

    fn write_file(&self, record: &Record) {
        let Some(path) = &self.file else { return };
        if record.level() > self.file_level() {
            return;
        }
        let line = file_line(
            SystemTime::now(),
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

/// One line per event, even when the message spans several.
fn file_line(now: SystemTime, level: Level, message: &str) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let level = level.as_str().to_lowercase();
    format!("{} {level} {}", rfc3339(secs), message.replace('\n', " "))
}

/// UTC, from days since the epoch (Hinnant's civil-from-days).
fn rfc3339(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let day_of_year =
        day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
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
    use super::{Logger, REPORT, file_line, level, line, ours, rfc3339};
    use log::{Level, LevelFilter, Metadata, Record};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn file_line_is_utc_rfc3339() {
        assert_eq!(
            file_line(at(0), Level::Info, "x"),
            "1970-01-01T00:00:00Z info x"
        );
        assert_eq!(
            file_line(at(1_793_880_000), Level::Warn, "m"),
            "2026-11-05T12:00:00Z warn m"
        );
        assert_eq!(rfc3339(1_835_481_599), "2028-02-29T23:59:59Z");
        assert_eq!(rfc3339(1_835_481_600), "2028-03-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn file_lines_stay_on_one_line() {
        assert_eq!(
            file_line(at(0), Level::Error, "a\nb"),
            "1970-01-01T00:00:00Z error a b"
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
        let logger =
            Logger::new(level(true, 0, None), false, Some(path.clone()));
        assert!(!path.exists());
        write(&logger, Level::Info, "hangar", "default: vm");
        write(&logger, Level::Debug, "hangar", "run: msb");
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.ends_with("Z info default: vm\n"), "{text}");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn reports_reach_the_file() {
        let path = scratch("report");
        let logger = Logger::new(LevelFilter::Info, false, Some(path.clone()));
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
        let logger = Logger::new(LevelFilter::Info, false, Some(path.clone()));
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
        let logger = Logger::new(LevelFilter::Debug, false, Some(path.clone()));
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
    fn lines_carry_a_level_prefix_and_color_only_on_request() {
        assert_eq!(line(Level::Info, "up", false), "==> up");
        assert_eq!(line(Level::Warn, "x", false), "warning: x");
        assert_eq!(line(Level::Error, "x", false), "error: x");
        assert_eq!(line(Level::Debug, "x", false), "debug: x");
        assert_eq!(line(Level::Trace, "x", false), "trace: x");
        assert_eq!(line(Level::Error, "x", true), "\x1b[31merror:\x1b[0m x");
    }
}
