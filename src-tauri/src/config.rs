//! Portals and runtime configuration, persisted as JSON in the user's
//! Application Support directory.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub portals: Vec<String>,
    pub selected: Option<String>,
    #[serde(default = "default_true")]
    pub reuse_cookie: bool,
    /// Absolute path to the `gpclient` executable; `None` = auto-detect.
    #[serde(default)]
    pub gpclient_path: Option<String>,
    /// Gateways seen in connect logs or added by the user. The picker lists
    /// these; `gpclient --gateway <name>` connects to a specific one.
    #[serde(default)]
    pub gateways: Vec<String>,
    /// The user's favorite gateway, used when the picker selection is "Auto"
    /// (`selected_gateway` is null): preferred wins over --auto-gateway.
    #[serde(default)]
    pub preferred_gateway: Option<String>,
    /// The gateway explicitly picked in the panel, `None` = "Auto".
    #[serde(default)]
    pub selected_gateway: Option<String>,
    /// Reverse display map: gateway FQDN → picker name. The cached-cookie
    /// re-auth path logs no gateway *name* — gpclient skips the selection
    /// entirely and reconnects to the last gateway, so only the FQDN
    /// appears in the log — the panel translates it through this map.
    /// Seeded by hand; learned automatically whenever a selection line
    /// in "name (address)" form appears (the FQDN is then remembered).
    #[serde(default)]
    pub gateway_hosts: std::collections::BTreeMap<String, String>,
    /// `None` = auto: native liquid glass panel backdrop on macOS 26+,
    /// CSS vibrancy below. `Some(false)` forces the CSS backdrop even where
    /// the glass is available. `Some(true)` = auto (see `glass::resolve_effect`).
    #[serde(default)]
    pub liquid_glass: Option<bool>,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            portals: vec!["vpn.example.com".to_string()],
            selected: Some("vpn.example.com".to_string()),
            reuse_cookie: true,
            gpclient_path: None,
            gateways: Vec::new(),
            preferred_gateway: None,
            selected_gateway: None,
            gateway_hosts: std::collections::BTreeMap::new(),
            liquid_glass: None,
        }
    }
}

impl Config {
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .expect("the user's Application Support directory must be available")
            .join("gp-bar")
    }

    /// The file the connect flow redirects `gpclient` stdout/stderr into.
    ///
    /// It lives in the root-owned, world-readable `/Library/Logs/gp-bar`
    /// (created 0755 root:wheel by `setup-admin.sh`): the ROOT worker
    /// writes it, so it must never sit under a user-writable directory —
    /// a user symlink there would let any user process truncate system
    /// files as root (S1). The wrapper recreates the file per attempt
    /// (0644), which is the tailer's "new attempt" rewind signal; the
    /// app only ever READS it, as the user. The app's own `panel.log`
    /// stays under `~/Library/Logs/gp-bar` (written by the app as user).
    pub fn connect_log_path() -> PathBuf {
        PathBuf::from("/Library/Logs/gp-bar/gpclient-connect.log")
    }

    pub fn load() -> Config {
        let path = Self::config_dir().join("config.json");
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                eprintln!(
                    "gp-bar: ignoring invalid config {}: {error}",
                    path.display()
                );
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = Self::config_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
        let path = dir.join("config.json");
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, raw).map_err(|e| e.to_string())
    }

    /// Resolve the gpclient executable, preferring the configured path.
    pub fn resolve_gpclient(&self) -> Option<PathBuf> {
        if let Some(configured) = &self.gpclient_path {
            let path = Path::new(configured);
            if path.is_file() {
                return Some(path.to_path_buf());
            }
        }
        Self::detect_gpclient()
    }

    /// Look for gpclient in the usual Homebrew locations.
    pub fn detect_gpclient() -> Option<PathBuf> {
        for candidate in ["/opt/homebrew/bin/gpclient", "/usr/local/bin/gpclient"] {
            let path = Path::new(candidate);
            if path.is_file() {
                return Some(path.to_path_buf());
            }
        }
        None
    }
}
