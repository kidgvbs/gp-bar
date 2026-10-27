//! Portals and runtime configuration, persisted as JSON in the user's
//! Application Support directory.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub portals: Vec<String>,
    pub selected: Option<String>,
    #[serde(default = "default_true")]
    pub reuse_cookie: bool,
    /// Absolute path to a `gpclient` executable used ONLY to override which
    /// binary `get_versions` reports (dev setups, non-standard installs).
    /// The worker itself always runs `vpn::WORKER_GPCLIENT` via the admin
    /// wrapper; this field never changes which binary connects.
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

/// Shared charset for every name that reaches the privileged wrapper
/// (portals and gateways): ASCII letters, digits, dots, dashes and
/// underscores. A LEADING DASH is rejected on purpose: gpclient's option
/// parser would read it as an option (argument injection), and `-`
/// collides with the wrapper's "auto-gateway" sentinel argument. This is
/// the app-side twin of `valid_name` in scripts/gp-admin; every path that
/// inserts a name into the config (manual adds AND log harvesting) must
/// go through it, or the picker would offer names the wrapper rejects.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
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
        Self::load_from(&Self::config_dir())
    }

    /// A missing config (first run) is the normal default; anything else —
    /// permissions, I/O — is reported but must never panic a menu-bar app.
    /// A parse error means the file is corrupt (e.g. a crash mid-write): it
    /// is renamed aside so the next `save()` cannot silently overwrite the
    /// user's portals and gateways with the defaults.
    ///
    /// eprintln, not `log::`: `Config::load` runs in `main` before the
    /// tauri-plugin-log logger is attached, so the `log` facade would
    /// silently drop these lines — stderr is the only sink this early.
    fn load_from(dir: &Path) -> Config {
        let path = dir.join("config.json");
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                let corrupt = path.with_extension("json.corrupt");
                match fs::rename(&path, &corrupt) {
                    Ok(()) => eprintln!(
                        "gp-bar: config {} is not valid JSON ({error}); kept as {}",
                        path.display(),
                        corrupt.display()
                    ),
                    Err(rename_error) => eprintln!(
                        "gp-bar: config {} is not valid JSON ({error}); cannot keep it aside: {rename_error}",
                        path.display()
                    ),
                }
                Config::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => {
                eprintln!("gp-bar: cannot read config {}: {error}", path.display());
                Config::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::config_dir())
    }

    /// Write temp + fsync + rename: `fs::write` truncates before writing, so
    /// a crash or a full disk mid-write would leave a truncated `config.json`
    /// that the next `save()` would overwrite with the defaults. The rename
    /// is atomic on the same filesystem.
    fn save_to(&self, dir: &Path) -> Result<(), String> {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join("config.json");
        let tmp = dir.join("config.json.tmp");
        let mut file =
            fs::File::create(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        serde_json::to_writer_pretty(&mut file, self).map_err(|e| e.to_string())?;
        file.sync_all()
            .map_err(|e| format!("cannot sync {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))
    }

    /// Resolve the gpclient executable: an explicit `gpclient_path` override
    /// wins, then the binary the admin wrapper actually runs (the one that
    /// produces the connect logs GP Bar parses), then Homebrew autodetect.
    pub fn resolve_gpclient(&self) -> Option<PathBuf> {
        if let Some(configured) = &self.gpclient_path {
            let path = Path::new(configured);
            if path.is_file() {
                return Some(path.to_path_buf());
            }
        }
        let worker = Path::new(crate::vpn::WORKER_GPCLIENT);
        if worker.is_file() {
            return Some(worker.to_path_buf());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gp-bar-config-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = temp_dir("roundtrip");
        let mut cfg = Config::default();
        cfg.portals.push("vpn-2.example.com".to_string());
        cfg.gateways.push("gw-1".to_string());
        cfg.selected_gateway = Some("gw-1".to_string());
        cfg.save_to(&dir).unwrap();
        let loaded = Config::load_from(&dir);
        assert_eq!(loaded.portals, cfg.portals);
        assert_eq!(loaded.gateways, cfg.gateways);
        assert_eq!(loaded.selected_gateway, Some("gw-1".to_string()));
        // The atomic save leaves no temp file behind.
        assert!(!dir.join("config.json.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_config_is_kept_aside_and_defaults_are_served() {
        let dir = temp_dir("corrupt");
        fs::write(dir.join("config.json"), r#"{"portals": ["trunc"#).unwrap();
        let cfg = Config::load_from(&dir);
        assert_eq!(cfg, Config::default());
        // The user's data is not lost: it survives beside the fresh defaults.
        assert!(dir.join("config.json.corrupt").exists());
        assert!(!dir.join("config.json").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_config_is_the_plain_default() {
        let dir = temp_dir("missing");
        let cfg = Config::load_from(&dir);
        assert_eq!(cfg, Config::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_match_the_wrapper_charset() {
        // Same contract as valid_name in scripts/gp-admin.
        for name in ["vpn.example.com", "vpn-1", "gw_2", "a", "10.0.0.1.x"] {
            assert!(valid_name(name), "{name:?} should be valid");
        }
        for name in [
            "",
            "-",
            "--ignore-tls-errors",
            "-auto",
            "space name",
            "vpn/example",
            "https://vpn.example.com",
            "café",
        ] {
            assert!(!valid_name(name), "{name:?} should be rejected");
        }
    }
}
