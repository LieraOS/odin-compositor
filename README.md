# Odin

An independent Wayland compositor written in Rust — **no
wlroots, no Smithay**. The full compositor stack is implemented
directly: DRM/KMS session handling, Vulkan rendering, scene
model, input and focus semantics, window management, workspaces,
and an own X window manager for XWayland. (The `wlr-*` protocol
extensions Odin implements — layer-shell, screencopy, output
management and friends — are public Wayland protocol
specifications Odin speaks itself, not wlroots library code.)

> **Status: experimental.** Odin runs real Wayland and X11
> clients on real hardware — but it is an evolving codebase,
> not a daily driver. Expect rough edges, unhandled corner
> cases and breaking changes. See `TODO.md` for the honest
> list of what is done and what is not.

> **Tested hardware: NVIDIA** (proprietary driver, 580.x
> series). The renderer uses no vendor-specific Vulkan
> features — but no other GPU vendor has been exercised yet,
> so AMD/Intel reports are welcome.

![A live Odin session — master/stack tiling on the left
output, floating on the right](docs/screenshot.png)

[German version / Deutsche Fassung](README_de.md) ·
[Protocol support](PROTOCOLS.md) · [TODO](TODO.md)

## Highlights

- **Direct DRM/KMS session** — runs on a real VT via
  libseat/seatd, no parent compositor required; can sit on a
  second VT next to an existing session
- **Vulkan renderer** — per-surface damage tracking,
  retained rendering, direct scanout of renderer parities and
  eligible fullscreen client buffers; CPU fallback when no GPU
  path exists (the OpenGL ES slot in the cascade is reserved
  but not yet implemented)
- **Own XWM** — XWayland clients are first-class citizens:
  windowing, focus, override-redirects, EWMH basics and
  clipboard bridging in both directions
- **Per-monitor layouts** — every output independently runs
  floating (default) or master/stack tiling, switchable live
  without a restart; a window dragged across monitors adopts
  the destination's policy; fullscreen clients always bypass
  the grid
- **Workspaces** — i3 model: workspaces own outputs, windows
  own workspaces
- **~40 Wayland protocols** — layer-shell, session-lock,
  screencopy and image-copy-capture, explicit sync via
  drm-syncobj, output management, IME, tablet input and more:
  see [PROTOCOLS.md](PROTOCOLS.md) for the full list with
  implementation notes
- **Hardware cursor plane** — pointer motion is a
  `MOVE_CURSOR` ioctl, not a composite; drivers without a
  cursor plane fall back to the software cursor automatically

## Requirements

- Linux with `seatd` (or another working libseat backend)
- System libraries: `libinput`, `libudev`, `xkbcommon`,
  `libdrm`
- A Vulkan-capable GPU and driver for the accelerated path
  (developed and tested on NVIDIA; the code uses no
  vendor-specific features — anything implementing the
  required Vulkan/dmabuf interfaces should work)
- Rust toolchain (edition 2021) — shaders are compiled to
  SPIR-V at build time via `naga`, no external shader
  toolchain needed

## Install and run

### 1. Build

```sh
cargo build --release
```

### 2. Install the session files

```sh
sudo install -Dm755 target/release/odin /usr/bin/odin
sudo install -Dm755 session/startodin /usr/bin/startodin
sudo install -Dm644 session/odin.desktop \
    /usr/share/wayland-sessions/odin.desktop
```

`startodin` accepts the session shell to launch once the
compositor is up — the shipped desktop file runs a bare
session (compositor only); desktop environments pass their
own session command, e.g. `startodin my-session`. Log out,
pick **Odin** in your display manager, log in.

Useful to know up front:

- **Quit bindings:** `C-A-q` or `C-A-Backspace` — keep these
  in mind on a first try
- **Terminal:** seed a binding in `~/.config/odin/bindings.conf`
  (created with defaults on first start) — the default file
  binds `C-A-t` to `weston-terminal`; point it at your
  terminal of choice
- **Autostart:** `~/.config/odin/autostart` runs one shell
  command per line once the session is up (panels, wallpaper,
  session components). `odin -S "<command>"` is the
  command-line equivalent
- **Logs:** `~/.local/state/odin/` — `odin.log` plus
  `stderr.log` for client output; check these first when
  something misbehaves

### 3. Try it without logging out

On a free VT, Odin can run next to an existing session —
libseat arbitrates the devices. Or run it nested as a window:

```sh
# Nested inside a running Wayland session:
ODIN_BACKEND=nested ./target/release/odin

# Scripted self-test — spawns real clients, drives input,
# dumps the composed frame for pixel checks:
ODIN_BACKEND=nested ODIN_NESTED_TEST=1 ./target/release/odin

# Verify Vulkan device/context on this machine:
./target/release/odin --vk-probe /dev/dri/card1

# Full pipeline test — upload, draw, readback, pixel check:
./target/release/odin --vk-render-test /dev/dri/card1
```

## Configuration

`~/.config/odin/bindings.conf`, one binding per line:

```
C-A-t = spawn weston-terminal
A-Tab = cycle-focus
C-A-Space = toggle-layout
C-A-q = quit
C-A-Backspace = quit
C-A-d = dump-frame
W-1..9 = ws-1..9
W-S-1..9 = mv-1..9
primary = DP-3
policy = floating
policy.HDMI-A-1 = tiling
decoration = auto
```

Modifiers: `C` Ctrl, `A` Alt, `S` Shift, `W` Super.
`primary` names the primary output, `policy` sets the layout
per output or globally, `decoration` picks the CSD/SSD
negotiation default. The file is watched and re-applied
live.

### Binding actions

The complete current set — deliberately small; Odin is a
compositor, not a shell. It grows as real needs show up:

| Action | Effect |
|--------|--------|
| `spawn <command>` | Launch a client (inherits `WAYLAND_DISPLAY`, and `DISPLAY` once Xwayland is up) |
| `cycle-focus` | Cycle keyboard focus through windows |
| `toggle-layout` | Flip the pointer's output between floating and tiling |
| `ws-<n>` | Switch the output under focus to workspace `n` (1–9) |
| `mv-<n>` | Send the focused window to workspace `n` (1–9) |
| `dump-frame` | Dump the composed frame to `~/.local/state/odin/` |
| `quit` | End the session (works even while the screen is locked) |

Everything else — volume keys, launchers, screenshots — is
one `spawn <command>` away from your favourite tool.

## Architecture

| Module | Responsibility |
|--------|----------------|
| `src/compositor/` | Wayland protocol implementation: globals, surface lifecycle, layer-shell, seat input, pointer extensions, cursor shapes, clipboard, rendering order, window operations |
| `src/wm.rs` | Layout policies and output layout (floating, master/stack tiling, primary output) |
| `src/backend/` | Platform side: DRM/KMS outputs, libinput, event loop, GPU probing, nested backend |
| `src/renderer/` | Swappable render backends: `vulkan/` (primary), `gles/` (reserved, unimplemented), `cpu/` (fallback) |
| `src/xwm/` | X window manager for XWayland: connection handshake, event dispatch, EWMH, selection bridging, X socket lifecycle |
| `src/session.rs` | Seat/VT management via libseat |
| `src/xcursor.rs`, `src/cursor.rs` | XCursor theme loading and the software pointer cursor |
| `src/config.rs` | Keybinding and settings parser |
| `src/log.rs` | Persistent tracing to `~/.local/state/odin/` |
| `shaders/` | Shader sources, compiled by `build.rs` |

Renderer selection is per output: Vulkan first, CPU when no
GPU path exists.

## Roadmap

The full status lives in `TODO.md` / `TODO_de.md` — done,
open and known issues. The short version of what is still
ahead:

- OpenGL ES fallback backend (slot exists; reports unavailable)
- DMABUF screencopy path (SHM capture exists)
- Output-management follow-ups (adaptive sync, primary
  selection on the output manager)
- XWayland XTEST via libei
- Broader automated coverage beyond the nested and Vulkan
  self-tests
- Real-session validation — Odin is verified through nested
  self-tests and instrumented runs, not yet by years of daily
  use

## Goals

- **Complete, not just working** — everything a compositor
  owns lives in this tree: protocol handling, scene graph,
  renderer, input, focus, window management, workspaces,
  multi-monitor, XWayland. The target is full feature
  coverage, not full bug-freedom.
- **No borrowed core** — no wlroots, no Smithay. Every frame
  and every event runs through code this project understands
  end to end — which is also why bugs get fixed here instead
  of reported upstream.
- **Performance as a feature** — Vulkan-first rendering,
  direct scanout, damage-driven frame pacing, zero-copy paths
  wherever the hardware allows them.

## License

Copyright (C) 2026 Jörg Schröder.

GPL-3.0-or-later — see `LICENSE`.
