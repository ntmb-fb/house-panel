//! tado° heating over the cloud API. tado limits free accounts to about 100 requests a day,
//! so zone states are read every `interval_minutes` and right after a change, and the
//! valve battery states only every 8th time.

use crate::Hub;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::{Duration, Instant}};
use tokio::sync::mpsc;

const API: &str = "https://my.tado.com/api/v2";
const TOKEN_URL: &str = "https://login.tado.com/oauth2/token";
const CLIENT_ID: &str = "1bb50063-6b0c-4d11-bd99-387f4a91cc46";

#[derive(Deserialize)]
pub struct Config {
    pub home: u64,
    #[serde(default = "default_interval")]
    pub interval_minutes: u64,
    pub zones: BTreeMap<String, Zone>,
}

#[derive(Deserialize)]
pub struct Zone {
    pub element: String,
    pub value: Option<String>,
    pub name: Option<String>,
}

fn default_interval() -> u64 {
    30
}

struct Client {
    http: reqwest::Client,
    path: PathBuf,
    access: String,
    expires: Instant,
}

impl Client {
    /// A valid access token; refreshes it (and saves the rotated refresh token) when expired.
    async fn token(&mut self) -> Result<String, String> {
        if Instant::now() < self.expires {
            return Ok(self.access.clone());
        }
        let saved: Value = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .ok_or_else(|| format!("{} missing or unreadable", self.path.display()))?;
        let refresh = saved["refresh_token"].as_str().ok_or("no refresh_token in token file")?;
        let res = self
            .http
            .post(TOKEN_URL)
            .form(&[("client_id", CLIENT_ID), ("grant_type", "refresh_token"), ("refresh_token", refresh)])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("token refresh failed ({}); redo the device login", res.status()));
        }
        let tok = save_token(&self.path, res.json().await.map_err(|e| e.to_string())?, refresh)?;
        let access = tok["access_token"].as_str().unwrap_or_default().to_string();
        self.expires = Instant::now() + Duration::from_secs(tok["expires_in"].as_u64().unwrap_or(600).saturating_sub(60));
        self.access = access.clone();
        Ok(access)
    }

    async fn call(&mut self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        let token = self.token().await?;
        let mut req = self.http.request(method, format!("{API}{path}")).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
        Ok(res.json().await.unwrap_or(Value::Null)) // DELETE answers with an empty body
    }
}

/// Saves a refresh reply, keeping the old refresh token if the reply has none,
/// so the file always holds a usable login.
fn save_token(path: &std::path::Path, mut tok: Value, old_refresh: &str) -> Result<Value, String> {
    if tok["access_token"].as_str().is_none() {
        return Err("no access_token in refresh reply".into());
    }
    if tok["refresh_token"].as_str().is_none() {
        tok["refresh_token"] = old_refresh.into();
    }
    // Write-then-rename so a crash never leaves a half-written token file.
    let tmp = path.with_extension("tmp");
    let write = || {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(tok.to_string().as_bytes())?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| e.to_string())?;
    Ok(tok)
}

pub async fn run(cfg: Config, token_path: PathBuf, hub: Arc<Hub>, mut rx: mpsc::Receiver<Value>) {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap();
    let mut c = Client { http, path: token_path, access: String::new(), expires: Instant::now() };
    let mut round = 0u64;
    loop {
        // After a failure (e.g. no internet yet after a reboot) try again in 2 minutes.
        let minutes = match poll(&mut c, &cfg, &hub, round % 8 == 0).await {
            Ok(()) => cfg.interval_minutes,
            Err(e) => {
                eprintln!("tado: {e}");
                2
            }
        };
        round += 1;
        let wait = tokio::time::sleep(Duration::from_secs(minutes * 60));
        tokio::select! {
            _ = wait => {}
            Some(cmd) = rx.recv() => {
                if let Err(e) = command(&mut c, &cfg, &cmd).await {
                    eprintln!("tado: command failed: {e}");
                }
            }
        }
    }
}

async fn poll(c: &mut Client, cfg: &Config, hub: &Hub, battery: bool) -> Result<(), String> {
    let states = c.call(reqwest::Method::GET, &format!("/homes/{}/zoneStates", cfg.home), None).await?;
    for (zone, patch) in parse_states(&states, cfg) {
        hub.merge(&format!("tado/{zone}"), patch);
    }
    if battery {
        let zones = c.call(reqwest::Method::GET, &format!("/homes/{}/zones", cfg.home), None).await?;
        for z in zones.as_array().into_iter().flatten() {
            let low = z["devices"].as_array().into_iter().flatten().any(|d| d["batteryState"] == "LOW");
            hub.merge(&format!("tado/{}", z["id"]), json!({ "low_battery": low }));
        }
    }
    Ok(())
}

fn parse_states(v: &Value, cfg: &Config) -> Vec<(String, Value)> {
    let Some(zones) = v["zoneStates"].as_object() else { return vec![] };
    zones
        .iter()
        .filter(|(id, _)| cfg.zones.contains_key(*id))
        .map(|(id, z)| {
            let s = &z["sensorDataPoints"];
            let patch = json!({
                "temp": s["insideTemperature"]["celsius"],
                "humidity": s["humidity"]["percentage"],
                "power": z["setting"]["power"],
                "target": z["setting"]["temperature"]["celsius"],
                "heating": z["activityDataPoints"]["heatingPower"]["percentage"].as_f64().unwrap_or(0.0) > 0.0,
                "overlay": !z["overlay"].is_null(),
            });
            (id.clone(), patch)
        })
        .collect()
}

/// `{"zone": "7", "target": 21.5}` sets a manual temperature until the next schedule block;
/// `{"zone": "7", "resume": true}` drops it and follows the schedule again.
async fn command(c: &mut Client, cfg: &Config, cmd: &Value) -> Result<(), String> {
    let zone = cmd["zone"].as_str().filter(|z| cfg.zones.contains_key(*z)).ok_or("unknown zone")?;
    let path = format!("/homes/{}/zones/{zone}/overlay", cfg.home);
    if cmd["resume"] == true {
        c.call(reqwest::Method::DELETE, &path, None).await?;
    } else {
        let t = cmd["target"].as_f64().filter(|t| (5.0..=25.0).contains(t)).ok_or("target must be 5-25")?;
        let body = json!({
            "setting": { "type": "HEATING", "power": "ON", "temperature": { "celsius": t } },
            "termination": { "typeSkillBasedApp": "NEXT_TIME_BLOCK" },
        });
        c.call(reqwest::Method::PUT, &path, Some(body)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zone_states() {
        let cfg: Config = toml::from_str("home = 1\n[zones.7]\nelement = \"dev-tado_stue\"\n").unwrap();
        assert_eq!(cfg.interval_minutes, 30);
        let v = json!({ "zoneStates": {
            "7": { "setting": { "power": "ON", "temperature": { "celsius": 19.0 } }, "overlay": null,
                   "activityDataPoints": { "heatingPower": { "percentage": 12.0 } },
                   "sensorDataPoints": { "insideTemperature": { "celsius": 21.4 }, "humidity": { "percentage": 55.1 } } },
            "8": { "setting": { "power": "OFF" } },
        }});
        let out = parse_states(&v, &cfg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1["temp"], 21.4);
        assert_eq!(out[0].1["heating"], true);
        assert_eq!(out[0].1["overlay"], false);
    }

    #[test]
    fn token_file_keeps_a_refresh_token() {
        let path = std::env::temp_dir().join(format!("tado-token-test-{}.json", std::process::id()));
        save_token(&path, json!({ "access_token": "a1" }), "old").unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["refresh_token"], "old");
        save_token(&path, json!({ "access_token": "a2", "refresh_token": "new" }), "old").unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["refresh_token"], "new");
        assert!(save_token(&path, json!({ "error": "x" }), "old").is_err());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_file(&path).unwrap();
    }
}
