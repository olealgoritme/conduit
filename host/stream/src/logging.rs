//! Minimal logger: `level message` lines on stderr, level from CONDUIT_STREAM_LOG
//! (error, warn, info, debug; default info). Timestamps are monotonic seconds,
//! which is what lines up with the backend's and viewer's logs.

use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

struct Logger {
    start: Instant,
    level: log::LevelFilter,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let t = self.start.elapsed().as_secs_f64();
        let _ = writeln!(
            std::io::stderr().lock(),
            "[{t:9.3}] {:<5} {}",
            r.level().as_str().to_ascii_lowercase(),
            r.args()
        );
    }
    fn flush(&self) {}
}

pub fn init() {
    let level = match std::env::var("CONDUIT_STREAM_LOG").as_deref() {
        Ok("error") => log::LevelFilter::Error,
        Ok("warn") => log::LevelFilter::Warn,
        Ok("debug") => log::LevelFilter::Debug,
        Ok("trace") => log::LevelFilter::Trace,
        _ => log::LevelFilter::Info,
    };
    let l = LOGGER.get_or_init(|| Logger {
        start: Instant::now(),
        level,
    });
    if log::set_logger(l).is_ok() {
        log::set_max_level(level);
    }
}
