//! Bluesound players over the BluOS HTTP API. `/Status` is long-polled with its etag,
//! so the player only answers when something changes.

use crate::{Hub, xml};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Deserialize)]
pub struct Config {
    pub name: String,
    /// Every address the player can have (wired and Wi-Fi); the first one that answers is used.
    pub ips: Vec<String>,
    pub element: String,
}

pub async fn run(cfg: Config, hub: Arc<Hub>, mut rx: mpsc::Receiver<Value>) {
    assert!(!cfg.ips.is_empty(), "bluos {}: ips is empty", cfg.name);
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build().unwrap();
    let current = Arc::new(AtomicUsize::new(0));
    hub.merge(&cfg.name, json!({ "name": cfg.name, "online": false }));

    let (h, hb, cur, ips, id) = (http.clone(), hub.clone(), current.clone(), cfg.ips.clone(), cfg.name.clone());
    tokio::spawn(async move {
        while let Some(c) = rx.recv().await {
            let ip = &ips[cur.load(Ordering::Relaxed)];
            let Some(path) = command_path(&c, &hb.get(&id)) else { continue };
            let res = h.get(format!("http://{ip}:11000{path}")).timeout(Duration::from_secs(10)).send().await;
            if let Err(e) = res.and_then(|r| r.error_for_status()) {
                eprintln!("{id}: command failed: {e}");
            }
        }
    });

    let mut i = 0;
    loop {
        current.store(i, Ordering::Relaxed);
        if let Err(e) = follow(&http, &cfg.ips[i], &cfg.name, &hub).await {
            eprintln!("{} at {}: {e}", cfg.name, cfg.ips[i]);
        }
        i = (i + 1) % cfg.ips.len();
        if i == 0 {
            hub.merge(&cfg.name, json!({ "online": false }));
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }
}

async fn follow(http: &reqwest::Client, ip: &str, id: &str, hub: &Hub) -> Result<(), reqwest::Error> {
    let get = |path: String| {
        // The player holds a long poll for up to 100 s.
        let req = http.get(format!("http://{ip}:11000{path}")).timeout(Duration::from_secs(130));
        async move { req.send().await?.error_for_status()?.text().await }
    };
    // shortcut: presets are read once per connection; edits in the BluOS app show after a reconnect or restart.
    let presets = get("/Presets".into()).await?;
    let presets: Vec<Value> = xml::elements(&presets, "preset")
        .iter()
        .map(|p| json!({ "id": xml::attr(p, "id"), "name": xml::attr(p, "name").map(|n| n.replace('+', " ").trim().to_string()) }))
        .collect();
    hub.merge(id, json!({ "presets": presets }));

    let mut etag = String::new();
    loop {
        let status = get(format!("/Status?timeout=100&etag={etag}")).await?;
        etag = xml::attr(&status, "etag").unwrap_or_default();
        hub.merge(id, parse_status(&status));
    }
}

fn parse_status(s: &str) -> Value {
    let t = |name| xml::tag(s, name);
    let num = |name| t(name).and_then(|v| v.parse::<i64>().ok());
    json!({
        "online": true,
        "state": t("state"),
        "volume": num("volume"),
        "mute": num("mute") == Some(1),
        "title": t("title1").or(t("name")),
        "artist": t("title2").or(t("artist")),
        "radio": t("serviceType").as_deref() == Some("RadioService"),
    })
}

/// Commands: play, pause, next, prev, preset (id), vol (+/- step), mute.
fn command_path(c: &Value, state: &Value) -> Option<String> {
    let radio = state["radio"].as_bool().unwrap_or(false);
    Some(match c["action"].as_str()? {
        "play" => "/Play".into(),
        "pause" => "/Pause".into(),
        "next" if radio => "/Preset?id=%2B1".into(),
        "prev" if radio => "/Preset?id=-1".into(),
        "next" => "/Skip".into(),
        "prev" => "/Back".into(),
        "preset" => format!("/Preset?id={}", c["id"].as_str().filter(|s| s.bytes().all(|b| b.is_ascii_digit()))?),
        "vol" => format!("/Volume?level={}", (state["volume"].as_i64()? + c["step"].as_i64()?).clamp(0, 100)),
        "mute" => format!("/Volume?mute={}", if state["mute"] == true { 0 } else { 1 }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_commands() {
        let s = parse_status(
            r#"<status etag="x"><mute>0</mute><serviceType>RadioService</serviceType><state>pause</state><title1>Radio Soft</title1><volume>24</volume></status>"#,
        );
        assert_eq!(s["volume"], 24);
        assert_eq!(s["radio"], true);
        assert_eq!(s["title"], "Radio Soft");
        assert_eq!(command_path(&json!({ "action": "next" }), &s).as_deref(), Some("/Preset?id=%2B1"));
        assert_eq!(command_path(&json!({ "action": "next" }), &json!({})).as_deref(), Some("/Skip"));
        assert_eq!(command_path(&json!({ "action": "vol", "step": 90 }), &s).as_deref(), Some("/Volume?level=100"));
        assert_eq!(command_path(&json!({ "action": "mute" }), &s).as_deref(), Some("/Volume?mute=1"));
        assert_eq!(command_path(&json!({ "action": "preset", "id": "3" }), &s).as_deref(), Some("/Preset?id=3"));
        assert_eq!(command_path(&json!({ "action": "preset", "id": "3&x" }), &s), None);
    }
}
