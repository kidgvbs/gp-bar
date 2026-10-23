# NSPanel design — replacing the borderless window with a native panel

Branch `feature-nspanel`. Reference behavior: GP Connect 3.2.0 (closed-source),
reverse-engineered evidence, replicated here with the **public** crate
[`tauri-nspanel`](https://github.com/ahkohd/tauri-nspanel), branch `v2.1`.

## 1. Why

Today the popover is a tauri borderless window (`tauri.conf.json`: `transparent`,
`decorations: false`, `alwaysOnTop`, `skipTaskbar`, `visible: false`, 360×540) shown /
hidden on tray click, with dismissal reimplemented in userspace:

- `on_window_event` → `WindowEvent::Focused(false)` → `window.hide()`,
- a `panel_hidden_at` timestamp guard in `toggle_panel` against the double-hide race
  (the same tray click that opens the panel also takes key away from it),
- `panel_pinned` to keep the panel on screen while an operation runs.

GP Connect's popover is instead a **real `NSPanel`** (crate `tauri-nspanel`) with the
native popover semantics AppKit gives for free:

| Evidence (GP Connect 3.2.0)                          | Native API                                   |
|------------------------------------------------------|----------------------------------------------|
| `setHidesOnDeactivate: true`                         | `NSPanel.setHidesOnDeactivate`               |
| `setBecomesKeyOnlyIfNeeded: true`                    | `NSPanel.setBecomesKeyOnlyIfNeeded`          |
| `setWorksWhenModal: true`                            | `NSPanel.setWorksWhenModal`                  |
| `setFloatingPanel: true`                             | `NSPanel.setFloatingPanel`                   |
| style mask `NSWindowStyleMaskNonactivatingPanel`     | `NSWindow.styleMask`                         |
| click-out dismissal via global event monitor         | `NSEvent.addGlobalMonitorForEventsMatchingMask` |
| re-anchored at every open, cursor/click position, monitor under the cursor, clamped to `visibleFrame` | AppKit coordinates |

`tauri-nspanel` exposes everything except the global event monitor; for that we
call `objc2-app-kit` directly (the exact bindings `tauri-nspanel` itself is built on:
`objc2 0.6`, `objc2-app-kit 0.3`, `objc2-foundation 0.3`).

## 2. Integration points

### `src-tauri/Cargo.toml`

```toml
tauri-nspanel = { git = "https://github.com/ahkohd/tauri-nspanel", branch = "v2.1" }
# For the click-out global monitor (not exposed by tauri-nspanel):
objc2-app-kit = { version = "0.3.1", features = ["NSEvent", "block2"] }
block2 = "0.6"
```

The `block2` feature of `objc2-app-kit` is what generates
`NSEvent::addGlobalMonitorForEventsMatchingMask_handler`. Feature unification makes
the rest of `objc2-app-kit` (already a `tauri-nspanel` dependency) agree on 0.3.1.

### Builder (`main.rs` → `main()`)

```rust
tauri::Builder::default()
    .plugin(tauri_nspanel::init())   // manages the panel store
    ...
```

### `tauri.conf.json`

**Unchanged.** The `main` window keeps its config (`macOSPrivateApi`, transparent,
borderless, `visible: false`); the crate requires the `macos-private-api` tauri
feature which we already enable.

### Conversion (setup, main thread)

```rust
use tauri_nspanel::{tauri_panel, WebviewWindowExt};

tauri_panel! {
    panel!(PopoverPanel {
        config: {
            can_become_key_window: true,
            can_become_main_window: false,
            becomes_key_only_if_needed: true,
            is_floating_panel: true,
            works_when_modal: true,
            hides_on_deactivate: true
        }
    })
}

let window = app.get_webview_window("main")?;
let panel = window.to_panel::<PopoverPanel>()?;   // NSWindow → NSPanel subclass
panel.add_style_mask(NSWindowStyleMask::NonactivatingPanel)?;
```

The macro subclasses `NSPanel` and overrides the boolean getters
(`canBecomeKeyWindow`, `becomesKeyOnlyIfNeeded`, …); `to_panel` swaps the class of
the existing tauri-managed window, so all tauri APIs (`get_webview_window("main")`,
`set_position`, `scale_factor`, …) keep working on the very same native object.
The panel store (`app.get_webview_panel("main")`) gives `Panel`-trait access
(`show`, `hide`, `is_visible`, `make_key_window`, …).

`add_style_mask` (not `set_style_mask`) is used on purpose: **adding** the
`NonactivatingPanel` bit keeps tauri's structural window styles intact. Replacing
the whole mask is the known footgun — see issue ahkohd/tauri-nspanel#123 ("In macOS
27 the panel focuses like a window"): the reporter replaced the entire style mask,
the maintainer could not reproduce any misbehavior on macOS 27 with 2.1.0 as
documented. We follow the maintainer-tested path.

### `toggle_panel` (kept, adjusted)

The click-anchored placement logic is **kept as is** (anchor = click position,
rect cross-check ±2px, `monitor_from_point`, `work_area()` clamp, DPI
re-projection). Only the show/hide primitives change:

- `panel.is_visible()` / `panel.hide()` / `panel.show()` instead of the window's,
- **no `window.set_focus()` on open.** With `becomesKeyOnlyIfNeeded` +
  `NonactivatingPanel` the panel takes key only when a text field needs it —
  without activating GP Bar. This is the "focus is never stolen" behavior of the
  evidence app.

### `on_window_event` (removed) and `panel_hidden_at` (removed)

The `Focused(false)` handler and the `panel_hidden_at` race guard are **deleted**:

- the race they defended against was self-inflicted: `toggle_panel` called
  `set_focus()`, so the *opening* tray click created a focus transition, and the
  *closing* tray click produced a `Focused(false)` milliseconds before
  `toggle_panel` ran, requiring the 250 ms timestamp guard. With a
  non-activating panel and no `set_focus()`, opening the panel causes **no focus
  change at all**, so the race cannot occur;
- dismissal is now native: `hidesOnDeactivate` covers app-switch / Space change
  whenever the app is active, and the global event monitor (below) covers
  click-outside.

`panel_pinned` **stays**: it gates the new dismissal paths the same way it gated
`Focused(false)`, so a connect/disconnect in flight can still keep the panel open
(see §4 for the one native path it cannot gate).

### Click-outside: global event monitor (new, `panel.rs`)

```rust
let mask = NSEventMask::LEFT_MOUSE_DOWN.union(NSEventMask::LEFT_MOUSE_DRAGGED);
let block = Block::new(move |event: NonNull<NSEvent>| { ... dismiss ... });
NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, block.dyn_ref());
```

- The monitor only fires for events **not** delivered to our own app, so clicks on
  the tray icon or inside the panel never dismiss it (exactly the evidence
  semantics of `addGlobalMonitorForEventsMatchingMask`).
- The handler compares `event.locationInWindow()` (screen coordinates,
  bottom-left origin for global monitors) against `panel.as_panel().frame()` with
  a ±2 pt tolerance, and hides only when the click lands outside the panel.
- It is skipped while `panel_pinned` is set, mirroring the old handler.
- The block and the returned monitor token are intentionally leaked: both live
  for the whole process lifetime (app-lifetime objects on the main thread).
- `Block::new` requires a `Send` closure; `AppHandle` is `Send + Sync`, so the
  closure just captures it.

## 3. The double-hide conflict, resolved

The feared conflict — native `hidesOnDeactivate` + our focus-loss handler both
hiding the panel, racing with the tray toggle — is dissolved by construction:

1. We do not run both: our `Focused(false)` handler is gone. `hide()` is
   idempotent anyway, so even a native hide landing between the toggle's
   `is_visible()` check and its `hide()` (or `show()`) just produces the same
   end state.
2. The native hide can no longer "steal" the panel between a tray click and
   `toggle_panel`: a tray click does not deactivate the app, so
   `hidesOnDeactivate` never fires as a side effect of the toggle itself. The
   global monitor explicitly ignores our own app's events, so it cannot fire for
   the tray click either.
3. The one interaction `hidesOnDeactivate` changes vs today: if the app happens
   to be active while `panel_pinned` is set (rare — with a non-activating panel
   GP Bar is active only transiently), AppKit will hide the panel on
   deactivation regardless of the pin. Documented as accepted: the state poller
   keeps running server-side and the panel reopens on the next tray click with
   live state.

## 4. Known community issues and how the prototype copes

- **ahkohd/tauri-nspanel#27 — "NSPanel closes when interacting with native context
  menu"**: with `hidesOnDeactivate` the panel hides as soon as a native menu (an
  `NSMenu` in another context / process, e.g. a status menu or a WebKit native
  popup that takes key) deactivates the app. Our UI has **no native menus**
  (tauri tray menu is deliberately absent, quit lives in the panel footer), and
  our HTML `<select>` dropdowns render inside our own app process, whose events
  the global monitor ignores — so neither dismissal path can misfire. This issue
  is also why `panel_pinned` exists for the in-flight connect/disconnect state.
- **ahkohd/tauri-nspanel#123 — "In macOS 27 the panel focuses like a window"**:
  reporter replaced the whole style mask, losing tauri's structural styles; the
  maintainer verified 2.1.0 behaves correctly on macOS 27 (our target OS beta)
  when the mask is only *extended*. We use `add_style_mask`, and the panel-class
  config rather than raw `set_style_mask`.
- **ahkohd/tauri-nspanel#132 / #104 — input methods blocked by elevated window
  level**: fixed upstream; we do not raise the level above `floating`.
- **Space change dismissal**: the evidence app also dismisses on Space change.
  `hidesOnDeactivate` covers it whenever the app is active (Space switch
  deactivates the app). If the app was never active (the common case with a
  non-activating panel), the panel is a normal-space window: switching Space
  removes it from view and it is still there when the user comes back — the same
  behavior the current borderless-window build has. Not additionally
  instrumented (no `NSWorkspace` observer); listed as a visual-test item.

## 5. Result

Feasible with the public crate; nothing essential is missing. The only hand-written
native code is the global click-out monitor (~40 lines of `objc2`), which
`tauri-nspanel` does not expose. Everything else (subclassing, flags, level) is
crate API.

## 7. Retry (branch `feature-nspanel-v2`) — diagnosis strategy

The v1 test failed with "no window at all" on the mixed-DPI desk, and the root
cause was never isolated. Two of the plausible causes were bugs in v1's base
that current `main` has since fixed, so this branch **re-ports the NSPanel
semantics on top of current `main`** instead of retesting the stale prototype:

1. **Space stickiness** (fixed on main by `visibleOnAllWorkspaces: true`,
   commit ba72fa2): v1's window belonged to the Space of whichever display was
   active at launch, so `set_position` could target a display the user was not
   looking at. This config **must survive** any merge.
2. **Cross-scale positioning** (fixed on main by global logical points,
   commit a1c1d24): v1 positioned in physical pixels through tao's
   scale-factor-of-the-current-window conversion, which lands off by the DPI
   ratio on a 2x↔1x hop. `show_panel` keeps main's logical math verbatim.

Residual "no window at all" candidates and their coverage:

- **convert_to_panel silently failing at setup** — now instrumented:
  `gp-bar: NSPanel setup: ...` prints the window lookup, the conversion result,
  and the post-conversion frame. Any setup failure prints and never aborts the
  app (`eprintln!`, not `?`, in `main`).
- **The show path** — verified in the `tauri-nspanel` v2.1 source: `Panel::show`
  maps to `orderFrontRegardless`, NOT `makeKeyAndOrderFront` — so a
  non-activating panel that can never become key still gets ordered front. No
  fix needed; the instrumentation confirms `visible` + frame after every show.
- **Click-out monitor mis-hiding** — the global monitor only fires for events
  NOT delivered to our own process and is skipped while `panel_pinned` is set.

### 7.1 How to test / capture diagnostics

The `eprintln!` lines go to stderr, so they appear only when the binary is
launched from a terminal:

```sh
/Applications/.../GP\ Bar.app/Contents/MacOS/gp-bar 2>&1 | tee /tmp/gpbar-nspanel2.log
```

Expected sequence on a tray click that OPENS the panel:

```
gp-bar: NSPanel setup: converting `main` (scale 2)
gp-bar: NSPanel setup: converted ok — visible=false frame=...
gp-bar: NSPanel show: anchor physical (...) size ...
gp-bar: NSPanel show: monitor <name> scale <s> work ... -> logical (x,y)
gp-bar: NSPanel show: orderFrontRegardless sent — visible=true frame=...
```

On the mixed-DPI desk (retina MacBook 1440x900@2x below, 1x external above),
test the tray click on BOTH displays: each click should log the monitor that
held the anchor (scale 2 vs 1) and a `visible=true` frame on that display. If
"no window" reproduces, the last printed line pins the failing stage:

- nothing after `setup: converting` → conversion failed (its `Err` text follows),
- `show:` anchor but no `monitor` line → `monitor_from_point` missed,
- `orderFrontRegardless sent — visible=false` → AppKit refused to order front.

### 7.2 Conflicts resolved against current main

- `main.rs` imports: kept `LogicalPosition` (show_panel math), dropped
  `WindowEvent` (no `on_window_event` anymore) and `PhysicalPosition`
  (unused). `tauri_nspanel::ManagerExt` added.
- `AppContext`: dropped `panel_hidden_at` (the 250 ms race guard dies with the
  `Focused(false)` handler — a non-activating panel causes no focus change on
  open); kept `panel_pinned` (now checked by the native click-out monitor) and
  main's `panel_effect`.
- `main()` setup: BOTH the NSPanel conversion (first) and main's liquid-glass
  backdrop application (second) run, in that order.
- `tauri.conf.json`: unchanged — `visibleOnAllWorkspaces: true` survives.
- `pin_panel`: the unpinned `set_focus()` handback is dropped — the panel is
  non-activating by design and never took key.