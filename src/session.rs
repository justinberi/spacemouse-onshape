//! One browser tab's connection: the WAMP v1 dialect spoken by 3Dconnexion's navigation library
//! (the JavaScript that Onshape loads), and the navigation loop that drives that tab's camera.
//!
//! The page connects and the server sends WELCOME. The page then sends PREFIX messages, then
//! CALLs `3dx_rpc:create` for a "3dmouse" and a "3dcontroller", and SUBSCRIBEs to the
//! controller's topic. It CALLs `3dx_rpc:update` with `{"focus": bool}` as its window gains and
//! loses focus.
//!
//! The server moves the camera by calling *into the page*: it publishes EVENTs on the controller
//! topic, each carrying a CALL (`self:read <property>` or `self:update <property> <value>`), and
//! the page answers with CALLRESULT or CALLERROR.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glam::{DQuat, DVec3};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use crate::device::DeviceState;
use crate::nav::{self, Camera, Input, Settings};

const WAMP_WELCOME: u64 = 0;
const WAMP_PREFIX: u64 = 1;
const WAMP_CALL: u64 = 2;
const WAMP_CALLRESULT: u64 = 3;
const WAMP_CALLERROR: u64 = 4;
const WAMP_SUBSCRIBE: u64 = 5;
const WAMP_UNSUBSCRIBE: u64 = 6;
const WAMP_EVENT: u64 = 8;

/// How long to wait for the page to answer a read or write.
const RPC_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the cap must rest before a motion ends.
const MOTION_END_DELAY: Duration = Duration::from_millis(120);
/// The longest frame time used for one step, so a stall doesn't make the view jump.
const MAX_STEP: f64 = 0.1;

pub const SERVER_IDENT: &str = concat!(
    "Nl-Proxy v1.4.0 (spacemouse-onshape ",
    env!("CARGO_PKG_VERSION"),
    ")"
);

/// What a button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ButtonAction {
    None,
    Fit,
    /// A standard view (see [`nav::standard_view`]), then fit.
    View(String),
}

impl std::str::FromStr for ButtonAction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "none" => Ok(Self::None),
            "fit" => Ok(Self::Fit),
            v if nav::standard_view(v).is_some() => Ok(Self::View(v.to_string())),
            _ => Err(format!(
                "unknown button action {s:?} (use none, fit, front, back, left, right, top, bottom or iso)"
            )),
        }
    }
}

/// Settings shared by all sessions.
#[derive(Clone, Debug)]
pub struct Config {
    pub nav: Settings,
    pub frame_interval: Duration,
    /// Actions for buttons 1, 2, …
    pub buttons: Vec<ButtonAction>,
}

/// What the status page shows about a session.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub client: Option<String>,
    pub focus: bool,
    pub subscribed: bool,
}

#[derive(Debug)]
pub enum RpcError {
    /// The page answered with an error, e.g. "unknown property".
    Remote(String),
    Timeout,
    Closed,
    NotSubscribed,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote(e) => write!(f, "page answered: {e}"),
            Self::Timeout => write!(f, "page did not answer in time"),
            Self::Closed => write!(f, "connection closed"),
            Self::NotSubscribed => write!(f, "page has not subscribed to the controller"),
        }
    }
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, RpcError>>>;

/// One connection's protocol state.
pub struct Session {
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<Pending>,
    state: Mutex<State>,
    next_id: AtomicU64,
    id_prefix: String,
    status: watch::Sender<Status>,
}

#[derive(Default)]
struct State {
    prefixes: HashMap<String, String>,
    mouse_id: Option<String>,
    controller_id: Option<String>,
    /// The topic exactly as the page subscribed to it, which is how events are addressed.
    topic: Option<String>,
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

impl Session {
    /// Creates a session that sends its outgoing messages to `out`.
    pub fn new(out: mpsc::UnboundedSender<String>) -> (Arc<Self>, watch::Receiver<Status>) {
        let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let (status, status_rx) = watch::channel(Status::default());
        let session = Arc::new(Self {
            out,
            pending: Mutex::new(HashMap::new()),
            state: Mutex::new(State::default()),
            next_id: AtomicU64::new(1),
            id_prefix: format!("{n:x}{nanos:08x}"),
            status,
        });
        (session, status_rx)
    }

    fn send(&self, msg: Value) {
        let _ = self.out.send(msg.to_string());
    }

    pub fn is_closed(&self) -> bool {
        self.out.is_closed()
    }

    fn new_id(&self) -> String {
        format!(
            "{}-{}",
            self.id_prefix,
            self.next_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Sends WELCOME; the first message on a new connection.
    pub fn welcome(&self) {
        self.send(json!([WAMP_WELCOME, self.id_prefix, 1, SERVER_IDENT]));
    }

    /// Expands a CURIE such as `3dx_rpc:create` using the page's PREFIX messages.
    fn resolve(&self, uri: &str) -> String {
        if let Some((prefix, rest)) = uri.split_once(':')
            && let Some(base) = self.state.lock().unwrap().prefixes.get(prefix)
        {
            return format!("{base}{rest}");
        }
        uri.to_string()
    }

    /// Handles one text message from the page.
    pub fn handle_message(&self, text: &str) {
        let Ok(Value::Array(msg)) = serde_json::from_str::<Value>(text) else {
            tracing::debug!("ignoring malformed message: {text}");
            return;
        };
        let Some(kind) = msg.first().and_then(Value::as_u64) else {
            return;
        };
        let str_at = |i: usize| {
            msg.get(i)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match kind {
            WAMP_PREFIX => {
                self.state
                    .lock()
                    .unwrap()
                    .prefixes
                    .insert(str_at(1), str_at(2));
            }
            WAMP_CALL => self.handle_call(&str_at(1), &str_at(2), msg.get(3..).unwrap_or_default()),
            WAMP_SUBSCRIBE => {
                let topic = str_at(1);
                let controller = self.state.lock().unwrap().controller_id.clone();
                let resolved = self.resolve(&topic);
                match controller {
                    Some(id) if resolved.ends_with(&format!("3dcontroller/{id}")) => {
                        tracing::debug!("page subscribed to {topic}");
                        self.state.lock().unwrap().topic = Some(topic);
                        self.status.send_modify(|s| s.subscribed = true);
                    }
                    _ => tracing::debug!("ignoring subscription to {topic}"),
                }
            }
            WAMP_UNSUBSCRIBE => {
                let mut state = self.state.lock().unwrap();
                if state.topic.as_deref() == Some(str_at(1).as_str()) {
                    state.topic = None;
                    self.status.send_modify(|s| s.subscribed = false);
                }
            }
            WAMP_CALLRESULT => {
                let result = msg.get(2).cloned().unwrap_or(Value::Null);
                self.complete(&str_at(1), Ok(result));
            }
            WAMP_CALLERROR => {
                let error = format!("{} {}", str_at(2), str_at(3));
                self.complete(&str_at(1), Err(RpcError::Remote(error)));
            }
            other => tracing::debug!("ignoring WAMP message type {other}"),
        }
    }

    fn complete(&self, call_id: &str, result: Result<Value, RpcError>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(call_id) {
            let _ = tx.send(result);
        }
    }

    /// Answers a CALL from the page.
    fn handle_call(&self, call_id: &str, proc_uri: &str, args: &[Value]) {
        let procedure = self.resolve(proc_uri);
        let method = procedure.rsplit(['#', ':']).next().unwrap_or_default();
        let arg_str = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or_default();
        let result = match method {
            "create" => {
                let what = self.resolve(arg_str(0));
                if what.ends_with("3dmouse") {
                    let id = format!("mouse{}", self.id_prefix);
                    let version = args.get(1).cloned().unwrap_or_default();
                    tracing::debug!("page created 3D mouse (library version {version})");
                    self.state.lock().unwrap().mouse_id = Some(id.clone());
                    Some(json!({ "connexion": id }))
                } else if what.ends_with("3dcontroller") {
                    let meta = args.get(2).cloned().unwrap_or(Value::Null);
                    let name = meta
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let version = meta.get("version").cloned().unwrap_or_default();
                    tracing::info!("{name} connected (version {version})");
                    // Numeric, as 3DxWare's own ids are.
                    let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1000;
                    self.state.lock().unwrap().controller_id = Some(id.to_string());
                    self.status.send_modify(|s| s.client = Some(name));
                    Some(json!({ "instance": id }))
                } else {
                    None
                }
            }
            "update" => {
                if let Some(focus) = args
                    .get(1)
                    .and_then(|a| a.get("focus"))
                    .and_then(Value::as_bool)
                {
                    tracing::debug!("focus {focus}");
                    self.status.send_modify(|s| s.focus = focus);
                }
                Some(json!({}))
            }
            "delete" => Some(json!({})),
            _ => None,
        };
        match result {
            Some(r) => self.send(json!([WAMP_CALLRESULT, call_id, r])),
            None => {
                tracing::debug!("unsupported call {procedure} {args:?}");
                self.send(json!([
                    WAMP_CALLERROR,
                    call_id,
                    "wamp.error.no_such_procedure",
                    format!("{proc_uri} is not supported")
                ]));
            }
        }
    }

    /// Calls `method` in the page (via an EVENT on the controller topic) and waits for the answer.
    async fn call(&self, method: &str, args: Vec<Value>) -> Result<Value, RpcError> {
        let topic = self
            .state
            .lock()
            .unwrap()
            .topic
            .clone()
            .ok_or(RpcError::NotSubscribed)?;
        let id = self.new_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let mut call = vec![json!(WAMP_CALL), json!(id), json!(method), json!("")];
        call.extend(args);
        self.send(json!([WAMP_EVENT, topic, call]));
        let result = tokio::time::timeout(RPC_TIMEOUT, rx).await;
        self.pending.lock().unwrap().remove(&id);
        match result {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(RpcError::Closed),
            Err(_) => Err(RpcError::Timeout),
        }
    }

    pub async fn read(&self, property: &str) -> Result<Value, RpcError> {
        self.call("self:read", vec![json!(property)]).await
    }

    pub async fn write(&self, property: &str, value: Value) -> Result<Value, RpcError> {
        self.call("self:update", vec![json!(property), value]).await
    }

    /// Fails every outstanding call; used when the connection closes.
    pub fn close(&self) {
        self.pending.lock().unwrap().clear();
    }
}

fn floats<const N: usize>(v: &Value) -> Option<[f64; N]> {
    let arr = v.as_array()?;
    let mut out = [0.0; N];
    for (o, x) in out.iter_mut().zip(arr.iter()) {
        *o = x.as_f64()?;
    }
    (arr.len() >= N).then_some(out)
}

/// A motion in progress: the camera as last written, and the point it turns about.
struct Motion {
    camera: Camera,
    pivot: DVec3,
    last_step: Instant,
}

/// Drives one session's camera from the device until the connection closes.
pub async fn run_navigation(
    session: Arc<Session>,
    mut status: watch::Receiver<Status>,
    device: watch::Receiver<DeviceState>,
    config: Arc<Config>,
) {
    let mut ticker = tokio::time::interval(config.frame_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut motion: Option<Motion> = None;
    let mut last_active = Instant::now();
    let mut previous_buttons = 0u32;
    let mut transaction = 1u64;
    let mut warned_pivot = false;
    // After a failure to read the view, wait before trying again rather than retrying each frame.
    let mut retry_at = Instant::now();

    while !session.is_closed() {
        ticker.tick().await;
        let dev = device.borrow().clone();
        let st = status.borrow_and_update().clone();
        let live = st.focus && st.subscribed && dev.connected;

        let pressed = dev.buttons & !previous_buttons;
        previous_buttons = dev.buttons;
        if live && pressed != 0 {
            for (i, action) in config.buttons.iter().enumerate() {
                if pressed & (1 << i) != 0 && *action != ButtonAction::None {
                    if let Some(m) = motion.take() {
                        end_motion(&session, &m).await;
                    }
                    if let Err(e) = run_button(&session, action).await {
                        tracing::warn!("button {}: {e}", i + 1);
                    }
                }
            }
        }

        let input = nav::input_from_raw(dev.axes, &config.nav);
        let now = Instant::now();
        if live && !input.is_zero() {
            last_active = now;
            if motion.is_none() {
                if now < retry_at {
                    continue;
                }
                match begin_motion(&session, &mut warned_pivot).await {
                    Ok(m) => motion = Some(m),
                    Err(e) => {
                        tracing::warn!("cannot start moving the view: {e}");
                        retry_at = Instant::now() + Duration::from_secs(1);
                        continue;
                    }
                }
            }
            if let Some(m) = motion.as_mut()
                && let Err(e) =
                    step_motion(&session, m, &input, &config.nav, &mut transaction).await
            {
                tracing::warn!("moving the view: {e}");
                motion = None;
            }
        } else if motion.is_some()
            && (!live || now - last_active > MOTION_END_DELAY)
            && let Some(m) = motion.take()
        {
            end_motion(&session, &m).await;
        }
    }
    tracing::debug!("navigation loop ended");
}

/// Reads the camera from the page, as it is right now.
async fn read_camera(session: &Session) -> Result<Camera, RpcError> {
    let perspective = session
        .read("view.perspective")
        .await?
        .as_bool()
        .unwrap_or(false);
    let affine = session.read("view.affine").await?;
    let affine: Vec<f64> = affine
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    let (rot, eye, layout) = Camera::pose_from_affine(&affine)
        .ok_or_else(|| RpcError::Remote(format!("unusable view.affine {affine:?}")))?;
    let (extents, frustum) = if perspective {
        (
            None,
            session
                .read("view.frustum")
                .await
                .ok()
                .as_ref()
                .and_then(floats::<6>),
        )
    } else {
        (
            session
                .read("view.extents")
                .await
                .ok()
                .as_ref()
                .and_then(floats::<6>),
            None,
        )
    };
    Ok(Camera {
        rot,
        eye,
        layout,
        perspective,
        extents,
        frustum,
    })
}

async fn read_model_extents(session: &Session) -> Option<[f64; 6]> {
    session
        .read("model.extents")
        .await
        .ok()
        .as_ref()
        .and_then(floats::<6>)
}

async fn begin_motion(session: &Session, warned_pivot: &mut bool) -> Result<Motion, RpcError> {
    let camera = read_camera(session).await?;
    // Onshape picks the pivot itself (it follows what is under the view); fall back to the middle
    // of the model, then to a point in front of the camera.
    let pivot = match session
        .read("pivot.position")
        .await
        .ok()
        .as_ref()
        .and_then(floats::<3>)
    {
        Some(p) => DVec3::from_array(p),
        None => {
            if !*warned_pivot {
                tracing::info!("page has no pivot.position; turning about the middle of the model");
                *warned_pivot = true;
            }
            match read_model_extents(session).await {
                Some(e) => (DVec3::new(e[0], e[1], e[2]) + DVec3::new(e[3], e[4], e[5])) * 0.5,
                None => camera.eye + camera.forward() * 10.0,
            }
        }
    };
    session.write("motion", json!(true)).await?;
    let _ = session.write("pivot.visible", json!(true)).await;
    Ok(Motion {
        camera,
        pivot,
        last_step: Instant::now(),
    })
}

async fn step_motion(
    session: &Session,
    m: &mut Motion,
    input: &Input,
    s: &Settings,
    transaction: &mut u64,
) -> Result<(), RpcError> {
    let now = Instant::now();
    let dt = (now - m.last_step).as_secs_f64().min(MAX_STEP);
    m.last_step = now;
    let before = m.camera.extents;
    m.camera.step(input, m.pivot, dt, s);
    write_camera(session, &m.camera, before != m.camera.extents, transaction).await
}

/// Writes the camera in one batch, bracketed by a transaction as 3DxWare does; the page redraws
/// when the transaction goes back to 0.
async fn write_camera(
    session: &Session,
    camera: &Camera,
    extents_changed: bool,
    transaction: &mut u64,
) -> Result<(), RpcError> {
    *transaction += 1;
    // Sent back to back without waiting, then all answers awaited, to keep latency down.
    let begin = session.write("transaction", json!(*transaction));
    let extents = async {
        match (extents_changed, camera.extents) {
            (true, Some(e)) => session.write("view.extents", json!(e)).await.map(|_| ()),
            _ => Ok(()),
        }
    };
    let affine = session.write("view.affine", json!(camera.affine()));
    let commit = session.write("transaction", json!(0));
    let (_, extents, affine, _) = tokio::join!(begin, extents, affine, commit);
    extents?;
    affine?;
    Ok(())
}

async fn end_motion(session: &Session, _m: &Motion) {
    let _ = session.write("pivot.visible", json!(false)).await;
    let _ = session.write("motion", json!(false)).await;
}

async fn run_button(session: &Session, action: &ButtonAction) -> Result<(), RpcError> {
    let mut camera = read_camera(session).await?;
    let rot = match action {
        ButtonAction::None => return Ok(()),
        ButtonAction::Fit => camera.rot,
        ButtonAction::View(name) => {
            let front = session
                .read("views.front")
                .await
                .ok()
                .and_then(|v| {
                    v.as_array()
                        .map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
                })
                .and_then(|a| Camera::pose_from_affine(&a))
                .map(|(rot, _, _)| rot)
                .unwrap_or(DQuat::IDENTITY);
            front * nav::standard_view(name).unwrap_or(DQuat::IDENTITY)
        }
    };
    let model = read_model_extents(session)
        .await
        .ok_or_else(|| RpcError::Remote("no model.extents to fit".into()))?;
    camera.fit(&model, rot);
    session.write("motion", json!(true)).await?;
    let mut transaction = 1;
    let result = write_camera(session, &camera, true, &mut transaction).await;
    let _ = session.write("motion", json!(false)).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(rx: &mut mpsc::UnboundedReceiver<String>) -> Value {
        serde_json::from_str(&rx.try_recv().expect("a message")).unwrap()
    }

    /// The handshake as captured from 3DxWare on Windows.
    #[test]
    fn handshake_matches_the_3dxware_capture() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (s, status) = Session::new(tx);
        s.welcome();
        let w = take(&mut rx);
        assert_eq!(w[0], 0);
        assert_eq!(w[2], 1);

        s.handle_message(r#"[1,"3dx_rpc","wss://127.51.68.120/3dconnexion#"]"#);
        s.handle_message(r#"[1,"3dconnexion","wss://127.51.68.120/3dconnexion"]"#);
        s.handle_message(r#"[1,"self","https://cad.onshape.com/documents/x"]"#);
        s.handle_message(r#"[2,"0.3uy1h4l77rf","3dx_rpc:create","3dconnexion:3dmouse","0.3.11"]"#);
        let r = take(&mut rx);
        assert_eq!(r[0], 3);
        assert_eq!(r[1], "0.3uy1h4l77rf");
        let mouse = r[2]["connexion"].as_str().unwrap().to_string();

        s.handle_message(&format!(
            r#"[2,"0.paydocbsmr","3dx_rpc:create","3dconnexion:3dcontroller","{mouse}",{{"version":0,"name":"Onshape"}}]"#
        ));
        let r = take(&mut rx);
        assert_eq!(r[1], "0.paydocbsmr");
        let instance = r[2]["instance"].as_u64().unwrap();
        assert_eq!(status.borrow().client.as_deref(), Some("Onshape"));

        s.handle_message(&format!(r#"[5,"3dconnexion:3dcontroller/{instance}"]"#));
        assert!(status.borrow().subscribed);
        s.handle_message(&format!(
            r#"[2,"0.yg35qrvwlc","3dx_rpc:update","3dconnexion:3dcontroller/{instance}",{{"focus":true}}]"#
        ));
        assert_eq!(take(&mut rx), json!([3, "0.yg35qrvwlc", {}]));
        assert!(status.borrow().focus);

        s.handle_message(r#"[2,"x","3dx_rpc:frobnicate"]"#);
        assert_eq!(take(&mut rx)[0], 4);
    }

    #[tokio::test]
    async fn reads_go_out_as_events_and_complete_on_callresult() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (s, _status) = Session::new(tx);
        s.handle_message(r#"[1,"3dconnexion","wss://127.51.68.120/3dconnexion"]"#);
        s.handle_message(r#"[2,"a","wss://127.51.68.120/3dconnexion#create","3dconnexion:3dcontroller","m",{"name":"t"}]"#);
        let id = take(&mut rx)[2]["instance"].as_u64().unwrap();
        let topic = format!("3dconnexion:3dcontroller/{id}");
        s.handle_message(&format!(r#"[5,"{topic}"]"#));

        let s2 = s.clone();
        let read = tokio::spawn(async move { s2.read("view.perspective").await });
        tokio::task::yield_now().await;
        let ev: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(ev[0], 8);
        assert_eq!(ev[1], topic.as_str());
        assert_eq!(ev[2][2], "self:read");
        assert_eq!(ev[2][4], "view.perspective");
        s.handle_message(&json!([3, ev[2][1], true]).to_string());
        assert_eq!(read.await.unwrap().unwrap(), json!(true));

        let s2 = s.clone();
        let read = tokio::spawn(async move { s2.read("view.target").await });
        let ev: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        s.handle_message(
            &json!([
                4,
                ev[2][1],
                "self:read#generic",
                "view.target unknown property"
            ])
            .to_string(),
        );
        assert!(matches!(read.await.unwrap(), Err(RpcError::Remote(_))));
    }
}
