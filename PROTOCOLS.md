# Wayland protocol support — Odin

[German version / Deutsche Fassung](PROTOCOLS_de.md)

Every protocol below is implemented in-tree — no wlroots, no
Smithay. Notes describe behavior beyond a bare global: lifecycle
handling, edge cases and how the implementation was verified.
Status is honest: entries marked *verified* ran against real
client programs in the nested selftest or a live session.

## Core shell

- **xdg-shell (xdg_wm_base v6)** — toplevels with state
  (activated/maximized/fullscreen), `xdg_popup` menus via
  `xdg_positioner`, correct SHM buffer lifecycle
  (`wl_buffer.release`, damage-only commits keep the shown
  buffer); `configure_bounds` reports the window's usable
  output rect, `wm_capabilities` the honest list
  (maximize/fullscreen/minimize — no window menu), the
  `suspended` state marks invisible/minimized toplevels, and
  `set_minimized` drives the real iconify path
- **`wlr-layer-shell` v4** — panels, wallpapers, launchers,
  notifications: anchors, margins, exclusive zones/edges,
  keyboard interactivity, the configure/ack handshake, all four
  bands in hit-testing and compositing, exclusive zones shrink
  the tiling area. Verified with a real layer-shell client
- **`zxdg_decoration_manager_v1`** + KDE `server_decoration` —
  CSD/SSD negotiation plus a real SSD renderer: titlebar,
  borders, close button, titlebar move and border resize grabs
  (`decoration =` config key picks the default mode)
- **`xdg_dialog_v1`** + **`xdg_system_bell_v1`** — modal
  dialogs block ancestor focus; bells route to the session tty
- **`xdg_toplevel_icon_manager_v1`** — window icons: named
  icons and SHM pixel snapshots (square, per-scale buffers)
  assigned double-buffered at surface commit
- **`zxdg_foreign_v2`** — exported/imported toplevel handles,
  transient-for across client boundaries; children restack
  above their parents
- **`xdg_activation_v1`** — launch-activation tokens;
  `activate` requests route through the real focus path

## Seat, input, clipboard

- **Seat input** — pointer motion/buttons/axis with
  `wl_pointer.frame`, click-to-focus, move/resize grabs;
  keyboard with keymap, live modifiers and repeat info
  (version-gated for older client binds)
- **`zwp_relative_pointer_v1`** — raw, unaccelerated deltas for
  games alongside the accelerated pair
- **`zwp_pointer_constraints_v1`** — pointer lock and confine
  with real `wl_region` rectangles, oneshot/persistent
  lifetimes, cursor-position hints on unlock
- **`wp_cursor_shape_v1`** — own XCursor theme loader, hotspot-
  correct themed cursors, plus client `set_cursor` surfaces and
  a compositor fallback pointer
- **`zwp_pointer_gestures_v1`** — touchpad swipe/pinch streams
  to the pointer-focus client
- **`zwp_keyboard_shortcuts_inhibit_v1`** — games and VMs grab
  every key; compositor bindings suspend while active
- **`zwp_tablet_manager_v2`** — tablet seats, tablets, tools
  (proximity/motion/pressure/tilt/tip/buttons) and pads
  (buttons/rings/strips in mode groups) from libinput
- **`wl_touch`** — slot-tracked multi-touch with implicit-grab
  semantics; touch-initiated move/resize grabs release on
  finger lift
- **Clipboard** — `wl_data_device` selection, copy/paste between
  Wayland clients
- **Drag & drop** — full `wl_data_device` v3 drag routing:
  implicit-grab semantics, enter/motion/leave re-targeting,
  mime + action negotiation, drop→receive→finish pipeline and
  a drag icon that rides the cursor — verified end-to-end by
  a scripted selftest client
- **`zwlr_data_control_manager_v1`** — clipboard-manager access
  (wl-copy/wl-paste verified), sharing one clipboard with
  wl_data_device and the X11 bridge
- **`ext_data_control_manager_v1`** — the stabilized
  data-control protocol; the same clipboard surface as
  zwlr_data_control for modern GTK4/KDE clients
- **`zwp_primary_selection_device_manager_v1`** — primary
  selection (middle-click paste): sources, offers, sends —
  verified with wl-copy --primary / wl-paste --primary
- **`zwp_text_input_v3`** + **`zwp_input_method_v2`** — the IME
  pair: field state to the input method, preedit/commit
  strings back, keyboard grabs and popup surfaces
- **`zwlr_virtual_pointer_manager_v1`** /
  **`zwp_virtual_keyboard_manager_v1`** — virtual input
  injection through the real input paths
- **`zwlr_input_inhibit_manager_v1`** — single-client input
  inhibition for lock screens

## Buffers, surfaces, presentation

- **`zwp_linux_dmabuf_v1` v3** — GPU-sharable buffers mmap'd
  into the blit path (LINEAR, XRGB/ARGB8888, single-plane)
- **`wp_linux_drm_syncobj_v1`** — explicit synchronization for
  dmabuf clients: acquire points hold an attach until its
  fence signals (polled non-blockingly at the frame tick),
  release points signal two presented frames out through the
  same staging wl_buffer.release uses. Implicit dmabuf fences
  stash the attach identically — no sample-early flicker.
  Timeline imports mint their handles on Odin's own DRM render
  node — drivers reject `fd_to_handle` on the client's exported
  syncobj fd, which is what killed XWayland and GTK4 clients
  on import
- **`wp_viewporter`** — client-driven surface crop/scale,
  pixel-verified through screencopy
- **`wp_fractional_scale_v1`** — per-surface preferred scale
  fed from the surface's output
- **`wp_single_pixel_buffer_v1`** — solid-color 1×1 buffers as
  real anonymous mmaps through the normal buffer path
- **`wp_alpha_modifier_v1`** — per-surface alpha multiplier in
  both compositing paths (CPU blit, Vulkan push constant)
- **`wp_content_type_v1`** — content hints (none/photo/video/
  game) on SurfaceData for renderer/output policy
- **`wp_tearing_control_v1`** — async presentation hint →
  ASYNC_PAGE_FLIP on the DRM path with driver fallback
- **`wp_presentation`** — per-surface present feedback with
  real timestamps, refresh interval and sequence numbers
- **`zwp_idle_inhibit_manager_v1`** — per-surface idle
  inhibition
- **`ext_idle_notifier_v1`** — idle/resumed notifications with
  per-seat timeouts; feeds the future DPMS/lock pipeline

## Outputs, capture, session

- **`zxdg_output_manager_v1`** — logical output name/position/
  size for capture tools
- **`zwlr_output_manager_v1`** — live monitor configuration
  for wlr-randr/wdisplays/kanshi: one `wl_output` global and
  one head object per output, serial-gated configuration
  transactions, enable/disable/position/mode applied live —
  verified with wlr-randr
- **`zwlr_output_power_manager_v1`** — real DPMS: legacy KMS
  CRTC blank on the DRM backend, present-stop on nested
- **`zwlr_gamma_control_manager_v1`** — per-output gamma ramps
  applied at present like a CRTC LUT; captures stay pre-gamma
- **`zwlr_screencopy_manager_v1`** — output capture into client
  SHM buffers with damage/frame events. Verified with grim
  producing a correct 1280×800 PNG of the composited scene
- **`ext_image_copy_capture_v1`** +
  **`ext_image_capture_source_v1`** — the stabilized
  screen-capture successor used by newer portals/OBS: output
  and toplevel sources, session/frame objects, SHM blit into
  the client pool, honest `stopped` when a source dies
- **`ext_session_lock_v1`** — session locking: lock surfaces
  configured per output render exclusively (verified by
  screencopy pixel capture), input restricted to the lock
  client, unlock + re-lock cycles verified end-to-end
- **`zwlr_foreign_toplevel_management_v1`** — taskbars and
  window lists: per-toplevel handles with title/app_id/state,
  replay for late-binding managers, and working window ops
  (activate, close, maximize, fullscreen, real minimize that
  removes the window from rendering until restored). Verified
  by an automated client in the nested selftest
- **`ext_foreign_toplevel_list_v1`** — read-only toplevel list:
  title, app_id, identifier, state and output association,
  mirroring the management handles' lifecycle
- **`ext_workspace_manager_v1`** — workspaces for taskbars and
  docks: one group per enabled output (output_enter on the
  client's own wl_output), nine workspaces with
  id/name/coordinates/capabilities/state, activate + assign
  batched by `commit`, live diffs after compositor-driven
  switches — verified by ws_proto_test
