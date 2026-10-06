use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    Http,
    Socks5,
    Socks4,
}

impl ProxyKind {
    pub fn label(self) -> &'static str {
        match self {
            ProxyKind::Http => "HTTP/HTTPS",
            ProxyKind::Socks5 => "SOCKS5",
            ProxyKind::Socks4 => "SOCKS4",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProxyEntry {
    pub id: u64,
    pub name: String,
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

impl ProxyEntry {
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Previous WinINET values, kept so we can put the machine back the way it was.
/// `None` means the value was not present at all.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedSysProxy {
    #[serde(default)]
    pub enable: Option<u32>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default, rename = "override")]
    pub bypass: Option<String>,
}

// Missing fields fall back to Default instead of failing the parse - one new
// field must not cost the user their whole proxy list.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub listen_port: u16,
    pub active_id: Option<u64>,
    /// Point Windows' system proxy at the local relay (covers Discord started outside this app).
    pub system_proxy: bool,
    /// The system proxy was turned on from the updater dialog, so the watcher
    /// winds it back down once Update.exe has been quiet for twenty seconds.
    pub sys_session: bool,
    /// Refuse any traffic that cannot be carried by the proxy (breaks Discord voice).
    pub strict_udp: bool,
    /// The close button hides the window to the tray instead of exiting.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    pub discord_path: Option<String>,
    #[serde(default)]
    pub saved_sys: Option<SavedSysProxy>,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen_port: 17999,
            active_id: None,
            system_proxy: false,
            sys_session: false,
            strict_udp: false,
            close_to_tray: true,
            discord_path: None,
            saved_sys: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub proxies: Vec<ProxyEntry>,
    #[serde(default)]
    pub settings: Settings,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            proxies: Vec::new(),
            settings: Settings::default(),
        }
    }
}

impl Store {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("store.json");
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Self::default(),
        };
        match serde_json::from_str(&text) {
            Ok(store) => store,
            Err(e) => {
                // The next save would overwrite these bytes for good - keep them.
                let _ = fs::rename(&path, dir.join("store.json.corrupt"));
                eprintln!("store.json is unreadable ({e}); kept as store.json.corrupt");
                Self::default()
            }
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        let _ = fs::create_dir_all(dir);
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Write aside, then swap: a kill mid-write must not truncate the config.
        let tmp = dir.join("store.json.tmp");
        fs::write(&tmp, text).map_err(|e| e.to_string())?;
        fs::rename(&tmp, dir.join("store.json")).map_err(|e| e.to_string())
    }

    pub fn active(&self) -> Option<&ProxyEntry> {
        let id = self.settings.active_id?;
        self.proxies.iter().find(|p| p.id == id)
    }

    pub fn upsert(&mut self, mut entry: ProxyEntry) {
        if entry.id == 0 {
            entry.id = new_id();
        }
        entry.name = entry.name.trim().to_string();
        entry.host = entry.host.trim().to_string();
        match self.proxies.iter_mut().find(|p| p.id == entry.id) {
            Some(slot) => *slot = entry,
            None => self.proxies.push(entry),
        }
        if self.settings.active_id.is_none() {
            self.settings.active_id = self.proxies.first().map(|p| p.id);
        }
    }

    pub fn remove(&mut self, id: u64) {
        self.proxies.retain(|p| p.id != id);
        if self.settings.active_id == Some(id) {
            self.settings.active_id = self.proxies.first().map(|p| p.id);
        }
    }
}

pub fn new_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    nanos ^ (std::process::id() as u64) << 32
}
