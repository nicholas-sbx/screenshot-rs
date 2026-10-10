//! Recording an area of the screen to a video file.
//!
//! As with screenshots, Wayland doesn't let a program read the screen
//! itself, so the recorder depends on the desktop:
//!
//! - KDE Plasma, GNOME and other Wayland desktops: the screen-cast portal.
//!   It hands over a PipeWire stream of a monitor, which GStreamer
//!   (`gst-launch-1.0` with `pipewiresrc`) crops to the area and encodes.
//!   The first time, the desktop asks which screen to share; the portal's
//!   restore token is kept per monitor, so later recordings of it start
//!   without asking.
//! - Sway, Hyprland and other wlroots compositors: `wf-recorder`.
//! - X11: `ffmpeg` with x11grab.
//!
//! A recording is a list of segments, one per stretch between pauses;
//! ffmpeg joins them at the end, and makes the GIF when asked. The
//! recorders run as programs of their own: nothing of the video passes
//! through Flatshot's memory.

pub mod portal;

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::geom::IRect;
use crate::util;

/// The recording formats: (config value, label).
pub const FORMATS: &[(&str, &str)] = &[("mp4", "MP4"), ("webm", "WebM"), ("gif", "GIF")];
/// Audio sources, by PulseAudio name (PipeWire's pulse server knows them too).
pub const MIC: &str = "@DEFAULT_SOURCE@";
pub const SYSTEM: &str = "@DEFAULT_MONITOR@";
/// The file descriptor number a recorder finds the PipeWire stream at.
const STREAM_FD: i32 = 3;

/// How this desktop records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// the screen-cast portal and GStreamer
    Portal,
    WfRecorder,
    /// ffmpeg's x11grab
    X11,
    /// a test pattern, made by ffmpeg (for tests)
    Test,
    /// a test pattern, through GStreamer (for tests)
    TestGst,
}

impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Method::Portal => "Screen-cast portal and GStreamer",
            Method::WfRecorder => "wf-recorder",
            Method::X11 => "ffmpeg (X11)",
            Method::Test => "Test pattern (ffmpeg)",
            Method::TestGst => "Test pattern (GStreamer)",
        }
    }

    /// Can it take an area across monitors? The portal (sharing the whole
    /// workspace) and X11 can; wf-recorder records one output.
    pub fn can_span(self) -> bool {
        self != Method::WfRecorder
    }
}

/// How this desktop records (`FLATSHOT_RECORDER` overrides it).
pub fn method() -> Method {
    match std::env::var("FLATSHOT_RECORDER").as_deref() {
        Ok("portal") => return Method::Portal,
        Ok("wf-recorder") => return Method::WfRecorder,
        Ok("x11") => return Method::X11,
        Ok("test") => return Method::Test,
        Ok("test-gst") => return Method::TestGst,
        _ => {}
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_some() {
        return Method::X11;
    }
    let wlroots = std::env::var_os("SWAYSOCK").is_some() || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some();
    if wlroots && util::have("wf-recorder") { Method::WfRecorder } else { Method::Portal }
}

/// What to record with.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// see [`FORMATS`]
    pub format: String,
    pub fps: u32,
    pub mic: bool,
    pub system_audio: bool,
    pub cursor: bool,
}

impl Options {
    /// The PulseAudio sources to record (none for a GIF).
    pub fn audio(&self) -> Vec<&'static str> {
        if self.format == "gif" {
            return Vec::new();
        }
        [(MIC, self.mic), (SYSTEM, self.system_audio)].into_iter().filter(|s| s.1).map(|s| s.0).collect()
    }
}

/// The file type segments are recorded as (a GIF is made at the end).
pub fn segment_extension(format: &str) -> &'static str {
    if format == "webm" { "webm" } else { "mp4" }
}

// -- what to record ---------------------------------------------------------------------

/// The area to record (global logical pixels), on the monitor it was
/// chosen on or, when it crosses monitors, on several.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub rect: IRect,
    pub monitor: IRect,
    pub monitor_name: String,
    /// the monitor's real pixels per logical pixel (as the screenshot had them)
    pub scale: f64,
    /// every monitor, by name (for an area across them)
    pub monitors: Vec<(String, IRect)>,
}

impl Target {
    pub fn spans(&self) -> bool {
        !self.monitor.contains_rect(&self.rect)
    }

    /// Its monitor, or the whole desktop when the area crosses monitors.
    pub fn shown_area(&self) -> IRect {
        if self.spans() { workspace(&self.monitors) } else { self.monitor }
    }

    /// For video that shows `shown` (global logical pixels) at `size`
    /// pixels (default: at this monitor's scale): that size, and the area
    /// within it, with an even size (H.264 and VP8 need one), kept inside it.
    pub fn crop_in(&self, shown: IRect, size: Option<(u32, u32)>) -> ((u32, u32), IRect) {
        let size = size.unwrap_or(((shown.w as f64 * self.scale).round() as u32, (shown.h as f64 * self.scale).round() as u32));
        let sx = size.0 as f64 / shown.w.max(1) as f64;
        let sy = size.1 as f64 / shown.h.max(1) as f64;
        let r = IRect::new(
            ((self.rect.x - shown.x) as f64 * sx).round() as i32,
            ((self.rect.y - shown.y) as f64 * sy).round() as i32,
            (self.rect.w as f64 * sx).round() as i32,
            (self.rect.h as f64 * sy).round() as i32,
        )
        .intersected(&IRect::new(0, 0, size.0 as i32, size.1 as i32));
        (size, IRect::new(r.x, r.y, r.w & !1, r.h & !1))
    }

    /// The area in the pixels of its monitor (or of the whole desktop).
    pub fn local_pixels(&self) -> IRect {
        self.crop_in(self.shown_area(), None).1
    }
}

/// All the monitors together (global logical pixels).
pub fn workspace(monitors: &[(String, IRect)]) -> IRect {
    monitors.iter().fold(IRect::default(), |acc, (_, r)| if acc.is_empty() { *r } else { acc.united(r) })
}

/// Which part of the desktop (global logical pixels) a portal stream at
/// `position` with `size` shows: one monitor, or the whole workspace.
/// Desktops differ in whether they report these in logical or device
/// pixels, so a monitor matches by place and shape; failing that, the
/// values are taken as logical, as the portal documents.
pub fn stream_shows(position: (i32, i32), size: (i32, i32), scale: f64, screens: &[IRect]) -> IRect {
    let ((x, y), (w, h)) = (position, size);
    if w <= 0 || h <= 0 {
        return IRect::new(x, y, w.max(1), h.max(1));
    }
    let whole = screens.iter().fold(IRect::default(), |acc, r| if acc.is_empty() { *r } else { acc.united(r) });
    let aspect = w as f64 / h as f64;
    let place_fits = |g: &IRect| (x, y) == (g.x, g.y) || ((x as f64 - g.x as f64 * scale).abs() <= 2.0 && (y as f64 - g.y as f64 * scale).abs() <= 2.0);
    let fits: Vec<IRect> = screens
        .iter()
        .chain(std::iter::once(&whole))
        .filter(|g| place_fits(g) && (g.w as f64 / g.h.max(1) as f64 - aspect).abs() <= 0.01 * aspect)
        .copied()
        .collect();
    for g in &fits {
        if (g.w, g.h) == (w, h) || ((g.w as f64 * scale - w as f64).abs() <= 2.0 && (g.h as f64 * scale - h as f64).abs() <= 2.0) {
            return *g;
        }
    }
    fits.first().copied().unwrap_or(IRect::new(x, y, w, h))
}

/// What a stream measured at `measured` pixels shows, checked against what
/// the portal `reported` (KDE leaves position and size out for a shared
/// workspace): a monitor or the whole workspace whose size, in logical or
/// device pixels, is the stream's. Where two fit (identical monitors), the
/// one with the `area` wins. None: nothing fits.
pub fn stream_measured(measured: (u32, u32), reported: Option<IRect>, area: IRect, scale: f64, screens: &[IRect]) -> Option<IRect> {
    let (mw, mh) = (measured.0 as f64, measured.1 as f64);
    if mw <= 0.0 || mh <= 0.0 {
        return reported;
    }
    let aspect = mw / mh;
    let same_shape = |g: &IRect| (g.w as f64 / g.h.max(1) as f64 - aspect).abs() <= 0.015 * aspect;
    if let Some(r) = reported.filter(|r| same_shape(r)) {
        return Some(r);
    }
    let whole = screens.iter().fold(IRect::default(), |acc, r| if acc.is_empty() { *r } else { acc.united(r) });
    let mut candidates: Vec<IRect> = Vec::new();
    for g in screens.iter().chain(std::iter::once(&whole)) {
        if !candidates.contains(g) {
            candidates.push(*g);
        }
    }
    candidates.sort_by_key(|g| !g.contains_rect(&area)); // (the area's own first)
    let mut scales = vec![1.0, scale];
    scales.sort_by(f64::total_cmp);
    scales.dedup();
    for k in scales {
        if let Some(g) = candidates.iter().find(|g| (g.w as f64 * k - mw).abs() <= 2.0 && (g.h as f64 * k - mh).abs() <= 2.0) {
            return Some(*g);
        }
    }
    candidates.into_iter().find(|g| same_shape(g))
}

// -- tools -------------------------------------------------------------------------------

/// Is the GStreamer `element` installed? (Asked once each.)
pub fn gst_has(element: &str) -> bool {
    static KNOWN: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
    if let Ok(known) = KNOWN.lock()
        && let Some(v) = known.as_ref().and_then(|k| k.get(element))
    {
        return *v;
    }
    let found = util::have("gst-inspect-1.0") && run_quietly(&["gst-inspect-1.0", "--exists", element], Duration::from_secs(10));
    if let Ok(mut known) = KNOWN.lock() {
        known.get_or_insert_default().insert(element.to_string(), found);
    }
    found
}

/// Run a program and say whether it succeeded (killed after `limit`).
fn run_quietly(argv: &[&str], limit: Duration) -> bool {
    let Some(mut cmd) = util::command(argv[0]) else { return false };
    let child = cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn();
    let Ok(mut child) = child else { return false };
    matches!(wait_for(&mut child, limit), Some(0))
}

/// Wait up to `limit` for `child` to end: its exit code (None: killed, or
/// ended by a signal).
fn wait_for(child: &mut Child, limit: Duration) -> Option<i32> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                kill_group(child, rustix::process::Signal::KILL);
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Signal a child and everything it started (it leads its own group).
pub fn kill_group(child: &Child, sig: rustix::process::Signal) {
    let _ = rustix::process::kill_process_group(rustix::process::Pid::from_child(child), sig);
}

/// Pausing records separate segments that ffmpeg joins at the end.
pub fn can_join() -> bool {
    util::have("ffmpeg")
}

/// Why recording in `format` can't work here (what to install), or None.
pub fn problem(how: Method, format: &str) -> Option<String> {
    match how {
        Method::X11 | Method::Test if !util::have("ffmpeg") => return Some("Recording needs ffmpeg.".into()),
        Method::WfRecorder if !util::have("wf-recorder") => return Some("Recording needs wf-recorder.".into()),
        Method::Portal | Method::TestGst => {
            if !util::have("gst-launch-1.0") {
                return Some("Recording needs GStreamer (gst-launch-1.0).".into());
            }
            if how == Method::Portal && !gst_has("pipewiresrc") {
                return Some("Recording needs GStreamer's PipeWire plugin (gstreamer1.0-pipewire or gst-plugin-pipewire).".into());
            }
            let fmt = if format == "webm" { "webm" } else { "mp4" };
            if gst_video_encoder(fmt).is_none() {
                return Some(
                    "Recording needs a GStreamer video encoder: x264enc or openh264enc for MP4, vp8enc for WebM (gst-plugins-ugly / -bad / -good).".into(),
                );
            }
            let mux = if fmt == "webm" { "webmmux" } else { "mp4mux" };
            if !["videoconvert", "videocrop", "videorate", "videoscale", mux].iter().all(|e| gst_has(e)) {
                return Some("Recording needs GStreamer's base and good plugins (gst-plugins-base, gst-plugins-good).".into());
            }
        }
        _ => {}
    }
    if format == "gif" && !util::have("ffmpeg") {
        return Some("GIFs need ffmpeg.".into());
    }
    None
}

// -- encoding ----------------------------------------------------------------------------

/// A generous screen-content bitrate, kbit/s.
fn bitrate(w: i32, h: i32, fps: u32) -> u32 {
    ((w as f64 * h as f64 * fps as f64 * 0.08 / 1000.0).round() as u32).clamp(1500, 40000)
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn ffmpeg_codecs(ext: &str, crop: IRect, fps: u32, audio: bool) -> Vec<String> {
    let (mut video, sound) = if ext == "webm" {
        let b = format!("{}k", bitrate(crop.w, crop.h, fps));
        (
            strings(&["-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-b:v", &b, "-crf", "10", "-auto-alt-ref", "0"]),
            strings(&["-c:a", "libopus", "-b:a", "128k"]),
        )
    } else {
        // mp4, and the intermediate file for a GIF
        (strings(&["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-tune", "zerolatency"]), strings(&["-c:a", "aac", "-b:a", "160k"]))
    };
    video.extend(strings(&["-pix_fmt", "yuv420p"]));
    if audio {
        video.extend(sound);
    }
    video
}

/// (inputs, mapping) for PulseAudio sources, mixed into one track.
fn ffmpeg_audio(sources: &[&str], first_index: usize) -> (Vec<String>, Vec<String>) {
    let mut inputs = Vec::new();
    for name in sources {
        inputs.extend(strings(&["-thread_queue_size", "1024", "-f", "pulse", "-i", name]));
    }
    match sources.len() {
        0 => (Vec::new(), Vec::new()),
        1 => (inputs, strings(&["-map", "0:v", "-map", &format!("{first_index}:a")])),
        n => {
            let mix: String = (0..n).map(|i| format!("[{}:a]", first_index + i)).collect();
            (inputs, strings(&["-filter_complex", &format!("{mix}amix=inputs={n}:duration=longest[a]"), "-map", "0:v", "-map", "[a]"]))
        }
    }
}

/// GStreamer's video encoder for `fmt` (with `{kbps}` and `{bps}` to fill in).
fn gst_video_encoder(fmt: &str) -> Option<Vec<&'static str>> {
    if fmt == "webm" {
        return gst_has("vp8enc")
            .then(|| vec!["vp8enc", "deadline=1", "cpu-used=8", "threads=4", "end-usage=cbr", "target-bitrate={bps}", "keyframe-max-dist=120"]);
    }
    let mut enc = if gst_has("x264enc") {
        vec!["x264enc", "speed-preset=veryfast", "tune=zerolatency", "bitrate={kbps}", "key-int-max=120", "!", "video/x-h264,profile=high"]
    } else if gst_has("openh264enc") {
        vec!["openh264enc", "bitrate={bps}", "complexity=low"]
    } else {
        return None;
    };
    if gst_has("h264parse") {
        enc.extend(["!", "h264parse"]);
    }
    Some(enc)
}

fn gst_audio_encoder(fmt: &str) -> Option<Vec<&'static str>> {
    let choices: &[&[&'static str]] = if fmt == "webm" {
        &[&["opusenc", "bitrate=128000"], &["vorbisenc"]]
    } else {
        &[&["avenc_aac", "bitrate=160000"], &["fdkaacenc", "bitrate=160000"], &["voaacenc", "bitrate=160000"], &["opusenc", "bitrate=128000"]]
    };
    choices.iter().find(|c| gst_has(c[0])).map(|c| c.to_vec())
}

/// gst-launch's arguments: `source` (frames of size `frame`) cropped to
/// `crop`, encoded for `opts.format`, written to `path`. Each argument is
/// one word of the pipeline (gst-launch keeps their spaces).
pub fn gst_pipeline(source: &[String], frame: (u32, u32), crop: IRect, opts: &Options, path: &Path) -> Result<Vec<String>, String> {
    let fmt = if opts.format == "webm" { "webm" } else { "mp4" };
    let encoder = gst_video_encoder(fmt).ok_or_else(|| problem(Method::TestGst, fmt).unwrap_or_else(|| "no GStreamer video encoder".into()))?;
    let kbps = bitrate(crop.w, crop.h, opts.fps);
    let (w, h) = (frame.0 as i32, frame.1 as i32);
    let (right, bottom) = ((w - crop.x - crop.w).max(0), (h - crop.y - crop.h).max(0));
    let mut a: Vec<String> = source.to_vec();
    // The first scale brings the stream to the size the crop is worked out
    // for, whatever size the desktop sends (nothing to do when it's as
    // expected), so the crop is always the area. Square pixels throughout,
    // so players never stretch the result.
    let caps = |w: i32, h: i32| format!("video/x-raw,width={w},height={h},pixel-aspect-ratio=1/1");
    for word in [
        "!".to_string(),
        "videoconvert".into(),
        "!".into(),
        "videoscale".into(),
        "!".into(),
        caps(w, h),
        "!".into(),
        "videocrop".into(),
        format!("left={}", crop.x),
        format!("top={}", crop.y),
        format!("right={right}"),
        format!("bottom={bottom}"),
        "!".into(),
        "videoscale".into(),
        "!".into(),
        caps(crop.w, crop.h),
        "!".into(),
        "videorate".into(),
        "!".into(),
        format!("video/x-raw,framerate={}/1", opts.fps),
        "!".into(),
        "videoconvert".into(),
        "!".into(),
        "video/x-raw,format=I420".into(),
        "!".into(),
        "queue".into(),
        "max-size-buffers=0".into(),
        "max-size-bytes=0".into(),
        "max-size-time=2000000000".into(),
        "!".into(),
    ] {
        a.push(word);
    }
    a.extend(encoder.iter().map(|w| w.replace("{kbps}", &kbps.to_string()).replace("{bps}", &(kbps * 1000).to_string())));
    a.extend(strings(&["!", "queue", "!"]));
    if fmt == "webm" {
        a.extend(strings(&["webmmux", "name=mux"]));
    } else {
        a.extend(strings(&["mp4mux", "fragment-duration=1000", "name=mux"]));
    }
    a.extend(["!".to_string(), "filesink".to_string(), format!("location={}", path.display())]);
    let audio = opts.audio();
    if !audio.is_empty() {
        let aenc = gst_audio_encoder(fmt).ok_or("Recording sound needs a GStreamer audio encoder (opusenc or an AAC encoder).")?;
        if !gst_has("pulsesrc") {
            return Err("Recording sound needs GStreamer's PulseAudio plugin (pulsesrc).".into());
        }
        let raw = "audio/x-raw,rate=48000,channels=2";
        let mut tail = strings(&["audioconvert", "!", "audioresample", "!", raw, "!"]);
        tail.extend(aenc.iter().map(|s| s.to_string()));
        tail.extend(strings(&["!", "queue", "!", "mux."]));
        if audio.len() == 1 {
            a.extend(strings(&["pulsesrc", &format!("device={}", audio[0]), "do-timestamp=true", "!", "queue", "!"]));
            a.extend(tail);
        } else {
            a.extend(strings(&["audiomixer", "name=amix", "!"]));
            a.extend(tail);
            for name in audio {
                a.extend(strings(&[
                    "pulsesrc",
                    &format!("device={name}"),
                    "do-timestamp=true",
                    "!",
                    "queue",
                    "!",
                    "audioconvert",
                    "!",
                    "audioresample",
                    "!",
                    raw,
                    "!",
                    "amix.",
                ]));
            }
        }
    }
    Ok(a)
}

// -- recording a segment -----------------------------------------------------------------

/// A recorder to start for one segment.
pub struct Launch {
    pub argv: Vec<String>,
    /// the PipeWire connection it reads the stream from (its fd 3)
    pub stream: Option<OwnedFd>,
    /// what makes it finish its file cleanly
    pub stop: rustix::process::Signal,
    /// what it was set up as, for the log
    pub about: String,
}

/// The recorder for one segment, written to `path`.
pub fn launch(how: Method, target: &Target, opts: &Options, path: &Path, cast: Option<&portal::ScreenCast>) -> Result<Launch, String> {
    let crop = target.local_pixels();
    if crop.w < 2 || crop.h < 2 {
        return Err("The area is too small to record.".into());
    }
    let audio = opts.audio();
    let ext = segment_extension(&opts.format);
    let fps = opts.fps.to_string();
    let int = rustix::process::Signal::INT;
    match how {
        Method::X11 | Method::Test => {
            let video = if how == Method::X11 {
                // X11's screen is in device pixels.
                let g = target.shown_area();
                let (x, y) = ((g.x as f64 * target.scale).round() as i32 + crop.x, (g.y as f64 * target.scale).round() as i32 + crop.y);
                let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
                strings(&[
                    "-f",
                    "x11grab",
                    "-framerate",
                    &fps,
                    "-video_size",
                    &format!("{}x{}", crop.w, crop.h),
                    "-draw_mouse",
                    if opts.cursor { "1" } else { "0" },
                    "-i",
                    &format!("{display}+{x},{y}"),
                ])
            } else {
                strings(&["-re", "-f", "lavfi", "-i", &format!("testsrc2=size={}x{}:rate={fps}", crop.w, crop.h)])
            };
            let (inputs, mapping) = ffmpeg_audio(&audio, 1);
            let mut argv = strings(&["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-thread_queue_size", "1024"]);
            argv.extend(video);
            argv.extend(inputs);
            argv.extend(mapping);
            argv.extend(ffmpeg_codecs(ext, crop, opts.fps, !audio.is_empty()));
            argv.extend(["-r".to_string(), fps.clone(), path.display().to_string()]);
            Ok(Launch { argv, stream: None, stop: int, about: format!("{}x{}+{}+{} with {}", crop.w, crop.h, crop.x, crop.y, how.label()) })
        }
        Method::WfRecorder => {
            let r = target.rect;
            // -D: frames all the time, not only when the area changes: a
            // steady rate, and a stop is heard at once (with no new frame
            // it would wait, and be killed with its file unfinished).
            let mut argv = strings(&["wf-recorder", "-y", "-D", "-g", &format!("{},{} {}x{}", r.x, r.y, r.w, r.h), "-r", &fps, "-x", "yuv420p", "-f"]);
            argv.push(path.display().to_string());
            if ext == "webm" {
                argv.extend(strings(&["-c", "libvpx", "-p", "deadline=realtime", "-p", "cpu-used=8"]));
            } else {
                argv.extend(strings(&["-c", "libx264", "-p", "preset=veryfast", "-p", "crf=20"]));
            }
            if let Some(source) = audio.first() {
                argv.push(format!("--audio={source}")); // (wf-recorder records one source)
            }
            Ok(Launch { argv, stream: None, stop: int, about: format!("{}x{}{:+}{:+} with wf-recorder", r.w, r.h, r.x, r.y) })
        }
        Method::Portal | Method::TestGst => {
            // What the stream shows (the portal says, else the area's
            // monitor), at the size it really is (measured, else that part
            // at the monitor's scale).
            let cast = cast.filter(|_| how == Method::Portal);
            let shown = cast.and_then(|c| c.stream_rect).unwrap_or_else(|| target.shown_area());
            let (frame, crop) = target.crop_in(shown, cast.and_then(|c| c.frame_size));
            if crop.w < 2 || crop.h < 2 {
                return Err("The area isn't in the shared screen.".into());
            }
            let mut about = format!("{}x{}+{}+{} of a {}x{} stream showing {}", crop.w, crop.h, crop.x, crop.y, frame.0, frame.1, geometry(shown));
            if let Some(c) = cast {
                let measured = c.frame_size.map(|(w, h)| format!("{w}x{h}")).unwrap_or_else(|| "nothing".into());
                let reported = if c.reported.is_empty() { "no position or size" } else { &c.reported };
                about.push_str(&format!(" (portal: {reported}, measured {measured})"));
            }
            about.push_str(&format!("; area {} on {}, scale {}", geometry(target.rect), target.monitor_name, target.scale));
            let (source, stream) = match cast {
                Some(c) => {
                    let fd = c.pipewire_fd()?;
                    (strings(&["pipewiresrc", &format!("fd={STREAM_FD}"), &format!("path={}", c.node), "do-timestamp=true", "keepalive-time=1000"]), Some(fd))
                }
                None if how == Method::TestGst => (
                    strings(&[
                        "videotestsrc",
                        "is-live=true",
                        "pattern=smpte",
                        "!",
                        &format!("video/x-raw,width={},height={},framerate=30/1", frame.0, frame.1),
                    ]),
                    None,
                ),
                None => return Err("the screen-cast portal shared no screen".into()),
            };
            let mut argv = strings(&["gst-launch-1.0", "-e", "-q"]);
            argv.extend(gst_pipeline(&source, frame, crop, opts, path)?);
            Ok(Launch { argv, stream, stop: int, about })
        }
    }
}

fn geometry(r: IRect) -> String {
    format!("{}x{}{:+}{:+}", r.w, r.h, r.x, r.y)
}

/// Start `argv` in a process group of its own (so it and what it starts can
/// be signalled together), with `stream` as its fd 3. Should Flatshot die
/// without stopping it, it's told to finish (SIGINT) rather than recording
/// on with no one to save the file.
pub fn spawn(argv: &[String], stream: Option<&OwnedFd>, stdout: Stdio, stderr: Stdio) -> std::io::Result<Child> {
    let exe = util::which(&argv[0]).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{} is not installed", argv[0])))?;
    let mut cmd = Command::new(exe);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(stdout).stderr(stderr).process_group(0);
    let raw = stream.map(|fd| fd.as_raw_fd());
    let parent = rustix::process::getpid().as_raw_nonzero().get();
    // SAFETY: only async-signal-safe calls (prctl, getppid, close_range,
    // fcntl, dup2) between fork and exec. Everything is marked
    // close-on-exec, then dup2 puts the stream at the number the recorder
    // is told (dup2 clears the mark): nothing else is inherited.
    unsafe {
        cmd.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT as libc::c_ulong);
            // (Flatshot gone already, before that took: no one would stop it)
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            util::cloexec_from(STREAM_FD as u32);
            match raw {
                Some(fd) if fd == STREAM_FD => {
                    // (already there: just kept open across exec)
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Some(fd) if libc::dup2(fd, STREAM_FD) < 0 => return Err(std::io::Error::last_os_error()),
                _ => {}
            }
            Ok(())
        });
    }
    cmd.spawn()
}

// -- finishing ---------------------------------------------------------------------------

/// Run ffmpeg with `args`; its last error line if it fails.
fn ffmpeg(args: &[String], limit: Duration) -> Result<(), String> {
    let mut argv = strings(&["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    argv.extend(args.iter().cloned());
    let mut child = spawn(&argv, None, Stdio::null(), Stdio::piped()).map_err(|e| format!("ffmpeg: {e}"))?;
    // (read as it comes, so a full pipe never stalls it)
    let mut err = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(e) = err.as_mut() {
            let _ = e.take(64 * 1024).read_to_string(&mut text);
        }
        text
    });
    let code = wait_for(&mut child, limit);
    let text = reader.join().unwrap_or_default();
    match code {
        Some(0) => Ok(()),
        _ => {
            let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
            Err(format!("ffmpeg: {}", if last.is_empty() { "failed".to_string() } else { last.trim().to_string() }))
        }
    }
}

/// Join segments recorded with the same settings, without re-encoding.
pub fn join(segments: &[std::path::PathBuf], out: &Path) -> Result<(), String> {
    let listing = out.with_extension("txt");
    let text: String = segments.iter().map(|p| format!("file '{}'\n", p.display().to_string().replace('\'', r"'\''"))).collect();
    std::fs::write(&listing, text).map_err(|e| format!("{}: {e}", listing.display()))?;
    let result = ffmpeg(
        &strings(&["-f", "concat", "-safe", "0", "-i", &listing.display().to_string(), "-c", "copy", &out.display().to_string()]),
        Duration::from_secs(600),
    );
    let _ = std::fs::remove_file(&listing);
    result
}

/// A GIF with a palette of its own (far better than the default one).
pub fn to_gif(video: &Path, out: &Path, fps: u32) -> Result<(), String> {
    let rate = fps.min(30);
    let filter = format!("fps={rate},split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle");
    ffmpeg(&strings(&["-i", &video.display().to_string(), "-vf", &filter, "-loop", "0", &out.display().to_string()]), Duration::from_secs(1800))
}

/// Can the video at `path` be read (a recorder that had to be killed may
/// have left it unfinished)? True when there's no ffprobe to ask.
pub fn playable(path: &Path) -> bool {
    !util::have("ffprobe") || duration(path).is_some()
}

/// Seconds of video, when ffprobe is around to say.
pub fn duration(path: &Path) -> Option<f64> {
    if !util::have("ffprobe") {
        return None;
    }
    let argv = strings(&["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", &path.display().to_string()]);
    let mut child = spawn(&argv, None, Stdio::piped(), Stdio::null()).ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut out).take(4096).read_to_string(&mut text);
        text
    });
    wait_for(&mut child, Duration::from_secs(20)).filter(|c| *c == 0)?;
    reader.join().ok()?.trim().parse().ok()
}

/// A recording's length as the bar shows it: 01:05, or 1:02:03.
pub fn clock(elapsed: Duration) -> String {
    let s = elapsed.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(rect: IRect, scale: f64) -> Target {
        let monitors = vec![("DP-1".to_string(), IRect::new(0, 0, 1920, 1080)), ("DP-2".to_string(), IRect::new(1920, 0, 1280, 1024))];
        Target { rect, monitor: monitors[0].1, monitor_name: "DP-1".into(), scale, monitors }
    }

    #[test]
    fn crops_are_even_and_inside() {
        let t = target(IRect::new(101, 51, 301, 201), 1.25);
        let (size, crop) = t.crop_in(t.shown_area(), None);
        assert_eq!(size, (2400, 1350));
        assert_eq!((crop.w % 2, crop.h % 2), (0, 0));
        assert!(IRect::new(0, 0, 2400, 1350).contains_rect(&crop));
        assert_eq!((crop.x, crop.y), (126, 64));
        // Measured at another size: the crop follows it.
        let (_, c2) = t.crop_in(t.shown_area(), Some((1920, 1080)));
        assert_eq!((c2.x, c2.y, c2.w, c2.h), (101, 51, 300, 200));
        // Across monitors: the whole desktop is shown.
        let wide = target(IRect::new(1800, 100, 400, 300), 1.0);
        assert!(wide.spans());
        assert_eq!(wide.shown_area(), IRect::new(0, 0, 3200, 1080));
    }

    #[test]
    fn streams_are_matched_to_monitors() {
        let screens = [IRect::new(0, 0, 1920, 1080), IRect::new(1920, 0, 1280, 1024)];
        // Logical or device pixels, at its place.
        assert_eq!(stream_shows((1920, 0), (1280, 1024), 1.0, &screens), screens[1]);
        assert_eq!(stream_shows((0, 0), (3840, 2160), 2.0, &screens), screens[0]);
        // The whole workspace.
        assert_eq!(stream_shows((0, 0), (3200, 1080), 1.0, &screens), IRect::new(0, 0, 3200, 1080));
        // Measured: KDE gives nothing for a workspace.
        let area = IRect::new(10, 10, 100, 100);
        assert_eq!(stream_measured((3200, 1080), None, area, 1.0, &screens), Some(IRect::new(0, 0, 3200, 1080)));
        assert_eq!(stream_measured((3840, 2160), None, area, 2.0, &screens), Some(screens[0]));
        assert_eq!(stream_measured((640, 480), None, area, 1.0, &screens), None);
    }

    #[test]
    fn recorders_are_set_up() {
        let t = target(IRect::new(100, 100, 640, 480), 1.0);
        let opts = Options { format: "mp4".into(), fps: 30, mic: false, system_audio: false, cursor: true };
        let l = launch(Method::Test, &t, &opts, Path::new("/tmp/x/part-001.mp4"), None).unwrap();
        assert_eq!(l.argv[0], "ffmpeg");
        assert!(l.argv.windows(2).any(|w| w[0] == "-i" && w[1] == "testsrc2=size=640x480:rate=30"));
        assert_eq!(l.argv.last().unwrap(), "/tmp/x/part-001.mp4");
        let l = launch(Method::WfRecorder, &t, &Options { mic: true, ..opts.clone() }, Path::new("/tmp/a b.mp4"), None).unwrap();
        assert!(l.argv.windows(2).any(|w| w[0] == "-g" && w[1] == "100,100 640x480"));
        assert!(l.argv.contains(&"/tmp/a b.mp4".to_string()));
        assert!(l.argv.contains(&format!("--audio={MIC}")));
        // GIFs have no sound; both sources are mixed.
        assert!(Options { format: "gif".into(), mic: true, ..opts.clone() }.audio().is_empty());
        let (inputs, map) = ffmpeg_audio(&[MIC, SYSTEM], 1);
        assert_eq!(inputs.iter().filter(|a| *a == "-i").count(), 2);
        assert!(map.iter().any(|m| m.starts_with("[1:a][2:a]amix=inputs=2")));
        assert_eq!(clock(Duration::from_secs(65)), "01:05");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
    }
}
