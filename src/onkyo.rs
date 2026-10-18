//! Onkyo receiver over eISCP (TCP 60128). The receiver pushes every change on the open connection.

use crate::{Hub, xml};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::timeout,
};

#[derive(Deserialize)]
pub struct Config {
    pub ip: String,
    pub element: String,
}

pub async fn run(cfg: Config, hub: Arc<Hub>, mut rx: mpsc::Receiver<Value>) {
    loop {
        if let Err(e) = session(&cfg.ip, &hub, &mut rx).await {
            eprintln!("onkyo: {e}");
        }
        hub.merge("onkyo", json!({ "online": false }));
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn session(ip: &str, hub: &Arc<Hub>, rx: &mut mpsc::Receiver<Value>) -> std::io::Result<()> {
    let mut sock = timeout(Duration::from_secs(5), TcpStream::connect((ip, 60128))).await??;
    for q in ["NRIQSTN", "PWRQSTN", "MVLQSTN", "AMTQSTN", "SLIQSTN"] {
        sock.write_all(&packet(q)).await?;
    }
    hub.merge("onkyo", json!({ "online": true }));
    let (rd, mut wr) = sock.into_split();
    let mut reader = tokio::spawn(read_loop(rd, hub.clone()));
    // A quiet query now and then so a dead connection fails the write and gets noticed.
    let mut ping = tokio::time::interval(Duration::from_secs(600));
    let res = loop {
        let r = tokio::select! {
            r = &mut reader => break r.unwrap_or_else(|e| Err(std::io::Error::other(e))),
            Some(c) = rx.recv() => match command(&c) {
                Some(cmd) => wr.write_all(&packet(&cmd)).await,
                None => Ok(()),
            },
            _ = ping.tick() => wr.write_all(&packet("PWRQSTN")).await,
        };
        if let Err(e) = r {
            break Err(e);
        }
    };
    reader.abort();
    res
}

async fn read_loop(mut rd: tokio::net::tcp::OwnedReadHalf, hub: Arc<Hub>) -> std::io::Result<()> {
    loop {
        let mut head = [0u8; 16];
        rd.read_exact(&mut head).await?;
        if &head[..4] != b"ISCP" {
            return Err(std::io::Error::other("bad eISCP header"));
        }
        let len = u32::from_be_bytes(head[8..12].try_into().unwrap()) as usize;
        if len > 1 << 20 {
            return Err(std::io::Error::other("eISCP message too large"));
        }
        let mut data = vec![0; len];
        rd.read_exact(&mut data).await?;
        if let Some(patch) = parse(&String::from_utf8_lossy(&data)) {
            hub.merge("onkyo", patch);
        }
    }
}

fn packet(cmd: &str) -> Vec<u8> {
    let data = format!("!1{cmd}\r");
    let mut p = b"ISCP".to_vec();
    p.extend(16u32.to_be_bytes());
    p.extend((data.len() as u32).to_be_bytes());
    p.extend([1, 0, 0, 0]);
    p.extend(data.as_bytes());
    p
}

fn parse(msg: &str) -> Option<Value> {
    let msg = msg.strip_prefix("!1")?.trim_end_matches(['\x1a', '\r', '\n']);
    let (cmd, param) = msg.split_at_checked(3)?;
    Some(match cmd {
        "PWR" => json!({ "power": param == "01" }),
        "AMT" => json!({ "mute": param == "01" }),
        "MVL" => json!({ "volume": u32::from_str_radix(param, 16).ok()? }),
        "SLI" => json!({ "input": param.to_lowercase() }),
        "NRI" => {
            let inputs: Vec<Value> = xml::elements(param, "selector")
                .iter()
                // zone is a bitmask; bit 1 = main zone. value 0 = hidden input.
                .filter(|s| xml::attr(s, "value").as_deref() != Some("0"))
                .filter(|s| xml::attr(s, "zone").and_then(|z| u8::from_str_radix(&z, 16).ok()).is_some_and(|z| z & 1 == 1))
                .map(|s| json!({ "id": xml::attr(s, "id").map(|i| i.to_lowercase()), "name": xml::attr(s, "name") }))
                .collect();
            json!({ "inputs": inputs })
        }
        _ => return None,
    })
}

/// Commands: power (on), vol (step +1/-1), mute, input (2-digit hex id).
fn command(c: &Value) -> Option<String> {
    Some(match c["action"].as_str()? {
        "power" => format!("PWR{}", if c["on"].as_bool()? { "01" } else { "00" }),
        "vol" => (if c["step"].as_i64()? > 0 { "MVLUP" } else { "MVLDOWN" }).into(),
        "mute" => "AMTTG".into(),
        "input" => {
            let id = c["id"].as_str().filter(|i| i.len() == 2 && i.bytes().all(|b| b.is_ascii_hexdigit()))?;
            format!("SLI{}", id.to_uppercase())
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_builds() {
        assert_eq!(parse("!1PWR01\x1a\r\n"), Some(json!({ "power": true })));
        assert_eq!(parse("!1MVL50\x1a"), Some(json!({ "volume": 80 })));
        assert_eq!(parse("!1MVLN/A\x1a"), None);
        assert_eq!(parse("!1SLI2B\x1a"), Some(json!({ "input": "2b" })));
        let nri = r#"!1NRI<selectorlist><selector id="11" value="1" name="STRM BOX" zone="03" /><selector id="80" value="1" name="Source" zone="06" /><selector id="2e" value="0" name="BT" zone="07" /></selectorlist>"#;
        assert_eq!(parse(nri), Some(json!({ "inputs": [{ "id": "11", "name": "STRM BOX" }] })));
        assert_eq!(command(&json!({ "action": "input", "id": "2b" })).as_deref(), Some("SLI2B"));
        assert_eq!(command(&json!({ "action": "input", "id": "2b\r" })), None);
        assert_eq!(command(&json!({ "action": "vol", "step": -1 })).as_deref(), Some("MVLDOWN"));
        assert_eq!(&packet("PWRQSTN")[..16], b"ISCP\0\0\0\x10\0\0\0\x0a\x01\0\0\0");
    }
}
