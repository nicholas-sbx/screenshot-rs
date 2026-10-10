//! Small helpers used everywhere: logging, randomness, careful file reads,
//! starting other programs.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A line on stderr (the journal, for the tray app): "flatshot-rs: ...".
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        eprintln!("flatshot-rs: {}", format_args!($($arg)*))
    };
}

/// `n` random bytes as hex, from the kernel's generator.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        match rustix::rand::getrandom(&mut buf[filled..], rustix::rand::GetRandomFlags::empty()) {
            Ok(k) => filled += k,
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => {
                // Never expected on Linux; still, don't hand out zeros.
                let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                let mix = t.as_nanos() as u64 ^ (std::process::id() as u64).rotate_left(32);
                for (i, b) in buf[filled..].iter_mut().enumerate() {
                    *b = (mix >> ((i % 8) * 8)) as u8;
                }
                break;
            }
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read a regular file of at most `max` bytes. FIFOs, devices and the like
/// are refused (and opened without blocking, so they can't hang us).
pub fn read_limited(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    if meta.len() > max {
        return Err(std::io::Error::other(format!("larger than {} MiB", max >> 20)));
    }
    let mut out = Vec::with_capacity(meta.len() as usize);
    (&mut f).take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(std::io::Error::other("file grew while being read"));
    }
    Ok(out)
}

/// Open a regular file for reading without following a FIFO into a hang.
pub fn open_regular(path: &Path) -> std::io::Result<File> {
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(f)
}

/// Where `name` is on $PATH, if it's there and executable (`shutil.which`).
pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    for dir in path.as_bytes().split(|&b| b == b':') {
        if !dir.starts_with(b"/") {
            continue; // (an empty or relative entry means the current folder: never)
        }
        let p = Path::new(OsStr::from_bytes(dir)).join(name);
        if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

pub fn have(name: &str) -> bool {
    which(name).is_some()
}

fn is_executable(p: &Path) -> bool {
    p.is_file() && rustix::fs::access(p, rustix::fs::Access::EXEC_OK).is_ok()
}

/// `name` from $PATH as a command, run by its full path (so the current
/// folder is never searched) and inheriting nothing but stdin, stdout and
/// stderr. None when it isn't installed.
pub fn command(name: &str) -> Option<Command> {
    let mut cmd = Command::new(which(name)?);
    inherit_nothing(&mut cmd);
    Some(cmd)
}

/// Hand the program `cmd` starts no descriptor but stdin, stdout and
/// stderr. Flatshot's own are close-on-exec already; this catches those
/// made without it by libraries (zbus receives D-Bus's that way, the
/// screen-cast stream among them).
pub fn inherit_nothing(cmd: &mut Command) -> &mut Command {
    // SAFETY: close_range(2) is a plain system call, async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            cloexec_from(3);
            Ok(())
        })
    }
}

/// Mark descriptors `first` and up close-on-exec (between fork and exec:
/// async-signal-safe). Kernels before 5.11 can't; nothing is done there.
pub fn cloexec_from(first: u32) {
    // SAFETY: a system call taking only integers.
    unsafe { libc::syscall(libc::SYS_close_range, first, libc::c_uint::MAX, libc::CLOSE_RANGE_CLOEXEC) };
}

/// Start a program that carries on by itself: found on $PATH, in its own
/// session, no terminal, nothing inherited but the environment. A thread
/// waits for it, so it never lingers as a zombie of ours (and it goes on
/// should Flatshot end first).
pub fn spawn_detached(argv: &[&OsStr], env: &[(&str, &OsStr)]) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let Some(exe) = program.to_str().and_then(which) else {
        return false;
    };
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    // SAFETY: setsid(2) and close_range(2) are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            cloexec_from(3);
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let _ = std::thread::Builder::new().name("flatshot-child".into()).spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// [`spawn_detached`] for plain strings.
pub fn spawn(argv: &[&str]) -> bool {
    let argv: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
    spawn_detached(&argv, &[])
}

/// A `file://` URI for an absolute path, with the bytes that need it escaped.
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path in a `file://` URI, if it is one.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // "file://host/path": only the local host is ours.
    let path = if rest.starts_with('/') { rest } else { rest.strip_prefix("localhost")? };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(OsStr::from_bytes(&out)))
}

/// A text shortened to `max` characters with "…" at the end.
pub fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// Text from somewhere else (a window title, a QR code) made safe to show:
/// no control characters, and no invisible direction overrides that could
/// make it read differently from what it is.
pub fn displayable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => '\u{FFFD}',
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}' => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// A line on stdout: as it is for a program reading it, and with control
/// characters made harmless for a terminal (a QR code's text could
/// otherwise set its title, or write its clipboard).
pub fn print_line(text: &str) {
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("{}", displayable(text));
    } else {
        println!("{text}");
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris() {
        let p = Path::new("/tmp/a b/ü#.png");
        let uri = file_uri(p);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%BC%23.png");
        assert_eq!(uri_to_path(&uri).unwrap(), p);
        assert_eq!(uri_to_path("file://localhost/x").unwrap(), Path::new("/x"));
        assert!(uri_to_path("file://evil/x").is_none());
        assert!(uri_to_path("http://x").is_none());
    }

    #[test]
    fn random_is_hex() {
        let a = random_hex(8);
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, random_hex(8));
    }

    #[test]
    fn display_safety() {
        assert_eq!(displayable("a\u{202E}b\nc"), "a\u{FFFD}b c");
        assert_eq!(elide("abcdef", 4), "abc…");
    }
}
