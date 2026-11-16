//! MMCSS (Multimedia Class Scheduler Service) — promotes audio threads to
//! "Pro Audio" priority. Without this, ordinary thread scheduling produces
//! sporadic 2-10 ms stalls under CPU load and audible glitches.

#![cfg(windows)]

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

pub struct ProAudio(HANDLE);

impl ProAudio {
    pub fn set_for_current_thread() -> Option<Self> {
        unsafe {
            let mut task_index: u32 = 0;
            match AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) {
                Ok(h) if !h.is_invalid() => Some(Self(h)),
                _ => {
                    tracing::warn!(
                        "AvSetMmThreadCharacteristicsW failed; running at normal priority"
                    );
                    None
                }
            }
        }
    }
}

impl Drop for ProAudio {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

/// Keep a windowless tray process from being treated as background work.
///
/// While a game is in the foreground, Windows 11 puts a process with no
/// visible window into "efficiency mode" (EcoQoS: low clocks, E-cores) and
/// ignores its timer-resolution requests. The audio threads then miss their
/// deadlines exactly when the CPU is busiest, which other apps hear as
/// dropouts. Raise the process above normal (not realtime: that can starve
/// the system) and opt out of both throttles. Best effort; failures only log.
pub fn boost_process() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
        HIGH_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    };
    unsafe {
        let me = GetCurrentProcess();
        if let Err(e) = SetPriorityClass(me, HIGH_PRIORITY_CLASS) {
            tracing::warn!(error = %e, "could not raise process priority");
        }
        // StateMask 0 with the bit in ControlMask = "never throttle this".
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        if let Err(e) = SetProcessInformation(
            me,
            ProcessPowerThrottling,
            &state as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        ) {
            tracing::warn!(error = %e, "could not opt out of power throttling");
        }
    }
}
