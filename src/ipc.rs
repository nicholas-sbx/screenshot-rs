//! Single-instance plumbing: the tray app listens on a Unix socket and
//! other invocations (`flatshot`, `flatshot --full`, ...) hand it commands.
//!
//! The socket lives in a folder only this user can enter
//! (`$XDG_RUNTIME_DIR/flatshot`), and both ends check that the other runs
//! as the same user (SO_PEERCRED), so no other account can talk to the tray
//! or pose as it. A lock file makes sure only one tray app serves.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

/// The longest command line accepted.
const MAX_LINE: usize = 8192;

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Our private runtime folder: created if missing, and checked to be ours
/// and closed to others if it exists.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute());
    let dir = match base {
        Some(b) if secure_dir(&b).is_ok() => b.join("flatshot"),
        _ => std::env::temp_dir().join(format!("flatshot-{}.run", uid())),
    };
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    secure_dir(&dir)?;
    Ok(dir)
}

/// A real folder (not a link), owned by us, that nobody else can use.
fn secure_dir(dir: &std::path::Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::other(format!("{} is not a folder", dir.display())));
    }
    if meta.uid() != uid() {
        return Err(io::Error::other(format!("{} belongs to someone else", dir.display())));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!("{} is open to other users", dir.display())));
    }
    Ok(())
}

pub fn socket_path() -> io::Result<PathBuf> {
    Ok(runtime_dir()?.join("ipc"))
}

/// Is the other end of `stream` this same user?
fn same_user(stream: &UnixStream) -> bool {
    match rustix::net::sockopt::socket_peercred(stream.as_fd()) {
        Ok(cred) => cred.uid.as_raw() == uid(),
        Err(_) => false,
    }
}

fn connect(path: &std::path::Path, timeout: Duration) -> Option<UnixStream> {
    let stream = UnixStream::connect(path).ok()?;
    if !same_user(&stream) {
        crate::warn!("ignoring {}: it isn't served by this user", path.display());
        return None;
    }
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    Some(stream)
}

fn exchange(path: &std::path::Path, command: &str, timeout: Duration) -> Option<String> {
    let mut stream = connect(path, timeout)?;
    let mut line = command.replace(['\n', '\r'], " ");
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    let mut reply = Vec::new();
    let mut reader = BufReader::new((&stream).take(MAX_LINE as u64 * 8));
    reader.read_until(b'\n', &mut reply).ok()?;
    let text = String::from_utf8_lossy(&reply).trim().to_string();
    text.strip_prefix("ok").map(|rest| rest.trim().to_string())
}

/// Deliver a command to the running tray app. False if none answers.
pub fn send(command: &str) -> bool {
    send_timeout(command, Duration::from_millis(1500))
}

pub fn send_timeout(command: &str, timeout: Duration) -> bool {
    match socket_path() {
        Ok(p) => exchange(&p, command, timeout).is_some(),
        Err(_) => false,
    }
}

/// Send `command` and return the reply after "ok" (None if no tray app answers).
pub fn query(command: &str, timeout: Duration) -> Option<String> {
    exchange(&socket_path().ok()?, command, timeout)
}

/// The Python Flatshot's tray socket (QLocalServer in the temp folder).
pub fn legacy_socket() -> PathBuf {
    std::env::temp_dir().join(format!("flatshot-{}", uid()))
}

/// Ask a running Python Flatshot tray app (from before the Rust version)
/// to quit, so this one takes over its shortcuts. True if one was there.
pub fn quit_legacy_tray() -> bool {
    let path = legacy_socket();
    if exchange(&path, "status", Duration::from_millis(500)).is_none() {
        return false;
    }
    let _ = exchange(&path, "quit", Duration::from_millis(1500));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && exchange(&path, "ping", Duration::from_millis(300)).is_some() {
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

pub struct Server {
    pub listener: UnixListener,
    _lock: File,
}

impl Server {
    /// Start serving, unless another tray app already does (Ok(None)).
    pub fn bind() -> io::Result<Option<Server>> {
        let dir = runtime_dir()?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("tray.lock"))?;
        if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
            return Ok(None); // another tray app has it
        }
        let path = dir.join("ipc");
        // Ours from a crash (the lock says nobody else serves): replace it.
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Some(Server { listener, _lock: lock }))
    }

    /// Take every waiting connection and read its command. `status` is
    /// answered with `status()`; anything else with "ok", then returned.
    pub fn accept(&self, status: &dyn Fn() -> String) -> Vec<String> {
        let mut commands = Vec::new();
        while let Ok((stream, _)) = self.listener.accept() {
            if let Some(cmd) = Self::serve(stream, status) {
                commands.push(cmd);
            }
        }
        commands
    }

    fn serve(mut stream: UnixStream, status: &dyn Fn() -> String) -> Option<String> {
        if !same_user(&stream) {
            return None;
        }
        stream.set_nonblocking(false).ok()?;
        // A client writes its line at once; don't let a slow one hold us up.
        stream.set_read_timeout(Some(Duration::from_millis(250))).ok()?;
        stream.set_write_timeout(Some(Duration::from_millis(250))).ok()?;
        let mut line = Vec::new();
        let mut reader = BufReader::new((&stream).take(MAX_LINE as u64));
        reader.read_until(b'\n', &mut line).ok()?;
        let cmd = String::from_utf8_lossy(&line).trim().to_string();
        if cmd == "status" {
            let _ = stream.write_all(format!("ok {}\n", status()).as_bytes());
            return None;
        }
        let _ = stream.write_all(b"ok\n");
        (!cmd.is_empty() && cmd != "ping").then_some(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let tmp = std::env::temp_dir().join(format!("flatshot-ipc-test-{}", crate::util::random_hex(4)));
        fs::DirBuilder::new().mode(0o700).create(&tmp).unwrap();
        // SAFETY: tests in this module run one at a time in this process.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &tmp) };
        let server = Server::bind().unwrap().expect("first server");
        assert!(Server::bind().unwrap().is_none(), "a second one must not serve");
        let t = std::thread::spawn(|| (send("screens 0 pin"), query("status", Duration::from_secs(2))));
        let mut got = Vec::new();
        let start = std::time::Instant::now();
        while got.is_empty() && start.elapsed() < Duration::from_secs(5) {
            got.extend(server.accept(&|| "{\"version\":\"x\"}".into()));
            std::thread::sleep(Duration::from_millis(5));
        }
        // (the status query comes after the command)
        let start = std::time::Instant::now();
        while !t.is_finished() && start.elapsed() < Duration::from_secs(5) {
            server.accept(&|| "{\"version\":\"x\"}".into());
            std::thread::sleep(Duration::from_millis(5));
        }
        let (sent, status) = t.join().unwrap();
        assert!(sent);
        assert_eq!(got, vec!["screens 0 pin".to_string()]);
        assert_eq!(status.as_deref(), Some("{\"version\":\"x\"}"));
        let _ = fs::remove_dir_all(&tmp);
    }
}
