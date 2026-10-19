//! Sends a burst of forged KDE shortcut presses (globalShortcutPressed for
//! Flatshot's "region") straight to one connection on the session bus, as
//! fast as it can: `dbus_flood :1.23 2000`. A stand-in for a hostile
//! program, for checking that Flatshot ignores them and isn't stalled.

fn main() {
    let mut args = std::env::args().skip(1);
    let dest = args.next().expect("usage: dbus_flood DESTINATION [COUNT]");
    let count: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(1000);
    let sent = zbus::block_on(async {
        let conn = zbus::Connection::session().await.expect("session bus");
        let mut sent = 0;
        for _ in 0..count {
            let r = conn
                .emit_signal(
                    Some(dest.as_str()),
                    "/component/flatshot_rs",
                    "org.kde.kglobalaccel.Component",
                    "globalShortcutPressed",
                    &("flatshot-rs", "region", 0i64),
                )
                .await;
            if r.is_ok() {
                sent += 1;
            }
        }
        sent
    });
    println!("sent {sent} forged presses to {dest}");
}
