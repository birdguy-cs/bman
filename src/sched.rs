//! Thread priorities, so playback wins the CPU on a Pi Zero. Background
//! work (library rescans, cover art, Bluetooth / Wi-Fi / battery polling,
//! update checks) runs at the lowest priority, which any user may set. The
//! audio threads ask for a real-time slot; that needs root or an `rtprio`
//! limit (e.g. the `audio` group with a limits.d rule), so it's best-effort
//! and silently skipped when not allowed.

/// The calling thread's id (Linux threads each have their own nice value).
fn tid() -> libc::id_t {
    unsafe { libc::syscall(libc::SYS_gettid) as libc::id_t }
}

/// Make the calling thread yield to everything else (nice 19).
pub fn background() {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, tid(), 19);
    }
}

/// Give the calling thread priority over normal work, if allowed: a low
/// real-time (SCHED_FIFO) priority, else a lower nice value. Returns
/// whether either worked.
pub fn audio() -> bool {
    let param = libc::sched_param { sched_priority: 10 };
    if unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) } == 0 {
        return true;
    }
    unsafe { libc::setpriority(libc::PRIO_PROCESS, tid(), -10) == 0 }
}
