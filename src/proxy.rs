//! The heart of the app: a small web server on this computer (127.0.0.1 only) that the app's window talks to.
//!
//! * `/__app/…` are the app's own pages and actions: finding the node, "Who's watching?", signing in, settings.
//! * Everything else is passed to the node, over the home network when it is reachable there, otherwise through the
//!   app's own Tor. The window never needs to know which.
//! * Each person who signed in on this computer has their own set of cookies, kept here (not in the window), so
//!   switching profile is instant and one person's sign-in never leaks into another's.
//! * Only the app's window can use it: it carries a secret made at every start, which other programs do not have.

use crate::discover;
use crate::store::{self, Node, Profile, Store};
use crate::vpn;
use crate::wake;
use anyhow::{anyhow, bail, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const APP_HTML: &str = include_str!("../web/app.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const LOGO: &str = include_str!("../web/logo.svg");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the app's pages can ask the window to do.
pub enum UiCmd {
    Tv(bool),
    Autostart(bool),
    Quit,
}
pub type UiFn = Arc<dyn Fn(UiCmd) + Send + Sync>;

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Home(String),
    Vpn(String),
}
impl Route {
    pub fn base(&self) -> &str {
        match self {
            Route::Home(b) | Route::Vpn(b) => b,
        }
    }
    fn kind(&self) -> &'static str {
        match self {
            Route::Home(_) => "home",
            Route::Vpn(_) => "vpn",
        }
    }
}

type Jar = BTreeMap<String, String>;

pub struct AppState {
    pub data: PathBuf,
    /// the app's bundled files (unused for now; Windows extras will live here)
    #[allow(dead_code)]
    pub resources: Option<PathBuf>,
    pub key: String,
    pub store: Mutex<Store>,
    route: tokio::sync::Mutex<Option<Route>>,
    current: Mutex<Option<Route>>,
    /// the app's own WireGuard to the VPN box, and the local address that leads to the Syphon server through it
    vpn: tokio::sync::Mutex<Option<(vpn::Tunnel, String)>>,
    vpn_client: reqwest::Client,
    home: reqwest::Client,
    jars: Mutex<HashMap<String, Jar>>,
    active: Mutex<Option<String>>,
    status: Mutex<String>,
    awake: Mutex<Option<keepawake::KeepAwake>>,
    ui: UiFn,
    /// for tests: where discovery broadcasts go
    pub discovery_port: u16,
}

pub type Shared = Arc<AppState>;

fn client(proxy: Option<String>, connect: Duration) -> reqwest::Client {
    // idle connections are closed before the node or the tunnel would close them under us
    let mut b = reqwest::Client::builder().connect_timeout(connect).redirect(reqwest::redirect::Policy::none()).pool_idle_timeout(Duration::from_secs(20));
    b = match proxy {
        Some(p) => b.proxy(reqwest::Proxy::all(p).expect("valid proxy")),
        None => b.no_proxy(),
    };
    b.build().expect("http client")
}

impl AppState {
    pub fn new(data: PathBuf, resources: Option<PathBuf>, ui: UiFn) -> Shared {
        let store = store::load(&data);
        let mut k = [0u8; 24];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut k);
        Arc::new(AppState {
            data,
            resources,
            key: hex::encode(k),
            store: Mutex::new(store),
            route: tokio::sync::Mutex::new(None),
            current: Mutex::new(None),
            vpn: tokio::sync::Mutex::new(None),
            vpn_client: client(None, Duration::from_secs(10)),
            home: client(None, Duration::from_secs(3)),
            jars: Mutex::new(HashMap::new()),
            active: Mutex::new(None),
            status: Mutex::new(String::new()),
            awake: Mutex::new(None),
            ui,
            discovery_port: discover::PORT,
        })
    }

    fn save(&self) {
        let s = self.store.lock().unwrap().clone();
        if let Err(e) = store::save(&self.data, &s) {
            eprintln!("syphon-app: could not save settings: {e}");
        }
    }

    fn say(&self, s: &str) {
        *self.status.lock().unwrap() = s.to_string();
    }

    fn client_for(&self, r: &Route) -> reqwest::Client {
        match r {
            Route::Home(_) => self.home.clone(),
            Route::Vpn(_) => self.vpn_client.clone(),
        }
    }

    /// Forget the way to the node (it went away): the next request looks again.
    pub async fn drop_route(&self) {
        *self.route.lock().await = None;
        *self.current.lock().unwrap() = None;
    }

    async fn probe(&self, c: &reqwest::Client, base: &str, max: Duration) -> Option<String> {
        let r = c.get(format!("{base}/api/status")).timeout(max).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        let v: Value = r.json().await.ok()?;
        v["name"].as_str().map(|s| s.to_string()).or(Some(String::new()))
    }

    /// Find the node: at home first (its saved addresses, then a quick search on the network in case its address
    /// changed), then through the VPN box, then through Tor (slow; only when no VPN box is set up or it can't be reached).
    pub async fn connect(&self) -> Result<Route> {
        let mut slot = self.route.lock().await;
        if let Some(r) = slot.clone() {
            return Ok(r);
        }
        let node = self.store.lock().unwrap().node.clone().ok_or_else(|| anyhow!("No Syphon has been chosen yet."))?;
        self.say(&format!("Looking for {} at home…", node.name));
        for u in &node.lan {
            if self.probe(&self.home, u, Duration::from_millis(2500)).await.is_some() {
                return Ok(self.settle(&mut slot, Route::Home(u.clone())));
            }
        }
        if !node.id.is_empty() {
            for f in discover::find(self.discovery_port, Duration::from_millis(1200), &[]).await {
                if f.id == node.id && self.probe(&self.home, &f.url, Duration::from_millis(2500)).await.is_some() {
                    if let Some(n) = self.store.lock().unwrap().node.as_mut() {
                        n.lan.retain(|x| x != &f.url);
                        n.lan.insert(0, f.url.clone());
                    }
                    self.save();
                    return Ok(self.settle(&mut slot, Route::Home(f.url)));
                }
            }
        }
        // at home and it doesn't answer: it may be asleep. Wake it and give it a minute.
        if let Some(mac) = node.mac.clone().filter(|_| wake::at_home(&node.lan)) {
            self.say(&format!("Waking {}…", node.name));
            let start = std::time::Instant::now();
            let mut sent: Option<std::time::Instant> = None;
            while start.elapsed() < Duration::from_secs(if node.sleeps { 75 } else { 30 }) {
                if sent.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(true) {
                    wake::send(&mac, &node.lan);
                    sent = Some(std::time::Instant::now());
                }
                for u in &node.lan {
                    if self.probe(&self.home, u, Duration::from_millis(1500)).await.is_some() {
                        return Ok(self.settle(&mut slot, Route::Home(u.clone())));
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        let mut vpn_err = None;
        if let Some(link) = node.vpn.clone() {
            self.say(&format!("Connecting to {} through your VPN box…", node.name));
            match self.ensure_vpn(&link).await {
                Ok(base) => {
                    // the box wakes a sleeping node as soon as a connection to it is tried; give it time to come up
                    let start = std::time::Instant::now();
                    let wait = Duration::from_secs(if node.sleeps { 75 } else { 24 });
                    let mut tries = 0;
                    while start.elapsed() < wait {
                        if self.probe(&self.vpn_client, &base, Duration::from_secs(8)).await.is_some() {
                            return Ok(self.settle(&mut slot, Route::Vpn(base)));
                        }
                        tries += 1;
                        if tries == 1 && node.sleeps {
                            self.say(&format!("Waking {} (it was asleep)…", node.name));
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    vpn_err = Some(format!("Your VPN box answered, but {} did not answer through it. It may be off, or asleep without a way to wake it (in Syphon, owner at home: Settings > Device > Sleep).", node.name));
                }
                Err(e) => vpn_err = Some(e.to_string()),
            }
        }
        self.say("");
        if let Some(e) = vpn_err {
            bail!("{e}");
        }
        bail!("{} cannot be reached on this network, and no VPN box is set up for away from home. At home, check that the node is on; away from home, paste a VPN code in App settings > Away from home.", node.name)
    }

    /// The tunnel to the VPN box, started when first needed (and again when the box stopped answering).
    async fn ensure_vpn(&self, link: &vpn::Link) -> Result<String> {
        let mut t = self.vpn.lock().await;
        if let Some((tun, base)) = t.as_ref() {
            if tun.up() {
                return Ok(base.clone());
            }
        }
        *t = None;
        let node = vpn::node_addr(&link.node)?;
        let tun = vpn::Tunnel::start(&link.endpoint, &link.server_public, &link.psk, &link.private, vpn::address_of(&link.address)?, vec![*node.ip()]).await?;
        if !tun.wait_up(Duration::from_secs(10)).await {
            bail!("Your VPN box did not answer at {}. Check that it is on and that its port is forwarded on your router.", link.endpoint);
        }
        let base = format!("http://127.0.0.1:{}", tun.forward(node).await?);
        *t = Some((tun, base.clone()));
        Ok(base)
    }

    /// First contact with the VPN box for a code: this computer's own key goes in place of the code's one-time key.
    /// The key is saved before asking, so an answer lost on the way is recovered by simply trying the key.
    async fn enroll_vpn(&self, c: &vpn::VpnCode) -> Result<vpn::Link> {
        let own = {
            let mut st = self.store.lock().unwrap();
            let n = st.node.get_or_insert_with(Node::default);
            let own = match n.vpn_pending.as_ref() {
                Some(p) if p.code.token == c.token => p.own_private.clone(),
                _ => vpn::new_private(),
            };
            n.vpn_pending = Some(store::PendingVpn { code: c.clone(), own_private: own.clone() });
            own
        };
        self.save();
        *self.vpn.lock().await = None;
        let link = vpn::Link::from_code(c, &own);
        let first = vpn::enroll(c, &own, &|s: &str| self.say(s)).await;
        if let Err(e) = first {
            // maybe the box took the key and only its answer got lost: try the key
            self.say("Checking whether the box already took this computer's key…");
            tokio::time::sleep(Duration::from_secs(2)).await;
            let worked = match self.ensure_vpn(&link).await {
                Ok(base) => self.probe(&self.vpn_client, &base, Duration::from_secs(10)).await.is_some(),
                Err(_) => false,
            };
            if !worked {
                *self.vpn.lock().await = None;
                self.say("");
                return Err(e);
            }
        } else {
            self.say("Key in place. Connecting with it…");
            tokio::time::sleep(Duration::from_secs(2)).await; // the box switches keys just after answering
        }
        self.say("");
        Ok(link)
    }

    fn settle(&self, slot: &mut Option<Route>, r: Route) -> Route {
        *slot = Some(r.clone());
        *self.current.lock().unwrap() = Some(r.clone());
        self.say("");
        r
    }

    /// Check the way through the VPN box step by step, so a problem says where it is.
    pub async fn vpn_check(&self) -> Vec<Value> {
        let mut out = vec![];
        let step = |out: &mut Vec<Value>, ok: bool, what: &str, detail: String| out.push(json!({ "ok": ok, "step": what, "detail": detail }));
        let Some(node) = self.store.lock().unwrap().node.clone() else {
            step(&mut out, false, "A Syphon is chosen", "No Syphon has been set up in this app yet.".into());
            return out;
        };
        for u in &node.lan {
            if self.probe(&self.home, u, Duration::from_millis(2000)).await.is_some() {
                step(&mut out, true, "At home", format!("{} answers directly at {u}, so the app does not use the VPN on this network. The test below still checks the box, but a home router may not let traffic out and back in to its own public address: for a real test, use another network (a phone hotspot).", node.name));
                break;
            }
        }
        let Some(link) = node.vpn.clone() else {
            step(&mut out, false, "VPN code", "This app has no VPN code yet. Paste one under App settings > Away from home (on the box: sudo syphon-vpn add \"this computer\").".into());
            return out;
        };
        step(&mut out, true, "VPN code", format!("Set up: this computer has its own key for the box at {}.", link.endpoint));
        match tokio::time::timeout(Duration::from_secs(6), tokio::net::lookup_host(link.endpoint.clone())).await {
            Ok(Ok(mut a)) => {
                let ip = a.next().map(|x| x.to_string()).unwrap_or_default();
                step(&mut out, true, "Finding your home", if link.endpoint.starts_with(ip.split(':').next().unwrap_or("-")) { format!("Using {ip}.") } else { format!("{} is {ip}.", link.endpoint) });
            }
            _ => {
                step(&mut out, false, "Finding your home", format!("{} could not be looked up. Check this computer's internet connection, and that the name is right (on the box: sudo syphon-vpn status).", link.endpoint));
                return out;
            }
        }
        // a fresh tunnel for the test, so an old one's state can't hide the answer
        self.reset_route().await;
        let Ok(target) = vpn::node_addr(&link.node) else {
            step(&mut out, false, "VPN box answers", "The saved VPN settings are damaged. Paste a new code.".into());
            return out;
        };
        let started = vpn::Tunnel::start(&link.endpoint, &link.server_public, &link.psk, &link.private, vpn::address_of(&link.address).unwrap_or(std::net::Ipv4Addr::UNSPECIFIED), vec![*target.ip()]).await;
        let tun = match started {
            Ok(t) => t,
            Err(e) => {
                step(&mut out, false, "VPN box answers", format!("Could not start the tunnel: {e}"));
                return out;
            }
        };
        if !tun.wait_up(Duration::from_secs(8)).await {
            step(&mut out, false, "VPN box answers", format!("No answer from {} within 8 seconds. Either nothing reaches the box, or the box does not know this computer. On the box, run: sudo syphon-vpn status\n- \"Packets arriving: 0\" means nothing reaches it: check the port forward to the box on your router (UDP {}), and that nothing on this computer (another VPN, a firewall) blocks outgoing UDP.\n- Packets arriving, but this computer not in \"sudo syphon-vpn list\" or still \"waiting for its app\": the code never finished; paste a new one.", link.endpoint, link.endpoint.rsplit(':').next().unwrap_or("51820")));
            return out;
        }
        step(&mut out, true, "VPN box answers", "The box answered and knows this computer's key.".into());
        let base = match tun.forward(target).await {
            Ok(p) => format!("http://127.0.0.1:{p}"),
            Err(e) => {
                step(&mut out, false, "Syphon answers through the box", format!("{e}"));
                return out;
            }
        };
        if self.probe(&self.vpn_client, &base, Duration::from_secs(10)).await.is_some() {
            step(&mut out, true, "Syphon answers through the box", format!("{} answered at {} through the box. Everything works.", node.name, link.node));
        } else {
            step(&mut out, false, "Syphon answers through the box", format!("The box answered, but {} did not answer at {} through it. Is the Syphon server on? Is that still its address (on the box: sudo syphon-vpn status, change it with sudo syphon-vpn node NEW-ADDRESS)? Can the box reach it on TCP {} (a firewall between them)?", node.name, link.node, target.port()));
        }
        out
    }

    /// Forget the way to the node and stop the tunnel (its settings changed); both start again when needed.
    async fn reset_route(&self) {
        *self.vpn.lock().await = None;
        self.drop_route().await;
    }

    fn jar_header(&self, who: &str) -> String {
        self.jars.lock().unwrap().get(who).map(|j| j.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")).unwrap_or_default()
    }

    /// Take the cookies a response sets into one person's jar (a removed cookie is dropped).
    fn take_cookies(&self, who: &str, h: &HeaderMap) {
        let mut jars = self.jars.lock().unwrap();
        let jar = jars.entry(who.to_string()).or_default();
        for v in h.get_all(header::SET_COOKIE) {
            let Ok(s) = v.to_str() else { continue };
            let mut parts = s.split(';');
            let Some((name, value)) = parts.next().and_then(|p| p.split_once('=')) else { continue };
            let (name, value) = (name.trim().to_string(), value.trim().to_string());
            let gone = value.is_empty() || parts.any(|a| a.trim().eq_ignore_ascii_case("max-age=0"));
            if gone {
                jar.remove(&name);
            } else {
                jar.insert(name.clone(), value.clone());
                if name == "syphon_dev" && !who.is_empty() {
                    let mut st = self.store.lock().unwrap();
                    if let Some(p) = st.profiles.iter_mut().find(|p| p.user == who) {
                        p.device = value;
                    }
                }
            }
        }
    }

    fn active(&self) -> String {
        self.active.lock().unwrap().clone().unwrap_or_default()
    }

    /// One call to the node's API on behalf of a person (their cookies), for the app's own actions.
    async fn call(&self, who: &str, method: Method, path: &str, body: Option<Value>) -> Result<(StatusCode, Value)> {
        let r = self.connect().await?;
        let c = self.client_for(&r);
        let mut req = c.request(method, format!("{}{}", r.base(), path)).header("x-requested-with", "syphon").header(header::COOKIE, self.jar_header(who)).timeout(Duration::from_secs(90));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = match req.send().await {
            Ok(x) => x,
            Err(e) => {
                self.drop_route().await;
                return Err(anyhow!("The connection to your Syphon dropped ({e}). Try again."));
            }
        };
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        self.take_cookies(who, resp.headers());
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        Ok((status, v))
    }
}

fn device_name() -> String {
    let host = std::env::var("COMPUTERNAME").ok().or_else(|| std::fs::read_to_string("/etc/hostname").ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match host {
        Some(h) => format!("Syphon app on {h}"),
        None => "Syphon app".into(),
    }
}

// ------------------------------------------------------------------ the gate: only the app's own window

fn has_key(st: &AppState, h: &HeaderMap) -> bool {
    h.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|c| c.split(';')).any(|c| c.trim() == format!("syphon_app={}", st.key))
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "This is the Syphon app's private helper. Open the Syphon app instead.").into_response()
}

#[derive(Deserialize)]
struct StartQ {
    k: String,
}

async fn start(State(st): State<Shared>, axum::extract::Query(q): axum::extract::Query<StartQ>) -> Response {
    if q.k != st.key {
        return forbidden();
    }
    let mut r = Redirect::to("/__app/").into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&format!("syphon_app={}; HttpOnly; SameSite=Strict; Path=/", st.key)).unwrap());
    r
}

fn page(body: &'static str, ctype: &'static str) -> Response {
    let mut r = ([(header::CONTENT_TYPE, ctype)], body).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"));
    r
}

async fn app_page(State(st): State<Shared>, h: HeaderMap, uri: Uri) -> Response {
    if !has_key(&st, &h) {
        return forbidden();
    }
    match uri.path() {
        "/__app/app.js" => page(APP_JS, "text/javascript; charset=utf-8"),
        "/__app/app.css" => page(APP_CSS, "text/css; charset=utf-8"),
        "/__app/logo.svg" => page(LOGO, "image/svg+xml"),
        _ => page(APP_HTML, "text/html; charset=utf-8"),
    }
}

// ------------------------------------------------------------------ the app's actions

type R = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;

fn oops(code: StatusCode, msg: impl ToString) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": msg.to_string() })))
}

macro_rules! guard {
    ($st:expr, $h:expr) => {
        if !has_key(&$st, &$h) {
            return Err(oops(StatusCode::FORBIDDEN, "not the app's window"));
        }
    };
}

async fn state(State(st): State<Shared>, h: HeaderMap) -> R {
    guard!(st, h);
    let s = st.store.lock().unwrap().clone();
    let route = st.current.lock().unwrap().clone();
    Ok(Json(json!({
        "node": s.node.as_ref().map(|n| json!({ "name": n.name, "lan": n.lan, "vpn": n.vpn.as_ref().map(|v| v.endpoint.clone()) })),
        "profiles": s.profiles.iter().map(|p| json!({ "user": p.user, "remembered": p.remembered })).collect::<Vec<_>>(),
        "last": s.last_profile,
        "active": st.active.lock().unwrap().clone(),
        "settings": { "tv_mode": s.settings.tv_mode, "ask_profile": s.settings.ask_profile, "autostart": s.settings.autostart, "pin_set": s.settings.pin_hash.is_some() },
        "route": route.as_ref().map(|r| r.kind()),
        "status": st.status.lock().unwrap().clone(),
        "version": VERSION,
        "os": std::env::consts::OS,
    })))
}

async fn connect(State(st): State<Shared>, h: HeaderMap) -> R {
    guard!(st, h);
    match st.connect().await {
        Ok(r) => Ok(Json(json!({ "ok": true, "via": r.kind() }))),
        Err(e) => Err(oops(StatusCode::BAD_GATEWAY, e)),
    }
}

async fn vpn_test(State(st): State<Shared>, h: HeaderMap) -> R {
    guard!(st, h);
    let steps = st.vpn_check().await;
    let ok = steps.iter().all(|s| s["ok"] == true);
    Ok(Json(json!({ "ok": ok, "steps": steps })))
}

async fn find_nodes(State(st): State<Shared>, h: HeaderMap) -> R {
    guard!(st, h);
    let found = discover::find(st.discovery_port, Duration::from_millis(1500), &[]).await;
    Ok(Json(json!({ "found": found })))
}

#[derive(Deserialize)]
struct NodeIn {
    /// a connection code from the owner (everything needed to connect from anywhere)
    code: Option<String>,
    url: Option<String>,
    name: Option<String>,
    id: Option<String>,
    /// stop using the VPN box
    #[serde(default)]
    vpn_off: bool,
}

/// "192.168.1.20", "192.168.1.20:8096" or "http://192.168.1.20:8096/" all mean the same.
pub fn normalise_url(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('/');
    if s.is_empty() || s.contains(' ') {
        return None;
    }
    let s = if s.contains("://") { s.to_string() } else { format!("http://{s}") };
    let rest = s.strip_prefix("http://").or_else(|| s.strip_prefix("https://"))?;
    let host = rest.split('/').next()?;
    let with_port = if host.rsplit_once(':').map(|(_, p)| p.parse::<u16>().is_ok()).unwrap_or(false) || host.starts_with('[') && host.ends_with("]:8096") { host.to_string() } else { format!("{host}:8096") };
    Some(format!("{}://{}", if s.starts_with("https://") { "https" } else { "http" }, with_port))
}

async fn set_node(State(st): State<Shared>, h: HeaderMap, Json(b): Json<NodeIn>) -> R {
    guard!(st, h);
    let mut node = st.store.lock().unwrap().node.clone().unwrap_or_default();
    let mut changed_node = false;
    let mut b = b;
    if b.vpn_off {
        node.vpn = None;
        node.vpn_pending = None;
    }
    if let Some(code) = b.code.as_deref().filter(|c| c.trim().starts_with(vpn::CODE_PREFIX)) {
        // a VPN code from the owner's VPN box: first contact happens now, so this needs the box to be reachable
        let c = vpn::decode_code(code).map_err(|e| oops(StatusCode::BAD_REQUEST, e))?;
        let url = normalise_url(&c.node).ok_or_else(|| oops(StatusCode::BAD_REQUEST, "The VPN code is damaged. Copy it again, all of it."))?;
        let name = if c.name.trim().is_empty() { "Syphon".to_string() } else { c.name.trim().chars().take(80).collect() };
        // the same Syphon when it has the same home address or the same name; otherwise this computer moves to the new one
        changed_node = !node.name.is_empty() && node.name != name && !node.lan.contains(&url);
        if changed_node {
            node = Node::default();
        }
        if node.name.is_empty() || node.name == "Syphon" {
            node.name = name;
        }
        if !node.lan.contains(&url) {
            node.lan.push(url);
        }
        node.vpn_pending = None;
        // hand the node (with the pending key) to the store first, so first contact can keep its key there
        let before = {
            let mut s = st.store.lock().unwrap();
            s.node.replace(node.clone())
        };
        st.drop_route().await;
        let link = match st.enroll_vpn(&c).await {
            Ok(l) => l,
            Err(e) => {
                if changed_node {
                    // a different Syphon that could not be reached: stay with the one this computer had
                    st.store.lock().unwrap().node = before;
                    st.save();
                }
                return Err(oops(StatusCode::BAD_GATEWAY, e));
            }
        };
        node = st.store.lock().unwrap().node.clone().unwrap_or(node);
        node.vpn = Some(link);
        node.vpn_pending = None;
        b.code = None;
        b.url = None;
    }
    if let Some(code) = b.code.as_deref().filter(|c| !c.trim().is_empty()) {
        let code = code.trim();
        if code.starts_with("SYPHON-CONNECT-1:") {
            return Err(oops(StatusCode::BAD_REQUEST, "That is an old Tor connection code. The app now connects from outside only through your VPN box: ask the owner for a VPN code (it starts with SYPHON-VPN-1:)."));
        }
        return Err(oops(StatusCode::BAD_REQUEST, "That is not a VPN code. It starts with SYPHON-VPN-1:"));
    }
    if let Some(u) = b.url.as_deref().filter(|u| !u.trim().is_empty()) {
        let url = normalise_url(u).ok_or_else(|| oops(StatusCode::BAD_REQUEST, "That does not look like an address. Type it like 192.168.1.20"))?;
        let name = st.probe(&st.home, &url, Duration::from_secs(4)).await.ok_or_else(|| oops(StatusCode::BAD_GATEWAY, format!("No Syphon answered at {url}. Check the address, and that this computer is on the same network.")))?;
        let name = b.name.clone().filter(|n| !n.is_empty()).unwrap_or(if name.is_empty() { "Syphon".into() } else { name });
        let id = b.id.clone().unwrap_or_default();
        changed_node = !node.name.is_empty() && (node.name != name || (!node.id.is_empty() && !id.is_empty() && node.id != id));
        if changed_node {
            node = Node::default();
        }
        node.name = name;
        if !id.is_empty() {
            node.id = id;
        }
        node.lan.retain(|x| x != &url);
        node.lan.insert(0, url);
    }
    {
        let mut s = st.store.lock().unwrap();
        if changed_node {
            s.profiles.clear();
            s.last_profile = None;
        }
        s.node = Some(node.clone());
    }
    if changed_node {
        st.jars.lock().unwrap().clear();
        *st.active.lock().unwrap() = None;
    }
    st.save();
    st.reset_route().await;
    Ok(Json(json!({ "ok": true, "name": node.name })))
}

#[derive(Deserialize)]
struct SignIn {
    user: String,
    #[serde(default)]
    password: String,
    totp: Option<String>,
    #[serde(default = "yes")]
    remember: bool,
    id: Option<String>,
    secret: Option<String>,
}
fn yes() -> bool {
    true
}

/// After any successful sign-in: keep the person as a profile on this computer.
/// At home, learn how to wake the node (its network card) and whether it sleeps at all.
async fn learn_wake(st: &AppState, user: &str) {
    if !matches!(st.current.lock().unwrap().clone(), Some(Route::Home(_))) {
        return;
    }
    if let Ok((s, v)) = st.call(user, Method::GET, "/api/wake", None).await {
        if s.is_success() {
            if let Some(n) = st.store.lock().unwrap().node.as_mut() {
                n.mac = v["mac"].as_str().filter(|m| wake::magic_packet(m).is_some()).map(|m| m.to_string()).or(n.mac.take());
                n.sleeps = v["sleeps"].as_bool().unwrap_or(false);
            }
        }
    }
}

async fn signed_in(st: &AppState, user: &str, _role: &str, remember: bool) -> Value {
    let pending = st.jars.lock().unwrap().remove("").unwrap_or_default();
    let mut jar = st.jars.lock().unwrap().remove(user).unwrap_or_default();
    jar.extend(pending);
    let device = jar.get("syphon_dev").cloned().unwrap_or_default();
    st.jars.lock().unwrap().insert(user.to_string(), jar);
    *st.active.lock().unwrap() = Some(user.to_string());
    let mut remembered = false;
    if remember && !device.is_empty() {
        remembered = matches!(st.call(user, Method::POST, "/api/devices/current/remember", Some(json!({ "remember": true }))).await, Ok((s, _)) if s.is_success());
    }
    let device = st.jars.lock().unwrap().get(user).and_then(|j| j.get("syphon_dev").cloned()).unwrap_or(device);
    {
        let mut s = st.store.lock().unwrap();
        s.put_profile(Profile { user: user.to_string(), device, remembered });
        s.last_profile = Some(user.to_string());
    }
    learn_wake(st, user).await;
    st.save();
    json!({ "ok": true, "user": user, "remembered": remembered })
}

async fn signin(State(st): State<Shared>, h: HeaderMap, Json(b): Json<SignIn>) -> R {
    guard!(st, h);
    let user = b.user.trim().to_string();
    // a person who signed in here before keeps their trusted-device cookie
    let known = st.store.lock().unwrap().profile(&user).map(|p| p.device.clone()).filter(|d| !d.is_empty());
    let mut jar = Jar::new();
    if let Some(d) = known {
        jar.insert("syphon_dev".into(), d);
    }
    st.jars.lock().unwrap().insert(String::new(), jar);
    let (status, v) = match (&b.id, &b.secret) {
        (Some(id), Some(secret)) => st.call("", Method::POST, "/api/login/poll", Some(json!({ "id": id, "secret": secret }))).await,
        _ => st.call("", Method::POST, "/api/login", Some(json!({ "user": user, "password": b.password, "totp": b.totp, "device_name": device_name() }))).await,
    }
    .map_err(|e| oops(StatusCode::BAD_GATEWAY, e))?;
    if status.is_success() && (v["user"].is_string() && (v["state"].is_null() || v["state"] == "ok")) {
        let who = v["user"].as_str().unwrap_or(&user).to_string();
        let role = v["role"].as_str().unwrap_or("viewer").to_string();
        return Ok(Json(signed_in(&st, &who, &role, b.remember).await));
    }
    if v["need"] == "totp" || v["pending"].is_object() || v["state"].is_string() {
        return Ok(Json(v));
    }
    Err(oops(status, v["error"].as_str().unwrap_or("Signing in did not work.")))
}

#[derive(Deserialize)]
struct UserIn {
    user: String,
}

/// "Who's watching?": sign in a remembered person without the password.
async fn use_profile(State(st): State<Shared>, h: HeaderMap, Json(b): Json<UserIn>) -> R {
    guard!(st, h);
    let p = st.store.lock().unwrap().profile(&b.user).cloned().ok_or_else(|| oops(StatusCode::NOT_FOUND, "nobody by that name on this computer"))?;
    // already signed in during this session?
    let has_session = st.jars.lock().unwrap().get(&p.user).map(|j| j.contains_key("syphon")).unwrap_or(false);
    if has_session {
        *st.active.lock().unwrap() = Some(p.user.clone());
        if let Ok((s, _)) = st.call(&p.user, Method::GET, "/api/me", None).await {
            if s.is_success() {
                st.store.lock().unwrap().last_profile = Some(p.user.clone());
                st.save();
                return Ok(Json(json!({ "ok": true })));
            }
        }
    }
    if !p.remembered {
        return Ok(Json(json!({ "need": "password" })));
    }
    st.jars.lock().unwrap().insert(p.user.clone(), Jar::from([("syphon_dev".to_string(), p.device.clone())]));
    let (status, v) = st.call(&p.user, Method::POST, "/api/login/remembered", Some(json!({ "user": p.user }))).await.map_err(|e| oops(StatusCode::BAD_GATEWAY, e))?;
    if status.is_success() {
        *st.active.lock().unwrap() = Some(p.user.clone());
        st.store.lock().unwrap().last_profile = Some(p.user.clone());
        learn_wake(&st, &p.user).await;
        st.save();
        return Ok(Json(json!({ "ok": true })));
    }
    if status == StatusCode::UNAUTHORIZED {
        if let Some(x) = st.store.lock().unwrap().profiles.iter_mut().find(|x| x.user == p.user) {
            x.remembered = false;
        }
        st.save();
        return Ok(Json(json!({ "need": "password", "message": v["error"] })));
    }
    Err(oops(status, v["error"].as_str().unwrap_or("Could not sign in.")))
}

/// Remove a person from this computer (and from the node's trusted devices, when it can).
async fn forget(State(st): State<Shared>, h: HeaderMap, Json(b): Json<UserIn>) -> R {
    guard!(st, h);
    let p = st.store.lock().unwrap().profile(&b.user).cloned();
    if let Some(p) = p {
        let id = p.device.split_once('.').map(|(i, _)| i.to_string());
        let signed = st.jars.lock().unwrap().get(&p.user).map(|j| j.contains_key("syphon")).unwrap_or(false);
        if !signed && p.remembered {
            st.jars.lock().unwrap().insert(p.user.clone(), Jar::from([("syphon_dev".to_string(), p.device.clone())]));
            let _ = st.call(&p.user, Method::POST, "/api/login/remembered", Some(json!({ "user": p.user }))).await;
        }
        if let Some(id) = id {
            let _ = st.call(&p.user, Method::DELETE, &format!("/api/my/devices/{id}"), None).await;
        }
        let _ = st.call(&p.user, Method::POST, "/api/logout", None).await;
    }
    st.jars.lock().unwrap().remove(&b.user);
    {
        let mut s = st.store.lock().unwrap();
        s.profiles.retain(|x| x.user != b.user);
        if s.last_profile.as_deref() == Some(&b.user) {
            s.last_profile = None;
        }
    }
    if st.active.lock().unwrap().as_deref() == Some(&b.user) {
        *st.active.lock().unwrap() = None;
    }
    st.save();
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SettingsIn {
    tv_mode: Option<bool>,
    ask_profile: Option<bool>,
    autostart: Option<bool>,
    /// a new PIN ("" removes it)
    pin: Option<String>,
    /// the current PIN, needed to change things while one is set
    pin_check: Option<String>,
}

async fn settings(State(st): State<Shared>, h: HeaderMap, Json(b): Json<SettingsIn>) -> R {
    guard!(st, h);
    let pin_hash = st.store.lock().unwrap().settings.pin_hash.clone();
    if !store::pin_ok(&pin_hash, b.pin_check.as_deref().unwrap_or("")) {
        return Err(oops(StatusCode::FORBIDDEN, "That PIN is not right."));
    }
    let mut tv = None;
    let mut auto = None;
    {
        let mut s = st.store.lock().unwrap();
        if let Some(v) = b.tv_mode {
            s.settings.tv_mode = v;
            tv = Some(v);
        }
        if let Some(v) = b.ask_profile {
            s.settings.ask_profile = v;
        }
        if let Some(v) = b.autostart {
            s.settings.autostart = v;
            auto = Some(v);
        }
        if let Some(p) = b.pin {
            let p = p.trim();
            if p.is_empty() {
                s.settings.pin_hash = None;
            } else if p.len() < 4 || p.len() > 12 || !p.chars().all(|c| c.is_ascii_digit()) {
                return Err(oops(StatusCode::BAD_REQUEST, "A PIN is 4 to 12 digits."));
            } else {
                let mut salt = [0u8; 8];
                rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
                s.settings.pin_hash = Some(store::hash_pin(p, &hex::encode(salt)));
            }
        }
    }
    st.save();
    if let Some(v) = tv {
        (st.ui)(UiCmd::Tv(v));
    }
    if let Some(v) = auto {
        (st.ui)(UiCmd::Autostart(v));
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct PinIn {
    #[serde(default)]
    pin: String,
}

async fn check_pin(State(st): State<Shared>, h: HeaderMap, Json(b): Json<PinIn>) -> R {
    guard!(st, h);
    let ok = store::pin_ok(&st.store.lock().unwrap().settings.pin_hash, &b.pin);
    if ok { Ok(Json(json!({ "ok": true }))) } else { Err(oops(StatusCode::FORBIDDEN, "That PIN is not right.")) }
}

#[derive(Deserialize)]
struct OnIn {
    on: bool,
}

/// Keep the screen on while something plays.
async fn awake(State(st): State<Shared>, h: HeaderMap, Json(b): Json<OnIn>) -> R {
    guard!(st, h);
    let mut a = st.awake.lock().unwrap();
    if b.on && a.is_none() {
        *a = keepawake::Builder::default().display(true).idle(true).reason("Playing a video").app_name("Syphon").app_reverse_domain("app.syphon.player").create().ok();
    } else if !b.on {
        *a = None;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn quit(State(st): State<Shared>, h: HeaderMap, Json(b): Json<PinIn>) -> R {
    guard!(st, h);
    if !store::pin_ok(&st.store.lock().unwrap().settings.pin_hash, &b.pin) {
        return Err(oops(StatusCode::FORBIDDEN, "That PIN is not right."));
    }
    (st.ui)(UiCmd::Quit);
    Ok(Json(json!({ "ok": true })))
}

/// Sign the current person out of the app (the node forgets the session; the profile stays).
async fn sign_out(State(st): State<Shared>, h: HeaderMap) -> R {
    guard!(st, h);
    let who = st.active();
    if !who.is_empty() {
        let _ = st.call(&who, Method::POST, "/api/logout", None).await;
        if let Some(j) = st.jars.lock().unwrap().get_mut(&who) {
            j.remove("syphon");
        }
    }
    *st.active.lock().unwrap() = None;
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------------ passing everything else to the node

const HOP: [&str; 8] = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailers", "transfer-encoding", "upgrade"];

fn passes_up(n: &HeaderName) -> bool {
    let n = n.as_str();
    !HOP.contains(&n) && !["host", "cookie", "origin", "referer", "forwarded", "x-real-ip"].contains(&n) && !n.starts_with("x-forwarded") && !n.starts_with("x-syphon-")
}

fn offline(wants_page: bool, why: &str) -> Response {
    if wants_page {
        let to = format!("/__app/?offline={}", url_escape(why));
        return Redirect::to(&to).into_response();
    }
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": why }))).into_response()
}

fn url_escape(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn forward(State(st): State<Shared>, req: Request) -> Response {
    if !has_key(&st, req.headers()) {
        return forbidden();
    }
    let wants_page = req.method() == Method::GET && req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).map(|a| a.contains("text/html")).unwrap_or(false);
    let route = match st.connect().await {
        Ok(r) => r,
        Err(e) => return offline(wants_page, &e.to_string()),
    };
    let who = st.active();
    let (parts, body) = req.into_parts();
    let pq = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    // a request without a body can be sent again: on a dropped connection it is, once, before anything is shown
    let safe = parts.method == Method::GET || parts.method == Method::HEAD;
    let build = |route: &Route| {
        let mut rb = st.client_for(route).request(method.clone(), format!("{}{}", route.base(), pq));
        for (k, v) in parts.headers.iter() {
            if passes_up(k) {
                rb = rb.header(k.as_str(), v.as_bytes());
            }
        }
        let jar = st.jar_header(&who);
        if !jar.is_empty() {
            rb = rb.header(header::COOKIE.as_str(), jar);
        }
        rb
    };
    let mut rb = build(&route);
    if !safe {
        rb = rb.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(first) => {
            // A connection that the far end closed while it sat in the pool, or a tunnel that just restarted: that is
            // not the node going away. Only when the node doesn't answer either is the way to it looked for again.
            let alive = st.probe(&st.client_for(&route), route.base(), Duration::from_secs(4)).await.is_some();
            if !alive {
                st.drop_route().await;
            }
            if !safe {
                return offline(wants_page, &format!("The connection to your Syphon dropped while sending this. Try again. ({first})"));
            }
            let route = if alive { route } else {
                match st.connect().await {
                    Ok(r) => r,
                    Err(e) => return offline(wants_page, &e.to_string()),
                }
            };
            match build(&route).send().await {
                Ok(r) => r,
                Err(e) => {
                    st.drop_route().await;
                    return offline(wants_page, &format!("Lost the connection to your Syphon ({e})."));
                }
            }
        }
    };
    st.take_cookies(&who, resp.headers());
    let mut out = Response::builder().status(resp.status().as_u16());
    for (k, v) in resp.headers() {
        let n = k.as_str();
        if !HOP.contains(&n) && n != "set-cookie" {
            out = out.header(n, v.as_bytes());
        }
    }
    let stream = resp.bytes_stream().map(|c| c.map_err(std::io::Error::other));
    out.body(Body::from_stream(stream)).unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

pub fn router(st: Shared) -> Router {
    Router::new()
        .route("/__app/start", get(start))
        .route("/__app/api/state", get(state))
        .route("/__app/api/connect", post(connect))
        .route("/__app/api/find", post(find_nodes))
        .route("/__app/api/vpn/test", post(vpn_test))
        .route("/__app/api/node", post(set_node))
        .route("/__app/api/signin", post(signin))
        .route("/__app/api/use", post(use_profile))
        .route("/__app/api/forget", post(forget))
        .route("/__app/api/settings", post(settings))
        .route("/__app/api/pin", post(check_pin))
        .route("/__app/api/awake", post(awake))
        .route("/__app/api/quit", post(quit))
        .route("/__app/api/signout", post(sign_out))
        .route("/__app", get(app_page))
        .route("/__app/", get(app_page))
        .route("/__app/*rest", get(app_page))
        .fallback(forward)
        .with_state(st)
}

/// Start the helper on a free port on this computer only. Returns the port.
pub async fn serve(st: Shared) -> Result<u16> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = l.local_addr()?.port();
    let app = router(st.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(l, app).await {
            eprintln!("syphon-app: the helper stopped: {e}");
        }
    });
    // while away from home, look every few minutes whether the node can be reached at home again (the direct way)
    let s2 = st.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(120)).await;
            let away = matches!(s2.current.lock().unwrap().clone(), Some(Route::Vpn(_)));
            if !away {
                continue;
            }
            let lan = s2.store.lock().unwrap().node.as_ref().map(|n| n.lan.clone()).unwrap_or_default();
            for u in lan {
                if s2.probe(&s2.home, &u, Duration::from_millis(2000)).await.is_some() {
                    s2.drop_route().await;
                    break;
                }
            }
        }
    });
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_understood_however_they_are_typed() {
        assert_eq!(normalise_url("192.168.1.20").unwrap(), "http://192.168.1.20:8096");
        assert_eq!(normalise_url("192.168.1.20:9000").unwrap(), "http://192.168.1.20:9000");
        assert_eq!(normalise_url("http://192.168.1.20:8096/").unwrap(), "http://192.168.1.20:8096");
        assert_eq!(normalise_url("http://media.lan/library").unwrap(), "http://media.lan:8096");
        assert!(normalise_url("").is_none());
    }

    #[test]
    fn the_node_never_sees_the_visitor_style_headers() {
        for n in ["host", "cookie", "origin", "referer", "x-forwarded-for", "forwarded", "x-real-ip", "x-syphon-zone", "connection"] {
            assert!(!passes_up(&HeaderName::from_bytes(n.as_bytes()).unwrap()), "{n}");
        }
        assert!(passes_up(&HeaderName::from_static("range")) && passes_up(&HeaderName::from_static("x-requested-with")));
    }
}
