//! The session bus, shared by everything that talks to the desktop: KWin,
//! the portals, notifications, global shortcuts and the tray.
//!
//! zbus's calls are async, and the rest of Flatshot isn't, so [`Bus`] blocks
//! on them with zbus::block_on. A connection's own work (reading its socket,
//! serving our objects) runs on the thread that made it while that thread is
//! inside zbus::block_on, else on a helper thread of zbus's. zbus::block_on
//! can't be called from inside a handler zbus runs (a served interface's
//! method), where it would panic: those only hand things on.

use std::num::NonZeroUsize;
use std::sync::OnceLock;
use std::time::Duration;

use zbus::ObjectPath;
use zbus::names::{BusName, InterfaceName, MemberName, OwnedUniqueName, WellKnownName};

static SESSION: OnceLock<Result<Bus, String>> = OnceLock::new();

/// No call waits on the desktop longer than this.
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// A connection to the session bus, its calls blocking.
#[derive(Clone, Debug)]
pub struct Bus(zbus::Connection);

impl Bus {
    /// A connection of its own (most share [`session`]'s).
    pub fn connect(timeout: Duration) -> Result<Bus, String> {
        zbus::block_on(zbus::connection::Builder::session().method_timeout(timeout).build()).map(Bus).map_err(|e| format!("no session bus: {e}"))
    }

    /// The connection itself, for its async API.
    pub fn inner(&self) -> &zbus::Connection {
        &self.0
    }

    pub fn unique_name(&self) -> Option<&OwnedUniqueName> {
        self.0.unique_name()
    }

    /// Call `method` and wait for its reply.
    pub fn call_method<'d, 'p, 'i, 'm, D, P, I, M, B>(
        &self,
        destination: Option<D>,
        path: P,
        interface: Option<I>,
        method: M,
        body: &B,
    ) -> zbus::Result<zbus::Message>
    where
        D: TryInto<BusName<'d>>,
        P: TryInto<ObjectPath<'p>>,
        I: TryInto<InterfaceName<'i>>,
        M: TryInto<MemberName<'m>>,
        D::Error: Into<zbus::Error>,
        P::Error: Into<zbus::Error>,
        I::Error: Into<zbus::Error>,
        M::Error: Into<zbus::Error>,
        B: serde::Serialize + zbus::DynamicType,
    {
        zbus::block_on(self.0.call_method(destination, path, interface, method, body))
    }

    /// Send a signal.
    pub fn emit_signal<'d, 'p, 'i, 'm, D, P, I, M, B>(&self, destination: Option<D>, path: P, interface: I, signal: M, body: &B) -> zbus::Result<()>
    where
        D: TryInto<BusName<'d>>,
        P: TryInto<ObjectPath<'p>>,
        I: TryInto<InterfaceName<'i>>,
        M: TryInto<MemberName<'m>>,
        D::Error: Into<zbus::Error>,
        P::Error: Into<zbus::Error>,
        I::Error: Into<zbus::Error>,
        M::Error: Into<zbus::Error>,
        B: serde::Serialize + zbus::DynamicType,
    {
        zbus::block_on(self.0.emit_signal(destination, path, interface, signal, body))
    }

    /// Own the well-known `name`.
    pub fn request_name<'w, W>(&self, name: W) -> zbus::Result<()>
    where
        W: TryInto<WellKnownName<'w>>,
        W::Error: Into<zbus::Error>,
    {
        zbus::block_on(self.0.request_name(name))
    }

    /// Close it, for all who share it: every stream on it ends.
    pub fn close(self) -> zbus::Result<()> {
        zbus::block_on(self.0.close())
    }
}

/// The session bus (connected once, then kept).
pub fn session() -> Result<&'static Bus, String> {
    SESSION.get_or_init(|| Bus::connect(TIMEOUT)).as_ref().map_err(|e| e.clone())
}

/// Connect in the background (startup: it's ready by the time it's needed).
pub fn connect_early() {
    let _ = std::thread::Builder::new().name("flatshot-dbus".into()).spawn(|| {
        let _ = session();
    });
}

/// Is `name` owned on the session bus?
pub fn has_owner(name: &str) -> bool {
    let Ok(conn) = session() else { return false };
    let Ok(reply) = conn.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "NameHasOwner", &(name,)) else {
        return false;
    };
    reply.body().deserialize::<bool>().unwrap_or(false)
}

/// Hear the signals `rule` matches (which names `service` as their sender)
/// from whoever owns `service`, on a thread of its own: `on_signal` gets
/// each, and stops the listening by returning false.
///
/// Anyone on the bus can send Flatshot a signal directly, claiming any
/// interface; the bus only vouches for the sender's unique name. So each
/// sender is checked against the name's current owner, followed through
/// the bus's own NameOwnerChanged (which only the bus can send). Nothing
/// here waits on the connection while its messages come in, so a flood of
/// them can't stall it for everyone else.
pub fn listen(
    service: &'static str,
    rule: zbus::MatchRule<'static>,
    thread: &str,
    on_signal: impl FnMut(&zbus::Message) -> bool + Send + 'static,
) -> Result<(), String> {
    listen_on(session()?.inner().clone(), service, rule, thread, on_signal)
}

/// [`listen`] on a connection of the caller's (one a portal session lives on).
pub fn listen_on(
    conn: zbus::Connection,
    service: &'static str,
    rule: zbus::MatchRule<'static>,
    thread: &str,
    mut on_signal: impl FnMut(&zbus::Message) -> bool + Send + 'static,
) -> Result<(), String> {
    let owners = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(BUS)
        .interface(BUS)
        .member("NameOwnerChanged")
        .add_arg(service)
        .build()
        .map_err(|e| e.to_string())?;
    let queue = |n: usize| NonZeroUsize::new(n);
    let (owners, signals) = zbus::block_on(async {
        let owners = zbus::MessageStream::for_match_rule(owners, &conn, queue(16)).await?;
        let signals = zbus::MessageStream::for_match_rule(rule, &conn, queue(64)).await?;
        Ok::<_, zbus::Error>((owners, signals))
    })
    .map_err(|e| e.to_string())?;
    // (not zbus::block_on: `on_signal` may make blocking calls of its own)
    std::thread::Builder::new()
        .name(thread.into())
        .spawn(move || futures_lite::future::block_on(follow(conn, service, owners, signals, &mut on_signal)))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

const BUS: &str = "org.freedesktop.DBus";

async fn follow(
    conn: zbus::Connection,
    service: &str,
    owners: zbus::MessageStream,
    signals: zbus::MessageStream,
    on_signal: &mut (dyn FnMut(&zbus::Message) -> bool + Send),
) {
    use futures_lite::StreamExt;
    enum Got {
        Owner(Option<String>),
        Message(Option<(bool, zbus::Result<zbus::Message>)>),
    }
    // (owner changes first: one comes before its new owner's signals)
    let mut all = owners.map(|m| (true, m)).or(signals.map(|m| (false, m)));
    // The owner now, asked while messages keep being taken.
    let lookup = async {
        let reply = conn.call_method(Some(BUS), "/org/freedesktop/DBus", Some(BUS), "GetNameOwner", &(service,)).await.ok()?;
        reply.body().deserialize::<String>().ok()
    };
    let mut lookup = std::pin::pin!(lookup);
    let mut looking = true;
    let mut owner: Option<String> = None;
    loop {
        let got = if looking {
            futures_lite::future::or(async { Got::Owner((&mut lookup).await) }, async { Got::Message(all.next().await) }).await
        } else {
            Got::Message(all.next().await)
        };
        match got {
            Got::Owner(o) => {
                // (as new as any change heard before it)
                owner = o;
                looking = false;
            }
            Got::Message(None) => return,
            Got::Message(Some((true, Ok(m)))) => {
                if let Some(o) = owner_change(&m, service) {
                    owner = o;
                }
            }
            Got::Message(Some((false, Ok(m)))) => {
                let from_owner = owner.is_some() && m.header().sender().map(|s| s.as_str()) == owner.as_deref();
                if from_owner && !on_signal(&m) {
                    return;
                }
            }
            Got::Message(Some((_, Err(_)))) => {}
        }
    }
}

/// `service`'s new owner (None: it has none), if `m` is the bus saying so.
fn owner_change(m: &zbus::Message, service: &str) -> Option<Option<String>> {
    let h = m.header();
    // (no one but the bus itself can send as org.freedesktop.DBus)
    if h.sender().map(|s| s.as_str()) != Some(BUS)
        || h.interface().map(|i| i.as_str()) != Some(BUS)
        || h.member().map(|n| n.as_str()) != Some("NameOwnerChanged")
    {
        return None;
    }
    let (name, _, new): (String, String, String) = m.body().deserialize().ok()?;
    (name == service).then(|| (!new.is_empty()).then_some(new))
}
