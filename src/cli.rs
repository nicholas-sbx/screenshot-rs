//! Command-line options, as the Python version's argparse took them.

use crate::geom::IRect;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Args {
    pub delay: f64,
    pub full: bool,
    pub monitor: bool,
    pub window: bool,
    pub last_region: bool,
    pub region: Option<String>,
    pub pin: bool,
    pub record: bool,
    pub ocr: bool,
    pub output: Option<String>,
    pub image: Option<String>,
    pub backend: Option<String>,
    pub no_copy: bool,
    pub no_save: bool,
    pub no_scan: bool,
    pub tray: bool,
    pub settings: bool,
    pub quit: bool,
    pub print_config: bool,
    pub diagnose: bool,
    pub verbose: bool,
    pub self_test: bool,
}

/// What a run of the program should do with its arguments.
pub enum Parsed {
    Run(Args),
    /// Print this to stdout and exit 0 (--help, --version).
    Exit(String),
    /// A usage error: print to stderr, exit 2.
    Error(String),
}

fn usage() -> String {
    format!(
        "usage: flatshot-rs [-h] [-d SEC] [-f | -m | -w | -l | -r WxH+X+Y] [--pin] [--record] [--ocr] [-o FILE]\n\
         \x20               [-i FILE] [--backend {{{}}}]\n\
         \x20               [--no-copy] [--no-save] [--no-scan] [--tray] [--settings] [--quit]\n\
         \x20               [--print-config] [--diagnose] [-v] [--version]",
        crate::config::BACKENDS.join(",")
    )
}

fn help() -> String {
    format!(
        "{usage}

Region screenshots with a floating draw toolbar.

options:
  -h, --help            show this help message and exit
  -d SEC, --delay SEC   wait before capturing
  --pin                 pin the result to the screen instead of saving it
  --record              choose an area and record it to a video (stops the recording in progress)
  --ocr                 choose an area and copy the text in it (needs tesseract)
  -o FILE, --output FILE
                        save to this file instead of the screenshot folder ('-' writes PNG to stdout)
  -i FILE, --image FILE
                        open an image in the annotation editor (Save writes back to it, or to --output)
  --backend {{{backends}}}
                        screen capture helper (default: from settings, else auto)
  --no-copy             don't copy the result to the clipboard
  --no-save             don't save the result to disk
  --no-scan             don't look for QR codes / barcodes
  --tray                run in the system tray with global shortcuts
  --settings            open the settings window
  --quit                stop the running tray app
  --print-config        print the effective config and exit
  --diagnose            show what this Flatshot and the running tray app use (layer-shell, capture, recorder)
  -v, --verbose         say on stderr how long each step of a capture takes (with --tray: every capture)
  --version             show program's version number and exit

instant captures (no UI):
  -f, --full            capture every screen
  -m, --monitor         capture the monitor under the pointer
  -w, --window          capture the active window
  -l, --last-region     capture the region captured last time
  -r WxH+X+Y, --region WxH+X+Y
                        capture this area (logical pixels)
",
        usage = usage(),
        backends = crate::config::BACKENDS.join(",")
    )
}

fn error(msg: impl AsRef<str>) -> Parsed {
    Parsed::Error(format!("{}\nflatshot-rs: error: {}", usage(), msg.as_ref()))
}

/// 'WxH+X+Y' (as in Flameshot and X11 geometry): an area in global logical pixels.
pub fn parse_geometry(text: &str) -> Option<IRect> {
    let t = text.trim();
    let (w, rest) = t.split_once('x')?;
    let w: i32 = parse_digits(w)?;
    // h runs up to the first sign.
    let sign = rest.find(['+', '-'])?;
    let h: i32 = parse_digits(&rest[..sign])?;
    let rest = &rest[sign..];
    let second = rest[1..].find(['+', '-'])? + 1;
    let x = parse_signed(&rest[..second])?;
    let y = parse_signed(&rest[second..])?;
    (w > 0 && h > 0).then_some(IRect::new(x, y, w, h))
}

fn parse_digits(s: &str) -> Option<i32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn parse_signed(s: &str) -> Option<i32> {
    let (neg, digits) = match s.as_bytes().first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => return None,
    };
    let v = parse_digits(digits)?;
    Some(if neg { -v } else { v })
}

const WITH_VALUE: &[(&str, &str)] = &[("-d", "--delay"), ("-r", "--region"), ("-o", "--output"), ("-i", "--image"), ("", "--backend")];
const FLAGS: &[(&str, &str)] = &[
    ("-f", "--full"),
    ("-m", "--monitor"),
    ("-w", "--window"),
    ("-l", "--last-region"),
    ("", "--pin"),
    ("", "--record"),
    ("", "--ocr"),
    ("", "--no-copy"),
    ("", "--no-save"),
    ("", "--no-scan"),
    ("", "--tray"),
    ("", "--settings"),
    ("", "--quit"),
    ("", "--print-config"),
    ("", "--diagnose"),
    ("-v", "--verbose"),
    ("", "--self-test"),
];

pub fn parse(argv: &[String]) -> Parsed {
    let mut a = Args::default();
    let mut instant: Option<&'static str> = None;
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        i += 1;
        if arg == "-h" || arg == "--help" {
            return Parsed::Exit(help());
        }
        if arg == "--version" {
            return Parsed::Exit(format!("flatshot-rs {VERSION}\n"));
        }
        // An option with a value: "--delay 3", "--delay=3", "-d3", "-d 3".
        let mut matched = false;
        for (short, long) in WITH_VALUE {
            let value = if arg == *long || (!short.is_empty() && arg == *short) {
                if i >= argv.len() {
                    return error(format!("argument {}: expected one argument", name(short, long)));
                }
                i += 1;
                Some(argv[i - 1].clone())
            } else if let Some(v) = arg.strip_prefix(&format!("{long}=")) {
                Some(v.to_string())
            } else if !short.is_empty() && arg.starts_with(short) && arg.len() > 2 && !arg.starts_with("--") {
                Some(arg[2..].to_string())
            } else {
                None
            };
            let Some(v) = value else { continue };
            matched = true;
            match *long {
                "--delay" => match v.trim().parse::<f64>() {
                    Ok(d) if d.is_finite() => a.delay = d,
                    _ => return error(format!("argument -d/--delay: invalid float value: '{v}'")),
                },
                "--region" => {
                    if let Some(other) = instant.replace("-r/--region")
                        && other != "-r/--region"
                    {
                        return error(format!("argument -r/--region: not allowed with argument {other}"));
                    }
                    a.region = Some(v);
                }
                "--output" => a.output = Some(v),
                "--image" => a.image = Some(v),
                "--backend" => {
                    if !crate::config::BACKENDS.contains(&v.as_str()) {
                        return error(format!(
                            "argument --backend: invalid choice: '{v}' (choose from {})",
                            crate::config::BACKENDS.iter().map(|b| format!("'{b}'")).collect::<Vec<_>>().join(", ")
                        ));
                    }
                    a.backend = Some(v);
                }
                _ => {}
            }
            break;
        }
        if matched {
            continue;
        }
        // Flags, also combined: "-fv".
        let flags: Vec<&str> = if arg.starts_with("--") || !arg.starts_with('-') || arg.len() <= 2 {
            vec![arg]
        } else {
            // "-fv" = "-f" "-v" (only when every letter is a flag)
            let letters: Vec<String> = arg[1..].chars().map(|c| format!("-{c}")).collect();
            if letters.iter().all(|l| FLAGS.iter().any(|(s, _)| s == l)) {
                letters.iter().map(|l| FLAGS.iter().find(|(s, _)| s == l).unwrap().0).collect()
            } else {
                vec![arg]
            }
        };
        for flag in flags {
            let Some((short, long)) = FLAGS.iter().find(|(s, l)| *l == flag || (!s.is_empty() && *s == flag)) else {
                return error(format!("unrecognized arguments: {arg}"));
            };
            let group = matches!(*long, "--full" | "--monitor" | "--window" | "--last-region");
            if group {
                let this: &'static str = match *long {
                    "--full" => "-f/--full",
                    "--monitor" => "-m/--monitor",
                    "--window" => "-w/--window",
                    _ => "-l/--last-region",
                };
                if let Some(other) = instant.replace(this)
                    && other != this
                {
                    return error(format!("argument {this}: not allowed with argument {other}"));
                }
            }
            let _ = short;
            match *long {
                "--full" => a.full = true,
                "--monitor" => a.monitor = true,
                "--window" => a.window = true,
                "--last-region" => a.last_region = true,
                "--pin" => a.pin = true,
                "--record" => a.record = true,
                "--ocr" => a.ocr = true,
                "--no-copy" => a.no_copy = true,
                "--no-save" => a.no_save = true,
                "--no-scan" => a.no_scan = true,
                "--tray" => a.tray = true,
                "--settings" => a.settings = true,
                "--quit" => a.quit = true,
                "--print-config" => a.print_config = true,
                "--diagnose" => a.diagnose = true,
                "--verbose" => a.verbose = true,
                "--self-test" => a.self_test = true,
                _ => {}
            }
        }
    }
    if let Some(r) = &a.region
        && parse_geometry(r).is_none()
    {
        return error(format!("--region: expected WxH+X+Y (like 800x600+100+50), got '{r}'"));
    }
    if a.record && (a.mode() != "region" || a.pin || a.image.is_some() || a.output.is_some()) {
        return error("--record picks its area on screen; it doesn't combine with instant captures, --pin, -i or -o");
    }
    if a.ocr && (a.mode() != "region" || a.pin || a.record || a.image.is_some() || a.output.is_some()) {
        return error("--ocr picks its area on screen; it doesn't combine with instant captures, --pin, --record, -i or -o");
    }
    Parsed::Run(a)
}

fn name(short: &str, long: &str) -> String {
    if short.is_empty() { long.to_string() } else { format!("{short}/{long}") }
}

impl Args {
    /// The capture mode: region (the overlay) or an instant one.
    pub fn mode(&self) -> &'static str {
        if self.full {
            "screens"
        } else if self.monitor {
            "monitor"
        } else if self.window {
            "window"
        } else if self.last_region {
            "last"
        } else if self.region.is_some() {
            "rect"
        } else {
            "region"
        }
    }

    /// The tray-app command equivalent to these arguments, if any.
    pub fn forwardable(&self) -> Option<String> {
        if let Some(image) = &self.image
            && self.output.is_none()
            && !self.verbose
        {
            // An editor window of the tray app's.
            let path = std::path::absolute(crate::config::expand_user(image)).ok()?;
            return Some(format!("edit {}", path.display()));
        }
        if self.image.is_some()
            || self.output.is_some()
            || self.backend.is_some()
            || self.no_copy
            || self.no_save
            || self.no_scan
            || (self.verbose && !(self.tray || self.settings || self.quit))
        {
            return None; // one-off options: handled in this process
        }
        if self.quit {
            return Some("quit".into());
        }
        if self.settings || self.tray {
            return Some("settings".into()); // already running: show something useful
        }
        if self.record {
            return Some(format!("record {}", fmt_delay(self.delay)));
        }
        if self.ocr {
            return Some(format!("ocr {}", fmt_delay(self.delay)));
        }
        let mut mode = self.mode().to_string();
        if mode == "rect" {
            mode = format!("rect:{}", self.region.as_deref().unwrap_or("").trim());
        }
        Some(format!("{mode} {}{}", fmt_delay(self.delay), if self.pin { " pin" } else { "" }))
    }
}

/// A delay as Python's `{:g}` writes it.
pub fn fmt_delay(d: f64) -> String {
    if d == d.trunc() && d.abs() < 1e15 { format!("{}", d as i64) } else { format!("{d}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Args {
        match parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()) {
            Parsed::Run(a) => a,
            Parsed::Exit(s) => panic!("exit: {s}"),
            Parsed::Error(e) => panic!("error: {e}"),
        }
    }

    fn err(args: &[&str]) -> String {
        match parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()) {
            Parsed::Error(e) => e,
            _ => panic!("expected an error for {args:?}"),
        }
    }

    #[test]
    fn geometry() {
        assert_eq!(parse_geometry("800x600+100+50"), Some(IRect::new(100, 50, 800, 600)));
        assert_eq!(parse_geometry(" 10x20-5-7 "), Some(IRect::new(-5, -7, 10, 20)));
        assert_eq!(parse_geometry("0x600+1+1"), None);
        assert_eq!(parse_geometry("800x600"), None);
        assert_eq!(parse_geometry("axb+1+1"), None);
    }

    #[test]
    fn options() {
        let a = run(&["-d", "3", "--full", "-o", "x.png"]);
        assert_eq!((a.delay, a.full, a.output.as_deref()), (3.0, true, Some("x.png")));
        let a = run(&["--delay=1.5", "-fv"]);
        assert_eq!((a.delay, a.full, a.verbose), (1.5, true, true));
        let a = run(&["-d2", "--region", "10x10+0+0"]);
        assert_eq!((a.delay, a.mode()), (2.0, "rect"));
        assert!(err(&["-f", "-m"]).contains("not allowed with argument -f/--full"));
        assert!(err(&["--record", "-f"]).contains("--record picks its area"));
        assert!(err(&["--ocr", "--pin"]).contains("--ocr picks its area"));
        assert!(run(&["--ocr", "-d", "1"]).ocr);
        assert!(err(&["--region", "nope"]).contains("expected WxH+X+Y"));
        assert!(err(&["--bogus"]).contains("unrecognized arguments"));
        assert!(err(&["--backend", "x"]).contains("invalid choice"));
    }

    #[test]
    fn forwarding() {
        assert_eq!(run(&[]).forwardable().as_deref(), Some("region 0"));
        assert_eq!(run(&["--full", "--pin", "-d", "2"]).forwardable().as_deref(), Some("screens 2 pin"));
        assert_eq!(run(&["-r", "5x5+1+2"]).forwardable().as_deref(), Some("rect:5x5+1+2 0"));
        assert_eq!(run(&["--record"]).forwardable().as_deref(), Some("record 0"));
        assert_eq!(run(&["--ocr", "-d", "2"]).forwardable().as_deref(), Some("ocr 2"));
        assert_eq!(run(&["--tray"]).forwardable().as_deref(), Some("settings"));
        assert_eq!(run(&["--no-save"]).forwardable(), None);
        assert_eq!(run(&["-v"]).forwardable(), None);
        assert_eq!(run(&["-d", "0.5"]).forwardable().as_deref(), Some("region 0.5"));
        assert!(run(&["-i", "/x/y.png"]).forwardable().unwrap().starts_with("edit /x/y.png"));
    }
}
