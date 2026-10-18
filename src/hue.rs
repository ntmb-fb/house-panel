//! Philips Hue bridge: v1 REST for the initial state and commands, the v2 event stream for live changes.

use crate::Hub;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;

#[derive(Deserialize)]
pub struct Config {
    pub ip: String,
    /// Floorplan element id -> "light/<id>" or "group/<id>" (v1 ids).
    pub elements: BTreeMap<String, String>,
}

pub async fn run(cfg: Config, key: String, hub: Arc<Hub>, mut rx: mpsc::Receiver<Value>) {
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // the bridge only has a self-signed certificate
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(300)) // the event stream sends keep-alives; silence means it died
        .build()
        .unwrap();
    let base = format!("http://{}/api/{key}", cfg.ip);

    let (h, b) = (http.clone(), base.clone());
    tokio::spawn(async move {
        while let Some(c) = rx.recv().await {
            if let Err(e) = command(&h, &b, &c).await {
                eprintln!("hue: command failed: {e}");
            }
        }
    });

    loop {
        // Re-read everything on each (re)connect so changes missed while disconnected are picked up.
        let res = async {
            sync(&http, &base, &hub).await?;
            follow(&http, &cfg.ip, &key, &hub).await
        };
        if let Err(e) = res.await {
            eprintln!("hue: {e}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn sync(http: &reqwest::Client, base: &str, hub: &Hub) -> Result<(), String> {
    let get = |path: &str| {
        let req = http.get(format!("{base}/{path}")).timeout(Duration::from_secs(10));
        async move { req.send().await?.json::<Map<String, Value>>().await }
    };
    for (id, l) in get("lights").await.map_err(|e| e.to_string())? {
        let s = &l["state"];
        hub.merge(&format!("hue/light/{id}"), json!({ "name": l["name"], "on": s["on"], "bri": s["bri"], "reachable": s["reachable"] }));
    }
    for (id, g) in get("groups").await.map_err(|e| e.to_string())? {
        hub.merge(&format!("hue/group/{id}"), json!({ "name": g["name"], "on": g["state"]["any_on"] }));
    }
    Ok(())
}

async fn follow(http: &reqwest::Client, ip: &str, key: &str, hub: &Hub) -> Result<(), String> {
    let res = http
        .get(format!("https://{ip}/eventstream/clip/v2"))
        .header("hue-application-key", key)
        .header("accept", "text/event-stream")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let mut body = res.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            if let Some(data) = line.strip_prefix(b"data: ") {
                if let Ok(events) = serde_json::from_slice::<Vec<Value>>(data) {
                    for (id, patch) in events.iter().flat_map(updates) {
                        hub.merge(&id, patch);
                    }
                }
            }
        }
    }
    Err("event stream closed".into())
}

/// State changes in one v2 event, translated to v1 state ids.
fn updates(event: &Value) -> Vec<(String, Value)> {
    let Some(items) = event["data"].as_array().filter(|_| event["type"] == "update") else { return vec![] };
    items
        .iter()
        .filter_map(|d| {
            let (kind, id) = d["id_v1"].as_str()?.trim_start_matches('/').split_once('/')?;
            let kind = match kind { "lights" => "light", "groups" => "group", _ => return None };
            let mut patch = Map::new();
            if let Some(on) = d["on"]["on"].as_bool() {
                patch.insert("on".into(), on.into());
            }
            if let Some(b) = d["dimming"]["brightness"].as_f64() {
                patch.insert("bri".into(), ((b * 2.54).round().max(1.0) as u64).into());
            }
            if let Some(status) = d["status"].as_str() {
                patch.insert("reachable".into(), (status == "connected").into());
            }
            (!patch.is_empty()).then(|| (format!("hue/{kind}/{id}"), Value::Object(patch)))
        })
        .collect()
}

/// `{"target": "light/19" | "group/86", "on": bool?, "bri": 1-254?}`
async fn command(http: &reqwest::Client, base: &str, c: &Value) -> Result<(), String> {
    let target = c["target"].as_str().unwrap_or("");
    let path = match target.split_once('/') {
        Some(("light", id)) if is_id(id) => format!("lights/{id}/state"),
        Some(("group", id)) if is_id(id) => format!("groups/{id}/action"),
        _ => return Err(format!("bad target {target:?}")),
    };
    let mut body = Map::new();
    if let Some(on) = c["on"].as_bool() {
        body.insert("on".into(), on.into());
    }
    if let Some(bri) = c["bri"].as_u64() {
        body.insert("bri".into(), bri.clamp(1, 254).into());
    }
    http.put(format!("{base}/{path}"))
        .timeout(Duration::from_secs(10))
        .json(&body)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn is_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_v2_events() {
        let ev = json!({ "type": "update", "data": [
            { "id_v1": "/lights/19", "on": { "on": true }, "dimming": { "brightness": 50.0 }, "type": "light" },
            { "id_v1": "/groups/86", "on": { "on": false }, "type": "grouped_light" },
            { "id_v1": "/lights/12", "status": "connectivity_issue", "type": "zigbee_connectivity" },
            { "id_v1": "/sensors/5", "button": {}, "type": "button" },
        ]});
        assert_eq!(updates(&ev), vec![
            ("hue/light/19".into(), json!({ "on": true, "bri": 127 })),
            ("hue/group/86".into(), json!({ "on": false })),
            ("hue/light/12".into(), json!({ "reachable": false })),
        ]);
        assert!(updates(&json!({ "type": "add", "data": [{ "id_v1": "/lights/1", "on": { "on": true } }] })).is_empty());
        assert!(is_id("19") && !is_id("19/../x") && !is_id(""));
    }
}
