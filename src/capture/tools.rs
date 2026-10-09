//! The desktop's own screenshot programs, as a last resort: Spectacle,
//! grim, gnome-screenshot. Each writes a PNG of the whole desktop into our
//! private folder, read back and removed.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::codec;
use crate::image::Image;
use crate::util;

const TIMEOUT: Duration = Duration::from_secs(20);

/// The whole desktop from `tool`, with the pointer when `cursor`.
pub fn run(tool: &str, cursor: bool) -> Result<Image, String> {
    let exe = util::which(tool).ok_or_else(|| format!("{tool} is not installed"))?;
    let path = codec::private_temp("png")?;
    let p = path.to_string_lossy().into_owned();
    let mut args: Vec<&str> = match tool {
        "spectacle" => vec!["--background", "--nonotify", "--fullscreen"],
        "grim" => vec![],
        "gnome-screenshot" => vec![],
        other => return Err(format!("unknown screenshot program {other}")),
    };
    if cursor {
        args.push(match tool {
            "spectacle" => "--pointer",
            "grim" => "-c",
            _ => "-p",
        });
    }
    match tool {
        "spectacle" => args.extend(["--output", &p]),
        "gnome-screenshot" => args.extend(["-f", &p]),
        _ => args.push(&p),
    }
    let result = run_waiting(Command::new(&exe).args(&args)).and_then(|()| codec::decode_file(&path));
    let _ = std::fs::remove_file(&path);
    result.map_err(|e| format!("{tool}: {e}"))
}

/// Run `cmd` to the end (at most `TIMEOUT`), its last line of errors as the failure.
fn run_waiting(cmd: &mut Command) -> Result<(), String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.by_ref().take(64 << 10).read_to_string(&mut err);
                }
                let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                return Err(if last.is_empty() { format!("failed ({status})") } else { util::displayable(&last) });
            }
            Ok(None) if started.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("took too long".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e.to_string()),
        }
    }
}
