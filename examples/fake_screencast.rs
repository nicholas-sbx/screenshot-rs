//! A stand-in for the screen-cast portal (org.freedesktop.portal.ScreenCast),
//! for testing recordings without a desktop that has one: it shares a real
//! PipeWire stream (GStreamer's test pattern, FAKE_CAST_SIZE pixels, default
//! 1600x1000) as "the monitor at 0,0". Needs PipeWire, WirePlumber and
//! gst-launch-1.0 with the PipeWire plugin. Prints what it's asked.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const PATH: &str = "/org/freedesktop/portal/desktop";
const NODE_NAME: &str = "flatshot-fake-cast";

struct Cast {
    size: (i32, i32),
    node: Mutex<Option<u32>>,
}

fn sender_part(header: &Header<'_>) -> String {
    header.sender().map(|s| s.trim_start_matches(':').replace('.', "_")).unwrap_or_default()
}

fn token(options: &HashMap<String, OwnedValue>, key: &str) -> String {
    options.get(key).and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_else(|| "t".into())
}

/// Answer a request: the Response signal on its object.
async fn respond(conn: &zbus::Connection, path: &str, results: HashMap<&str, Value<'_>>) {
    let r = conn.emit_signal(None::<()>, path, "org.freedesktop.portal.Request", "Response", &(0u32, results)).await;
    if let Err(e) = r {
        println!("screencast: can't answer: {e}");
    }
}

/// The stream's node, once the producer's up.
fn find_node() -> Option<u32> {
    let out = Command::new("pw-dump").stderr(Stdio::null()).output().ok()?;
    let all: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    all.as_array()?.iter().find_map(|o| {
        let props = o.get("info")?.get("props")?;
        (props.get("node.name")?.as_str()? == NODE_NAME).then(|| o.get("id")?.as_u64().map(|i| i as u32)).flatten()
    })
}

#[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
impl Cast {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        5
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        3
    }

    #[zbus(name = "CreateSession")]
    async fn create_session(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        let sender = sender_part(&header);
        let request = format!("{PATH}/request/{sender}/{}", token(&options, "handle_token"));
        let session = format!("{PATH}/session/{sender}/{}", token(&options, "session_handle_token"));
        println!("screencast: CreateSession -> {session}");
        respond(conn, &request, HashMap::from([("session_handle", Value::from(session))])).await;
        OwnedObjectPath::try_from(request).unwrap()
    }

    #[zbus(name = "SelectSources")]
    async fn select_sources(
        &self,
        session: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        let mut keys: Vec<String> = options.iter().filter(|(k, _)| k.as_str() != "handle_token").map(|(k, v)| format!("{k}={v:?}")).collect();
        keys.sort();
        println!("screencast: SelectSources {} {}", session.as_str(), keys.join(" "));
        let request = format!("{PATH}/request/{}/{}", sender_part(&header), token(&options, "handle_token"));
        respond(conn, &request, HashMap::new()).await;
        OwnedObjectPath::try_from(request).unwrap()
    }

    #[zbus(name = "Start")]
    async fn start(
        &self,
        session: OwnedObjectPath,
        _parent: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        println!("screencast: Start {}", session.as_str());
        let request = format!("{PATH}/request/{}/{}", sender_part(&header), token(&options, "handle_token"));
        let node = *self.node.lock().unwrap();
        let mut results: HashMap<&str, Value<'_>> = HashMap::new();
        if let Some(node) = node {
            let props: HashMap<&str, Value<'_>> =
                HashMap::from([("position", Value::from((0i32, 0i32))), ("size", Value::from(self.size)), ("source_type", Value::from(1u32))]);
            results.insert("streams", Value::from(vec![(node, props)]));
            results.insert("restore_token", Value::from("fake-restore-token"));
        }
        respond(conn, &request, results).await;
        OwnedObjectPath::try_from(request).unwrap()
    }

    #[zbus(name = "OpenPipeWireRemote")]
    fn open_pipewire_remote(&self, session: OwnedObjectPath, _options: HashMap<String, OwnedValue>) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        println!("screencast: OpenPipeWireRemote {}", session.as_str());
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        let stream = UnixStream::connect(format!("{dir}/pipewire-0")).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(std::os::fd::OwnedFd::from(stream).into())
    }
}

fn main() {
    let size = std::env::var("FAKE_CAST_SIZE")
        .ok()
        .and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
        .unwrap_or((1600, 1000));
    // The stream: a moving test pattern, as a PipeWire video source.
    let caps = format!("video/x-raw,format=BGRx,width={},height={},framerate=30/1", size.0, size.1);
    let props = format!("props,node.name={NODE_NAME},media.class=Video/Source");
    let mut producer: Child = Command::new("gst-launch-1.0")
        .args(["-q", "videotestsrc", "is-live=true", "pattern=ball", "!", &caps, "!", "pipewiresink", "mode=provide", &format!("stream-properties={props}")])
        .stdin(Stdio::null())
        .spawn()
        .expect("gst-launch-1.0");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut node = None;
    while node.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        node = find_node();
    }
    println!("screencast: stream node {node:?}");
    let _conn = zbus::blocking::connection::Builder::session()
        .unwrap()
        .name("org.freedesktop.portal.Desktop")
        .unwrap()
        .serve_at(PATH, Cast { size, node: Mutex::new(node) })
        .unwrap()
        .build()
        .unwrap();
    println!("fake screen-cast portal ready");
    let _ = producer.wait();
}
