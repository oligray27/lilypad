//! Only one tracker may run per user at a time. The desktop app and the Gaming Mode engine share
//! one data directory and session store, and Decky keeps the engine's host process alive in both
//! modes, so two trackers would otherwise record every session twice.
//!
//! The rule: whoever holds `tracker.lock` tracks. The desktop app has priority. When it starts,
//! it asks the holder to yield (by creating `tracker.yield`) and waits for the lock. The engine
//! polls for that request and exits, releasing the lock. Its in-progress session is left `active`
//! in the store, so the desktop app's startup recovery resumes it on the same record if the game
//! is still running: the crash-recovery path, used as a hand-over.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = "tracker.lock";
const YIELD_FILE: &str = "tracker.yield";

/// Held for as long as this process is the tracker. Dropping it releases the lock.
pub struct TrackerLock {
    _file: File,
    dir: PathBuf,
}

impl TrackerLock {
    /// Takes the lock if nobody holds it.
    pub fn try_acquire_in(dir: &Path) -> std::io::Result<Option<Self>> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK_FILE))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file, dir: dir.to_path_buf() })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }

    pub fn try_acquire() -> std::io::Result<Option<Self>> {
        Self::try_acquire_in(&crate::config::app_data_dir())
    }

    /// The desktop app's entry: take the lock, asking any other holder to yield and waiting up to
    /// `timeout` for it to do so. `Err` with the holder still running means it did not yield.
    pub fn take_over_in(dir: &Path, timeout: Duration) -> std::io::Result<Self> {
        if let Some(lock) = Self::try_acquire_in(dir)? {
            return Ok(lock);
        }
        log::info!("[LilyPad] another LilyPad tracker is running; asking it to hand over");
        std::fs::write(dir.join(YIELD_FILE), b"")?;
        let deadline = Instant::now() + timeout;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            if let Some(lock) = Self::try_acquire_in(dir)? {
                let _ = std::fs::remove_file(dir.join(YIELD_FILE));
                log::info!("[LilyPad] took over tracking");
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                let _ = std::fs::remove_file(dir.join(YIELD_FILE));
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the other LilyPad did not hand over tracking"));
            }
        }
    }

    pub fn take_over(timeout: Duration) -> std::io::Result<Self> {
        Self::take_over_in(&crate::config::app_data_dir(), timeout)
    }

    /// Whether another instance has asked this holder to step aside.
    pub fn yield_requested(&self) -> bool {
        self.dir.join(YIELD_FILE).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_holder_at_a_time_and_a_takeover_waits_for_the_holder_to_yield() {
        let dir = tempfile::tempdir().unwrap();
        let engine = TrackerLock::try_acquire_in(dir.path()).unwrap().expect("free lock");
        assert!(TrackerLock::try_acquire_in(dir.path()).unwrap().is_none(), "a second holder was allowed");
        assert!(!engine.yield_requested());

        // The engine, polling: yields as soon as it is asked.
        let path = dir.path().to_path_buf();
        let engine_thread = std::thread::spawn(move || {
            while !engine.yield_requested() {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(engine);
        });

        let desktop = TrackerLock::take_over_in(&path, Duration::from_secs(5)).expect("took over");
        engine_thread.join().unwrap();
        assert!(!desktop.yield_requested(), "the yield request must be cleared after the hand-over");
        assert!(TrackerLock::try_acquire_in(&path).unwrap().is_none());
    }

    #[test]
    fn a_holder_that_never_yields_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let _stubborn = TrackerLock::try_acquire_in(dir.path()).unwrap().unwrap();
        let err = TrackerLock::take_over_in(dir.path(), Duration::from_millis(600)).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(!dir.path().join(YIELD_FILE).exists());
    }
}
