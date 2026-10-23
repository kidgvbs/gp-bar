//! Replicates gpclient's authenticated portal-config request so the panel
//! can list ALL the portal's gateways BEFORE connecting.
//!
//! `gpclient` logs only a count ("Found N gateways in portal config"), so
//! the picker's list used to grow one harvest-at-a-time from the connect
//! log. The cached portal cookie (gpclient's `--cookie-cache`, written by
//! the root worker and exposed read-only through `gp-admin show-cookie`)
//! is enough to re-issue the same `POST /global-protect/getconfig.esp`
//! request the official client makes, and parse the gateway list from the
//! XML response — no full handshake, no MFA, while the cookie is valid.
//!
//! Reference: `reference/GlobalProtect-openconnect/crates/gpapi/src`
//! (`params/portal_getconfig.rs`, `params/credential.rs`,
//! `portal/config.rs`, `gateway/parse_gateways.rs`, `cookie_store.rs`).

use serde::Deserialize;
use tokio::process::Command;
use xmltree::Element;

/// The client version the official macOS client reports (gpapi
/// `GP_CLIENT_VERSION_MACOS`); the user agent is derived from it.
const CLIENT_VERSION: &str = "6.3.3-915";

const COOKIE_VERSION: u32 = 1;

// -- cached cookie ---------------------------------------------------------------

/// `gpapi::cookie_store::StoredCookie` (only what the request needs).
#[derive(Debug, Deserialize)]
struct StoredCookie {
    version: u32,
    server: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    last_gateway: Option<String>,
    auth_cookie: AuthCookieCredential,
}

/// `gpapi::credential::AuthCookieCredential` — serde camelCase.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthCookieCredential {
    #[serde(default)]
    username: String,
    user_auth_cookie: String,
    #[serde(default)]
    prelogon_user_auth_cookie: String,
}

/// The auth material a config request carries.
#[derive(Debug)]
pub struct PortalAuth {
    pub username: String,
    pub user_auth_cookie: String,
    pub prelogon_user_auth_cookie: String,
    /// The gateway the cached credential authenticated to last — a free,
    /// always-fresh seed for the picker even where the portal config request
    /// cannot work (password-auth portals reject cookie-only requests).
    pub last_gateway: Option<String>,
}

/// Parse gpclient's cookie cache and check it matches `portal`.
pub fn parse_stored_cookie(json: &str, portal: &str) -> Result<PortalAuth, String> {
    let stored: StoredCookie =
        serde_json::from_str(json).map_err(|e| format!("cookie cache is unreadable: {e}"))?;
    if stored.version != COOKIE_VERSION {
        return Err(format!(
            "cookie cache version {} is unsupported",
            stored.version
        ));
    }
    if !stored.server.trim().eq_ignore_ascii_case(portal.trim()) {
        return Err(format!(
            "the cached cookie is for another portal ({})",
            stored.server
        ));
    }
    let username = if stored.username.trim().is_empty() {
        stored.auth_cookie.username.clone()
    } else {
        stored.username
    };
    if stored.auth_cookie.user_auth_cookie.is_empty() {
        return Err("the cached cookie has no auth cookie".to_string());
    }
    Ok(PortalAuth {
        username,
        user_auth_cookie: stored.auth_cookie.user_auth_cookie,
        prelogon_user_auth_cookie: stored.auth_cookie.prelogon_user_auth_cookie,
        last_gateway: stored
            .last_gateway
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty()),
    })
}

// -- machine profile ---------------------------------------------------------------

/// The identity fields the request reports (gpapi `os_profile`).
#[derive(Debug)]
pub struct MachineProfile {
    pub computer: String,
    pub host_id: String,
    pub serial: String,
    pub os_version: String,
}

/// Best-effort native collection, empty string on any failure: the portal
/// answers with defaults for these fields, they are identity hints.
async fn run_trim(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

pub async fn collect_machine_profile() -> MachineProfile {
    let hostname = run_trim("/usr/sbin/scutil", &["--get", "HostName"]).await;
    let hostname = if hostname.is_empty() {
        run_trim("/bin/hostname", &[]).await
    } else {
        hostname
    };
    let serial = run_trim(
        "/usr/sbin/ioreg",
        &["-c", "IOPlatformExpertDevice", "-d", "2"],
    )
    .await
    .lines()
    .find_map(|line| {
        // `"IOPlatformSerialNumber" = "XXXX1234"`
        let rest = line.split('=').nth(1)?;
        rest.trim_matches('"')
            .split_whitespace()
            .next()
            .map(|s| s.to_string())
    })
    .unwrap_or_default();
    MachineProfile {
        computer: hostname.clone(),
        host_id: hostname,
        serial,
        os_version: run_trim("/usr/bin/sw_vers", &["-productVersion"]).await,
    }
}

// -- request ----------------------------------------------------------------------

fn swg_nonce() -> String {
    // gpapi `csc::swg_nonce` — a timestamp-derived number.
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    (millis % 2_147_483_647).to_string()
}

/// gpapi `utils::normalize_server` / `remove_url_scheme`, applied to the
/// configured portal value: a bare hostname.
fn server_without_scheme(portal: &str) -> &str {
    portal
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
}

/// The form body `params::portal_getconfig::build` produces for an
/// auth-cookie credential on macOS (empty SAML password, `clientos=Mac`).
pub fn build_form(
    auth: &PortalAuth,
    machine: &MachineProfile,
    server: &str,
) -> Vec<(&'static str, String)> {
    // gpapi `portal_cookie_param`: empty values travel as "empty".
    let cookie = |v: &str| {
        if v.is_empty() {
            "empty".to_string()
        } else {
            v.to_string()
        }
    };
    vec![
        ("user", auth.username.clone()),
        // macOS SAML password is empty (gpapi `saml_password`).
        ("passwd", String::new()),
        ("prelogin-cookie", String::new()),
        ("portal-userauthcookie", cookie(&auth.user_auth_cookie)),
        (
            "portal-prelogonuserauthcookie",
            cookie(&auth.prelogon_user_auth_cookie),
        ),
        ("token", String::new()),
        ("inputStr", String::new()),
        ("ok", "Login".to_string()),
        ("clientVer", "4100".to_string()),
        ("clientos", "Mac".to_string()),
        ("clientgpversion", CLIENT_VERSION.to_string()),
        ("computer", machine.computer.clone()),
        ("os-version", machine.os_version.clone()),
        ("host-id", machine.host_id.clone()),
        ("ipv6-support", "yes".to_string()),
        ("serialno", machine.serial.clone()),
        ("csc-digest", String::new()),
        ("config-digest", String::new()),
        ("csc-support", "no".to_string()),
        ("host", server.to_string()),
        ("swg-auth-token", "0".to_string()),
        ("swg-nonce", swg_nonce()),
    ]
}

/// Issue the authenticated config request and return the portal's gateway
/// addresses (the `entry name` attributes of the external list).
pub async fn fetch_gateways(portal: &str, auth: &PortalAuth) -> Result<Vec<String>, String> {
    let server = server_without_scheme(portal);
    let machine = collect_machine_profile().await;
    let form = build_form(auth, &machine, server);
    let user_agent = format!(
        "PAN GlobalProtect/{CLIENT_VERSION} (Apple Mac OS X {})",
        if machine.os_version.is_empty() {
            "13.0"
        } else {
            &machine.os_version
        }
    );
    let url = format!("https://{server}/global-protect/getconfig.esp");
    let client = reqwest::Client::builder()
        .user_agent(&user_agent)
        .build()
        .map_err(|e| format!("cannot build the HTTP client: {e}"))?;
    let res = client
        .post(&url)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("portal config request failed: {e}"))?;
    let status = res.status();
    let body = res
        .text()
        .await
        .map_err(|e| format!("portal config response unreadable: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "portal returned {status} (the cookie may have expired)"
        ));
    }
    parse_gateways_xml(&body)
}

// -- response parsing --------------------------------------------------------------

/// Recursive descendant search (gpapi `utils::xml::ElementExt::descendant`).
fn find_descendant<'a>(element: &'a Element, name: &str) -> Option<&'a Element> {
    if element.name == name {
        return Some(element);
    }
    element
        .children
        .iter()
        .filter_map(|child| child.as_element())
        .find_map(|child| find_descendant(child, name))
}

/// Parse the external gateway list out of a portal config response
/// (gpapi `gateway::parse_gateways`, `prefer_internal=false`: off-VPN the
/// client always gets the external list, which is what the picker needs).
pub fn parse_gateways_xml(xml: &str) -> Result<Vec<String>, String> {
    let root =
        Element::parse(xml.as_bytes()).map_err(|e| format!("portal config is not XML: {e}"))?;
    let gateways = find_descendant(&root, "gateways")
        .ok_or_else(|| "no gateways in the portal config".to_string())?;
    // Prefer the external list; the internal one (behind the VPN) is not
    // reachable from here by definition.
    let list = ["external", "internal"]
        .iter()
        .find_map(|kind| {
            find_descendant(gateways, kind).and_then(|node| find_descendant(node, "list"))
        })
        .ok_or_else(|| "no gateway list in the portal config".to_string())?;
    let mut out = Vec::new();
    for entry in list
        .children
        .iter()
        .filter_map(|child| child.as_element())
        .filter(|child| child.name == "entry")
    {
        // The `name` attribute is the gateway address; `find_gateway` in
        // gpclient matches it directly, and it survives the wrapper's
        // plist charset validation (unlike free-form descriptions).
        if let Some(address) = entry
            .attributes
            .get("name")
            .map(String::as_str)
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            if !out.iter().any(|g| g == address) {
                out.push(address.to_string());
            }
        }
    }
    Ok(out)
}

// -- tests --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie_json() -> String {
        serde_json::json!({
            "version": 1,
            "server": "portal.example.com",
            "username": "alice",
            "host_id": "macbook",
            "last_gateway": "gw-1.example.com",
            // Top-level fields are snake_case (StoredCookie has no
            // rename_all in gpapi); the auth cookie inside is camelCase
            // (AuthCookieCredential has rename_all = "camelCase").
            "auth_cookie": {
                "username": "alice",
                "userAuthCookie": "auth-cookie-1",
                "prelogonUserAuthCookie": "prelogon-1",
            },
            "saved_at": 1700000000,
        })
        .to_string()
    }

    #[test]
    fn parses_cached_cookie_for_matching_portal() {
        let auth = parse_stored_cookie(&cookie_json(), "portal.example.com").unwrap();
        assert_eq!(auth.username, "alice");
        assert_eq!(auth.user_auth_cookie, "auth-cookie-1");
        assert_eq!(auth.prelogon_user_auth_cookie, "prelogon-1");
    }

    #[test]
    fn rejects_cookie_for_another_portal() {
        assert!(parse_stored_cookie(&cookie_json(), "other.example.com").is_err());
    }

    #[test]
    fn rejects_unsupported_cookie_version() {
        // serde_json::json! writes compact separators, so patch via Value.
        let mut value: serde_json::Value = serde_json::from_str(&cookie_json()).unwrap();
        value["version"] = serde_json::json!(2);
        let json = value.to_string();
        assert!(parse_stored_cookie(&json, "portal.example.com").is_err());
    }

    #[test]
    fn parses_external_gateway_addresses() {
        let xml = r#"
<policy>
  <gateways>
    <external>
      <list>
        <entry name="vpn-1.example.com"><description>US_East</description></entry>
        <entry name="vpn-2.example.com"><description>US_West</description></entry>
      </list>
    </external>
  </gateways>
</policy>
"#;
        assert_eq!(
            parse_gateways_xml(xml).unwrap(),
            vec![
                "vpn-1.example.com".to_string(),
                "vpn-2.example.com".to_string()
            ]
        );
    }

    #[test]
    fn parses_gateways_nested_under_response_root() {
        // The real endpoint wraps the policy: descend by name, not depth.
        let xml = r#"
<response><status>Success</status>
  <policy><gateways><external><list>
    <entry name="vpn-1.example.com"/>
  </list></external></gateways></policy>
</response>
"#;
        assert_eq!(
            parse_gateways_xml(xml).unwrap(),
            vec!["vpn-1.example.com".to_string()]
        );
    }

    #[test]
    fn rejects_config_without_gateways() {
        assert!(parse_gateways_xml("<response><status>Success</status></response>").is_err());
    }

    #[test]
    fn form_carries_auth_and_machine_fields() {
        let auth = PortalAuth {
            username: "alice".to_string(),
            user_auth_cookie: "auth-1".to_string(),
            prelogon_user_auth_cookie: "pre-1".to_string(),
            last_gateway: None,
        };
        let machine = MachineProfile {
            computer: "macbook".to_string(),
            host_id: "macbook".to_string(),
            serial: "SERIAL1".to_string(),
            os_version: "13.6".to_string(),
        };
        let form = build_form(&auth, &machine, "portal.example.com");
        let get = |k: &str| {
            form.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("user"), "alice");
        assert_eq!(get("passwd"), "");
        assert_eq!(get("portal-userauthcookie"), "auth-1");
        assert_eq!(get("portal-prelogonuserauthcookie"), "pre-1");
        assert_eq!(get("ok"), "Login");
        assert_eq!(get("clientVer"), "4100");
        assert_eq!(get("clientos"), "Mac");
        assert_eq!(get("clientgpversion"), CLIENT_VERSION);
        assert_eq!(get("computer"), "macbook");
        assert_eq!(get("os-version"), "13.6");
        assert_eq!(get("host-id"), "macbook");
        assert_eq!(get("ipv6-support"), "yes");
        assert_eq!(get("serialno"), "SERIAL1");
        assert_eq!(get("csc-support"), "no");
        assert_eq!(get("host"), "portal.example.com");
        assert_eq!(get("swg-auth-token"), "0");
        let nonce: u64 = get("swg-nonce").parse().unwrap();
        assert!(nonce < 2_147_483_647);
    }

    #[test]
    fn empty_auth_cookies_travel_as_empty_literal() {
        let auth = PortalAuth {
            username: "alice".to_string(),
            user_auth_cookie: "auth-1".to_string(),
            prelogon_user_auth_cookie: String::new(),
            last_gateway: None,
        };
        let machine = MachineProfile {
            computer: String::new(),
            host_id: String::new(),
            serial: String::new(),
            os_version: String::new(),
        };
        let form = build_form(&auth, &machine, "portal.example.com");
        let get = |k: &str| {
            form.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("portal-prelogonuserauthcookie"), "empty");
    }

    #[test]
    fn server_scheme_and_slash_are_stripped() {
        assert_eq!(
            server_without_scheme("https://portal.example.com/"),
            "portal.example.com"
        );
        assert_eq!(
            server_without_scheme("portal.example.com"),
            "portal.example.com"
        );
    }
}
