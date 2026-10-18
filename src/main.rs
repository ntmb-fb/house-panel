mod bluos;
mod hue;
mod lgtv;
mod onkyo;
mod tado;
mod xml;

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{Html, sse::{Event, KeepAlive, Sse}},
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::{broadcast, mpsc};
use tower_http::services::ServeDir;

/// Latest known state of every device, keyed by state id ("hue/light/19", "onkyo", ...).
/// Every change is broadcast to the open dashboards as `{id: state}`.
pub struct Hub {
    state: Mutex<BTreeMap<String, Value>>,
    tx: broadcast::Sender<String>,
}

impl Hub {
    /// Merges the fields of `patch` into the device's state object; only real changes are broadcast.
    pub fn merge(&self, id: &str, patch: Value) {
        let mut all = self.state.lock().unwrap();
        let cur = all.entry(id.to_string()).or_insert_with(|| json!({}));
        let before = cur.clone();
        if let (Some(cur), Value::Object(patch)) = (cur.as_object_mut(), patch) {
            cur.extend(patch);
        }
        if *cur != before {
            let _ = self.tx.send(json!({ id: cur }).to_string());
        }
    }

    pub fn get(&self, id: &str) -> Value {
        self.state.lock().unwrap().get(id).cloned().unwrap_or(Value::Null)
    }

    fn snapshot(&self) -> String {
        serde_json::to_string(&*self.state.lock().unwrap()).unwrap()
    }
}

#[derive(Deserialize)]
struct Config {
    hue: Option<hue::Config>,
    #[serde(default)]
    bluos: Vec<bluos::Config>,
    onkyo: Option<onkyo::Config>,
    lgtv: Option<lgtv::Config>,
    tado: Option<tado::Config>,
    #[serde(default)]
    panel: Panel,
}

/// Floorplan elements with no live device behind them.
#[derive(Deserialize, Default)]
struct Panel {
    /// Badges shown greyed out (devices deliberately never contacted, e.g. battery cameras).
    #[serde(default)]
    unavailable: Vec<String>,
    /// Value texts to clear.
    #[serde(default)]
    blank: Vec<String>,
}

struct App {
    hub: Arc<Hub>,
    cmds: HashMap<String, mpsc::Sender<Value>>,
    bindings: Value,
}

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

fn env(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or(default.into())
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider().install_default().unwrap();
    let data = PathBuf::from(env("DATA", "/data"));
    let floorplan = env("FLOORPLAN", "/floorplan");
    let port = env("PORT", "8770");

    let path = data.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let cfg: Config = toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let read_secret = |name: &str| {
        let p = data.join(name);
        std::fs::read_to_string(&p).map(|s| s.trim().to_string()).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    };

    let hub = Arc::new(Hub { state: Mutex::new(BTreeMap::new()), tx: broadcast::channel(256).0 });
    let mut cmds = HashMap::new();
    let mut bindings = Vec::new();
    let mut channel = |dev: &str| {
        let (tx, rx) = mpsc::channel(16);
        cmds.insert(dev.to_string(), tx);
        rx
    };

    if let Some(c) = cfg.hue {
        for (el, target) in &c.elements {
            let kind = if target.starts_with("group/") { "group" } else { "light" };
            bindings.push(json!({ "el": el, "id": format!("hue/{target}"), "kind": kind, "dev": "hue", "target": target }));
        }
        tokio::spawn(hue::run(c, read_secret("hue-key"), hub.clone(), channel("hue")));
    }
    for c in cfg.bluos {
        bindings.push(json!({ "el": c.element, "id": c.name, "kind": "bluos", "dev": c.name }));
        let rx = channel(&c.name);
        tokio::spawn(bluos::run(c, hub.clone(), rx));
    }
    if let Some(c) = cfg.onkyo {
        bindings.push(json!({ "el": c.element, "id": "onkyo", "kind": "onkyo", "dev": "onkyo" }));
        tokio::spawn(onkyo::run(c, hub.clone(), channel("onkyo")));
    }
    if let Some(c) = cfg.lgtv {
        bindings.push(json!({ "el": c.element, "id": "lgtv", "kind": "lgtv", "dev": "lgtv" }));
        tokio::spawn(lgtv::run(c, read_secret("lgtv-key"), hub.clone(), channel("lgtv")));
    }
    if let Some(c) = cfg.tado {
        for (zone, z) in &c.zones {
            bindings.push(json!({ "el": z.element, "id": format!("tado/{zone}"), "kind": "tado", "dev": "tado",
                                  "zone": zone, "val": z.value, "name": z.name }));
        }
        tokio::spawn(tado::run(c, data.join("tado-token.json"), hub.clone(), channel("tado")));
    }
    for el in cfg.panel.unavailable {
        bindings.push(json!({ "el": el, "kind": "unavailable" }));
    }
    for el in cfg.panel.blank {
        bindings.push(json!({ "el": el, "kind": "blank" }));
    }

    let app = Arc::new(App { hub, cmds, bindings: bindings.into() });
    let router = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/app.js", get(|| async { ([(CONTENT_TYPE, "text/javascript")], APP_JS) }))
        .route("/app.css", get(|| async { ([(CONTENT_TYPE, "text/css")], APP_CSS) }))
        .route("/api/config", get(|State(app): State<Arc<App>>| async move { Json(app.bindings.clone()) }))
        .route("/api/events", get(events))
        .route("/api/cmd", post(command))
        .nest_service("/floorplan", ServeDir::new(floorplan))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    eprintln!("house-panel listening on :{port}");
    axum::serve(listener, router).await.unwrap();
}

/// Server-sent events: a full snapshot first, then one message per change.
async fn events(State(app): State<Arc<App>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before taking the snapshot so no change falls in between.
    let rx = app.hub.tx.subscribe();
    let first = Event::default().data(app.hub.snapshot());
    let rest = stream::unfold((rx, app), |(mut rx, app)| async move {
        let data = match rx.recv().await {
            Ok(msg) => msg,
            // A slow client missed changes: resend everything instead.
            Err(broadcast::error::RecvError::Lagged(_)) => app.hub.snapshot(),
            Err(broadcast::error::RecvError::Closed) => return None,
        };
        Some((Ok(Event::default().data(data)), (rx, app)))
    });
    Sse::new(stream::once(async { Ok(first) }).chain(rest)).keep_alive(KeepAlive::default())
}

/// `{"dev": "<device>", ...}` is handed to that device's task; each task validates its own fields.
async fn command(State(app): State<Arc<App>>, Json(cmd): Json<Map<String, Value>>) -> StatusCode {
    let Some(tx) = cmd.get("dev").and_then(Value::as_str).and_then(|d| app.cmds.get(d)) else {
        return StatusCode::NOT_FOUND;
    };
    match tx.try_send(Value::Object(cmd)) {
        Ok(()) => StatusCode::ACCEPTED,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}
