# Wayland Cursor Position Security Research Report

*Compiled 2026-03-26*

---

## 1. Wayland Protocol Design: Why Global Cursor Position Is Hidden

### The Core Design Principle

Wayland was architected from the ground up with **input isolation** as a fundamental security property. Unlike X11, the Wayland protocol provides **no mechanism** for a client to query the global cursor position. Clients only receive pointer coordinates relative to their own surfaces, and only when the pointer is actually over one of their surfaces.

The official `wl_pointer` interface provides exactly two position-related events:

- **`wl_pointer::enter`** — Sent when the pointer moves over one of the client's surfaces. Includes surface-local coordinates (x, y).
- **`wl_pointer::motion`** — Sent while the pointer moves within a client's surface. Contains surface-local coordinates only.

There is no `get_position()`, no `query_pointer()`, no global coordinate event. This is intentional.

### Security/Privacy Motivations

Per the Wayland protocol specification and LWN.net's 2014 analysis of Wayland security (by Jake Edge):

> "Unlike X, the Wayland input stack doesn't allow applications to snoop on the input of other programs (preserving **confidentiality**), to generate input events that appear to come from the user (preserving **input integrity**), or to capture all the input events to the exclusion of the user's application (preserving **availability**)."

The three properties Wayland enforces:

1. **Confidentiality** — No application can observe another application's input events, including pointer position over other windows.
2. **Integrity** — No application can inject fake input events.
3. **Availability** — No application can grab all input away from other applications.

Global cursor position is classified as a **confidentiality violation** because knowing where the cursor is at all times reveals what windows the user is interacting with, where they're clicking, and by inference what they're doing. A malicious application could use this for user tracking, UI element identification, or as part of a broader attack chain.

### Protocol Specification References

- [Wayland Protocol Spec — Appendix A, `wl_pointer`](https://wayland.freedesktop.org/docs/html/apa.html)
- [The Wayland Protocol Book — Pointer Input](https://wayland-book.com/seat/pointer.html)
- [LWN.net — The Status of Wayland Security (2014)](https://lwn.net/Articles/589147/)

---

## 2. X11 vs Wayland: What Broke and Why

### How X11 Handled Cursor Position

X11 provided several mechanisms for querying and monitoring the global cursor position:

- **`XQueryPointer()`** — Any client could call this at any time to get the absolute screen coordinates of the cursor, plus which window it was over and the button/modifier state.
- **`XTest` extension** — Allowed synthetic mouse/keyboard events. Used by xdotool, screen recorders, accessibility tools, and unfortunately, keyloggers.
- **`XRecord` extension** — Allowed recording all X events, including pointer motion across all windows.
- **`XInput2`** — Raw input events available to any client.

### Security Problems This Created

X11's "every client is trusted" model created well-documented security vulnerabilities:

1. **Trivial keylogging** — Any X11 client could subscribe to raw keyboard events across the entire session using `xinput` or `XRecord`. No elevated privileges required. As documented by LinuxSecurity.com: "Keyloggers are entirely trivial on X11, because keylogger functionality is effectively built into it."

2. **Screen scraping** — Any client could capture the contents of any other window via `XGetImage()` or `XComposite`.

3. **Input injection** — `XTest` allowed any client to synthesize keyboard and mouse events that were indistinguishable from real hardware input.

4. **Global cursor tracking** — `XQueryPointer()` let any application continuously poll the cursor position, enabling user behavior tracking.

5. **Focus stealing** — Any client could forcibly take keyboard focus via `XSetInputFocus()`.

These weren't bugs — they were fundamental to X11's architecture where the X server is a shared resource and all clients are peers.

### Why Wayland Explicitly Broke This

Wayland's designers (Kristian Hogsberg et al.) made the deliberate choice to eliminate these capabilities at the protocol level:

> "Control now sits with the compositor and portal layer, not the application, and apps talk to it, not to each other." — GNOME 50 Wayland security announcement

The compositor is the sole arbiter of input routing. Clients cannot observe, intercept, or query input state that doesn't belong to their own surfaces.

### References

- [X11 Security — Preventing Global Keylogging (dec05eba)](https://dec05eba.com/2021/09/19/x11-security-preventing-global-keylogging/)
- [LinuxSecurity — Keylogging in Linux Part 2](https://linuxsecurity.com/features/complete-guide-to-keylogging-in-linux-part-2)
- [Passive and Active Attacks via X11 — Wayland-devel mailing list](https://wayland-devel.freedesktop.narkive.com/SSrj4U4S/passive-and-active-attacks-via-x11-is-wayland-any-better)
- [Exploiting X11 Unauthenticated Access — Infosec Institute](https://resources.infosecinstitute.com/topic/exploiting-x11-unauthenticated-access/)
- [Exploring the Fragmentation of Wayland — xdotool author](https://www.semicomplete.com/blog/xdotool-and-exploring-wayland-fragmentation/)

---

## 3. Existing Wayland Protocols and Cursor Position

### `wl_pointer` (Core Protocol)

The core protocol provides **surface-local coordinates only**, delivered via events:

- `enter(serial, surface, surface_x, surface_y)` — Pointer entered one of your surfaces
- `motion(time, surface_x, surface_y)` — Pointer moved within your surface
- `leave(serial, surface)` — Pointer left your surface

**Key limitation**: Coordinates are surface-relative, not global. You only get them when the pointer is over YOUR surface.

**Protocol spec**: [wayland.app/protocols/wayland#wl_pointer](https://wayland.app/protocols/wayland#wl_pointer)

### `wlr-virtual-pointer-unstable-v1`

This protocol allows clients to **emulate** a physical pointer device (create a virtual mouse). It supports:

- `motion(time, dx, dy)` — Relative motion
- `motion_absolute(time, x, y, x_extent, y_extent)` — Absolute positioning
- `button(time, button, state)` — Button events
- `axis(time, axis, value)` — Scroll events

**Does NOT expose cursor position.** This is a write-only protocol — you can inject pointer events, but cannot read the current position. It's used by VNC servers (wayvnc), virtual KVM tools, and automation utilities.

**Cosmic-comp status**: Does NOT implement this protocol. See [cosmic-comp #2094](https://github.com/pop-os/cosmic-comp/issues/2094) (open, Feb 2026) — wayvnc fails with "Virtual Pointer protocol not supported by compositor."

**Compositor support**: Implemented by wlroots-based compositors (Sway, Hyprland, etc.). NOT implemented by GNOME/Mutter or KDE/KWin.

**Protocol spec**: [wayland.app/protocols/wlr-virtual-pointer-unstable-v1](https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1)

### `ext-input-emulation-v1`

This does **not exist** as a finalized protocol. The closest equivalent is **libei** (Library for Emulated Input), which is a transport library rather than a Wayland protocol. libei works through the **XDG RemoteDesktop portal** (D-Bus) to allow sandboxed/authorized input injection.

libei provides:
- **EI** (client side) — `libei`
- **EIS** (server side) — `libeis`
- **oeffis** — D-Bus helper for portal communication

It does NOT provide cursor position queries. It's write-only (inject events).

**Reference**: [libei — Opening the Portal Doors (Peter Hutterer, 2022)](http://who-t.blogspot.com/2022/12/libei-opening-portal-doors.html)

### `zwlr-layer-shell-v1`

Layer shell lets clients create surfaces at specific z-depth layers (background, bottom, top, overlay). Key properties:

- **Layer surfaces DO receive pointer events normally.** Per the spec: "Layer surfaces receive pointer, touch, and tablet events normally."
- `set_keyboard_interactivity(exclusive)` — For overlay layer, grabs keyboard focus.
- Anchoring to edges/corners with configurable margins.
- Can cover full output with transparent overlay.

**The layer-shell trick for getting cursor position**: Create a fullscreen transparent overlay surface on each output. When mapped, if the compositor sends `wl_pointer.enter`, the surface-local coordinates tell you where the cursor is relative to that output. Combined with output geometry from `wl_output`, this gives you the global position.

**Critical caveat**: This relies on the compositor sending `wl_pointer.enter` when a new surface appears under a stationary cursor (see Section 6 for the protocol ambiguity here).

**Protocol spec**: [wayland.app/protocols/wlr-layer-shell-unstable-v1](https://wayland.app/protocols/wlr-layer-shell-unstable-v1)

### `xdg_popup` / `xdg_positioner`

XDG popups are positioned **relative to a parent surface** using `xdg_positioner`:

- `set_anchor_rect(x, y, width, height)` — Rectangle in parent surface coordinates
- `set_offset(x, y)` — Offset from anchor
- Gravity and constraint adjustment for edge cases

**No global positioning.** Popups are always relative to their parent. To show a popup "at the cursor", you must track the last-known cursor position from `wl_pointer.motion` events on the parent surface, then use those surface-local coordinates as the anchor rect origin.

**Protocol spec**: [wayland-book.com/xdg-shell-in-depth/popups.html](https://wayland-book.com/xdg-shell-in-depth/popups.html)

### Cosmic-Specific Protocols

The COSMIC desktop defines custom protocols in [cosmic-protocols](https://github.com/pop-os/cosmic-protocols). Based on the available search results, there is no COSMIC-specific protocol for cursor position queries. The cosmic-comp compositor focuses on:

- `cosmic-toplevel-info`/`cosmic-toplevel-management` — Window management
- `cosmic-workspace` — Workspace management
- `cosmic-overlap-notify` — Overlap notification
- `cosmic-screencopy` — Screen capture

No cursor-position-query protocol was found in the COSMIC protocol set.

---

## 4. GitHub Issues and PRs

### pop-os/cosmic-comp

| Issue | Status | Title | Relevance |
|-------|--------|-------|-----------|
| [#2029](https://github.com/pop-os/cosmic-comp/issues/2029) | **Open** | Input Capture & Cursor Position APIs for Input Sharing Applications | Directly requests cursor position query API. User building cross-machine input sharing (Barrier-like) tool. Explicitly asks for D-Bus API or portal for absolute cursor coordinates. |
| [#2094](https://github.com/pop-os/cosmic-comp/issues/2094) | **Open** | Missing Wayland Virtual Pointer protocol | wayvnc fails on COSMIC because virtual-pointer protocol isn't implemented. |
| [#2137](https://github.com/pop-os/cosmic-comp/issues/2137) | **Open** | Wrong position of context menu | Context menus appear at wrong position. |
| [#2128](https://github.com/pop-os/cosmic-comp/issues/2128) | **Open** | Opening context menu with menu key at wrong position | Related positioning bug. |
| [#636](https://github.com/pop-os/cosmic-comp/issues/636) | **Open** | Cursor position isn't updated after window resize | Stale cursor position after resize. |

### pop-os/libcosmic

| Issue | Status | Title | Relevance |
|-------|--------|-------|-----------|
| [#717](https://github.com/pop-os/libcosmic/issues/717) | **Open** | Regressions popup applet | Popup regressions in applets. |
| [#822](https://github.com/pop-os/libcosmic/issues/822) | **Closed** | Bad vertical positioning of new popup_dropdown when view is scrolled down | Popup positioning bugs. |
| [#258](https://github.com/pop-os/libcosmic/issues/258) | **Closed** | Menus bounded by application window | Menu positioning constraints. |

### pop-os/cosmic-applets

| Issue | Status | Title | Relevance |
|-------|--------|-------|-----------|
| [#1290](https://github.com/pop-os/cosmic-applets/issues/1290) | **Open** | COSMIC Clipboard Manager | Feature request for built-in clipboard manager applet. |
| [#308](https://github.com/pop-os/cosmic-applets/issues/308) | **Closed** | Clipboard Manager feature request | Earlier clipboard manager request. |
| [#827](https://github.com/pop-os/cosmic-applets/issues/827) | **Open** | Mouse cursor consistency problem hovering over workspace button | Cursor state issues in applets. |

### iced-rs/iced

| Issue | Status | Title | Relevance |
|-------|--------|-------|-----------|
| [#2773](https://github.com/iced-rs/iced/issues/2773) | **Open** | Get absolute mouse position in screen space | User trying to move a decoration-free window by dragging. Only has window-space coordinates; needs screen-space coordinates for the math to work. **Directly demonstrates the Wayland limitation in iced.** |

### swaywm/sway

| Issue/PR | Status | Title | Relevance |
|----------|--------|-------|-----------|
| [#8679](https://github.com/swaywm/sway/issues/8679) | **Open** | Randomly, may not send wl_pointer::Enter | Bug report: Sway sometimes fails to send pointer enter events. Demonstrates compositor unreliability with enter events. |
| [#7984](https://github.com/swaywm/sway/issues/7984) | **Closed** (fixed in 1.10) | Layer-shell wl_surface::enter events not delivered until next resize | `shotman` creates fullscreen layer-shell overlays to detect which output the cursor is on. Relied on `wl_surface::enter` which wasn't being delivered. **Directly relevant** — shows the fragility of the layer-shell approach. |
| [PR #8780](https://github.com/swaywm/sway/pull/8780) | **Open** (not merged) | New swaymsg message type: get_cursor | Proposed adding `swaymsg -t get_cursor` to return cursor position as JSON. **Sway maintainer emersion rejected the approach**, saying: "I don't think [the app] needs access to the global cursor position via the Sway IPC. [It] can open a full-screen layer-shell surface, receive the `wl_pointer.enter` event to get the position relative to the surface, and show the menu there." Another commenter agreed: "while adding this to Sway's IPC would be convenient, it isn't the proper way to get this information. It would be better to use the wlr-layer-protocol." |

### Smithay/smithay

| Issue | Status | Title | Relevance |
|-------|--------|-------|-----------|
| [#1257](https://github.com/Smithay/smithay/issues/1257) | **Open** | Smithay doesn't send leave events when surfaces/popups are destroyed | Smithay (used by cosmic-comp) does not send `wl_pointer::leave` when a popup is destroyed. Violates protocol expectation: "The leave notification is sent before the enter notification for the new focus." Detected by WLCS test suite. |
| [#1420](https://github.com/Smithay/smithay/issues/1420) | **Open** | Pointer surface focus not checked before button press | Pointer focus state issues. |
| [#1555](https://github.com/Smithay/smithay/issues/1555) | **Open** | Issues with anvil's cursor_position_hint | cursor_position_hint problems in Smithay's example compositor. |

---

## 5. How Applications Work Around This

### The Full-Screen Layer-Shell Overlay Technique

This is the **canonical workaround** recommended by Wayland compositor developers (see emersion's response on Sway PR #8780):

1. Create a fullscreen transparent layer-shell surface on each output (overlay layer)
2. Wait for `wl_pointer.enter` event, which delivers surface-local coordinates
3. Surface covers the entire output, so surface-local coords = output-relative coords
4. Combine with output geometry from `wl_output` to get global position
5. Show your UI at those coordinates, then destroy the overlay

**Used by**: Kando (pie menu), shotman (screenshot tool), wl-find-cursor

**Problems**:
- Requires `wl_pointer.enter` to fire for a newly-mapped surface (not always reliable — see Section 6)
- Briefly covers the entire screen (even if transparent)
- Doesn't work on GNOME (which rejected layer-shell protocol)
- Race condition: if user moves mouse during surface mapping, coordinates may be stale

### Compositor-Specific IPC

| Compositor | Method | Command |
|------------|--------|---------|
| **Hyprland** | hyprctl IPC | `hyprctl cursorpos` → returns `x, y` |
| **KDE/KWin** | KWin scripting + D-Bus | Load a JS script via D-Bus that reads `workspace.cursorPos.x/y` and writes to journalctl. Ugly but works. |
| **Sway** | No built-in command | PR #8780 proposed `swaymsg -t get_cursor` but was not merged. Developers say to use layer-shell instead. |
| **GNOME/Mutter** | No known method | GNOME rejects layer-shell AND compositor-specific IPC for this purpose. |
| **COSMIC** | No known method | No IPC, no virtual-pointer, no custom protocol. |

### Other Approaches

- **wl-find-cursor**: Uses layer-shell + virtual-pointer to create an overlay, then reads pointer enter coordinates. If virtual-pointer is unavailable, layer-shell alone may suffice. Does NOT work on GNOME.
- **Wayland-automation (Python library)**: For Hyprland/Sway/wlroots compositors. Uses multiple backends: Hyprland IPC → wl-find-cursor → xdotool fallback → evdev.
- **uinput / evdev (kernel bypass)**: Read `/dev/input/event*` directly. Requires root or membership in `input` group. Not a Wayland protocol solution; bypasses the compositor entirely.
- **XDG RemoteDesktop Portal + libei**: Can inject input but **cannot read cursor position**. The RemoteDesktop portal provides no position query.

### Clipboard Managers Specifically

Wayland clipboard managers (cliphist, clipman, clapboard) typically:
- Use `wl-copy`/`wl-paste` for clipboard access (via `wlr-data-control` protocol)
- Show their UI through rofi/wofi/tofi which are **fullscreen or centered** — they do NOT position at cursor
- Some (cursor-clip) claim to position at cursor, likely using the layer-shell overlay technique

### References

- [wl-find-cursor (GitHub)](https://github.com/cjacker/wl-find-cursor)
- [Wayland-automation (GitHub)](https://github.com/OTAKUWeBer/Wayland-automation)
- [KDE Discuss — Cursor Location](https://discuss.kde.org/t/is-there-a-better-way-to-obtain-the-mouse-cursor-location/20559)

---

## 6. Compositor Behavior: `wl_pointer.enter` on New Surfaces

### What the Protocol Spec Says

The Wayland protocol specification describes `wl_pointer.enter` as:

> "Notification that this seat's pointer is focused on a certain surface."

And `wl_pointer.leave`:

> "The leave notification is sent before the enter notification for the new focus."

Both are described as:

> "The `wl_pointer.enter` and `wl_pointer.leave` events are **logical events generated by the compositor** and not the hardware."

The specification says compositors SHOULD group leave/enter in the same frame when moving between surfaces, but clients MUST NOT rely on them being in the same frame.

### The Critical Ambiguity

**The spec does NOT explicitly state** that the compositor MUST send `wl_pointer.enter` when a new surface is mapped underneath a stationary cursor. The spec only describes enter/leave in the context of pointer movement ("when a pointer moves from one surface to another"). This creates a gray area:

- **Conservative interpretation**: Enter/leave events reflect pointer movement. If the pointer hasn't moved, no enter event is required even if a new surface appears under it.
- **Liberal interpretation**: The compositor should re-evaluate focus whenever the surface stack changes. If a new surface is now the topmost under the cursor, an enter event should be sent.

### How Compositors Actually Behave

| Compositor | Behavior | Evidence |
|------------|----------|----------|
| **Sway (wlroots)** | **Buggy/inconsistent.** Sway issue [#8679](https://github.com/swaywm/sway/issues/8679) (open): "Randomly, may not send wl_pointer::Enter." Issue [#7984](https://github.com/swaywm/sway/issues/7984) (closed, fixed in 1.10): layer-shell surfaces did not receive `wl_surface::enter` until a window resize occurred. This was treated as a **bug** and fixed. |
| **Smithay (cosmic-comp's base)** | **Has known issues.** Issue [#1257](https://github.com/Smithay/smithay/issues/1257) (open): Smithay doesn't send leave events when surfaces/popups are destroyed. Issue [#1420](https://github.com/Smithay/smithay/issues/1420) (open): Pointer surface focus not checked before button press. These suggest Smithay's focus re-evaluation may be incomplete. |
| **GNOME/Mutter** | Generally sends enter events when surface stack changes, but does not implement layer-shell so the question is moot for overlay-based techniques. |
| **Qtile** | PR [#2619](https://github.com/qtile/qtile/pull/2619) fixed an issue where enter events were being sent redundantly (on every button press, not just on focus change), showing the complexity of getting this right. |

### Is cosmic-comp's Behavior a Bug?

Based on the evidence:

1. **Smithay (cosmic-comp's Wayland library) has open issues** about not sending leave events on surface destruction (#1257) and not checking pointer focus before button presses (#1420). These indicate the focus re-evaluation logic is incomplete.

2. **Sway treated the same behavior as a bug** and fixed it in version 1.10 (issue #7984). The fix ensured `wl_surface::enter` events were delivered to layer-shell surfaces on mapping.

3. **The protocol spec is ambiguous** but the practical consensus among compositor developers is that focus should be re-evaluated when the surface stack changes, and enter events should be sent accordingly.

**Conclusion**: If cosmic-comp is not sending `wl_pointer.enter` when a layer surface appears under the cursor, this is most likely a **bug in Smithay's focus management**, consistent with the open issues. It is not "by design" in the sense that other compositors have either fixed this behavior or never had the issue.

---

## 7. Proposed Solutions in the Wayland Ecosystem

### freedesktop.org Proposals

| Proposal | Status | Description |
|----------|--------|-------------|
| **Cursor-spy protocol** ([wayland-protocols MR #39](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/39)) | **WIP/Stale** (opened Jul 2020) | Proposed by Andri Yngvason (wayvnc author). Allows spying on cursor images for VNC client-side cursor rendering. Does NOT expose position. |
| **Cursor-geometry protocol** ([wayland-devel mailing list, Dec 2024](https://www.mail-archive.com/wayland-devel@lists.freedesktop.org/msg43163.html)) | **Rough proposal** | Proposed by Campbell Barton (Blender maintainer). Addresses cursor theme/size discovery. Does NOT address position query. |
| **Pointer warp protocol** ([wayland-protocols](https://wayland.app/protocols/pointer-warp-v1), [KWin MR #6460](https://invent.kde.org/plasma/kwin/-/merge_requests/6460)) | **Exists** | Allows clients to warp the cursor to a different position on their own surfaces. Write-only (set position, not read). Restricted to surfaces the client owns. |
| **Mouse position portal** ([xdg-desktop-portal #880](https://github.com/flatpak/xdg-desktop-portal/issues/880)) | **Closed as not planned** | Proposed a D-Bus portal for cursor x/y queries. Closed without implementation. |
| **Input capture protocol** (ext-input-capture-v1) | **In discussion** | For cross-machine input sharing (Input Leap, Barrier). Would need cursor position for edge detection. Referenced in [cosmic-comp #2029](https://github.com/pop-os/cosmic-comp/issues/2029). |
| **libei (Emulated Input)** | **Exists** (freedesktop.org) | Transport library for input injection via RemoteDesktop portal. Write-only — can inject events but CANNOT query cursor position. |

### Compositor-Specific Extensions

| Compositor | Extension | Exposes Cursor Position? |
|------------|-----------|--------------------------|
| **Hyprland** | `hyprctl cursorpos` (IPC) | **Yes** — returns global x,y |
| **KDE/KWin** | KWin scripting API (`workspace.cursorPos`) | **Yes** — via D-Bus + JS script. Clunky but functional. |
| **Sway** | No equivalent (PR #8780 rejected) | **No** — developers insist on layer-shell approach |
| **GNOME/Mutter** | No known extension | **No** — also rejects layer-shell |
| **COSMIC** | No extension found | **No** |

### The Fundamental Gap

**There is no standard, cross-compositor Wayland protocol for querying cursor position.** This is intentional from a security perspective, but it creates real problems for:

- Clipboard managers wanting to popup at cursor
- Pie menus / radial menus (Kando)
- Input sharing tools (Barrier, Input Leap, waynergy)
- Automation tools (xdotool replacements)
- Accessibility tools
- Screenshot tools needing to identify the active output

The recommended workaround (fullscreen layer-shell overlay) is fragile, compositor-dependent, and rejected by GNOME. The xdotool author's summary is apt:

> "Wayland comes along and eliminates *everything* xdotool can do. Some of that elimination is given excuses that it is 'for security' with little found to acknowledge what is being elided and why."

---

## Summary Table: Can You Get Cursor Position?

| Method | Cross-compositor? | Requires privileges? | Read position? | Write position? |
|--------|-------------------|---------------------|---------------|-----------------|
| `wl_pointer.enter`/`motion` | Yes | No | Surface-local only | No |
| Layer-shell overlay trick | Partial (no GNOME) | No | Yes (with caveats) | No |
| `hyprctl cursorpos` | Hyprland only | No | Yes (global) | No |
| KWin scripting + D-Bus | KDE only | No | Yes (global) | No |
| uinput / evdev | Yes | Root or `input` group | Yes | Yes |
| libei + RemoteDesktop portal | GNOME, KDE | Portal authorization | No | Yes |
| `wlr-virtual-pointer` | wlroots only | No | No | Yes |
| XWayland + XQueryPointer | Yes (if XWayland available) | No | Stale/inaccurate | No |

---

## Key Takeaways for Clipboard Manager Development

1. **There is no clean, cross-compositor way to position a popup at the cursor on Wayland.** This is a known, intentional limitation.

2. **The best available technique** is the fullscreen transparent layer-shell overlay, but it depends on the compositor reliably sending `wl_pointer.enter` to newly-mapped surfaces. Sway fixed this in 1.10; cosmic-comp (via Smithay) likely has this bug.

3. **For COSMIC specifically**, the path forward likely involves:
   - Filing a Smithay issue / confirming the enter-event behavior
   - Using the layer-shell overlay technique once the enter event works
   - Or contributing a cosmic-specific protocol extension for cursor position query

4. **Most Wayland clipboard managers avoid the problem entirely** by showing their UI centered on screen or as a fullscreen overlay (rofi/wofi style) rather than positioning at the cursor.

5. **The Wayland ecosystem is slowly addressing these gaps** through portals and per-compositor extensions, but a standard cursor position query protocol has been explicitly rejected at the freedesktop.org level.
