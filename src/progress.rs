//! Shared progress counter: converters add the work they have done, and the
//! command-line tool draws it. Cheap enough to update from hot loops.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Progress {
    done: AtomicU64,
    total: AtomicU64,
    phase: Mutex<String>,
    paused: AtomicBool,
    /// When the current pause began, and the total of earlier pauses.
    pause: Mutex<(Option<Instant>, Duration)>,
}

impl Progress {
    /// Begin a new phase of `total` units of work (0 = unknown amount).
    pub fn start(&self, phase: impl Into<String>, total: u64) {
        *self.phase.lock().unwrap() = phase.into();
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    #[inline]
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    /// Current phase, work done, total work.
    pub fn snapshot(&self) -> (String, u64, u64) {
        let phase = self.phase.lock().unwrap().clone();
        (phase, self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }

    /// While paused (e.g. during a question on the console), nothing is drawn and
    /// the time does not count (see [`Progress::paused_time`]).
    pub fn set_paused(&self, paused: bool) {
        let mut p = self.pause.lock().unwrap();
        match (paused, p.0) {
            (true, None) => p.0 = Some(Instant::now()),
            (false, Some(since)) => {
                p.1 += since.elapsed();
                p.0 = None;
            }
            _ => {}
        }
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// Total time spent paused so far, including a pause in progress.
    pub fn paused_time(&self) -> Duration {
        let p = self.pause.lock().unwrap();
        p.1 + p.0.map_or(Duration::ZERO, |since| since.elapsed())
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}
