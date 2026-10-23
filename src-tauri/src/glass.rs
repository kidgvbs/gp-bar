//! Panel background: native liquid glass on macOS 26+, CSS vibrancy below.
//!
//! The popover window (`main`, undecorated + transparent in tauri.conf.json)
//! gets its backdrop in one of two ways:
//!
//! - **macOS 26+** — `window-vibrancy::apply_liquid_glass` installs a native
//!   `NSGlassEffectView` behind the webview and reparents the `WKWebView`
//!   into the glass view's content view (the crate's own layering fix for
//!   tauri-apps/window-vibrancy#198, shipped in 0.8.0/0.8.1).
//! - **older macOS** — the CSS vibrancy in `panel.css` (`backdrop-filter`
//!   blur + tint) stays untouched.
//!
//! Which mode is active is resolved once at startup from the OS version and
//! the optional `liquid_glass` config override, and reported to the frontend
//! through the `get_panel_effect` command so the webview can drop its own
//! blur/background when the glass does the job natively.

/// Which backdrop draws the panel. Resolved once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelEffect {
    /// Native `NSGlassEffectView` behind the webview (macOS 26+).
    LiquidGlass,
    /// CSS `backdrop-filter` vibrancy in `panel.css` (any macOS).
    CssVibrancy,
}

impl PanelEffect {
    /// Stable wire name for the frontend (`get_panel_effect`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiquidGlass => "liquid-glass",
            Self::CssVibrancy => "css-vibrancy",
        }
    }
}

/// Resolve the panel backdrop from the OS and the config override.
///
/// `prefer: None` (the serde default) means *auto*: liquid glass where the
/// OS supports it, CSS vibrancy elsewhere. `Some(false)` forces the CSS
/// fallback even on macOS 26+. `Some(true)` only asks for glass; on an older
/// macOS the native path is impossible, so it degrades to CSS.
pub fn resolve_effect(prefer: Option<bool>) -> PanelEffect {
    match prefer {
        Some(false) => PanelEffect::CssVibrancy,
        _ if supports_liquid_glass() => PanelEffect::LiquidGlass,
        _ => PanelEffect::CssVibrancy,
    }
}

/// True when this macOS ships the `NSGlassEffectView` API (macOS 26+).
fn supports_liquid_glass() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_version_at_least(26)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Runtime macOS version check via `NSProcessInfo`, the documented way:
/// major release number, not build strings. `macOSPrivateApi` already links
/// AppKit, so this costs nothing extra.
#[cfg(target_os = "macos")]
fn macos_version_at_least(major: isize) -> bool {
    use objc2_foundation::NSProcessInfo;

    let version = NSProcessInfo::processInfo().operatingSystemVersion();
    version.majorVersion >= major
}

/// Install the native glass behind the panel's webview. Must run on the main
/// thread (the crate enforces it); called from `setup` where we already are.
///
/// The webview pointer goes through `.content_view()` so the crate reparents
/// the `WKWebView` into the glass view's `contentView` — the tauri-specific
/// recipe from the crate's own example (without it the webview stays a
/// sibling of the glass view and the window edges break on focus, the
/// symptom of issue #198 before 0.8.0).
pub fn apply_liquid_glass(window: &tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use window_vibrancy::{apply_liquid_glass, LiquidGlassOptions, NSGlassEffectViewStyle};

        // `with_webview` hands us the platform webview on the main thread;
        // the closure must be `Send + 'static` and outlives the borrowed
        // receiver, so it takes its own clone.
        let callback_window = window.clone();
        window
            .with_webview(move |webview| {
                // The panel's own WKWebView, reparented by the crate below.
                use objc2_web_kit::WKWebView;
                let webview: &WKWebView = unsafe { &*webview.inner().cast() };

                let options = LiquidGlassOptions::new(NSGlassEffectViewStyle::Clear)
                    // Native corner radius in points — matches the 16px CSS
                    // radius of the popover design (GP Connect uses 26).
                    // The crate also clips the webview layer to the same
                    // radius, so the CSS border-radius becomes redundant.
                    .radius(16.0)
                    // Keep the glass translucent; the webview paints a light
                    // scrim over it for text contrast (see panel.css).
                    .opaque(false)
                    // Visual response to interaction — macOS 27+ only,
                    // silently ignored on 26. We run on 27, so enable it.
                    .interactive(true)
                    // The webview-aware layering: reparent instead of
                    // stacking (fixes the focus-edge breakage of #198).
                    .content_view(webview);

                if let Err(error) = apply_liquid_glass(&callback_window, options) {
                    // Unreachable in practice (gated on the OS version), but
                    // never take the panel down over a cosmetic effect.
                    eprintln!("gp-bar: liquid glass unavailable: {error}");
                }
            })
            .map_err(|error| format!("with_webview failed: {error}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        Ok(())
    }
}