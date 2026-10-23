# Liquid Glass panel backdrop — prototype

Branch `feature-liquid-glass`. Goal: replace the popover's CSS vibrancy
(`-webkit-backdrop-filter` blur + rgba tint in `src/panel.css`) with the
**native** macOS 26+ liquid glass (`NSGlassEffectView`) behind the webview,
with the CSS path kept intact as a fallback.

## Crate and API used

- **`window-vibrancy` 0.8.1** (crates.io, published 2026-09-22 — latest).
  Imports: `apply_liquid_glass`, `LiquidGlassOptions`, `NSGlassEffectViewStyle`.
- Advanced webview usage (from the crate README + official Tauri example):
  `window.with_webview(|webview| …)` grabs the panel's own `WKWebView`
  (`objc2-web-kit` 0.3.2, feature `WKWebView`) and hands it to
  `LiquidGlassOptions::content_view(&WKWebView)`, so the crate **reparents**
  the webview into the glass view's `contentView` instead of stacking it as
  a sibling.
- `objc2-foundation` 0.3 (`NSProcessInfo` feature) for the runtime
  OS-version check.

## Issue #198 — "apply_liquid_glass breaks window edges"

- [tauri-apps/window-vibrancy#198](https://github.com/tauri-apps/window-vibrancy/issues/198)
  (opened against 0.7.1): on focus, corner edges no longer aligned with the
  window edge — the glass view and the webview were siblings with different
  corner rounding.
- **Fixed by PR #199** (shiped in **0.8.0**): the `apply_liquid_glass` API
  was redesigned into the `LiquidGlassOptions` builder with `content_view`
  (reparenting, exactly what we use) and `opaque`. The changelog credits
  #199 with "Changed the apply_liquid_glass API … Added content_view and
  opaque settings."
- 0.8.1 additionally pairs `cornerRadius` with `clipsToBounds` on the glass
  view and clips the moved webview's layer to the same radius
  (`setCornerRadius:` / `setWantsLayer:` on the CALayer — matching the
  native approach seen in GP Connect 3.2.0).
- **Conclusion: #198 closed.** Using 0.8.1 with `.content_view()` is the
  documented fix. No workaround needed.

## Issue #229 — macOS 27 packaged-crash warning (OPEN, watch it)

- [tauri-apps/window-vibrancy#229](https://github.com/tauri-apps/window-vibrancy/issues/229)
  (opened 2026-09-21, still **open**, labels `platform: macOS`, `type: bug`):
  on **macOS 27**, a *packaged* Tauri app (custom-protocol on) with an
  `NSGlassEffectView` behind the WK host can hit two AppKit SIGABRTs that
  `tauri dev` does not reproduce:
  1. **Crash 1** — native edge drag (`_resizeChangedForDirectTouch` →
     `objc_initWeak`). **Not applicable to GP Bar**: our popover is
     undecorated and non-resizable, there is no native resize gesture.
  2. **Crash 2** — first paint via `WKURLSchemeHandler`
     (`_realMakeFirstResponder` → `objc_storeWeak`), i.e. exactly our
     packaged load path (`npm run tauri build` + `custom-protocol`).
     **Potentially applicable**: the reporter ships packaged builds
     *without* `NSGlassEffectView` for this reason; the 0.8
     `content_view()` reparent is not a fix for either crash.
- Consequence: the packaged bundle's **first open is a mandatory test**
  (checklist below). `tauri dev` staying healthy proves nothing here.
- If it reproduces: set `"liquid_glass": false` in the config for the
  CSS fallback (no recompile needed) and keep the glass off packaged
  builds until #229 is understood upstream.
- Also worth watching, non-blocking for us (window non-resizable):
  upstream reports of square corners on undecorated windows with a
  custom `cornerRadius` — mitigated in 0.8.1 by the paired
  `clipsToBounds` layer clipping we rely on for the radius.

## Version detection (runtime)

`src-tauri/src/glass.rs::macos_version_at_least` reads
`NSProcessInfo.processInfo().operatingSystemVersion()` and checks
`majorVersion >= 26`. The check is **runtime**, not compile-time, so the same
binary degrades correctly on older macOS. (The crate itself additionally
gates on `NSAppKitVersionNumber >= 2685.0`.)

## Resolution logic

`glass::resolve_effect(config.liquid_glass)` runs once at startup
(`Config::load()` → `AppContext.panel_effect`):

| config `liquid_glass` | macOS 26+ | older macOS |
|---|---|---|
| `null` / absent (default, auto) | **liquid glass** | CSS vibrancy |
| `true` | **liquid glass** | CSS vibrancy (impossible, degrades) |
| `false` | CSS vibrancy (forced) | CSS vibrancy |

`Some(true)` is equivalent to auto (there is nothing to force above "auto"),
documented as such in `Config`. The effect is applied once in `setup`, on
the main thread (the crate requires it), before the first `show`.

## Options chosen — and why

- **Style: `NSGlassEffectViewStyle::Clear`** — the fully translucent,
  refracting glass (Apple's "clear" tool-over-material style). `Regular`
  is the more opaque "frosted panel" style. GP Connect 3.2.0 (our
  closed-source reference) also uses the Clear-style `NSGlassEffectView`.
  `Clear` needs a contrast scrim over busy backgrounds — see below.
- **Radius: `16.0`** points, matching the popover's existing 16px CSS
  radius (GP Connect uses 26 for a much larger window; 16 keeps the panel
  footprint unchanged). The CSS `border-radius` on `body` is kept as a
  harmless default for the CSS path, but under the glass it is redundant:
  the crate clips the webview's CALayer to the native radius.
- **Scrim / contrast**: with Clear glass the webview must remain
  transparent (`transparent: true` already set) but the previous 0.68-alpha
  tint would hide the glass. The frontend drops the CSS blur via
  `body.liquid-glass` and switches to a **light tint scrim**:
  `rgba(246,246,248,0.35)` light / `rgba(38,38,42,0.45)` dark. Native glass
  already blurs + saturates the backdrop far better than the CSS filter, so
  the scrim only needs to lift the text contrast. Alternatives in the test
  checklist: no scrim (fully clear), `Regular` style instead of Clear,
  or `opaque(true)` (NSBox window-background behind the glass).
- **`interactive(true)`** — enables the glass's visual response to pointer
  interaction (`effectIsInteractive`). macOS 27+ only; silently ignored on
  26. We run macOS 27, so it is enabled.
- **`opaque(false)`** — keep the glass translucent; the scrim handles
  contrast. `opaque(true)` inserts an `NSBox` with the window background
  color behind the glass (kills the see-through look).

## Files

- `src-tauri/src/glass.rs` — new: effect resolution, version detection,
  `apply_liquid_glass` via `with_webview`/`content_view`.
- `src-tauri/src/main.rs` — `mod glass`, `AppContext.panel_effect`,
  `get_panel_effect` command, application in `setup`.
- `src-tauri/src/config.rs` — optional `liquid_glass` field (`serde(default)`),
  saved through the existing `cfg.save()`.
- `src-tauri/Cargo.toml` — `window-vibrancy = "0.8.1"`,
  macOS-only `objc2-web-kit`, `objc2-foundation`.
- `src/panel.css` — `body.liquid-glass` variant (no blur, scrim tint).
- `src/main.ts` — `invoke("get_panel_effect")` → toggles `body.liquid-glass`.

Config example (JSON, `~/Library/Application Support/gp-bar/config.json`):

```json
{ "liquid_glass": false }
```

To force the old CSS vibrancy on macOS 26+. Remove the key (or set `true`)
for native glass.

## Limitations

- Applied once at startup only; toggling the config requires a relaunch
  (no runtime `clear_liquid_glass`/re-apply path — the crate supports it,
  but no UI exists for it yet, by design of this prototype).
- `interactive` needs macOS 27; on 26 it is silently ignored (fine).
- The scrim values were chosen by semantics, not by eye: the visual test
  checklist below is where they get validated — and possibly tuned.
- Only the panel window gets the glass; there is no other window.

## Visual test checklist (user)

Build the bundle (see the final report) and run it. On macOS 27 with
`liquid_glass` unset/`true`:

- [ ] **Packaged first open (issue #229, crash 2)**: launch the built
  `.app` (not `tauri dev`) and open the panel — the process must NOT
  SIGABRT on first paint (`Console.app` → Crash Reports, `Namespace OBJC`).
  `tauri dev` healthy proves nothing: the crash is custom-protocol only.

- [ ] **Blur**: panel over a busy desktop (photo, browser with text) —
  background visibly blurred/refracted behind the panel; not a flat gray.
- [ ] **Light background**: open the panel over a bright/white area —
  is the text readable? If not: raise the scrim alpha in
  `body.liquid-glass` (`0.35`) toward 0.5–0.68, or try variants below.
- [ ] **Dark background**: same over a dark area, in **dark mode**
  (scrim 0.45) and with a dark wallpaper while in light mode.
- [ ] **Corners**: 16pt rounded corners on the glass AND the webview
  content — no square-corner bleed at the panel's edges.
- [ ] **Focus/blur**: click the panel, click away, focus the panel again
  several times — the window edges must stay aligned (issue #198's
  symptom). Also drag/resize isn't applicable (fixed size).
- [ ] **Interactivity**: hover/press inside the panel — the glass should
  visibly react (`interactive(true)`, e.g. a soft bend on press near edges).
- [ ] **Light/dark mode switch**: toggle system appearance with the panel
  open and closed — scrim and text contrast follow.
- [ ] **Height change**: collapse/expand the activity log (window resizes)
  — glass and radius still correct after the resize.
- [ ] **VPN states**: connect/disconnect with the panel pinned open —
  status colors stay readable over the glass.

Variants to compare (one at a time, `src-tauri/src/glass.rs` +
`src/panel.css`):

1. Scrim alpha: current (0.35/0.45) vs none (`background: transparent`) vs
   old full tint (0.68).
2. `NSGlassEffectViewStyle::Regular` instead of `Clear` (more frosted).
3. `opaque(true)` (window-background NSBox behind the glass).
4. Radius 26 (GP Connect's value) instead of 16.

## Recommendation

Adopt **conditionally, pending the packaged first-open test**. The native
path itself is sound (upstream #198 closed by the #199 reparent we use,
webview reparenting is the crate's documented recipe) and the config
override gives an instant rollback (`"liquid_glass": false`, no rebuild).
But upstream **#229 is open** and describes a first-paint SIGABRT in
packaged custom-protocol Tauri apps on macOS 27 — our exact build/run
combination — that `tauri dev` cannot catch. Until that first open of the
bundle is verified clean, treat the glass as a prototype: run the
checklist, and if the crash reproduces, keep `"liquid_glass": false`
(config-level default-off is a one-line change in `resolve_effect`).