//! Finding Syphon nodes on the home network: a broadcast on UDP 8095, answered by every node that hears it.

use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

pub const PORT: u16 = 8095;
const ASK: &[u8] = b"SYPHON-DISCOVER 1";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Found {
    pub name: String,
    pub id: String,
    pub url: String,
}

/// Every broadcast address this computer can send to (the general one plus each home network's own).
fn targets(port: u16) -> Vec<SocketAddr> {
    // the general broadcast, this computer itself (a node on the same machine), and each home network's own broadcast
    let mut v = vec![SocketAddr::from((Ipv4Addr::BROADCAST, port)), SocketAddr::from((Ipv4Addr::LOCALHOST, port))];
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(a) = i.addr {
            if a.ip.is_loopback() {
                continue;
            }
            if let Some(b) = a.broadcast {
                v.push(SocketAddr::from((b, port)));
            }
        }
    }
    v.dedup();
    v
}

pub fn parse_answer(data: &[u8], from: SocketAddr) -> Option<Found> {
    let v: serde_json::Value = serde_json::from_slice(data).ok()?;
    if v["syphon"].as_u64() != Some(1) {
        return None;
    }
    let port = v["port"].as_u64().filter(|p| *p > 0 && *p < 65536)?;
    Some(Found { name: v["name"].as_str().unwrap_or("Syphon").chars().take(80).collect(), id: v["id"].as_str().unwrap_or("").chars().take(32).collect(), url: format!("http://{}:{}", from.ip(), port) })
}

/// Ask, and collect answers for a moment.
pub async fn find(port: u16, wait: Duration, extra: &[SocketAddr]) -> Vec<Found> {
    let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else { return vec![] };
    let _ = sock.set_broadcast(true);
    for t in targets(port).iter().chain(extra.iter()) {
        let _ = sock.send_to(ASK, t).await;
    }
    let mut out: Vec<Found> = vec![];
    let end = Instant::now() + wait;
    let mut buf = [0u8; 1024];
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(left, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if let Some(f) = parse_answer(&buf[..n], from) {
                    if !out.iter().any(|x| x.url == f.url) {
                        out.push(f);
                    }
                }
            }
            _ => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read_and_junk_is_ignored() {
        let from: SocketAddr = "192.168.1.20:8095".parse().unwrap();
        let f = parse_answer(br#"{"syphon":1,"name":"Living Room","id":"ab12","port":8096}"#, from).unwrap();
        assert_eq!(f.url, "http://192.168.1.20:8096");
        assert_eq!(f.name, "Living Room");
        assert!(parse_answer(b"hello", from).is_none());
        assert!(parse_answer(br#"{"syphon":2,"port":8096}"#, from).is_none());
        assert!(parse_answer(br#"{"syphon":1,"port":0}"#, from).is_none());
    }

    #[tokio::test]
    async fn a_node_that_answers_is_found() {
        let node = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = node.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut b = [0u8; 64];
            let (n, from) = node.recv_from(&mut b).await.unwrap();
            assert_eq!(&b[..n], ASK);
            node.send_to(br#"{"syphon":1,"name":"Test","id":"x","port":18096}"#, from).await.unwrap();
        });
        let found = find(port, Duration::from_millis(800), &[SocketAddr::from(([127, 0, 0, 1], port))]).await;
        assert_eq!(found, vec![Found { name: "Test".into(), id: "x".into(), url: "http://127.0.0.1:18096".into() }]);
    }
}
