//! The HTTPS and WebSocket server that stands in for 3DxWare's "Nl-Proxy".
//!
//! - `GET /3dconnexion/nlproxy` tells the page which port to open its WebSocket on.
//! - A WebSocket upgrade on any path, with subprotocol `wamp`, starts a [`Session`].
//! - `GET /` is a status page: open it once to check the certificate and see the device.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::device::DeviceState;
use crate::nav;
use crate::session::{self, Config, Session, Status};

/// The version 3DxWare's Nl-Proxy reports.
const NLPROXY_VERSION: &str = "1.4.8.21486";

pub const USERSCRIPT: &str = include_str!("../extension/onshape-spacemouse.user.js");

pub struct App {
    pub config: Arc<Config>,
    pub device: watch::Receiver<DeviceState>,
    pub port: u16,
    pub allowed_origins: Vec<String>,
    pub allow_any_origin: bool,
    sessions: Mutex<BTreeMap<u64, watch::Receiver<Status>>>,
    next_session: AtomicU64,
}

impl App {
    pub fn new(
        config: Config,
        device: watch::Receiver<DeviceState>,
        port: u16,
        allowed_origins: Vec<String>,
        allow_any_origin: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(config),
            device,
            port,
            allowed_origins,
            allow_any_origin,
            sessions: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(1),
        })
    }

    /// Whether a page served from `origin` may use the bridge.
    pub fn origin_allowed(&self, origin: &str) -> bool {
        if self.allow_any_origin {
            return true;
        }
        let host = origin.strip_prefix("https://").unwrap_or("");
        let host = host.split(':').next().unwrap_or("");
        host == "onshape.com"
            || host.ends_with(".onshape.com")
            || host == "3dconnexion.com"
            || host.ends_with(".3dconnexion.com")
            || host == crate::certs::NLPROXY_IP.to_string()
            || host == "localhost"
            || host == "127.0.0.1"
            || self
                .allowed_origins
                .iter()
                .any(|o| o.trim_end_matches('/') == origin)
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/3dconnexion/nlproxy", get(nlproxy))
        .route("/status.json", get(status_json))
        .route("/onshape-spacemouse.user.js", get(userscript))
        .route("/", get(root))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(app.clone(), cors))
        .with_state(app)
}

/// CORS for the page's `fetch` of `/3dconnexion/nlproxy`, including Chrome's private network
/// access preflight (a public site reaching a loopback address).
async fn cors(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .map(str::to_string);
    let allowed = origin
        .as_deref()
        .filter(|o| app.origin_allowed(o))
        .map(str::to_string);
    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        if let Some(o) = &allowed {
            let h = resp.headers_mut();
            h.insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_str(o).unwrap_or(HeaderValue::from_static("null")),
            );
            h.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, OPTIONS"),
            );
            let requested = req
                .headers()
                .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
                .cloned()
                .unwrap_or(HeaderValue::from_static("*"));
            h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
            h.insert(
                "access-control-allow-private-network",
                HeaderValue::from_static("true"),
            );
            h.insert(
                header::ACCESS_CONTROL_MAX_AGE,
                HeaderValue::from_static("600"),
            );
            h.insert(header::VARY, HeaderValue::from_static("Origin"));
        }
        return resp;
    }
    let mut resp = next.run(req).await;
    if let Some(o) = allowed {
        let h = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&o) {
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
        h.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    resp
}

async fn nlproxy(State(app): State<Arc<App>>) -> impl IntoResponse {
    Json(json!({ "port": app.port, "version": NLPROXY_VERSION }))
}

async fn userscript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        USERSCRIPT,
    )
}

async fn status_json(State(app): State<Arc<App>>) -> impl IntoResponse {
    let dev = app.device.borrow().clone();
    let input = nav::input_from_raw(dev.axes, &app.config.nav);
    let sessions: Vec<_> = app
        .sessions
        .lock()
        .unwrap()
        .values()
        .map(|s| {
            let s = s.borrow();
            json!({ "client": s.client, "focus": s.focus, "subscribed": s.subscribed })
        })
        .collect();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "device": {
            "connected": dev.connected,
            "name": dev.name,
            "problem": dev.problem,
            "raw": dev.axes,
            "buttons": dev.buttons,
            "camera": {
                "pan_x": input.t.x, "pan_y": input.t.y, "zoom": input.t.z,
                "pitch": input.w.x, "yaw": input.w.y, "roll": input.w.z,
            },
        },
        "sessions": sessions,
    }))
}

async fn root(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    match ws {
        Ok(ws) => upgrade(app, &headers, ws),
        Err(_) => Html(include_str!("status.html")).into_response(),
    }
}

async fn fallback(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    match ws {
        Ok(ws) => upgrade(app, &headers, ws),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn upgrade(app: Arc<App>, headers: &HeaderMap, ws: WebSocketUpgrade) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok());
    if let Some(origin) = origin
        && !app.origin_allowed(origin)
    {
        tracing::warn!(
            "refused a connection from {origin} (allow it with --allow-origin {origin})"
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    tracing::debug!("WebSocket from {}", origin.unwrap_or("(no origin)"));
    ws.protocols(["wamp"])
        .on_upgrade(move |socket| serve_socket(app, socket))
}

async fn serve_socket(app: Arc<App>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let (session, status) = Session::new(out_tx);
    let id = app.next_session.fetch_add(1, Ordering::Relaxed);
    app.sessions.lock().unwrap().insert(id, status.clone());

    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    session.welcome();
    let nav = tokio::spawn(session::run_navigation(
        session.clone(),
        status,
        app.device.clone(),
        app.config.clone(),
    ));

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => session.handle_message(text.as_str()),
            Message::Close(_) => break,
            _ => {}
        }
    }

    session.close();
    writer.abort();
    nav.abort();
    app.sessions.lock().unwrap().remove(&id);
    let client = app.sessions.lock().unwrap().len();
    tracing::info!("page disconnected ({client} still connected)");
}

/// Serves until the process is stopped.
pub async fn serve(
    app: Arc<App>,
    addr: SocketAddr,
    tls: rustls::ServerConfig,
) -> anyhow::Result<()> {
    let config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));
    axum_server::bind_rustls(addr, config)
        .serve(router(app).into_make_service())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certs;
    use crate::nav::Settings;
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    use serde_json::Value;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

    /// A fake Onshape page: connects over TLS with the CA the bridge made, does 3DxWare's
    /// handshake, answers the bridge's reads with a camera, and checks that pushing the cap to the
    /// right makes the bridge write a camera that has moved to the left.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_page_connects_and_the_cap_moves_its_camera() {
        let dir =
            std::env::temp_dir().join(format!("spacemouse-onshape-test-{}", std::process::id()));
        let paths = certs::Paths::new(dir.clone());
        let tls = certs::load_or_create(&paths).unwrap();

        let (dev_tx, dev_rx) = watch::channel(DeviceState {
            connected: true,
            ..DeviceState::default()
        });
        let config = Config {
            nav: Settings::default(),
            frame_interval: Duration::from_millis(10),
            buttons: vec![],
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = App::new(config, dev_rx, port, vec![], false);
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));
        let server = axum_server::tls_rustls::from_tcp_rustls(listener, rustls_config).unwrap();
        tokio::spawn(server.serve(router(app).into_make_service()));

        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_file(&paths.ca).unwrap())
            .unwrap();
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let mut request = format!("wss://127.0.0.1:{port}/")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("wamp"));
        request.headers_mut().insert(
            "origin",
            HeaderValue::from_static("https://cad.onshape.com"),
        );
        let (mut ws, response) = tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(Arc::new(client))),
        )
        .await
        .expect("TLS WebSocket connection with the generated CA");
        assert_eq!(response.headers()["sec-websocket-protocol"], "wamp");

        async fn next(
            ws: &mut (impl StreamExt<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin),
        ) -> Value {
            loop {
                let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                if let tungstenite::Message::Text(t) = msg {
                    return serde_json::from_str(t.as_str()).unwrap();
                }
            }
        }
        let send = |v: Value| tungstenite::Message::Text(v.to_string().into());

        assert_eq!(next(&mut ws).await[0], 0, "WELCOME");
        ws.send(send(json!([
            1,
            "3dx_rpc",
            "wss://127.51.68.120/3dconnexion#"
        ])))
        .await
        .unwrap();
        ws.send(send(json!([
            1,
            "3dconnexion",
            "wss://127.51.68.120/3dconnexion"
        ])))
        .await
        .unwrap();
        ws.send(send(json!([
            2,
            "c1",
            "3dx_rpc:create",
            "3dconnexion:3dmouse",
            "0.3.11"
        ])))
        .await
        .unwrap();
        let mouse = next(&mut ws).await[2]["connexion"].clone();
        ws.send(send(json!([2, "c2", "3dx_rpc:create", "3dconnexion:3dcontroller", mouse, {"version": 0, "name": "Onshape"}])))
            .await
            .unwrap();
        let instance = next(&mut ws).await[2]["instance"].clone();
        let topic = format!("3dconnexion:3dcontroller/{instance}");
        ws.send(send(json!([5, topic]))).await.unwrap();
        ws.send(send(
            json!([2, "c3", "3dx_rpc:update", topic, {"focus": true}]),
        ))
        .await
        .unwrap();
        assert_eq!(next(&mut ws).await, json!([3, "c3", {}]));

        // Push the cap to the right.
        dev_tx.send_modify(|d| d.axes = [350, 0, 0, 0, 0, 0]);

        let mut wrote_motion = false;
        loop {
            let ev = next(&mut ws).await;
            assert_eq!(ev[0], 8, "EVENT");
            assert_eq!(ev[1], topic.as_str());
            let call = &ev[2];
            let (id, method, property) = (
                call[1].clone(),
                call[2].as_str().unwrap(),
                call[4].as_str().unwrap(),
            );
            let answer = match (method, property) {
                ("self:read", "view.perspective") => json!([3, id, false]),
                ("self:read", "view.affine") => {
                    json!([3, id, [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 10, 1]])
                }
                ("self:read", "view.extents") => json!([3, id, [-4, -3, -1000, 4, 3, 1000]]),
                ("self:read", "pivot.position") => json!([3, id, [0, 0, 0]]),
                ("self:read", _) => json!([
                    4,
                    id,
                    "self:read#generic",
                    format!("{property} unknown property")
                ]),
                ("self:update", "motion") => {
                    wrote_motion |= call[5] == json!(true);
                    json!([3, id, null])
                }
                ("self:update", "view.affine") => {
                    let m: Vec<f64> = call[5]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_f64().unwrap())
                        .collect();
                    assert!(wrote_motion, "motion is set before the view moves");
                    assert!(
                        m[12] < 0.0,
                        "the camera moves left so the model moves right: {m:?}"
                    );
                    assert!(
                        (m[14] - 10.0).abs() < 1e-9 && m[13].abs() < 1e-9,
                        "only x changes: {m:?}"
                    );
                    break;
                }
                ("self:update", _) => json!([3, id, null]),
                other => panic!("unexpected call {other:?}"),
            };
            ws.send(send(answer)).await.unwrap();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn origins() {
        let (_tx, rx) = watch::channel(DeviceState::default());
        let config = Config {
            nav: Settings::default(),
            frame_interval: Duration::from_millis(10),
            buttons: vec![],
        };
        let app = App::new(config, rx, 8181, vec!["https://example.com".into()], false);
        assert!(app.origin_allowed("https://cad.onshape.com"));
        assert!(app.origin_allowed("https://127.51.68.120:8181"));
        assert!(app.origin_allowed("https://example.com"));
        assert!(!app.origin_allowed("https://evil.com"));
        assert!(!app.origin_allowed("https://onshape.com.evil.com"));
        assert!(!app.origin_allowed("http://cad.onshape.com"));
    }
}
