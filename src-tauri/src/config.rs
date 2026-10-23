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
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            portals: vec!["gp.imm.re".to_string()],
            selected: Some("gp.imm.re".to_string()),
            reuse_cookie: true,
            gpclient_path: None,
            gateways: Vec::new(),
            preferred_gateway: None,
            selected_gateway: None,
        }
    }
}

impl Config {
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .expect("the user's Application Support directory must be available")
            .join("gp-bar")
    }

    pub fn log_dir() -> PathBuf {
        dirs::home_dir()
            .expect("the user's home directory must be available")
            .join("Library")
            .join("Logs")
            .join("gp-bar")
    }

    /// The file the connect flow redirects `gpclient` stdout/stderr into.
    /// It is created by the privileged shell with umask 022, so it stays
    /// readable by the user and can be tailed live by the panel.
    pub fn connect_log_path() -> PathBuf {
        Self::log_dir().join("gpclient-connect.log")
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
