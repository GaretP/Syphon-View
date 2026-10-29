//! What the app remembers on this computer: which node, how to reach it, who has signed in here, and its settings.
//! Kept in one small file in the app's data folder, readable only by the user who runs the app.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Node {
    /// the node's own name
    pub name: String,
    /// a short id the node gives when it is found on the home network
    #[serde(default)]
    pub id: String,
    /// its addresses on the home network, e.g. http://192.168.1.20:8096
    #[serde(default)]
    pub lan: Vec<String>,
    /// how to connect through the VPN box at home, once this computer has its own key there
    #[serde(default)]
    pub vpn: Option<crate::vpn::Link>,
    /// the node's network card, for waking it at home (it tells signed-in people at home)
    #[serde(default)]
    pub mac: Option<String>,
    /// the node is set to sleep (so waiting a little longer for it is worth it)
    #[serde(default)]
    pub sleeps: bool,
    /// a VPN code whose first contact has not been confirmed yet, with the key this computer made for it
    #[serde(default)]
    pub vpn_pending: Option<PendingVpn>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PendingVpn {
    pub code: crate::vpn::VpnCode,
    pub own_private: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub user: String,
    /// the node's "trusted device" cookie for this person on this computer (id.secret)
    pub device: String,
    /// this computer may sign them in without the password
    #[serde(default)]
    pub remembered: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// full screen, big-screen layout, made for a computer plugged into a TV
    #[serde(default)]
    pub tv_mode: bool,
    /// show "Who's watching?" every time the app starts (always in TV mode)
    #[serde(default)]
    pub ask_profile: bool,
    /// start the app when the computer starts
    #[serde(default)]
    pub autostart: bool,
    /// a PIN needed to leave TV mode or open the app settings from it (hashed)
    #[serde(default)]
    pub pin_hash: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { tv_mode: false, ask_profile: false, autostart: false, pin_hash: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Store {
    #[serde(default)]
    pub node: Option<Node>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub last_profile: Option<String>,
    #[serde(default)]
    pub settings: Settings,
}

pub fn file(dir: &Path) -> PathBuf {
    dir.join("syphon-app.json")
}

pub fn load(dir: &Path) -> Store {
    std::fs::read(file(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(dir: &Path, s: &Store) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("syphon-app.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, file(dir))?;
    Ok(())
}

impl Store {
    pub fn profile(&self, user: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.user == user)
    }
    pub fn put_profile(&mut self, p: Profile) {
        self.profiles.retain(|x| x.user != p.user);
        self.profiles.push(p);
        self.profiles.sort_by(|a, b| a.user.to_lowercase().cmp(&b.user.to_lowercase()));
    }
}

/// A PIN is stored as a salted hash, never as itself.
pub fn hash_pin(pin: &str, salt: &str) -> String {
    use sha2::{Digest, Sha256};
    // the PIN only keeps children in TV mode; a salted SHA-256 is enough for that
    let d = Sha256::digest(format!("syphon-app-pin|{salt}|{pin}").as_bytes());
    format!("{salt}:{}", hex::encode(d))
}

pub fn pin_ok(stored: &Option<String>, pin: &str) -> bool {
    match stored {
        None => true,
        Some(s) => s.split_once(':').map(|(salt, _)| hash_pin(pin, salt) == *s).unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_round_trips_and_a_pin_is_checked() {
        let d = std::env::temp_dir().join(format!("syphon-app-test-{}", std::process::id()));
        let mut s = Store::default();
        s.node = Some(Node { name: "Home".into(), lan: vec!["http://192.168.1.2:8096".into()], ..Default::default() });
        s.put_profile(Profile { user: "zoe".into(), device: "a.b".into(), remembered: true });
        s.put_profile(Profile { user: "Al".into(), device: "c.d".into(), remembered: false });
        s.put_profile(Profile { user: "zoe".into(), device: "e.f".into(), remembered: true });
        save(&d, &s).unwrap();
        let back = load(&d);
        assert_eq!(back.profiles.iter().map(|p| p.user.as_str()).collect::<Vec<_>>(), vec!["Al", "zoe"]);
        assert_eq!(back.profile("zoe").unwrap().device, "e.f");
        let h = Some(hash_pin("1234", "salt"));
        assert!(pin_ok(&h, "1234") && !pin_ok(&h, "4321") && pin_ok(&None, "anything"));
        let _ = std::fs::remove_dir_all(d);
    }
}
