//! Capturing with wlr-screencopy (sway, Hyprland, river, Wayfire, ...):
//! the compositor copies each monitor straight into Flatshot's shared
//! memory, which then is the picture, with no copy of our own.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use wayland_client::protocol::{wl_buffer, wl_shm};
use wayland_client::{QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1};

use super::protocols::{BufferData, FrameData, NoEvents};
use super::{Monitor, Wayland};
use crate::app::App;
use crate::image::Image;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrabState {
    Waiting,
    Copying,
    Ready,
    Failed,
}

/// One monitor being copied.
pub struct Grab {
    /// which of the monitors asked for
    pub monitor: usize,
    /// with the mouse pointer drawn in
    pub cursor: bool,
    frame: ZwlrScreencopyFrameV1,
    format: Option<(wl_shm::Format, u32, u32, u32)>,
    img: Option<Image>,
    buffer: Option<wl_buffer::WlBuffer>,
    y_invert: bool,
    pub state: GrabState,
    pub error: String,
}

impl Drop for Grab {
    fn drop(&mut self) {
        if let Some(b) = self.buffer.take() {
            b.destroy();
        }
        self.frame.destroy();
    }
}

impl Wayland {
    /// Start copying `monitors` (each twice, with and without the pointer,
    /// when `both`). Progress comes as events; see `screencopy_done`.
    pub fn start_screencopy(&mut self, monitors: &[Monitor], cursor: bool, both: bool) -> Result<(), String> {
        let Some(manager) = &self.screencopy else { return Err("the compositor has no wlr-screencopy".into()) };
        self.captures.clear();
        for (i, m) in monitors.iter().enumerate() {
            let wants: &[bool] = if both {
                &[false, true]
            } else if cursor {
                &[true]
            } else {
                &[false]
            };
            for &c in wants {
                let id = self.captures.len();
                let frame = manager.capture_output(c as i32, &m.output, &self.qh, FrameData(id));
                self.captures.push(Grab {
                    monitor: i,
                    cursor: c,
                    frame,
                    format: None,
                    img: None,
                    buffer: None,
                    y_invert: false,
                    state: GrabState::Waiting,
                    error: String::new(),
                });
            }
        }
        let _ = self.conn.flush();
        Ok(())
    }

    /// Are all the copies done (or failed)?
    pub fn screencopy_done(&self) -> bool {
        self.captures.iter().all(|g| matches!(g.state, GrabState::Ready | GrabState::Failed))
    }

    /// The pictures: (monitor, with the pointer, picture or why not).
    pub fn take_screencopy(&mut self) -> Vec<(usize, bool, Result<Image, String>)> {
        let grabs = std::mem::take(&mut self.captures);
        grabs
            .into_iter()
            .map(|mut g| {
                let result = match (g.state, g.img.take()) {
                    (GrabState::Ready, Some(mut img)) => {
                        finish(&mut img, g.format.map(|f| f.0), g.y_invert);
                        Ok(img)
                    }
                    _ => Err(if g.error.is_empty() { "the compositor didn't copy the screen".to_string() } else { g.error.clone() }),
                };
                (g.monitor, g.cursor, result)
            })
            .collect()
    }

    pub(crate) fn screencopy_event(&mut self, id: usize, frame: &ZwlrScreencopyFrameV1, event: zwlr_screencopy_frame_v1::Event, qh: &QueueHandle<App>) {
        let shm = self.shm.wl_shm().clone();
        let Some(g) = self.captures.get_mut(id) else { return };
        if &g.frame != frame {
            return;
        }
        use zwlr_screencopy_frame_v1::Event as E;
        match event {
            E::Buffer { format, width, height, stride } => {
                if let WEnum::Value(f) = format
                    && g.format.is_none()
                    && matches!(f, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 | wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888)
                    && stride == width * 4
                {
                    g.format = Some((f, width, height, stride));
                }
                if frame.version() < 3 {
                    copy(g, &shm, qh); // (no buffer_done before version 3)
                }
            }
            E::BufferDone => copy(g, &shm, qh),
            E::Flags { flags: WEnum::Value(f) } => g.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            E::Ready { .. } => g.state = GrabState::Ready,
            E::Failed => {
                g.state = GrabState::Failed;
                g.error = "the compositor refused to copy the screen".into();
            }
            _ => {}
        }
    }
}

use wayland_client::Proxy;

/// The buffer offered is known: make it in shared memory and ask for the copy.
fn copy(g: &mut Grab, shm: &wl_shm::WlShm, qh: &QueueHandle<App>) {
    if g.state != GrabState::Waiting {
        return;
    }
    let Some((format, w, h, stride)) = g.format else {
        g.state = GrabState::Failed;
        g.error = "no 32-bit picture format offered for the screen copy".into();
        return;
    };
    let img = match Image::shared(w, h) {
        Ok(img) => img,
        Err(e) => {
            g.state = GrabState::Failed;
            g.error = e;
            return;
        }
    };
    let Some(fd) = img.fd() else {
        g.state = GrabState::Failed;
        return;
    };
    let pool = shm.create_pool(fd, img.byte_len() as i32, qh, NoEvents);
    let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, BufferData(Arc::new(AtomicBool::new(false))));
    pool.destroy();
    g.frame.copy(&buffer);
    g.buffer = Some(buffer);
    g.img = Some(img);
    g.state = GrabState::Copying;
}

/// Into Flatshot's layout: BGRA rows top to bottom, opaque.
fn finish(img: &mut Image, format: Option<wl_shm::Format>, y_invert: bool) {
    let swap = matches!(format, Some(wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888));
    if swap {
        for px in img.data_mut().as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
    }
    if y_invert {
        let (h, stride) = (img.height() as usize, img.stride());
        let data = img.data_mut();
        for y in 0..h / 2 {
            let (top, bottom) = data.split_at_mut((h - 1 - y) * stride);
            top[y * stride..(y + 1) * stride].swap_with_slice(&mut bottom[..stride]);
        }
    }
    img.force_opaque();
}
