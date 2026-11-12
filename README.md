# Odin

An independent Wayland compositor written in Rust — its own scene
graph, its own renderer, its own window management. Built to run a
full desktop session directly on DRM/KMS, with hardware
acceleration as the default path.

[German version / Deutsche Fassung](README_de.md)

## Status

**Early development — experimental.** The core is functional and
runs real Wayland clients on real hardware, but this is not yet a
production desktop. Expect missing protocol coverage and sharp
edges. The roadmap below is the honest list of what is not done.

## What works today

- **Direct DRM/KMS backend** — runs on a real VT via
  libseat/seatd, no parent session required
- **Multi-monitor** — one CRTC per connector, a global scene
  coordinate space, per-output compositing with viewport clipping
- **Vulkan renderer** — per-output backend: texture uploads from
  client surfaces, textured-quad compositing, readback into the
  scanout buffer. Verified end-to-end on real hardware by an
  automated render test (`--vk-render-test`)
- **Renderer cascade** — Vulkan → OpenGL ES → CPU, selected per
  output with graceful fallback; the CPU rasterizer keeps the
  compositor alive where no GPU path exists
- **Damage-driven frame scheduling** — frames are produced on
  scene changes, not on a blind timer
- **xdg-shell clients** — real applications (terminals and more),
  SHM buffer lifecycle handled correctly
- **Client cursors** — `wl_pointer.set_cursor` surfaces with
  hotspot support, plus a compositor fallback pointer
- **Floating and tiling layouts** — master/stack arrangement,
  switched live via keybinding, new windows tile immediately
- **Configurable keybindings** — `~/.config/odin/bindings.conf`
- **Display manager integration** — session wrapper and desktop
  file for LightDM-style login

## Requirements

- Linux with `seatd` (or a working libseat backend)
- `libinput`, `libudev`, `xkbcommon`, `libdrm`
- A Vulkan-capable GPU and driver for the accelerated path
  (tested on NVIDIA; the code uses no vendor-specific features —
  AMD and Intel drivers expose the required interfaces)
- Rust toolchain (edition 2021)

## Build

```sh
cargo build --release
```

Shaders are compiled to SPIR-V at build time via `naga` — no
external shader toolchain needed.

## Run

From a display manager, install the session files:

```sh
sudo install -Dm755 target/release/odin /usr/bin/odin
sudo install -Dm755 session/startodin /usr/bin/startodin
sudo install -Dm644 session/odin.desktop /usr/share/wayland-sessions/odin.desktop
```

Logs land in `~/.local/state/odin/` — `odin.log` plus
`stderr.log` for client output.

### Development modes

```sh
# Nested inside a running session (no VT switch):
ODIN_BACKEND=nested ./target/release/odin

# Verify Vulkan device/context on this machine:
./target/release/odin --vk-probe /dev/dri/card1

# Full pipeline test — upload, draw, readback, pixel check:
./target/release/odin --vk-render-test /dev/dri/card1
```

## Configuration

`~/.config/odin/bindings.conf`, one binding per line:

```
C-A-t = spawn weston-terminal
C-A-c = spawn weston-clickdot
A-Tab = cycle-focus
C-A-Space = toggle-layout
C-A-Backspace = quit
```

Modifiers: `C` Ctrl, `A` Alt, `S` Shift, `W` Super. Actions:
`spawn <command>`, `cycle-focus`, `toggle-layout`, `quit`.

## Architecture

| Module | Responsibility |
|--------|----------------|
| `src/compositor.rs` | Wayland globals, surface state, input routing, grabs |
| `src/scene.rs` | Global scene coordinates, stacking order |
| `src/wm.rs` | Layout policies (floating, master/stack tiling) |
| `src/backend/` | DRM/KMS outputs, libinput, event loop, nested backend |
| `src/renderer/` | Swappable backends: `vulkan/` (primary), `gles/` (fallback slot), `cpu/` (last resort) |
| `src/session.rs` | Seat/VT management via libseat |
| `src/config.rs` | Keybinding parser |
| `shaders/` | GLSL sources, compiled to SPIR-V by `build.rs` |

Renderer selection is per output: Vulkan first, GLES when Vulkan
is unavailable, CPU only when no GPU path exists at all.

## Roadmap

Renderer:

- Zero-copy scanout via DMABUF import and format modifiers
  (extension probing already implemented)
- Texture, staging and descriptor caching — uploads currently
  allocate per frame
- Per-surface damage tracking
- Hardware cursor planes
- OpenGL ES fallback implementation

Protocol and desktop:

- `wlr-layer-shell` (panels, backgrounds, lock surfaces)
- Screencopy / DMABUF capture
- `foreign-toplevel-management`
- Pointer constraints, keyboard inhibitors
- Output management and live primary-output selection
- D-Bus settings interface
- XWayland support

Session and integration:

- Move/resize pointer grabs for floating windows
- Window decorations policy
- Automated test coverage beyond the Vulkan selftest

## License

GPL-3.0-or-later — see `LICENSE`.
