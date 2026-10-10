//! Ctrl+C, logging out and `kill`: SIGINT, SIGTERM and SIGHUP come to the
//! event loop as events, so a recording in progress is stopped and saved
//! rather than left running with no one to finish it.
//!
//! A handler writes each signal's number to a pipe the event loop reads
//! (the self-pipe trick). Unlike blocking the signals, this leaves nothing
//! for the programs Flatshot starts to inherit: exec puts caught signals
//! back to their defaults.

use std::os::fd::{AsFd, BorrowedFd, IntoRawFd, OwnedFd};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

const SIGNALS: [i32; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// The pipe's write end, for the handler (kept for the program's life).
static WRITE_END: AtomicI32 = AtomicI32::new(-1);
static READ_END: OnceLock<Option<OwnedFd>> = OnceLock::new();

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = WRITE_END.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        // SAFETY: write(2) is async-signal-safe; with the pipe full, the
        // signal is dropped (others are waiting to be read anyway).
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

/// From now on SIGINT, SIGTERM and SIGHUP come as events on [`fd`] (if
/// that can't be set up, they keep their default, which ends the program).
pub fn take() {
    READ_END.get_or_init(|| {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK).ok()?;
        WRITE_END.store(write.into_raw_fd(), Ordering::Relaxed);
        for s in SIGNALS {
            // SAFETY: the handler does only async-signal-safe things.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as usize;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(s, &action, std::ptr::null_mut());
            }
        }
        Some(read)
    });
}

/// Where the signals come, once taken.
pub fn fd() -> Option<BorrowedFd<'static>> {
    READ_END.get().and_then(|f| f.as_ref()).map(|f| f.as_fd())
}

/// The signals waiting on `fd`, by number.
pub fn read(fd: BorrowedFd<'_>) -> Vec<i32> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16];
    while let Ok(n) = rustix::io::read(fd, &mut buf) {
        if n == 0 {
            break;
        }
        out.extend(buf[..n].iter().map(|b| *b as i32));
    }
    out
}

/// Wait up to `wait` for one of the signals (a capture's delay that Ctrl+C
/// can cut short). The signal, if one came.
pub fn wait(wait: std::time::Duration) -> Option<i32> {
    let Some(fd) = fd() else {
        std::thread::sleep(wait);
        return None;
    };
    let deadline = std::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
        let ts = rustix::event::Timespec { tv_sec: left.as_secs() as i64, tv_nsec: left.subsec_nanos() as i64 };
        if let Ok(n) = rustix::event::poll(&mut fds, Some(&ts))
            && n > 0
            && let Some(s) = read(fd).into_iter().next()
        {
            return Some(s);
        }
    }
}

/// The exit code for dying of `signal`, as a shell reports it.
pub fn exit_code(signal: i32) -> i32 {
    128 + signal
}
