//! The windows a desktop reports (KWin's script, sway's tree, Hyprland's
//! clients and monitors): whatever comes back, windows with room in them,
//! and no more of them than there's room for.
#![no_main]

use flatshot::ui::overlay::Window;
use flatshot::ui::state::Desktop;
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fn sane(windows: &[Window]) {
    assert!(windows.len() <= 512);
    for w in windows {
        assert!(w.rect.w > 0 && w.rect.h > 0, "{:?}", w.rect);
        let _ = (w.rect.x2(), w.rect.y2(), w.rect.area());
        assert!(w.title.chars().count() <= 512 && w.app.chars().count() <= 512);
    }
}

fn desktop(d: &Desktop) {
    sane(&d.windows);
    sane(d.active.as_slice());
    assert!(d.cursor.is_none_or(|c| c.x.is_finite() && c.y.is_finite()));
}

fuzz_target!(|report: &str| {
    desktop(&flatshot::desktop::windows::parse_kwin(report));
    let Ok(v) = serde_json::from_str::<Value>(report) else { return };
    desktop(&flatshot::desktop::windows::parse_sway(&v));
    // (a report as both the clients and the monitors, and either beside a plain one)
    let monitors: Value = serde_json::json!([{"activeWorkspace": {"id": 1}, "specialWorkspace": {"id": -98}}]);
    sane(&flatshot::desktop::windows::parse_hyprland(&v, &v));
    sane(&flatshot::desktop::windows::parse_hyprland(&v, &monitors));
    if let Some(clients) = v.get("clients") {
        sane(&flatshot::desktop::windows::parse_hyprland(clients, v.get("monitors").unwrap_or(&monitors)));
    }
});
