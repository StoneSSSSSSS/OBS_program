//! The recording clock: the performance counter in 100 ns units, the same
//! timebase WASAPI uses for its capture timestamps.

use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

fn freq() -> i64 {
    static F: OnceLock<i64> = OnceLock::new();
    *F.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    })
}

/// Now, in 100 ns units since boot.
pub fn now_hns() -> i64 {
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    (c as i128 * 10_000_000 / freq() as i128) as i64
}

/// Map between the performance counter and wall-clock (Unix ms).
#[derive(Debug, Clone, Copy)]
pub struct Epoch {
    pub hns: i64,
    pub unix_ms: i64,
}

impl Epoch {
    pub fn now() -> Epoch {
        let unix_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        Epoch { hns: now_hns(), unix_ms }
    }
    pub fn to_unix_ms(&self, hns: i64) -> i64 {
        self.unix_ms + (hns - self.hns) / 10_000
    }
}
