//! The app's own WireGuard, for reaching Syphon through the VPN box at home. It runs entirely inside the app: WireGuard
//! itself (boringtun) and a small TCP/IP stack (smoltcp). No network adapter, no driver and no administrator rights are
//! needed, and nothing else on the computer goes through it. The rest of the app talks to it through a port on
//! 127.0.0.1 that leads to the Syphon server at home.
//!
//! First contact: a code from `syphon-vpn add` holds a one-time welcome key, which reaches only the box's welcome desk.
//! The app makes a key of its own, hands its public half to the desk, and from then on connects with its own key.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use serde::{Deserialize, Serialize};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

pub const CODE_PREFIX: &str = "SYPHON-VPN-1:";
const MTU: usize = 1420;
const SOCKET_BUF: usize = 512 * 1024;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn key32(s: &str) -> Result<[u8; 32]> {
    let v = b64().decode(s.trim()).map_err(|_| anyhow!("a key is damaged"))?;
    v.try_into().map_err(|_| anyhow!("a key is damaged"))
}

/// A new private key, as WireGuard writes it (base64).
pub fn new_private() -> String {
    b64().encode(StaticSecret::random_from_rng(rand::rngs::OsRng).to_bytes())
}

pub fn public_of(private: &str) -> Result<String> {
    Ok(b64().encode(PublicKey::from(&StaticSecret::from(key32(private)?)).as_bytes()))
}

/// What a code from the VPN box holds (made by `syphon-vpn add`).
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct VpnCode {
    pub v: u32,
    pub name: String,
    pub endpoint: String,
    pub server_public: String,
    pub psk: String,
    /// the one-time welcome key
    pub private: String,
    pub token: String,
    /// the box's welcome desk inside the VPN, "10.77.0.1:51821"
    pub welcome: String,
    /// this device's address inside the VPN, "10.77.0.5/32"
    pub address: String,
    /// the Syphon server as seen through the VPN, "http://192.168.1.20:8096"
    pub node: String,
}

pub fn decode_code(s: &str) -> Result<VpnCode> {
    let damaged = || anyhow!("The VPN code is damaged. Copy it again, all of it.");
    let body = s.trim().strip_prefix(CODE_PREFIX).ok_or_else(|| anyhow!("That is not a VPN code. It starts with {CODE_PREFIX}"))?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body.trim().trim_end_matches('=')).map_err(|_| damaged())?;
    let c: VpnCode = serde_json::from_slice(&raw).map_err(|_| damaged())?;
    if c.v != 1 {
        bail!("This VPN code is from a newer Syphon VPN box. Update the app.");
    }
    if c.token.len() != 48 || !c.token.chars().all(|x| x.is_ascii_hexdigit()) {
        return Err(damaged());
    }
    for k in [&c.server_public, &c.psk, &c.private] {
        key32(k).map_err(|_| damaged())?;
    }
    c.welcome.parse::<SocketAddrV4>().map_err(|_| damaged())?;
    address_of(&c.address).map_err(|_| damaged())?;
    node_addr(&c.node).map_err(|_| damaged())?;
    if c.endpoint.trim().is_empty() || !c.endpoint.contains(':') {
        return Err(damaged());
    }
    Ok(c)
}

/// "10.77.0.5/32" → 10.77.0.5
pub fn address_of(a: &str) -> Result<Ipv4Addr> {
    a.split('/').next().unwrap_or("").parse().map_err(|_| anyhow!("not an address: {a}"))
}

/// "http://192.168.1.20:8096" → 192.168.1.20:8096
pub fn node_addr(u: &str) -> Result<SocketAddrV4> {
    let hp = u.trim().trim_start_matches("http://").trim_end_matches('/');
    let hp = if hp.contains(':') { hp.to_string() } else { format!("{hp}:8096") };
    hp.parse().map_err(|_| anyhow!("not an address: {u}"))
}

/// How to connect through the box once this device has its own key.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Link {
    pub endpoint: String,
    pub server_public: String,
    pub psk: String,
    /// this device's own private key (never leaves this computer)
    pub private: String,
    pub address: String,
    pub node: String,
}

impl Link {
    pub fn from_code(c: &VpnCode, private: &str) -> Link {
        Link { endpoint: c.endpoint.clone(), server_public: c.server_public.clone(), psk: c.psk.clone(), private: private.to_string(), address: c.address.clone(), node: c.node.clone() }
    }
}

// ---- the small TCP/IP stack's view of the tunnel: packets in, packets out ----

#[derive(Default)]
struct Pipe {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}
struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl<'a> TxToken for Tx<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0; len];
        let r = f(&mut b);
        self.0.push_back(b);
        r
    }
}
impl Device for Pipe {
    type RxToken<'a> = Rx where Self: 'a;
    type TxToken<'a> = Tx<'a> where Self: 'a;
    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(Rx, Tx<'_>)> {
        let p = self.rx.pop_front()?;
        Some((Rx(p), Tx(&mut self.tx)))
    }
    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }
}

// ---- the tunnel ----

enum Cmd {
    Open(SocketAddrV4, TcpStream),
}

struct Conn {
    h: SocketHandle,
    /// data from the app, waiting to go into the tunnel
    up: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
    up_done: bool,
    /// data from the tunnel, on its way to the app
    down: Option<mpsc::Sender<Vec<u8>>>,
    closed_sent: bool,
}

/// A running tunnel. Dropping it stops it.
pub struct Tunnel {
    cmds: mpsc::Sender<Cmd>,
    /// when the last handshake with the box happened (seconds since 1970; 0 = never)
    handshake: Arc<AtomicI64>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

async fn resolve(endpoint: &str) -> Result<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host(endpoint.trim()).await.with_context(|| format!("could not find {endpoint}"))?.collect();
    addrs.sort_by_key(|a| !a.is_ipv4()); // the box listens on IPv4 for certain
    addrs.into_iter().next().ok_or_else(|| anyhow!("could not find {endpoint}"))
}

impl Tunnel {
    /// Start a tunnel with `private` as this device's key. `allowed` are the addresses inside the home network this
    /// tunnel may carry (anything else coming out of it is thrown away).
    pub async fn start(endpoint: &str, server_public: &str, psk: &str, private: &str, address: Ipv4Addr, allowed: Vec<Ipv4Addr>) -> Result<Tunnel> {
        let peer = resolve(endpoint).await?;
        let sock = UdpSocket::bind(if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await?;
        let index = rand::random::<u32>() >> 8;
        let tunn = Tunn::new(StaticSecret::from(key32(private)?), PublicKey::from(key32(server_public)?), Some(key32(psk)?), Some(25), index, None).map_err(|e| anyhow!("WireGuard: {e}"))?;
        let (tx, rx) = mpsc::channel(64);
        let handshake = Arc::new(AtomicI64::new(0));
        let hs = handshake.clone();
        let endpoint = endpoint.to_string();
        let main = tokio::spawn(async move {
            if let Err(e) = run(sock, peer, endpoint, tunn, address, allowed, rx, hs).await {
                eprintln!("syphon-app: the VPN tunnel stopped: {e:#}");
            }
        });
        Ok(Tunnel { cmds: tx, handshake, tasks: std::sync::Mutex::new(vec![main]) })
    }

    /// A port on 127.0.0.1 that leads to `target` at home through the tunnel.
    pub async fn forward(&self, target: SocketAddrV4) -> Result<u16> {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let port = l.local_addr()?.port();
        let cmds = self.cmds.clone();
        let t = tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let _ = s.set_nodelay(true);
                if cmds.send(Cmd::Open(target, s)).await.is_err() {
                    break;
                }
            }
        });
        self.tasks.lock().unwrap().push(t);
        Ok(port)
    }

    /// Whether the box answered recently (WireGuard renews its handshake every two minutes while in use).
    pub fn up(&self) -> bool {
        let t = self.handshake.load(Ordering::Relaxed);
        t > 0 && unix_now() - t < 180
    }

    /// Wait up to `max` for the box to answer.
    pub async fn wait_up(&self, max: Duration) -> bool {
        let end = Instant::now() + max;
        while Instant::now() < end {
            if self.up() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.up()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(sock: UdpSocket, mut peer: SocketAddr, endpoint: String, mut tunn: Tunn, address: Ipv4Addr, allowed: Vec<Ipv4Addr>, mut cmds: mpsc::Receiver<Cmd>, handshake: Arc<AtomicI64>) -> Result<()> {
    let start = Instant::now();
    let now = || smoltcp::time::Instant::from_millis(start.elapsed().as_millis() as i64);
    let mut dev = Pipe::default();
    let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut dev, now());
    iface.update_ip_addrs(|a| {
        let _ = a.push(IpCidr::new(IpAddress::Ipv4(address), 32));
    });
    let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Addr::new(address.octets()[0], address.octets()[1], address.octets()[2], 1));
    let mut sockets = SocketSet::new(vec![]);
    let mut conns: HashMap<SocketHandle, Conn> = HashMap::new();
    let wake = Arc::new(Notify::new());
    let mut next_port: u16 = 40000;
    let mut buf = vec![0u8; 65536];
    let mut out = vec![0u8; 65536];
    let mut last_timers = Instant::now();
    let mut last_resolve = Instant::now();

    // start the handshake at once rather than on the first request
    if let TunnResult::WriteToNetwork(p) = tunn.format_handshake_initiation(&mut out, false) {
        let _ = sock.send_to(p, peer).await;
    }

    loop {
        // move data between the app's connections and the TCP stack, let the stack work, send what it made
        for _ in 0..4 {
            for c in conns.values_mut() {
                let s = sockets.get_mut::<tcp::Socket>(c.h);
                // app → tunnel
                loop {
                    if !c.pending.is_empty() {
                        if !s.can_send() {
                            break;
                        }
                        let n = s.send_slice(&c.pending).unwrap_or(0);
                        c.pending.drain(..n);
                        if n == 0 {
                            break;
                        }
                        continue;
                    }
                    if c.up_done {
                        break;
                    }
                    match c.up.try_recv() {
                        Ok(b) => c.pending = b,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            c.up_done = true;
                            break;
                        }
                    }
                }
                if c.up_done && c.pending.is_empty() && !c.closed_sent && s.may_send() {
                    s.close();
                    c.closed_sent = true;
                }
                // tunnel → app (only as fast as the app takes it)
                if let Some(d) = c.down.as_ref() {
                    while s.can_recv() {
                        let Ok(permit) = d.try_reserve() else { break };
                        let mut chunk = vec![0u8; 16384];
                        let n = s.recv_slice(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        chunk.truncate(n);
                        permit.send(chunk);
                    }
                    if d.is_closed() {
                        s.abort();
                    }
                }
                if c.down.is_some() && !s.may_recv() && !s.can_recv() && s.state() != tcp::State::SynSent && s.state() != tcp::State::SynReceived {
                    c.down = None; // the server is done sending: the app's side is told
                }
            }
            iface.poll(now(), &mut dev, &mut sockets);
            let mut sent = false;
            while let Some(p) = dev.tx.pop_front() {
                sent = true;
                match tunn.encapsulate(&p, &mut out) {
                    TunnResult::WriteToNetwork(w) => {
                        let _ = sock.send_to(w, peer).await;
                    }
                    TunnResult::Err(e) => eprintln!("syphon-app: VPN: {e:?}"),
                    _ => {}
                }
            }
            if !sent {
                break;
            }
        }
        // forget connections that are finished
        let done: Vec<SocketHandle> = conns.iter().filter(|(h, c)| {
            let s = sockets.get::<tcp::Socket>(**h);
            s.state() == tcp::State::Closed && c.down.is_none()
                || (s.state() == tcp::State::Closed && c.up_done && c.pending.is_empty())
        }).map(|(h, _)| *h).collect();
        for h in done {
            conns.remove(&h);
            sockets.remove(h);
        }

        // WireGuard's own timers: handshakes, keepalives
        if last_timers.elapsed() >= Duration::from_millis(250) {
            last_timers = Instant::now();
            if let TunnResult::WriteToNetwork(w) = tunn.update_timers(&mut out) {
                let _ = sock.send_to(w, peer).await;
            }
            if let Some(d) = tunn.time_since_last_handshake() {
                handshake.store(unix_now() - d.as_secs() as i64, Ordering::Relaxed);
            }
            // the home address may have changed (dynamic DNS): look it up again while the box is not answering
            let quiet = handshake.load(Ordering::Relaxed) == 0 || unix_now() - handshake.load(Ordering::Relaxed) > 150;
            if quiet && last_resolve.elapsed() > Duration::from_secs(30) {
                last_resolve = Instant::now();
                if let Ok(Ok(p)) = tokio::time::timeout(Duration::from_secs(5), resolve(&endpoint)).await {
                    if p != peer && p.is_ipv4() == peer.is_ipv4() {
                        peer = p;
                    }
                }
            }
        }

        let delay = iface.poll_delay(now(), &sockets).map(|d| Duration::from_micros(d.total_micros())).unwrap_or(Duration::from_millis(250)).min(Duration::from_millis(250));
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                if from != peer {
                    continue;
                }
                let mut first = true;
                loop {
                    let res = if first { tunn.decapsulate(Some(from.ip()), &buf[..n], &mut out) } else { tunn.decapsulate(None, &[], &mut out) };
                    first = false;
                    match res {
                        TunnResult::WriteToNetwork(w) => {
                            let _ = sock.send_to(w, peer).await;
                            continue; // more may be waiting (packets held during the handshake)
                        }
                        TunnResult::WriteToTunnelV4(p, src) => {
                            if allowed.contains(&src) {
                                dev.rx.push_back(p.to_vec());
                            }
                        }
                        _ => {}
                    }
                    break;
                }
                if let Some(d) = tunn.time_since_last_handshake() {
                    handshake.store(unix_now() - d.as_secs() as i64, Ordering::Relaxed);
                }
            }
            c = cmds.recv() => {
                let Some(Cmd::Open(target, stream)) = c else { return Ok(()) };
                let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; SOCKET_BUF]), tcp::SocketBuffer::new(vec![0; SOCKET_BUF]));
                s.set_nagle_enabled(false);
                s.set_timeout(Some(smoltcp::time::Duration::from_secs(60)));
                s.set_keep_alive(Some(smoltcp::time::Duration::from_secs(20)));
                next_port = if next_port >= 60000 { 40000 } else { next_port + 1 };
                if s.connect(iface.context(), (IpAddress::Ipv4(*target.ip()), target.port()), next_port).is_err() {
                    continue;
                }
                let h = sockets.add(s);
                let (up_tx, up_rx) = mpsc::channel::<Vec<u8>>(16);
                let (down_tx, mut down_rx) = mpsc::channel::<Vec<u8>>(32);
                let (mut r, mut w) = stream.into_split();
                let wk = wake.clone();
                tokio::spawn(async move {
                    let mut b = vec![0u8; 16384];
                    loop {
                        match r.read(&mut b).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if up_tx.send(b[..n].to_vec()).await.is_err() {
                                    break;
                                }
                                wk.notify_one();
                            }
                        }
                    }
                    drop(up_tx);
                    wk.notify_one();
                });
                let wk = wake.clone();
                tokio::spawn(async move {
                    while let Some(b) = down_rx.recv().await {
                        wk.notify_one(); // room again for more from the tunnel
                        if w.write_all(&b).await.is_err() {
                            break;
                        }
                    }
                    let _ = w.shutdown().await;
                });
                conns.insert(h, Conn { h, up: up_rx, pending: vec![], up_done: false, down: Some(down_tx), closed_sent: false });
            }
            _ = wake.notified() => {}
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// First contact: with the code's welcome key, hand this device's own public key to the box's welcome desk.
/// `own_private` must already be saved by the caller, so a lost answer can be recovered by just trying the own key.
pub async fn enroll(c: &VpnCode, own_private: &str, step: &(dyn Fn(&str) + Sync)) -> Result<()> {
    step(&format!("Reaching your VPN box at {}…", c.endpoint));
    let address = address_of(&c.address)?;
    let desk: SocketAddrV4 = c.welcome.parse()?;
    let node = node_addr(&c.node)?;
    let t = Tunnel::start(&c.endpoint, &c.server_public, &c.psk, &c.private, address, vec![*desk.ip(), *node.ip()]).await?;
    if !t.wait_up(Duration::from_secs(12)).await {
        bail!("Your VPN box did not answer at {}. Check that the box is on, that UDP port {} is forwarded to it on your router, and that this computer is not on your home network.", c.endpoint, c.endpoint.rsplit(':').next().unwrap_or("51820"));
    }
    step("The VPN box answered. Putting this computer's own key in place…");
    let port = t.forward(desk).await?;
    let body = serde_json::json!({ "token": c.token, "public_key": public_of(own_private)? });
    let r = reqwest::Client::builder().no_proxy().build()?.post(format!("http://127.0.0.1:{port}/enroll")).json(&body).timeout(Duration::from_secs(20)).send().await.context("The VPN box's welcome desk did not answer")?;
    let v: serde_json::Value = r.json().await.unwrap_or_default();
    if v["ok"].as_bool() != Some(true) {
        bail!("The VPN box refused the code: {}", v["error"].as_str().unwrap_or("no reason given"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> VpnCode {
        let k = new_private();
        VpnCode { v: 1, name: "Living Room".into(), endpoint: "home.example.net:51820".into(), server_public: public_of(&new_private()).unwrap(), psk: new_private(), private: k, token: "ab".repeat(24), welcome: "10.77.0.1:51821".into(), address: "10.77.0.2/32".into(), node: "http://192.168.1.20:8096".into() }
    }

    fn encode(c: &VpnCode) -> String {
        format!("{CODE_PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(c).unwrap()))
    }

    #[test]
    fn a_vpn_code_is_read_and_damage_is_refused() {
        let c = code();
        let s = encode(&c);
        assert_eq!(decode_code(&format!(" {s}\n")).unwrap(), c);
        assert!(decode_code("SYPHON-CONNECT-1:abc").is_err());
        assert!(decode_code(&s[..s.len() - 10]).is_err());
        let mut bad = c.clone();
        bad.token = "zz".into();
        assert!(decode_code(&encode(&bad)).is_err());
        let mut newer = c.clone();
        newer.v = 2;
        assert!(decode_code(&encode(&newer)).unwrap_err().to_string().contains("newer"));
        assert_eq!(node_addr(&c.node).unwrap(), "192.168.1.20:8096".parse().unwrap());
        assert_eq!(address_of(&c.address).unwrap(), Ipv4Addr::new(10, 77, 0, 2));
    }

    #[test]
    fn keys_match_what_wireguard_makes() {
        // from `wg genkey` / `wg pubkey`
        assert_eq!(public_of("eGSBWREtNthSSuJ40IXjDpZYNUaGbOUkwU6JDAPE8UY=").unwrap(), "8aOk1XWxp8jBFxMkeErEksPwwrFJoUNxJGxJE1g4T0o=");
        let p = new_private();
        assert_eq!(key32(&p).unwrap().len(), 32);
        assert_ne!(new_private(), p);
    }
}
