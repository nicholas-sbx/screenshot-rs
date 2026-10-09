//! A stand-in notification server (org.freedesktop.Notifications), for
//! testing: prints each notification it's sent, one per line, and, with
//! FAKE_NOTIFY_CLICK=KEY, clicks that button of each notification that
//! has it (ActionInvoked) half a second later.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use zbus::zvariant::OwnedValue;

struct FakeServer {
    next: AtomicU32,
    click: Option<String>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl FakeServer {
    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "body-markup".into(), "actions".into()]
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        app_name: String,
        _replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        _expire_timeout: i32,
    ) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let mut keys: Vec<&String> = hints.keys().collect();
        keys.sort();
        println!("notify id={id} app={app_name} icon={app_icon} summary={summary:?} body={body:?} actions={actions:?} hints={keys:?}");
        if let Some(click) = &self.click
            && actions.chunks(2).any(|a| &a[0] == click)
        {
            let (conn, click) = (conn.clone(), click.clone());
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let r = zbus::block_on(conn.emit_signal(None::<()>, "/org/freedesktop/Notifications", "org.freedesktop.Notifications", "ActionInvoked", &(id, click.as_str())));
                println!("clicked {click} on {id}: {r:?}");
            });
        }
        id
    }

    fn close_notification(&self, _id: u32) {}

    fn get_server_information(&self) -> (String, String, String, String) {
        ("fake".into(), "flatshot".into(), "1".into(), "1.2".into())
    }
}

fn main() {
    let click = std::env::var("FAKE_NOTIFY_CLICK").ok().filter(|s| !s.is_empty());
    let _conn = zbus::blocking::connection::Builder::session()
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at("/org/freedesktop/Notifications", FakeServer { next: AtomicU32::new(0), click })
        .unwrap()
        .build()
        .unwrap();
    println!("fake notification server ready");
    loop {
        std::thread::park();
    }
}
