//! Event handling for the protocol objects Flatshot makes itself (SCTK
//! handles its own): buffers, viewports, fractional scale, screencopy
//! frames, text input.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use smithay_client_toolkit::dispatch2::Dispatch2;
use wayland_client::protocol::{wl_buffer, wl_shm_pool};
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::relative_pointer::zv1::client::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1};
use wayland_protocols::wp::text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};

use crate::app::App;

/// User data for objects whose events Flatshot doesn't use (or that have none).
pub struct NoEvents;

macro_rules! no_events {
    ($($i:ty),* $(,)?) => {
        $(
            impl Dispatch2<$i, App> for NoEvents {
                fn event(&self, _: &mut App, _: &$i, _: <$i as Proxy>::Event, _: &Connection, _: &QueueHandle<App>) {}
            }
        )*
    };
}

no_events!(
    wp_viewporter::WpViewporter,
    wp_viewport::WpViewport,
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wl_shm_pool::WlShmPool,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
);

/// A buffer: set free again when the compositor releases it.
pub struct BufferData(pub Arc<AtomicBool>);

impl Dispatch2<wl_buffer::WlBuffer, App> for BufferData {
    fn event(&self, app: &mut App, _: &wl_buffer::WlBuffer, event: wl_buffer::Event, _: &Connection, _: &QueueHandle<App>) {
        if let wl_buffer::Event::Release = event {
            self.0.store(false, Ordering::Release);
            app.wl.buffer_released = true;
        }
    }
}

/// Pointer motion as it moves, wherever surfaces are (for dragging pins).
pub struct RelativeData;

impl Dispatch2<zwp_relative_pointer_v1::ZwpRelativePointerV1, App> for RelativeData {
    fn event(
        &self,
        app: &mut App,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
            app.relative_motion(dx, dy);
        }
    }
}

/// A surface's preferred scale, in 120ths.
pub struct FractionalData(pub wayland_client::backend::ObjectId);

impl Dispatch2<wp_fractional_scale_v1::WpFractionalScaleV1, App> for FractionalData {
    fn event(
        &self,
        app: &mut App,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(s) = app.wl.surfaces.iter_mut().find(|s| s.wl.id() == self.0)
        {
            s.preferred_scale = Some(scale as f32 / 120.0);
        }
    }
}

/// A screencopy frame: which capture it belongs to.
pub struct FrameData(pub usize);

impl Dispatch2<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, App> for FrameData {
    fn event(
        &self,
        app: &mut App,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &Connection,
        qh: &QueueHandle<App>,
    ) {
        app.wl.screencopy_event(self.0, frame, event, qh);
    }
}

/// Text from an input method.
pub struct TextInputData;

impl Dispatch2<zwp_text_input_v3::ZwpTextInputV3, App> for TextInputData {
    fn event(&self, app: &mut App, _: &zwp_text_input_v3::ZwpTextInputV3, event: zwp_text_input_v3::Event, _: &Connection, _: &QueueHandle<App>) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => app.wl.text_focus = Some(surface.id()),
            zwp_text_input_v3::Event::Leave { .. } => {
                app.wl.text_focus = None;
                app.wl.text_enabled = false;
            }
            zwp_text_input_v3::Event::CommitString { text: Some(t) } => app.wl.pending_text.push_str(&t),
            zwp_text_input_v3::Event::Done { .. } => {
                let text = std::mem::take(&mut app.wl.pending_text);
                if !text.is_empty() {
                    app.commit_string(&text);
                }
            }
            _ => {}
        }
    }
}
