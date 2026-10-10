//! The clipboard's pieces common to Wayland and X11: what's offered (which
//! may still be on its way), serving it to a program pasting, and reading
//! what another program offers, all within limits.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The text types offered and asked for, best first.
pub const TEXT_MIMES: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];
/// The most text taken from the clipboard.
pub const MAX_PASTE: u64 = 1 << 20;

/// How long a paste waits for data still being made.
pub const WAIT_FOR_DATA: Duration = Duration::from_secs(30);
/// How long a program pasting may go without reading what it asked for.
pub const STALL: Duration = Duration::from_secs(10);
/// Pastes being served now (each by a thread), and the most at once.
static SENDING: AtomicUsize = AtomicUsize::new(0);
const MAX_SENDING: usize = 16;

/// What's offered, which may still be on its way (a picture being encoded
/// after the overlay that had the keyboard has gone).
#[derive(Default)]
pub struct Slot {
    state: Mutex<SlotState>,
    changed: Condvar,
}

#[derive(Default)]
enum SlotState {
    #[default]
    Coming,
    Ready(Arc<Vec<u8>>),
    Gone,
}

impl Slot {
    pub fn ready(data: Arc<Vec<u8>>) -> Arc<Slot> {
        Arc::new(Slot { state: Mutex::new(SlotState::Ready(data)), changed: Condvar::new() })
    }

    pub fn fill(&self, data: Arc<Vec<u8>>) {
        self.set(SlotState::Ready(data));
    }

    /// Nothing will come (whoever is waiting gets nothing).
    pub fn abandon(&self) {
        self.set(SlotState::Gone);
    }

    fn set(&self, state: SlotState) {
        if let Ok(mut s) = self.state.lock() {
            *s = state;
            self.changed.notify_all();
        }
    }

    /// The data, once it's there.
    pub fn wait(&self) -> Option<Arc<Vec<u8>>> {
        let guard = self.state.lock().ok()?;
        let (guard, _) = self.changed.wait_timeout_while(guard, WAIT_FOR_DATA, |s| matches!(s, SlotState::Coming)).ok()?;
        match &*guard {
            SlotState::Ready(d) => Some(d.clone()),
            _ => None,
        }
    }

    /// Is the data still being made?
    pub fn coming(&self) -> bool {
        self.state.lock().is_ok_and(|s| matches!(*s, SlotState::Coming))
    }

    /// The data, if it's there now.
    pub fn get(&self) -> Option<Arc<Vec<u8>>> {
        match &*self.state.lock().ok()? {
            SlotState::Ready(d) => Some(d.clone()),
            _ => None,
        }
    }
}

/// Serve `slot` to a program pasting through `fd` (a pipe), once its data
/// is there: big pictures fill the pipe, and may still be being made, so a
/// thread of its own writes it. A program that asks again and again
/// without reading gets nothing past the first few.
pub fn serve(slot: Arc<Slot>, fd: OwnedFd) {
    if SENDING.fetch_add(1, Ordering::AcqRel) >= MAX_SENDING {
        SENDING.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    let sent = std::thread::Builder::new().name("flatshot-clipboard".into()).spawn(move || {
        if let Some(data) = slot.wait() {
            let _ = write_for(fd, &data, STALL);
        }
        SENDING.fetch_sub(1, Ordering::AcqRel);
    });
    if sent.is_err() {
        SENDING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Write all of `data` to `fd` (a pipe to a program pasting), giving up
/// when it takes nothing for `stall`: it may never read.
pub fn write_for(fd: OwnedFd, data: &[u8], stall: Duration) -> std::io::Result<()> {
    use rustix::event::{PollFd, PollFlags};
    use rustix::io::Errno;
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    let mut at = 0;
    while at < data.len() {
        match rustix::io::write(&fd, &data[at..]) {
            Ok(n) => at += n,
            Err(Errno::AGAIN) => {
                let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
                let ts = rustix::time::Timespec { tv_sec: stall.as_secs() as i64, tv_nsec: stall.subsec_nanos() as i64 };
                match rustix::event::poll(&mut fds, Some(&ts)) {
                    Ok(0) => return Err(std::io::ErrorKind::TimedOut.into()),
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Read `from` to its end (at most MAX_PASTE), giving up after `limit`:
/// whoever owns the clipboard may never write.
pub fn read_for(from: std::fs::File, into: &mut Vec<u8>, limit: Duration) -> std::io::Result<()> {
    use rustix::event::{PollFd, PollFlags};
    let deadline = std::time::Instant::now() + limit;
    let mut buf = [0u8; 16384];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let mut fds = [PollFd::new(&from, PollFlags::IN)];
        let ts = rustix::time::Timespec { tv_sec: left.as_secs() as i64, tv_nsec: left.subsec_nanos() as i64 };
        if rustix::event::poll(&mut fds, Some(&ts))? == 0 {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match (&from).read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if into.len() as u64 + n as u64 > MAX_PASTE {
                    return Err(std::io::ErrorKind::FileTooLarge.into());
                }
                into.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}
