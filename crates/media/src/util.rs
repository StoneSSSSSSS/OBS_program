//! Process/thread scheduling helpers for smooth recording.

use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, SetProcessInformation, SetThreadPriority, ProcessPowerThrottling, PROCESS_POWER_THROTTLING_STATE, THREAD_PRIORITY_ABOVE_NORMAL};

/// Raise the current thread's priority a little (capture/encode threads).
pub fn boost_thread() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// Keep Windows from throttling the recorder when it has no visible window:
/// no "efficiency mode" CPU slowdown and precise (1 ms) timers.
pub fn disable_background_throttling() {
    unsafe {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: 0x1 | 0x4, // EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION
            StateMask: 0,
        };
        let _ = SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, &state as *const _ as *const _, std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32);
        let _ = windows::Win32::Media::timeBeginPeriod(1);
    }
}
