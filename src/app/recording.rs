//! A screen recording in progress: the recorder (a program of its own,
//! started once per stretch between pauses), the controls on screen while
//! it runs, and the finished file (saved, copied, announced).
//!
//! The controls are small layer-shell surfaces outside the recorded area,
//! so none of them end up in the video: a bar with the time, pause, stop
//! and discard, and red marks at the area's corners. Without layer-shell
//! (GNOME) the bar is a small window wherever the desktop puts it.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::{Interest, Mode as PollMode, PostAction};

use super::countdown::{self, DialPaint};
use super::tray::NoteTarget;
use super::{App, Input};
use crate::clipboard::TEXT_MIMES;
use crate::config::{self, Config};
use crate::deliver;
use crate::desktop::notify;
use crate::geom::{Damage, IPoint, IRect, Point};
use crate::image::{Canvas, Image};
use crate::platform::{Monitor, Owner, Painted};
use crate::record::portal::{Failure, ScreenCast};
use crate::record::{self, Method, Options, Target};
use crate::template::{self, Shot};
use crate::ui::recbar::{self, BarAction, Corner, RecBar};
use crate::ui::session::{RecordJob, RecordSupport, Waker};
use crate::util;

/// For a recorder to finish its file after being asked to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Let the compositor take the overlays down before the first frame.
const SETTLE: Duration = Duration::from_millis(150);
/// Without a process descriptor: how often a recorder is looked at.
const POLL: Duration = Duration::from_millis(150);
/// A segment that ended sooner than this by itself is broken (the
/// recorder couldn't start).
const BROKEN: Duration = Duration::from_secs(1);
/// Thumbnails kept for recordings' notifications.
const THUMBNAILS: usize = 10;

/// What this desktop's recorder can do, for the capture overlay.
pub struct Recorder {
    pub how: Method,
}

impl RecordSupport for Recorder {
    fn problem(&mut self, format: &str) -> Option<String> {
        warm_up(); // (by the time an area is chosen, the GPU's encoders are known)
        record::problem(self.how, format)
    }
    fn can_span(&self) -> bool {
        self.how.can_span()
    }
}

/// Find out in the background which GStreamer parts are installed (the
/// GPU's encoders too), so neither the overlay's record tool nor a
/// recording's start waits for it. Once a run.
pub fn warm_up() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let how = record::method();
        if !matches!(how, Method::Portal | Method::TestGst) || !util::have("gst-inspect-1.0") {
            return;
        }
        let elements = [
            "pipewiresrc",
            "x264enc",
            "openh264enc",
            "h264parse",
            "vp8enc",
            "videoconvert",
            "videocrop",
            "videorate",
            "videoscale",
            "mp4mux",
            "webmmux",
            "pulsesrc",
            "opusenc",
            "avenc_aac",
            "vah264enc",
            "vah264lpenc",
            "nvh264enc",
        ];
        for e in elements {
            let _ = std::thread::Builder::new().name("flatshot-gst".into()).spawn(move || record::gst_has(e));
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Recording,
    Paused,
    Saving,
    Done,
}

/// What to do once the current segment's file is finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    Resume,
    Save,
    Discard,
}

/// How a recording ended.
#[derive(Debug)]
pub enum End {
    /// where, and how long (when ffprobe could tell)
    Saved(PathBuf, Option<Duration>),
    /// discarded, or the desktop's screen sharing was declined
    Discarded,
    Failed(String),
}

/// A finished video: where, and how long.
type Saved = (PathBuf, Option<Duration>);

/// Things the app is to do for the recording.
#[derive(Debug)]
pub enum Event {
    /// the first segment started: show the controls
    Started,
    /// it's being finished: the controls go
    Saving,
    Ended(End),
}

pub struct Recording {
    pub state: State,
    target: Target,
    opts: Options,
    how: Method,
    cfg: Config,
    shot: Shot,
    thumbnail: Option<Image>,
    /// when the first segment may start (the overlays gone by then, and
    /// the countdown over)
    start_at: Option<Instant>,
    /// the countdown before it: when it began, and how long it is
    counting: Option<(Instant, Duration)>,
    cast: Option<Arc<ScreenCast>>,
    /// the screen cast has been asked for
    cast_asked: bool,
    cast_rx: Option<mpsc::Receiver<Result<ScreenCast, Failure>>>,
    proc: Option<Child>,
    /// it encodes with the GPU
    gpu: bool,
    /// the recorder's process descriptor is watched by the event loop
    watched: bool,
    /// it didn't stop when asked, and was killed
    killed: bool,
    /// a part was lost that way
    lost: bool,
    stop_signal: rustix::process::Signal,
    stopping_since: Option<Instant>,
    then: Option<Then>,
    segments: Vec<PathBuf>,
    /// recorded before the current segment
    recorded: Duration,
    segment_start: Option<Instant>,
    /// a private folder for the segments, gone at the end
    dir: PathBuf,
    saving: Option<mpsc::Receiver<Result<Saved, String>>>,
    pub can_pause: bool,
    events: Vec<Event>,
    /// wakes the event loop from the saving thread
    waker: Waker,
}

/// A private folder for a recording's segments: in the cache (on disk,
/// not in memory as /tmp can be), named after this process so a crashed
/// run's leftovers can be told apart and cleared.
fn segment_folder() -> Result<PathBuf, String> {
    let base = config::cache_home().join(crate::APP_ID);
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    clear_leftovers(&base);
    let dir = base.join(format!("recording-{}-{}", std::process::id(), util::random_hex(4)));
    fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Segment folders of recordings whose process is gone.
fn clear_leftovers(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("recording-").and_then(|r| r.split('-').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

/// Move the finished video to `target` (or `name_1`, ... if that's taken),
/// never over another file. Within a filesystem it's renamed; else copied.
fn place(src: &Path, target: &Path) -> Result<PathBuf, String> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let stem = target.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Recording".into());
    let ext = target.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for n in 0..10_000 {
        let candidate = if n == 0 { target.to_path_buf() } else { target.with_file_name(format!("{stem}_{n}.{ext}")) };
        match renameat_with(CWD, src, CWD, &candidate, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(candidate),
            Err(rustix::io::Errno::EXIST) => continue,
            Err(_) => {
                // Another filesystem (or one without no-replace renames): copied.
                let out = fs::OpenOptions::new().write(true).create_new(true).mode(0o644).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&candidate);
                let mut out = match out {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(format!("{}: {e}", candidate.display())),
                };
                let mut input = fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
                if let Err(e) = std::io::copy(&mut input, &mut out) {
                    let _ = fs::remove_file(&candidate);
                    return Err(format!("{}: {e}", candidate.display()));
                }
                let _ = fs::remove_file(src);
                return Ok(candidate);
            }
        }
    }
    Err(format!("no free file name next to {}", target.display()))
}

impl Recording {
    /// `refresh`: how often the area's monitor shows a new picture, if known.
    pub fn new(job: RecordJob, monitors: &[Monitor], refresh: Option<f64>, cfg: Config, waker: Waker) -> Result<Recording, String> {
        let RecordJob { area, output, scale, shot, thumbnail, format, fps, mic, system_audio, cursor, countdown } = job;
        let target = Target {
            rect: area,
            monitor: output.rect,
            monitor_name: output.name,
            scale: scale as f64,
            monitors: monitors.iter().map(|m| (m.name.clone(), m.rect)).collect(),
            refresh,
        };
        // (0: native)
        let opts = Options { format, fps: (fps > 0).then(|| fps.clamp(1, 120) as u32), mic, system_audio, cursor };
        let (now, countdown) = (Instant::now(), Duration::from_secs(countdown.into()));
        Ok(Recording {
            state: State::Starting,
            target,
            opts,
            how: record::method(),
            cfg,
            shot,
            // (only a small copy is kept while recording)
            thumbnail: small_thumbnail(thumbnail),
            start_at: Some(now + countdown.max(SETTLE)),
            counting: (!countdown.is_zero()).then_some((now, countdown)),
            cast: None,
            cast_asked: false,
            cast_rx: None,
            proc: None,
            gpu: false,
            watched: false,
            killed: false,
            lost: false,
            stop_signal: rustix::process::Signal::INT,
            stopping_since: None,
            then: None,
            segments: Vec::new(),
            recorded: Duration::ZERO,
            segment_start: None,
            dir: segment_folder()?,
            saving: None,
            can_pause: record::can_join(),
            events: Vec::new(),
            waker,
        })
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn area(&self) -> IRect {
        self.target.rect
    }

    pub fn mic(&self) -> bool {
        !self.opts.audio().is_empty() && self.opts.mic
    }

    pub fn elapsed(&self) -> Duration {
        self.recorded + self.segment_start.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// When the countdown before recording ends, while it runs.
    pub fn countdown_end(&self) -> Option<Instant> {
        let (began, total) = self.counting?;
        (self.state == State::Starting && Instant::now() < began + total).then_some(began + total)
    }

    /// The countdown, while it runs: the seconds left (as shown), and how
    /// much of it is left (1 to 0).
    pub fn countdown(&self) -> Option<(u64, f32)> {
        let end = self.countdown_end()?;
        Some(countdown::dial_shows(end.saturating_duration_since(Instant::now()), self.counting?.1))
    }

    // -- control -------------------------------------------------------------------------

    pub fn toggle_pause(&mut self) {
        match self.state {
            State::Recording if self.can_pause => {
                self.state = State::Paused;
                self.end_segment(None);
            }
            State::Paused => {
                if self.proc.is_none() {
                    self.begin_segment();
                } else {
                    self.then = Some(Then::Resume); // (once the last segment is written)
                }
            }
            _ => {}
        }
    }

    /// Finish and save.
    pub fn stop(&mut self) {
        match self.state {
            State::Recording | State::Paused => {
                self.state = State::Saving;
                self.events.push(Event::Saving);
                self.end_segment(Some(Then::Save));
            }
            State::Starting => self.discard(),
            _ => {}
        }
    }

    pub fn discard(&mut self) {
        if matches!(self.state, State::Saving | State::Done) {
            return;
        }
        self.state = State::Saving;
        self.events.push(Event::Saving);
        self.end_segment(Some(Then::Discard));
    }

    // -- segments ------------------------------------------------------------------------

    /// Start the next segment's recorder. A new process for the event loop
    /// to watch, if any.
    fn begin_segment(&mut self) {
        if matches!(self.state, State::Saving | State::Done) {
            return;
        }
        let path = self.dir.join(format!("part-{:03}.{}", self.segments.len() + 1, record::segment_extension(&self.opts.format)));
        let launch = match record::launch(self.how, &self.target, &self.opts, &path, self.cast.as_deref()) {
            Ok(l) => l,
            Err(e) => return self.fail(e),
        };
        if self.segments.is_empty() {
            eprintln!("flatshot-rs: recording {}", launch.about);
        }
        let log =
            fs::OpenOptions::new().create(true).append(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(self.dir.join("recorder.log"));
        let err = log.map(Stdio::from).unwrap_or_else(|_| Stdio::null());
        let child = match record::spawn(&launch.argv, launch.stream.as_ref(), Stdio::null(), err) {
            Ok(c) => c,
            Err(e) => return self.fail(format!("{}: {e}", launch.argv[0])),
        };
        drop(launch.stream); // (the recorder has its own)
        self.stop_signal = launch.stop;
        self.gpu = launch.gpu;
        self.segments.push(path);
        self.segment_start = Some(Instant::now());
        self.proc = Some(child);
        self.watched = false;
        let first = self.state == State::Starting;
        self.state = State::Recording;
        if first {
            self.events.push(Event::Started);
        }
    }

    /// Ask the recorder to finish its file; `then` once it has.
    fn end_segment(&mut self, then: Option<Then>) {
        if let Some(t) = self.segment_start.take() {
            self.recorded += t.elapsed();
        }
        self.then = then;
        match &self.proc {
            None => {
                if let Some(t) = self.then.take() {
                    self.after_stop(t);
                }
            }
            Some(child) => {
                if self.stopping_since.is_none() {
                    self.stopping_since = Some(Instant::now());
                    record::kill_group(child, self.stop_signal);
                }
            }
        }
    }

    fn after_stop(&mut self, then: Then) {
        match then {
            Then::Resume => self.begin_segment(),
            Then::Save => self.save(),
            Then::Discard => self.done(End::Discarded),
        }
    }

    /// Has the recorder ended? (Called when its process descriptor says
    /// so, or on a timer.)
    pub fn check(&mut self) {
        let Some(child) = &mut self.proc else { return };
        let status = match child.try_wait() {
            Ok(None) => {
                if self.stopping_since.is_some_and(|t| t.elapsed() > STOP_TIMEOUT) {
                    record::kill_group(child, rustix::process::Signal::KILL);
                    self.killed = true;
                }
                return;
            }
            Ok(Some(s)) => s.to_string(),
            Err(e) => e.to_string(),
        };
        let stopping = self.stopping_since.take().is_some();
        self.proc = None;
        if std::mem::take(&mut self.killed)
            && let Some(last) = self.segments.last()
            && !record::playable(last)
        {
            // (killed before it finished the file: what it wrote can't be used)
            crate::warn!("the recorder had to be stopped by force; its last part is lost: {}", self.log_tail());
            if let Some(last) = self.segments.pop() {
                let _ = fs::remove_file(last);
            }
            self.lost = true;
        }
        if stopping {
            if let Some(t) = self.then.take() {
                self.after_stop(t);
            }
            return;
        }
        // The recorder quit by itself: keep what it recorded, if anything.
        let ran = self.segment_start.map(|t| t.elapsed()).unwrap_or_default();
        self.recorded = self.elapsed();
        self.segment_start = None;
        if ran < BROKEN
            && let Some(last) = self.segments.pop()
        {
            let _ = fs::remove_file(last);
            // The GPU's encoder didn't start (a driver that can't, a size it
            // won't take): the CPU's, from now on, and this recording goes
            // on with it. (Only at the start: parts made by different
            // encoders may not join.)
            if self.gpu && self.segments.is_empty() && self.state == State::Recording {
                crate::warn!("the GPU's video encoder didn't start, so the CPU's records instead: {}", self.log_tail());
                record::gpu_failed();
                self.recorded = Duration::ZERO;
                return self.begin_segment();
            }
        }
        if self.state == State::Recording && self.has_video() {
            crate::warn!("the recorder stopped by itself: {}", self.log_tail());
            self.state = State::Saving;
            self.events.push(Event::Saving);
            self.save();
        } else {
            let tail = self.log_tail();
            self.fail(if tail.is_empty() { format!("the recorder {status}") } else { tail });
        }
    }

    fn has_video(&self) -> bool {
        self.segments.iter().any(|p| fs::metadata(p).is_ok_and(|m| m.len() > 0))
    }

    /// The recorder's last words.
    fn log_tail(&self) -> String {
        let text = util::read_limited(&self.dir.join("recorder.log"), 1 << 20).unwrap_or_default();
        let text = String::from_utf8_lossy(&text);
        util::elide(text.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or(""), 300)
    }

    /// The screen cast's answer, when it comes.
    fn poll_cast(&mut self) {
        let Some(rx) = &self.cast_rx else { return };
        let Ok(answer) = rx.try_recv() else { return };
        self.cast_rx = None;
        if self.state != State::Starting {
            return; // (discarded while the desktop was still asking)
        }
        match answer {
            Ok(cast) => {
                self.cast = Some(Arc::new(cast));
                // (else once the countdown's over)
                if self.start_at.is_none() {
                    self.begin_segment();
                }
            }
            Err(Failure::Cancelled) => self.done(End::Discarded),
            Err(Failure::Failed(e)) => self.fail(e),
        }
    }

    // -- the end -------------------------------------------------------------------------

    fn save(&mut self) {
        if let Some(c) = self.cast.take() {
            c.close();
        }
        let parts: Vec<PathBuf> = self.segments.iter().filter(|p| fs::metadata(p).is_ok_and(|m| m.len() > 0)).cloned().collect();
        if parts.is_empty() {
            if self.lost {
                return self.fail("The recorder didn't finish its file when asked to stop.".into());
            }
            let tail = self.log_tail();
            return self.fail(if tail.is_empty() { "Nothing was recorded.".into() } else { tail });
        }
        let crop = self.target.local_pixels();
        let target =
            template::target_path(&self.cfg.record_dir, &self.cfg.record_filename, &self.shot, (crop.w as u32, crop.h as u32), &self.opts.format, false);
        let (dir, format, fps, waker) = (self.dir.clone(), self.opts.format.clone(), self.opts.fps, self.waker.clone());
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("flatshot-saving".into()).spawn(move || {
            let result = (|| {
                let ext = record::segment_extension(&format);
                let video = if parts.len() > 1 {
                    let joined = dir.join(format!("joined.{ext}"));
                    record::join(&parts, &joined)?;
                    joined
                } else {
                    parts[0].clone()
                };
                let finished = if format == "gif" {
                    let gif = dir.join("recording.gif");
                    record::to_gif(&video, &gif, fps)?;
                    gif
                } else {
                    video
                };
                let placed = place(&finished, &target)?;
                let length = record::duration(&placed).map(Duration::from_secs_f64);
                Ok((placed, length))
            })();
            let _ = tx.send(result);
            waker();
        });
        match spawned {
            Ok(_) => self.saving = Some(rx),
            Err(e) => self.fail(e.to_string()),
        }
    }

    fn poll_saving(&mut self) {
        let Some(rx) = &self.saving else { return };
        match rx.try_recv() {
            Ok(Ok((path, length))) => {
                self.saving = None;
                self.done(End::Saved(path, length));
            }
            Ok(Err(e)) => {
                self.saving = None;
                self.fail(e);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.saving = None;
                self.fail("saving the recording failed".into());
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn fail(&mut self, message: String) {
        self.done(End::Failed(message));
    }

    fn done(&mut self, end: End) {
        if self.state == State::Done {
            return;
        }
        if let Some(child) = self.proc.take() {
            record::kill_group(&child, rustix::process::Signal::KILL);
            let mut child = child;
            let _ = child.wait();
        }
        if let Some(c) = self.cast.take() {
            c.close();
        }
        let _ = fs::remove_dir_all(&self.dir);
        self.state = State::Done;
        self.events.push(Event::Ended(end));
    }

    /// When it next needs a look (the event loop sleeps until then).
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut at: Vec<Instant> = Vec::new();
        if let Some(t) = self.start_at {
            at.push(t);
        }
        if let Some(t) = self.stopping_since {
            at.push(t + STOP_TIMEOUT);
        }
        if self.proc.is_some() && !self.watched {
            at.push(Instant::now() + POLL);
        }
        if self.state == State::Recording && self.segment_start.is_some() {
            // The clock's next second.
            let into = self.elapsed().subsec_nanos() as u64;
            at.push(Instant::now() + Duration::from_nanos(1_000_000_000 - into));
        }
        at.into_iter().min()
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Never leave a recorder running: asked to finish, then made to.
        if let Some(mut child) = self.proc.take() {
            record::kill_group(&child, self.stop_signal);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            record::kill_group(&child, rustix::process::Signal::KILL);
            let _ = child.wait();
        }
    }
}

// -- the app's side ----------------------------------------------------------------------

/// A recording, and its controls on screen.
pub struct RecordingRun {
    pub rec: Recording,
    /// the bar and marks have been put up (for the countdown, or since it started)
    controls: bool,
    /// the countdown's dial over the area: the scale it was drawn at, and what it showed
    dial: Option<(f32, Option<countdown::Shows>)>,
    bar: Option<RecBar>,
    /// the bar's monitor and place on it (logical), as a layer surface
    bar_place: Option<(Monitor, (i32, i32))>,
    bar_drag: Option<BarDrag>,
    /// the marks shown: (number, corner, the scale drawn at)
    marks: Vec<(u8, Corner, f32)>,
    /// the clock last shown
    shown: String,
}

/// The bar being dragged: where it was, where it was held, and how far
/// the pointer has moved since.
#[derive(Clone, Copy, Debug)]
struct BarDrag {
    from: (i32, i32),
    grab: Point,
    moved: (f64, f64),
}

struct BarPaint<'a>(&'a mut RecBar);

impl Painted for BarPaint<'_> {
    fn damage(&mut self, _: usize) -> Damage {
        self.0.damage()
    }
    fn paint(&self, _: usize, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        self.0.paint(target, origin, region);
    }
    fn buffer_size(&self, _: usize) -> (u32, u32) {
        self.0.device_size()
    }
}

struct MarkPaint {
    corner: Corner,
    scale: f32,
    damaged: bool,
}

impl Painted for MarkPaint {
    fn damage(&mut self, _: usize) -> Damage {
        if std::mem::take(&mut self.damaged) { Damage::full() } else { Damage::new() }
    }
    fn paint(&self, _: usize, target: &mut Canvas<'_>, origin: (i32, i32), _: IRect) {
        recbar::paint_mark(self.corner, self.scale, target, origin);
    }
    fn buffer_size(&self, _: usize) -> (u32, u32) {
        let s = (recbar::MARK as f32 * self.scale).ceil() as u32;
        (s, s)
    }
}

impl App {
    /// The overlay chose an area to record: start.
    pub(super) fn start_recording(&mut self, job: RecordJob) -> bool {
        if self.recording.is_some() {
            return false;
        }
        let monitors = self.dpy.monitors();
        // (an area across monitors: the fastest of them)
        let refresh = monitors
            .iter()
            .filter(|m| m.name == job.output.name || m.rect.intersects(&job.area))
            .filter_map(|m| self.dpy.refresh_rate(m))
            .max_by(f64::total_cmp);
        let rec = match Recording::new(job, &monitors, refresh, self.cfg.clone(), self.waker.clone()) {
            Ok(r) => r,
            Err(e) => {
                self.report_failure("Recording failed", &e);
                return false;
            }
        };
        let counting = rec.countdown().is_some();
        self.recording =
            Some(RecordingRun { rec, controls: false, dial: None, bar: None, bar_place: None, bar_drag: None, marks: Vec::new(), shown: String::new() });
        if counting {
            // Counting down: the bar (one button, that cancels it) and the
            // marks show where it'll be, the dial in the middle; none of
            // them takes the keyboard, and only the bar the pointer.
            self.show_controls();
            self.show_dial();
        }
        self.recording_changed();
        self.refresh_tray_menu();
        true
    }

    pub(super) fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Stop and save the recording in progress.
    pub(super) fn stop_recording(&mut self) {
        if let Some(r) = &mut self.recording {
            r.rec.stop();
        }
        self.serve_recording();
    }

    pub(super) fn recording_deadline(&self) -> Option<Instant> {
        let r = self.recording.as_ref()?;
        let next = r.rec.next_deadline();
        let Some(end) = r.rec.countdown_end() else { return next };
        // Counting down: its ring runs on (where frames don't come by
        // themselves), and the dial goes a moment before the end.
        let mut tick = Instant::now() + countdown::FRAME;
        if r.dial.is_some() {
            tick = tick.min(end.checked_sub(countdown::GONE_BEFORE).unwrap_or(end));
        }
        Some(next.map_or(tick, |n| n.min(tick)))
    }

    /// Start what's due, hear the recorder, the portal and the saving.
    pub(super) fn poll_recording(&mut self) {
        let Some(r) = &mut self.recording else { return };
        let rec = &mut r.rec;
        // The screen cast is asked for at once: the desktop may ask which
        // screen to share, which can be answered while it counts down.
        if rec.how == Method::Portal && !rec.cast_asked && rec.state == State::Starting {
            rec.cast_asked = true;
            let (tx, rx) = mpsc::channel();
            let (target, cursor, waker) = (rec.target.clone(), rec.opts.cursor, self.waker.clone());
            let spawned = std::thread::Builder::new().name("flatshot-screencast".into()).spawn(move || {
                let _ = tx.send(ScreenCast::open(&target, cursor));
                waker();
            });
            match spawned {
                Ok(_) => rec.cast_rx = Some(rx),
                Err(e) => rec.fail(e.to_string()),
            }
        }
        if rec.start_at.is_some_and(|t| t <= Instant::now()) {
            rec.start_at = None;
            // (the portal's stream, once it's there: see poll_cast)
            if rec.how != Method::Portal || rec.cast.is_some() {
                rec.begin_segment();
            }
        }
        rec.poll_cast();
        rec.check();
        rec.poll_saving();
        // The dial is over the area: it's gone before the recording starts.
        if r.dial.is_some() && r.rec.countdown_end().is_none_or(|end| Instant::now() + countdown::GONE_BEFORE >= end) {
            self.hide_dial();
        }
        self.serve_recording();
    }

    /// Watch the recorder's process (when it ends, the loop wakes), and do
    /// what the recording asked for.
    fn serve_recording(&mut self) {
        let Some(r) = &mut self.recording else { return };
        if let Some(child) = &r.rec.proc
            && !r.rec.watched
        {
            r.rec.watched = true;
            let pid = rustix::process::Pid::from_child(child);
            match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
                Ok(fd) => {
                    let source = Generic::new(fd, Interest::READ, PollMode::Level);
                    let inserted = self.loop_handle.insert_source(source, |_, _, app: &mut App| {
                        if let Some(r) = &mut app.recording {
                            r.rec.check();
                        }
                        app.serve_recording();
                        Ok(PostAction::Remove)
                    });
                    if inserted.is_err() {
                        r.rec.watched = false;
                    }
                }
                Err(_) => r.rec.watched = false, // (polled instead)
            }
        }
        let events = r.rec.take_events();
        for e in events {
            match e {
                Event::Started => {
                    self.hide_dial();
                    self.show_controls();
                }
                Event::Saving => self.hide_controls(),
                Event::Ended(end) => {
                    self.hide_controls();
                    self.recording_ended(end);
                    return;
                }
            }
        }
        self.recording_changed();
    }

    /// The bar's clock and the tray icon follow the recording.
    fn recording_changed(&mut self) {
        let Some(r) = &mut self.recording else {
            if let Some(t) = &self.tray.tray {
                t.set_recording(false);
            }
            return;
        };
        let clock = record::clock(r.rec.elapsed());
        let paused = r.rec.state == State::Paused;
        let countdown = r.rec.countdown().map(|c| c.0);
        if let Some(bar) = &mut r.bar {
            // Until it starts, the bar cancels it (with the seconds left
            // where there's no dial to show them).
            let cancel = (r.rec.state == State::Starting).then(|| match countdown {
                Some(n) if !self.dpy.can_place() => format!("Cancel  ·  {n}"),
                _ => "Cancel".to_string(),
            });
            bar.set_cancel(cancel);
            bar.set_state(clock.clone(), paused);
        }
        if clock != r.shown || r.rec.state != State::Recording {
            r.shown = clock.clone();
            if let Some(t) = &self.tray.tray {
                t.set_recording(true);
                let state = match (r.rec.state, countdown) {
                    (State::Starting, Some(n)) => format!("Recording in {n}  ·  click to cancel"),
                    (State::Starting, None) => "Starting to record  ·  click to cancel".into(),
                    (State::Paused, _) => format!("Recording paused  {clock}  ·  click to stop"),
                    (State::Saving | State::Done, _) => format!("Saving the recording  {clock}"),
                    (State::Recording, _) => format!("Recording  {clock}  ·  click to stop"),
                };
                t.set_tooltip(&format!("{}: {state}", crate::APP_NAME));
            }
        }
    }

    // -- the controls ----------------------------------------------------------------------

    /// The bar and the marks (once: put up for the countdown, the bar turns
    /// into the recording's controls when it starts).
    fn show_controls(&mut self) {
        let Some(r) = &mut self.recording else { return };
        if std::mem::replace(&mut r.controls, true) {
            self.recording_changed();
            return;
        }
        let area = r.rec.area();
        let monitors = self.dpy.monitors();
        let own = monitors.iter().find(|m| m.name == r.rec.target.monitor_name).or_else(|| monitors.iter().find(|m| m.rect.contains(area.center()))).cloned();
        let others: Vec<&Monitor> = monitors.iter().filter(|m| Some(&m.name) != own.as_ref().map(|o| &o.name)).collect();
        let bar = RecBar::new(r.rec.can_pause, r.rec.mic());
        let size = bar.size();
        let mut spot = own.as_ref().and_then(|m| recbar::bar_spot(area, m.rect, size));
        for m in &others {
            spot = spot.or_else(|| recbar::bar_spot(area, m.rect, size));
        }
        let (w, h) = (size.0 as i32, size.1 as i32);
        let mut no_bar = false;
        if spot.is_none() {
            if let Some(m) = others.first() {
                spot = Some(IRect::new(m.rect.center().x - w / 2, m.rect.y2() - h - 48, w, h));
            } else if !self.one_shot && self.tray.tray.is_some() {
                // Nowhere outside the recording: the tray icon stops it.
                no_bar = true;
            } else {
                spot = Some(IRect::new(area.center().x - w / 2, area.y2() - h - 24, w, h));
            }
        }
        let namespace = super::layer_namespace();
        if let Some(spot) = spot {
            let on = monitors.iter().find(|m| m.rect.contains(spot.center())).or(own.as_ref());
            let placed = match on {
                Some(m) => {
                    let at = (spot.x - m.rect.x, spot.y - m.rect.y);
                    let ok = self.dpy.add_control(Owner::RecBar, m, at, size, &namespace, false);
                    if ok {
                        r.bar_place = Some((m.clone(), at));
                    }
                    ok
                }
                None => false,
            };
            if !placed {
                // (no layer-shell: a small window the desktop places)
                match self.dpy.add_window(Owner::RecBar, &format!("{} recording", crate::APP_NAME), size, false) {
                    Ok(()) => self.dpy.set_transparent(Owner::RecBar),
                    Err(e) => crate::warn!("no room for the recording's controls: {e}"),
                }
            }
            r.bar = Some(bar);
        }
        // Marks round the area, when they fit outside it.
        let screen = if r.rec.target.spans() { record::workspace(&r.rec.target.monitors) } else { own.as_ref().map(|m| m.rect).unwrap_or(area) };
        if area.intersected(&screen) != screen && self.dpy.can_place() {
            for (i, (corner, rect)) in recbar::mark_rects(area).into_iter().enumerate() {
                let Some(m) = monitors.iter().find(|m| m.rect.intersects(&rect)) else { continue };
                if self.dpy.add_control(
                    Owner::RecMark(i as u8),
                    m,
                    (rect.x - m.rect.x, rect.y - m.rect.y),
                    (recbar::MARK as u32, recbar::MARK as u32),
                    &namespace,
                    true,
                ) {
                    r.marks.push((i as u8, corner, 0.0));
                }
            }
        }
        self.dpy.flush();
        if no_bar {
            self.notify(notify::Note {
                summary: "Recording the whole screen".into(),
                body: "Click the Flatshot RS tray icon, or press the shortcut again, to stop.".into(),
                event: "notice",
                ..Default::default()
            });
        }
        self.recording_changed(); // (the bar cancels it, until it starts)
    }

    /// The countdown's dial, over the middle of the area. It takes neither
    /// the keyboard nor the pointer: clicks go to what's under it.
    fn show_dial(&mut self) {
        let Some(r) = &mut self.recording else { return };
        if r.rec.countdown().is_none() || !self.dpy.can_place() {
            return;
        }
        let area = r.rec.area();
        let monitors = self.dpy.monitors();
        let Some(m) = monitors.iter().find(|m| m.rect.contains(area.center())).or_else(|| monitors.iter().find(|m| m.name == r.rec.target.monitor_name)) else {
            return;
        };
        let size = countdown::DIAL.ceil() as i32;
        let at = (
            (area.center().x - size / 2 - m.rect.x).clamp(0, (m.rect.w - size).max(0)),
            (area.center().y - size / 2 - m.rect.y).clamp(0, (m.rect.h - size).max(0)),
        );
        if self.dpy.add_control(Owner::RecDial, m, at, (size as u32, size as u32), &super::layer_namespace(), true) {
            r.dial = Some((0.0, None));
        }
        self.dpy.flush();
    }

    fn hide_dial(&mut self) {
        let Some(r) = &mut self.recording else { return };
        if r.dial.take().is_some() {
            self.dpy.remove(Owner::RecDial);
            self.dpy.flush();
        }
    }

    fn hide_controls(&mut self) {
        self.hide_dial();
        let Some(r) = &mut self.recording else { return };
        if r.bar.take().is_some() {
            self.dpy.remove(Owner::RecBar);
        }
        r.bar_place = None;
        r.bar_drag = None;
        for (i, _, _) in std::mem::take(&mut r.marks) {
            self.dpy.remove(Owner::RecMark(i));
        }
        self.dpy.flush();
    }

    /// Input on the bar.
    pub(super) fn recbar_input(&mut self, ev: Input) {
        let Some(r) = &mut self.recording else { return };
        let Some(bar) = &mut r.bar else { return };
        let p = |at: (f64, f64)| Point::new(at.0 as f32, at.1 as f32);
        let mut action = None;
        let mut dropped = false;
        match ev {
            Input::Enter(at) | Input::Motion(at, _) => {
                bar.pointer_motion(Some(p(at)));
                if !self.dpy.has_relative()
                    && let Some(d) = &mut r.bar_drag
                {
                    // (no relative motion: where the pointer is on the bar,
                    // as compositors report it while a button is held,
                    // relative to where the bar was when it was pressed)
                    d.moved = ((at.0 as f32 - d.grab.x) as f64, (at.1 as f32 - d.grab.y) as f64);
                    self.place_recbar();
                    return;
                }
            }
            Input::Leave => bar.pointer_motion(None),
            Input::Button(0x110, true, at) if !bar.on_button(p(at)) => {
                if let Some((_, place)) = &r.bar_place {
                    r.bar_drag = Some(BarDrag { from: *place, grab: p(at), moved: (0.0, 0.0) });
                } else {
                    self.dpy.start_move(Owner::RecBar);
                }
            }
            Input::Button(0x110, pressed, at) => {
                dropped = !pressed && r.bar_drag.take().is_some();
                action = bar.button(pressed, p(at));
            }
            _ => {}
        }
        match action {
            Some(BarAction::Pause) => r.rec.toggle_pause(),
            Some(BarAction::Stop) => r.rec.stop(),
            Some(BarAction::Discard | BarAction::Cancel) => r.rec.discard(),
            None => {}
        }
        if dropped {
            self.rehome_recbar();
        }
        self.serve_recording();
    }

    /// The pointer moved by (`dx`, `dy`): the bar, when dragged, follows.
    /// True if it was dragged.
    pub(super) fn drag_recbar(&mut self, dx: f64, dy: f64) -> bool {
        let Some(d) = self.recording.as_mut().and_then(|r| r.bar_drag.as_mut()) else { return false };
        d.moved = (d.moved.0 + dx, d.moved.1 + dy);
        self.place_recbar();
        true
    }

    /// Put the dragged bar where the pointer took it: on the monitor its
    /// middle is over, whichever that is (it's that monitor's once let go:
    /// see `rehome_recbar`).
    fn place_recbar(&mut self) {
        let monitors = self.dpy.monitors();
        let Some(r) = &mut self.recording else { return };
        let (Some(d), Some((m, at))) = (r.bar_drag, &mut r.bar_place) else { return };
        let size = r.bar.as_ref().map(|b| b.size()).unwrap_or((1, 1));
        let (w, h) = (size.0 as i32, size.1 as i32);
        let home = m.rect;
        let x = home.x + (d.from.0 as f64 + d.moved.0).round() as i32;
        let y = home.y + (d.from.1 as f64 + d.moved.1).round() as i32;
        let middle = IPoint::new(x + w / 2, y + h / 2);
        let on = monitors.iter().find(|m| m.rect.contains(middle)).map_or(home, |m| m.rect);
        let x = x.clamp(on.x, (on.x2() - w).max(on.x)) - home.x;
        let y = y.clamp(on.y, (on.y2() - h).max(on.y)) - home.y;
        if (x, y) != *at {
            *at = (x, y);
            self.dpy.place_layer(Owner::RecBar, m, (x, y), size);
        }
    }

    /// The bar let go over another monitor is that monitor's now (a layer
    /// surface stays on the monitor it was made on: one of its own there).
    fn rehome_recbar(&mut self) {
        let monitors = self.dpy.monitors();
        let Some(r) = &mut self.recording else { return };
        let (Some(bar), Some((m, at))) = (&mut r.bar, &mut r.bar_place) else { return };
        let size = bar.size();
        let g = IRect::new(m.rect.x + at.0, m.rect.y + at.1, size.0 as i32, size.1 as i32);
        let Some(to) = monitors.iter().find(|n| n.rect.contains(g.center())).cloned() else { return };
        if to.name == m.name && to.rect == m.rect {
            return;
        }
        let place = (g.x - to.rect.x, g.y - to.rect.y);
        *m = to.clone();
        *at = place;
        bar.invalidate();
        if self.dpy.is_x11() {
            self.dpy.place_layer(Owner::RecBar, &to, place, size);
        } else {
            self.dpy.remove(Owner::RecBar);
            if !self.dpy.add_control(Owner::RecBar, &to, place, size, &super::layer_namespace(), false) {
                crate::warn!("the recording's controls could not go over to {}", to.name);
            }
        }
        self.dpy.flush();
    }

    /// Draw the bar and the marks, if they changed.
    pub(super) fn redraw_recording(&mut self) {
        let Some(r) = &mut self.recording else { return };
        for owner in self.dpy.owners() {
            let Some((_, scale)) = self.dpy.surface(owner) else { continue };
            match owner {
                Owner::RecBar => {
                    if let Some(bar) = &mut r.bar {
                        bar.set_scale(scale);
                        self.dpy.redraw(owner, 0, &mut BarPaint(bar));
                    }
                }
                Owner::RecMark(n) => {
                    // (drawn once; a new scale draws it again)
                    if let Some(m) = r.marks.iter_mut().find(|m| m.0 == n)
                        && m.2 != scale
                        && self.dpy.redraw(owner, 0, &mut MarkPaint { corner: m.1, scale, damaged: true })
                    {
                        m.2 = scale;
                    }
                }
                Owner::RecDial => {
                    // (when its second or ring changed)
                    if let (Some((drawn_at, drawn)), Some((seconds, fraction))) = (&mut r.dial, r.rec.countdown()) {
                        let shows = countdown::shows(seconds, fraction);
                        if (*drawn != Some(shows) || *drawn_at != scale) && self.dpy.redraw(owner, 0, &mut DialPaint { scale, seconds, fraction }) {
                            (*drawn_at, *drawn) = (scale, Some(shows));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // -- the finished recording --------------------------------------------------------

    fn recording_ended(&mut self, end: End) {
        let Some(r) = self.recording.take() else { return };
        let RecordingRun { rec, .. } = r;
        if let Some(t) = &self.tray.tray {
            t.set_recording(false);
        }
        let code = match end {
            End::Saved(path, length) => {
                self.deliver_recording(&path, length, &rec);
                0
            }
            End::Discarded => 1,
            End::Failed(e) => {
                self.report_failure("Recording failed", &e);
                2
            }
        };
        drop(rec);
        self.refresh_tray_menu();
        crate::mem::trim();
        if self.one_shot {
            self.finished = Some(code);
        }
        if self.quit_after_recording {
            self.exit = Some(if self.one_shot { code } else { 0 });
        }
    }

    /// Copy, open, run the command and announce the saved recording.
    fn deliver_recording(&mut self, path: &Path, length: Option<Duration>, rec: &Recording) {
        crate::util::print_line(&path.to_string_lossy());
        let cfg = rec.cfg.clone();
        let copied = match cfg.clipboard.as_str() {
            "image" => self.copy_file(path),
            "path" => {
                let text = Arc::new(path.to_string_lossy().into_owned().into_bytes());
                let mimes: Vec<String> = TEXT_MIMES.iter().map(|m| m.to_string()).collect();
                if let Some(helper) = self.dpy.copy_helper().filter(|_| self.one_shot) {
                    deliver::hand_to(helper, &mimes[0], &text)
                } else {
                    self.dpy.offer_each(mimes.into_iter().map(|m| (m, text.clone())).collect())
                }
            }
            _ => false,
        };
        self.dpy.flush();
        match cfg.open_after.as_str() {
            "image" => {
                deliver::open_file(path);
            }
            "folder" => {
                deliver::show_in_folder(path);
            }
            _ => {}
        }
        if !cfg.run_command.trim().is_empty() {
            deliver::run_command(&cfg.run_command, path);
        }
        let length = length.unwrap_or(rec.recorded);
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let thumbnail = rec.thumbnail.as_ref().and_then(|t| save_thumbnail(t, path));
        // (the size it was recorded at: the area in the screen's pixels)
        let r = rec.target.rect;
        let (width, height) = ((r.w as f64 * rec.target.scale).round() as u32, (r.h as f64 * rec.target.scale).round() as u32);
        crate::history::add(&crate::history::Entry {
            path: std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
            time: util::now_ms(),
            kind: "recording".into(),
            width,
            height,
            bytes: size,
            app: rec.shot.app.clone(),
            title: rec.shot.title.clone(),
            monitor: rec.shot.monitor.clone(),
            area: Some(r),
            copied: if copied { cfg.clipboard.clone() } else { String::new() },
            seconds: Some(length.as_secs_f64()),
            thumbnail: thumbnail.clone(),
            ..Default::default()
        });
        self.recent_changed();
        if !cfg.notify {
            return;
        }
        let mut bits = vec![util::displayable(&notify::file_name(path)), record::clock(length), human_size(size)];
        if copied {
            bits.push(if cfg.clipboard == "path" { "path copied" } else { "copied to clipboard" }.to_string());
        }
        let mut note = notify::Note {
            summary: "Recording saved".into(),
            body: bits.join("  ·  "),
            image: thumbnail,
            file: Some(path.to_path_buf()),
            event: "recording",
            ..Default::default()
        };
        let mut target = None;
        if !self.one_shot {
            note.actions = [("default", "Open"), ("open", "Open"), ("folder", "Show in folder")].iter().map(|(k, l)| (k.to_string(), l.to_string())).collect();
            target = Some(NoteTarget { path: path.to_path_buf(), at: None, layout: Vec::new() });
        }
        self.notify_with(note, target);
    }

    /// A file on the clipboard, to paste into file managers and chats.
    fn copy_file(&mut self, path: &Path) -> bool {
        let uri = util::file_uri(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
        if let Some(helper) = self.dpy.copy_helper().filter(|_| self.one_shot) {
            return deliver::hand_to(helper, "text/uri-list", format!("{uri}\r\n").as_bytes());
        }
        let parts = vec![
            ("text/uri-list".to_string(), Arc::new(format!("{uri}\r\n").into_bytes())),
            ("x-special/gnome-copied-files".to_string(), Arc::new(format!("copy\n{uri}").into_bytes())),
        ];
        self.dpy.offer_each(parts)
    }
}

fn human_size(n: u64) -> String {
    if n >= 1_000_000 { format!("{:.1} MB", n as f64 / 1e6) } else { format!("{} KB", (n as f64 / 1e3).round().max(1.0)) }
}

/// The frozen screen at no more than 640 pixels across.
fn small_thumbnail(thumb: Image) -> Option<Image> {
    let (w, h) = thumb.size();
    if w <= 640 && h <= 640 {
        return Some(thumb);
    }
    let k = 640.0 / w.max(h) as f64;
    thumb.scaled_area(((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1)).ok()
}

/// The first frame (the frozen screen), for the notification; the newest
/// few are kept.
fn save_thumbnail(thumb: &Image, video: &Path) -> Option<PathBuf> {
    let folder = config::cache_home().join(crate::APP_ID).join("thumbnails");
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&folder).ok()?;
    let mut old: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&folder)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    old.sort();
    while old.len() >= THUMBNAILS {
        let (_, p) = old.remove(0);
        let _ = fs::remove_file(p);
    }
    let png = crate::codec::encode_png(thumb, false).ok()?;
    let stem = video.file_stem()?.to_string_lossy().into_owned();
    let path = folder.join(format!("{stem}.png"));
    let _ = fs::remove_file(&path);
    deliver::write_unique(&path, &png).ok()
}
