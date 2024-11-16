//! Global logger behaviour. The logger and the maximum level are process
//! wide, so everything that touches them runs inside one test function.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::log::{self, CaptureLogger, Filter, Level, LevelFilter, LogFormat, StreamLogger};
use belay_common::sync::{lock, spawn_periodic, ThreadPool};
use belay_common::{info, trace, warn, ManualClock, Shutdown, Timestamp};

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        lock(&self.0).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SharedBuf {
    fn text(&self) -> String {
        let bytes = lock(&self.0).clone();
        String::from_utf8(bytes).expect("log output is UTF-8")
    }
}

#[test]
fn global_logger_sees_every_thread_and_respects_levels() {
    // 1. Records from worker threads reach a globally installed logger.
    let capture = CaptureLogger::new();
    log::set_logger(Box::new(capture.clone()));
    log::set_max_level(Level::Debug);

    let pool = ThreadPool::new("it-pool", 2).expect("spawning pool");
    pool.execute(|| panic!("task failure on purpose"));
    pool.execute(|| info!("from the pool"; worker = true));
    pool.join();

    let shutdown = Shutdown::new();
    let stop = shutdown.clone();
    let mut calls = 0;
    let ticker = spawn_periodic("it-ticker", Duration::from_millis(1), shutdown, move |_| {
        calls += 1;
        if calls == 1 {
            panic!("periodic failure on purpose");
        }
        stop.trigger();
    })
    .expect("spawning ticker");
    ticker.join().expect("ticker thread exits cleanly");

    let panic_warning = capture
        .find(|r| r.message == "thread pool task panicked")
        .expect("pool panic is logged");
    assert_eq!(panic_warning.level, Level::Warn);
    assert_eq!(panic_warning.field("pool"), Some("it-pool"));
    assert_eq!(
        panic_warning.field("panic"),
        Some("task failure on purpose")
    );
    assert!(capture.contains("from the pool"));
    let periodic = capture
        .find(|r| r.message == "periodic task panicked")
        .expect("ticker panic is logged");
    assert_eq!(periodic.level, Level::Error);
    assert_eq!(periodic.field("thread"), Some("it-ticker"));

    // 2. The global maximum level gates records before the logger sees them.
    capture.clear();
    trace!("too verbose");
    log::set_max_level(LevelFilter::Off);
    warn!("disabled entirely");
    assert!(capture.is_empty());
    assert!(!log::enabled(Level::Error, "any"));

    // 3. A stream logger with a filter, JSON output and a fixed clock.
    let buf = SharedBuf::default();
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let filter: Filter = "warn,global_logging=debug".parse().expect("valid filter");
    log::set_max_level(filter.max_level());
    log::set_logger(Box::new(
        StreamLogger::new(buf.clone())
            .with_filter(filter)
            .with_format(LogFormat::Json)
            .with_clock(clock.shared()),
    ));
    info!("kept by the per-target directive"; n = 1);
    info!(target: "elsewhere", "dropped by the default level");
    warn!(target: "elsewhere", "kept"; reason = "warn passes");
    log::flush();

    assert_eq!(
        buf.text(),
        "{\"ts\":\"1970-01-01T00:00:00.000Z\",\"level\":\"info\",\"target\":\"global_logging\",\"msg\":\"kept by the per-target directive\",\"n\":1}\n\
         {\"ts\":\"1970-01-01T00:00:00.000Z\",\"level\":\"warn\",\"target\":\"elsewhere\",\"msg\":\"kept\",\"reason\":\"warn passes\"}\n"
    );
}
