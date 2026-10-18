//! LG webOS TV over its SSAP websocket (wss :3001). Power, volume and the foreground app are
//! subscriptions, so the TV pushes changes. The TV is off the network when off; Wake-on-LAN turns it on.

use crate::Hub;
use futures_util::{SinkExt, StreamExt};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpStream, sync::mpsc, time::timeout};
use tokio_tungstenite::tungstenite::Message;

#[derive(Deserialize)]
pub struct Config {
    pub ip: String,
    pub mac: String,
    pub element: String,
}

const MANIFEST: &str = include_str!("lg_manifest.json");
const APPS: [&str; 3] = ["com.webos.app.home", "netflix", "youtube.leanback.v4"];

pub async fn run(cfg: Config, key: String, hub: Arc<Hub>, mut rx: mpsc::Receiver<Value>) {
    hub.merge("lgtv", json!({ "power": false }));
    loop {
        if let Err(e) = session(&cfg, &key, &hub, &mut rx).await {
            // Connection refused/timeout is the normal "TV is off" case, so only log the rest.
            if !e.contains("connect") {
                eprintln!("lgtv: {e}");
            }
        }
        hub.merge("lgtv", json!({ "power": false }));
        // While off, only "power on" can be served; retry the connection every 15 s.
        if let Ok(Some(c)) = timeout(Duration::from_secs(15), rx.recv()).await {
            if c["action"] == "power" && c["on"] == true {
                wake(&cfg).await;
            }
        }
    }
}

async fn session(cfg: &Config, key: &str, hub: &Hub, rx: &mut mpsc::Receiver<Value>) -> Result<(), String> {
    let tcp = timeout(Duration::from_secs(3), TcpStream::connect((cfg.ip.as_str(), 3001)))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;
    let tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert)) // the TV only has a self-signed certificate
        .with_no_client_auth();
    let name = ServerName::try_from(cfg.ip.clone()).map_err(|e| e.to_string())?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(tls)).connect(name, tcp).await.map_err(|e| e.to_string())?;
    let (mut ws, _) = tokio_tungstenite::client_async(format!("wss://{}:3001", cfg.ip), tls).await.map_err(|e| e.to_string())?;

    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    let register = json!({ "type": "register", "id": "register_0", "payload": {
        "client-key": key, "forcePairing": false, "manifest": manifest, "pairingType": "PROMPT" } });
    ws.send(Message::text(register.to_string())).await.map_err(|e| e.to_string())?;
    loop {
        let msg = timeout(Duration::from_secs(10), ws.next()).await.map_err(|_| "no registration reply")?;
        let msg: Value = match msg {
            Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap_or_default(),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.to_string()),
            None => return Err("closed during registration".into()),
        };
        match msg["type"].as_str() {
            Some("registered") => break,
            Some("error") => return Err(format!("registration refused: {}", msg["error"])),
            _ => {}
        }
    }

    for (id, uri) in [
        ("power", "com.webos.service.tvpower/power/getPowerState"),
        ("volume", "audio/getVolume"),
        ("app", "com.webos.applicationManager/getForegroundAppInfo"),
    ] {
        let sub = json!({ "type": "subscribe", "id": id, "uri": format!("ssap://{uri}") });
        ws.send(Message::text(sub.to_string())).await.map_err(|e| e.to_string())?;
    }
    hub.merge("lgtv", json!({ "power": true }));

    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut n = 0;
    loop {
        tokio::select! {
            // Pings every 20 s get pongs, so 60 s of silence means the TV went away.
            msg = timeout(Duration::from_secs(60), ws.next()) => {
                match msg.map_err(|_| "TV stopped answering")? {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if let Some(patch) = parse(&v) {
                                hub.merge("lgtv", patch);
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.to_string()),
                    None => return Ok(()),
                }
            }
            _ = ping.tick() => ws.send(Message::Ping(Default::default())).await.map_err(|e| e.to_string())?,
            Some(c) = rx.recv() => {
                if c["action"] == "power" && c["on"] == true {
                    wake(cfg).await; // on but in standby: WoL wakes it fully
                } else if let Some((uri, payload)) = request(&c, &hub.get("lgtv")) {
                    n += 1;
                    let req = json!({ "type": "request", "id": format!("cmd_{n}"), "uri": format!("ssap://{uri}"), "payload": payload });
                    ws.send(Message::text(req.to_string())).await.map_err(|e| e.to_string())?;
                }
            }
        }
    }
}

fn parse(v: &Value) -> Option<Value> {
    let p = &v["payload"];
    match v["id"].as_str()? {
        // "Active", "Screen Off" (sound only) = on; "Active Standby", "Suspend", "Power Off" = off.
        "power" => Some(json!({ "power": matches!(p["state"].as_str()?, "Active" | "Screen Off") })),
        "volume" => {
            let s = if p["volumeStatus"].is_object() { &p["volumeStatus"] } else { p };
            let mute = s["muteStatus"].as_bool().or(s["muted"].as_bool());
            Some(json!({ "volume": s["volume"].as_i64()?, "mute": mute }))
        }
        "app" => Some(json!({ "app": p["appId"].as_str()? })),
        _ => None,
    }
}

/// Commands: power (on=false), vol (step), mute, app (one of APPS).
fn request(c: &Value, state: &Value) -> Option<(&'static str, Value)> {
    Some(match c["action"].as_str()? {
        "power" => ("system/turnOff", json!({})),
        "vol" if c["step"].as_i64()? > 0 => ("audio/volumeUp", json!({})),
        "vol" => ("audio/volumeDown", json!({})),
        "mute" => ("audio/setMute", json!({ "mute": state["mute"] != true })),
        "app" => {
            let id = c["id"].as_str().filter(|id| APPS.contains(id))?;
            ("system.launcher/launch", json!({ "id": id }))
        }
        _ => return None,
    })
}

/// Magic packet (6×FF + MAC×16) to the global and the subnet broadcast address.
async fn wake(cfg: &Config) {
    let mac: Vec<u8> = cfg.mac.split(':').filter_map(|b| u8::from_str_radix(b, 16).ok()).collect();
    if mac.len() != 6 {
        eprintln!("lgtv: bad mac {:?}", cfg.mac);
        return;
    }
    let mut pkt = vec![0xff; 6];
    for _ in 0..16 {
        pkt.extend(&mac);
    }
    let subnet = cfg.ip.rsplit_once('.').map(|(net, _)| format!("{net}.255")).unwrap_or_default();
    let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else { return };
    let _ = sock.set_broadcast(true);
    for addr in ["255.255.255.255", subnet.as_str()] {
        for port in [9, 7] {
            if let Err(e) = sock.send_to(&pkt, (addr, port)).await {
                eprintln!("lgtv: wake-on-lan to {addr}:{port}: {e}");
            }
        }
    }
}

#[derive(Debug)]
struct AnyCert;

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer,
        _: &[CertificateDer],
        _: &ServerName,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_builds() {
        assert_eq!(parse(&json!({ "id": "power", "payload": { "state": "Active Standby" } })), Some(json!({ "power": false })));
        assert_eq!(
            parse(&json!({ "id": "volume", "payload": { "volumeStatus": { "volume": 12, "muteStatus": true } } })),
            Some(json!({ "volume": 12, "mute": true }))
        );
        assert_eq!(parse(&json!({ "id": "volume", "payload": { "volume": 5, "muted": false } })), Some(json!({ "volume": 5, "mute": false })));
        assert_eq!(request(&json!({ "action": "mute" }), &json!({ "mute": true })).unwrap().1, json!({ "mute": false }));
        assert!(request(&json!({ "action": "app", "id": "com.evil" }), &json!({})).is_none());
        assert!(serde_json::from_str::<Value>(MANIFEST).is_ok());
    }
}
