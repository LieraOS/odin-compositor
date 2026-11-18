# TODO — Odin

[German version / Deutsche Fassung](TODO_de.md)

## Done

- `dump-frame` no longer freezes the session — the dump read
  ~9 MB pixel-by-pixel out of a write-combined dumb-BO mapping
  and wrote the PPM synchronously inside `present()`, blocking
  the event loop ~8 s per output (~16 s dead input on dual
  monitor). The frame is now bulk-copied into cached RAM once;
  BGRA→PPM conversion and the file write run on a worker
  thread, so the stall shrinks to one memcpy.
- `zwlr_screencopy_v1` captures the REQUESTED output —
  `CaptureOutput`/`CaptureOutputRegion` ignored the `output`
  argument and always captured `primary_rect()`, so tools
  iterating all monitors got the primary screen N times.
  Both requests now resolve the bound wl_output's connector
  name to that output's global-space rect (ext-image-copy-
  capture already did this correctly).
- `dump-frame` now captures every output — the binding used to
  set a boolean that the first presenting output consumed, so a
  multi-monitor session dumped exactly one screen and which one
  depended on present order. The flag is now a `dump_seq`
  generation in `CompState`; each `Output` tracks `dump_done`
  and writes its own `dump-<x>-<y>.ppm` once per generation at
  its next present, so all monitors land in the state dir —
  including an idle output that presents late.
- Vulkan parity retirement picked by cover_seq, not slot index
  — `take_retired`/the render-time retire scan iterated
  parities 0..3 and kept the last-signaled index as "newest",
  but once `cur` wraps, index order is not submission order.
  Presenting the older of two retirees swapped a stale frame
  in for the newest (visible flicker under continuous damage,
  e.g. fullscreen video) and could regress `presentable`,
  which mis-ordered the settle-time damage queue. Both scans
  now select the max-cover_seq retiree and only raise the
  bound.
- Vulkan mid-flight destruction fixed — imported dmabuf
  textures replaced on buffer-id reuse and shm textures
  replaced on commit were destroyed/re-pooled immediately,
  while a previous submission's descriptor set could still
  sample them (GPU use-after-free → VK_ERROR_DEVICE_LOST on
  NVIDIA, then a forced CPU-renderer demotion). Both now go
  through `tex_retire`/`imp_retire` lists drained by the
  existing all-parities-retired sweep.
- `xdg_toplevel_icon_v1`: `immutable` error only on the mutating
  requests (`set_name`, `add_buffer`) — `destroy` must always
  pass since clients drop the icon object right after
  `set_icon` (spec: the icon stays set). Erroring on destroy
  killed Electron apps (Discord/devin-desktop) with
  "error 2: icon is assigned to a toplevel" mid-window-setup.
- CPU-composite `fill_rect` bounds guard — clip rects are
  intersected against surface bounds, never the output, so an
  off-screen rect (x0 ≥ x1) sliced `frame[start..end]`
  backwards and panicked the compositor; hit on GPU-device-loss
  demotion when a 1025×882 Electron window repainted.
- Ordered session teardown — after the event loop stops,
  `backend::teardown::shutdown` kills the Xwayland child
  (SIGTERM→SIGKILL grace; `Child::drop` alone never kills),
  closes listener + display so every client sees a real
  disconnect, releases the seat via `Seat::disable`, and arms a
  10 s watchdog (`_exit`) around the drop cascade — a wedged
  `vkDeviceWaitIdle` can no longer pin the session leader for
  ~50 min while the display manager waits (observed hang).
- `new_id` requests initialise their object on every path —
  error/`failed`/`stopped` replies now leave a valid server-side
  object behind (audit across dmabuf/shm/decoration/syncobj/
  session_lock/fractional-scale/alpha/content-type/tearing/
  input-inhibit/copy-capture/spb). Previously an early error
  return skipped `init` and a later request on that id panicked
  the backend — the process-wide abort that killed the session
  on an Electron `create_immed`.
- Client identity logging — accepts log `pid`+`comm` via
  `SO_PEERCRED`; disconnects log pid/comm/`DisconnectReason`,
  distinguishing client-side exits (`ConnectionClosed`) from
  compositor protocol kills (`ProtocolError`).
- Pointer motion coalescing — raw libinput events set a pending
  flag; `flush_motion` delivers one `wl_pointer.motion` (plus one
  accumulated `relative_motion`) per frame tick, flushed ahead of
  ordered button/axis events. A 1 kHz device no longer floods
  clients into repaint backlogs (menu-hover input stalls).
- Damage-clipped texture uploads restricted to same-buffer
  (damage-only) commits — per the spec both `damage` and
  `damage_buffer` are diffs against the previous frame, never
  against a re-attached buffer's own content, so attaches always
  full-upload; per-buffer damage accumulates across commits that
  outpace frames (>24 rects collapses to full).
- Render-parity coherence: retire->pick->reset->record->submit a
  single parity; blocked set = scanned + flip-pending; no free
  parity re-presents the last retired frame. Fence-reset dead
  parities can no longer wedge the queue into CPU demotion.
- Offscreen surface culling — surfaces outside an output's
  rect skip upload+draw instead of landing in every output's
  texture cache; per-frame upload volume and counts log as
  `up_kb`/`up_texs` in the present-stats line.
- Client dmabuf → Vulkan import: `zwp_linux_dmabuf_v1` buffers
  (LINEAR, single-plane XR24/AR24) import as sampled images via
  `VkImportMemoryFdInfoKHR` + explicit modifier — zero memcpy
  where clients deliver GPU memory; XRGB views swizzle alpha to
  1. Failed imports mark the buffer id and draw via the mmap'd
  fallback. `up_big` in present-stats names the biggest upload.
- Deferred `wl_buffer.release` — replaced buffers release two
  presented frames after their swap (pending→next→ready), not
  at attach time; dmabuf clients may legally rewrite a released
  buffer instantly, and an early release let them paint over
  pixels a CRTC still scanned (sporadic flicker).
- Per-surface shm textures — the cache keys on wl_surface, not
  wl_buffer: commit damage is a frame diff by spec, so every
  commit (attach included) legitimately patches the persistent
  texture; buffer alternation costs a rect patch instead of a
  full re-upload. `set_window_geometry` w/h now bounds
  hit-testing — shadow margins no longer eat clicks on the
  right/bottom window edges.
- `zxdg_output_v1` now emits the trailing `wl_output.done`
  after its logical-size burst (v3 deprecation rule) — Xwayland
  only recalculates the RANDR/screen geometry on wl_output.done,
  so the X screen stayed 0×0 and clamped every X11 pointer event
  to the origin (Steam, xev, XTEST all dead); verified end-to-end
  that ButtonPress now reaches X windows at exact coordinates
- `zxdg_output_v1` resolves the queried wl_output by its bound
  connector name instead of always reporting the primary rect
- Hardware cursor plane on DRM — `set_cursor2`/`move_cursor`
  with automatic per-output software fallback; pointer motion
  no longer touches the render pipeline
- `wl_surface.set_input_region` honored in every hit-test path —
  XShape masks and transparent overlays pass clicks through
- `_NET_WM_MOVERESIZE` client-initiated move/resize for X11
  titlebar drags (Steam)
- Vulkan texture cache invalidates on buffer-rewrite commits —
  surfaces repainted under the same wl_buffer id upload fresh
- CPU blitter iterates damage ∩ surface ∩ frame bounds directly —
  no more full-surface pixel scans with per-pixel clip skips

- libseat session with DRM/KMS multi-output — disjoint CRTCs,
  global scene coordinate space, per-output compositing
- xdg-shell clients with correct SHM buffer lifecycle
  (`wl_buffer.release`) and frame callbacks
- Client pointer cursors with hotspot support plus compositor
  fallback pointer
- Per-output layout policies — Floating (default) and
  master/stack Tiling per monitor, toggled on the output under
  the pointer; windows adopt the policy of the output their
  center lands on (drop onto Tiling folds into the grid, drop
  onto Floating restores natural size), fullscreen clients are
  exempt; `primary = <connector>`, `policy =` (all monitors)
  and `policy.<name> =` config keys, live config reload via
  inotify on the config dir, unit-tested master/stack tile math
- Configurable keybindings via `~/.config/odin/bindings.conf`
- Damage-driven frame scheduling — frames on scene changes only
- Direct scanout via dmabuf export: the three readback parities
  carry DMA_BUF handles, become DRM framebuffers, and presents
  page-flip with zero CPU copies (CPU-copy path stays as the
  automatic fallback); CRTC/retired parity discipline keeps
  writes off the scanned buffer
- **Client-buffer direct scanout** — a fullscreen surface that
  covers the whole output, has a dmabuf-backed buffer, and sits
  in an otherwise-empty scene imports its dmabuf straight into
  a DRM framebuffer (`direct.rs`): the client's own buffer is
  page-flipped with zero compositor touch — no render, no
  upload, no readback. Scene safety rules out popups, layer
  surfaces, drag icons, sw-cursor fallback and screencopy;
  fb import or flip failure falls back to compositing on the
  next frame. The imported FB keeps a ref on the client buffer
  until the CRTC retires it — wl_buffer.release is held back
  the same way as for readback parities
- **Tearing control** — `wp_tearing_control_v1` async toggle
  honored per surface: a tearing-allowed surface switches the
  page-flip to `PageFlipFlags::ASYNC` (driver permitting) so
  fullscreen games can push frames without vsync wait
- Damage-clipped surface uploads — in-place wl_buffer rewrites
  patch only the client's damage rects into the cached texture
  instead of re-uploading the whole surface
- Cooperative render yields — input drain + hardware cursor
  moves interleave inside long uploads, copies and fence waits,
  bounding pointer latency to ~8 ms under load
- Vulkan renderer end-to-end: device context, graphics pipeline,
  surface texture uploads, compositing, readback — verified by the
  automated `--vk-render-test` on real hardware
- Renderer cascade Vulkan → GLES → CPU per output
- Session integration: `startodin` wrapper (takes the session
  shell command and passes it through as `-S`),
  wayland-session entry via `startodin`
- XWayland core: async `-wm` socket handshake, substructure
  redirect, `WM_S0` selection claim, composite off-screen
  redirect, `WL_SURFACE_ID` association (client-scoped, with
  cross-channel race resolution), X geometry applied to scene
  placement, real X11 clients verified (`xeyes`, `xclock`),
  click-to-focus and keyboard focus delivery to X surfaces,
  EWMH basics (`_NET_SUPPORTED`, `_NET_SUPPORTING_WM_CHECK`,
  `_NET_ACTIVE_WINDOW` both directions: compositor focus mirrors
  to X, X client activation requests focus the surface),
  ConfigureWindow push-back on move grabs, X-side raise
  mirroring, Wayland→X clipboard bridging (CLIPBOARD claim,
  TARGETS, async pipe transfers — verified with wl-copy and
  X11 SelectionRequest/convert_selection round-trips),
  X→Wayland clipboard via XFIXES SelectionNotify tracking
  (TARGETS negotiation, SelectionRequest pumping into paste
  pipe fds — verified end-to-end with wl-paste against a real
  X11 clipboard owner), `_NET_WM_STATE` application
  (fullscreen/maximized mapped onto window state), X windows
  in tiling layouts
- Focus-on-map (newly mapped toplevels get keyboard focus —
  required for serial-dependent clients like clipboard tools),
  protocol-version gating (`wl_keyboard.repeat_info` only on
  v4+ binds), display-handle safety inside request handlers
- Pointer protocol extensions: `zwp_relative_pointer_v1`
  (unaccelerated deltas for games), `zwp_pointer_constraints_v1`
  (lock/confine incl. regions, lifetime, cursor hint restore),
  `wp_cursor_shape_v1` (Xcursor theme loading, hotspot-correct
  compositing) — verified end-to-end by an automated client
  inside the nested selftest
- `wlr-layer-shell` v4 — anchors, margins, exclusive zones and
  edges, keyboard interactivity, configure/ack placement, all
  four bands in hit-testing and compositing order, exclusive
  zones shrink the tiling area — verified end-to-end with
  swaybg rendering an output-filling background
- Surface role gating: only xdg-toplevel/X-window surfaces join
  the stacking order — aux surfaces (pointer cursors, drag
  icons) committing buffers no longer get tiles or keyboard
  focus (that enter segfaulted real clients)
- Damage-only commits keep the shown buffer (tri-state pending
  attach) — required for clients like swaybg
- Session autostart — `~/.config/odin/autostart` (one command
  per line) and the `-S`/`--session` flag spawn session
  components onto the compositor socket at startup; verified
  in nested mode and correctly skipped during the scripted
  selftest
- One `wl_output` global per layout output — clients see every
  physical monitor as its own object with real position/mode
  (previously a single global hid everything but the primary)
- `zwlr_output_manager_v1` v4 — heads with name/description/
  physical size/mode/position/transform/scale/adaptive-sync
  replayed on bind, configuration transactions with serial
  gating (stale → cancelled), enable/disable/position/mode
  applied to the live layout, validation rejects configs that
  would leave zero outputs — verified end-to-end with
  wlr-randr (head listing, live position apply, --off
  correctly refused on the only output)
- `zwlr_screencopy_manager_v1` — output capture into client SHM
  buffers incl. damage/frame events — verified end-to-end with
  grim producing a 1280×800 PNG that contains the layer-shell
  wallpaper pixels
- `zxdg_output_manager_v1` — logical position/size/name for the
  wl_output; required by capture tools before they accept the
  output
- `zwlr_foreign_toplevel_management_v1` — one handle object per
  mapped toplevel carrying title/app_id/output/state (maximized,
  minimized, activated, fullscreen), replayed to managers that
  bind late; all window ops implemented: activate, close,
  maximize, fullscreen and real minimize (iconified windows
  leave rendering + hit-testing until restored) — verified
  end-to-end by an automated client in the nested selftest
  (minimize/activate/close round-trips)
- `zwlr_gamma_control_manager_v1` — per-output gamma ramps:
  wl_output resolution to connector, 256-entry ramp advertised,
  set_gamma ramp fd validated + read via pread (shared fd offset
  safe), stored per output and applied as a LUT at present like
  a CRTC — screencopy stays pre-gamma; ramp dies with its
  control object — verified by block_proto_test
- `zwlr_input_inhibit_manager_v1` — lock-screen input
  inhibition: single active inhibitor enforced via the
  already_inhibited protocol error, pointer hit-test and
  keyboard routing darken for all other clients while held,
  released on destroy or client death — verified by
  block_proto_test incl. disconnect cleanup
- `zwlr_virtual_pointer_manager_v1` — virtual pointer injection
  (ydotool/wayvnc class): relative + absolute motion mapped
  onto compositor space, buttons and axis events fed through
  the real input paths — verified by block_proto_test
- `zwp_virtual_keyboard_manager_v1` — virtual keyboard
  injection: keymap fd validated, key/modifier events routed
  through key_event like hardware input (bindings, repeat,
  inhibitor gating apply) — verified by block_proto_test
- `zwlr_data_control_manager_v1` v2 — clipboard-manager access:
  device/source/offer objects, selection + primary-selection
  observation, unified SelSrc model so wl_data_device and
  data-control owners share one clipboard incl. X11 bridging —
  verified with wl-copy/wl-paste round-trips
- Drag & drop (`wl_data_device` v3) — full DnD routing:
  start_drag under the implicit-grab rule (a button must be
  held), pointer events suppressed while dragging, per-motion
  re-targeting with drag_enter/motion/leave, mime + action
  negotiation (accept→target, set_actions→action),
  drop→receive→send→finish pipeline, drag icon renders on the
  cursor and never in captures — verified end-to-end by
  dnd_proto_test driving a scripted press/drag/release in the
  nested selftest, with real payload bytes read back
- `zwp_idle_inhibit_manager_v1` — per-surface idle inhibition
  tracked with liveness pruning; the query the future
  DPMS/lock path uses (idle_inhibited)
- `ext_idle_notifier_v1` + `ext_session_lock_v1` +
  `xdg_activation_v1` — session block: seat-timeout idle
  notifications, per-output lock surfaces that render
  exclusively (pixel-verified via screencopy), input
  restriction while locked, unlock + re-lock cycles,
  launch-activation token focus — all verified by
  session_proto_test
- Nested backend: client-request dispatch no longer depends
  on the display poll fd (proved unreliable — requests once
  sat unread for seconds); pump now runs every poll turn
- shm pool mmap failures (e.g. a write-only fd) now warn at
  create_pool instead of silently drawing black
- **`zwp_linux_dmabuf_v1` v3** — GPU-sharable client buffers:
  LINEAR XRGB8888/ARGB8888 single-plane dmabufs are mmap'd into
  the standard blit path; `create_params`/`create`/`create_immed`
  + honest `failed` on unsupported planes/modifiers/formats.
  v4+ feedback objects are intentionally not advertised.
  Verified end-to-end: a memfd-backed dmabuf rendered into a
  live toplevel, `created` + `failed` events correct.
- **`wp_presentation` v2** — per-surface present feedback:
  one-shot `feedback` objects fire `presented` with the real
  CLOCK_MONOTONIC timestamp, the output's wl_output (per-client
  object — cross-client sends are a protocol bug), refresh
  interval and sequence; `discarded` on surface destroy.
  Verified with a presentation-feedback test client (757 feedback events).
- Workspaces (i3 model) — workspaces own outputs, windows
  own workspaces: per-output visible workspace, single
  visibility gate shared by tiling/render/hit/focus-cycle,
  steal-fallback via prev_ws so no output goes blank,
  ws-N/mv-N keybinds (Super+1..9 / Super+Shift+1..9),
  focus follows to the top visible window — verified by a
  scripted selftest: the ws2 frame dump contains no window
  pixels, switching back restores them all
- `zwp_primary_selection_device_manager_v1` — primary
  selection: source/offer/device objects, mime offers, send
  to requester fds, sharing the SelSrc model with
  wl_data_device + data-control — verified with
  wl-copy --primary / wl-paste --primary round-trips
- `ext_workspace_manager_v1` — workspace protocol for
  taskbars/docks: one group per enabled output (output_enter
  resolves the client's own wl_output, retried per tick until
  bound), nine workspaces with stable ids, names, 1-D
  coordinates, capabilities (activate|assign) and state;
  requests queue until `commit`, apply via switch_ws/show_ws
  steal semantics, then `done`; `ws_sync` pushes per-tick
  diffs (state flips, group enter/leave, hotplug groups);
  `stop` → `finished` — verified end-to-end by ws_proto_test
- GPLv3-or-later licensing, English/German documentation
- Layer popups (GTK menus): `xdg_surface.get_popup` with a NULL
  parent used to leave the popup object without object data — a
  hard server panic on next use, reproducibly killed by the
  taskbar's start menu. The object now always initializes and
  `zwlr_layer_surface.get_popup` performs the reparent —
  verified end-to-end by lpop_proto_test
- Shared scene list (`CompState::scene`) — one stacking/visibility
  model consumed by every render backend: layer bands, popups,
  session-lock gating, minimize/workspace filters. Fixed a real
  gap where the Vulkan path drew only toplevels (invisible
  taskbar/panels on hardware) and adds themed cursor textures
  to the Vulkan pointer path
- Vulkan frame pipelining — the backend used to submit and then
  block the event loop on a full GPU roundtrip every frame;
  commit-per-frame clients (GTK menu hover) starved input
  dispatch and the pointer visibly lagged. The drain now
  double-buffers its readback and returns the previous
  frame's completed parity — submits never stall the loop.
  Correctness half of the pipeline: the in-flight frame is
  tracked by fence and `Backend::take_retired` hands it to a
  per-tick settle pass (`Output::settle`) that presents it the
  moment it retires — the last committed frame always lands
  even when no further damage arrives.
  Dead-buffer textures recycle through a size-keyed pool,
  ending a per-commit vkImage allocation churn plus a GPU
  resource leak (evicted textures were never destroyed;
  tex_cache, pool and cursor textures are now destroyed on
  backend drop)
- Present-path damage — the udev output used to memcpy a full
  ~9 MB frame into a write-combined dumb buffer and re-map it on
  every present, which is what actually starved input whenever a
  client committed per frame. The copy now clips to merged
  damage rects through a per-output queue that respects each
  backend's pipeline latency (`Backend::latency`), the dumb map
  is cached for the session, outputs untouched by the damage
  skip render entirely, and cursor updates damage a 128 px
  rect instead of the whole screen. The themed-cursor path also
  stopped hitting the filesystem per SetShape — theme images
  cache per (name, size) in CompState
- Cursor-ownership fix — set_cursor/set_shape now only apply
  from the client the pointer is actually over (the protocol's
  own validity rule), and a pointer-focus change voids the
  previous surface's cursor. Without it the last client shape
  (resize grip, dnd feedback) stayed stuck over windows that
  never re-armed one and over the desktop itself
- Ghost-surface fix — `prune_dead` only ran inside input-event
  dispatch and removed dead surfaces without damaging their
  vacated pixels, so windows of clients that died between
  input events stayed on screen as ghosts forever. The sweep
  now runs every frame tick and damages the scene whenever it
  actually removes an object
- Nested backend honors host sizing — xdg_toplevel.configure
  now rebuilds the memfd frame and the virtual output at the
  assigned extent instead of attaching a fixed 1280x800 buffer
  the host then clipped to the tile (the window showed only a
  corner of itself under tiling hosts)
- Socket instance pick — the compositor binds the first free
  odin-N name after a connect probe separates live instances
  from stale files. Previously a second instance (nested dev
  run) unlinked a running session's socket, silently orphaning
  all future clients. Spawned clients get the real bound name
  via WAYLAND_DISPLAY — verified live: a nested Odin bound
  odin-1 inside a running odin-0 session and ran the full
  selftest without touching it
- `wp_viewporter` — surface crop/scale: double-buffered pending
  state applied at commit, buffer-space damage transformed
  through the src/dst rect math (defaults derived from buffer
  dims), effective viewport size feeds hit-tests and whole-
  surface damage, CPU blit_scaled and Vulkan UV/destination
  draw semantics share one model — verified by vp_proto_test
  with pixel checks through screencopy (crop+stretch, reset)
- xdg_toplevel owner resolution — requests before the first
  commit (set_fullscreen, set_title, set_app_id, maximize)
  used to be silently dropped because the surface only enters
  the stacking order at first buffer attach. The toplevel now
  carries its owning wl_surface as object data — real client
  titles/app-ids reach foreign-toplevel taskbars again
- `wp_fractional_scale_v1` — manager global, per-surface
  preferred_scale fed from the surface's output; output scale
  is a real wm::Output field applied by the output-management
  path — verified via vp_proto_test (scale 120 event)
- `zwp_pointer_gestures_v1` — swipe/pinch gesture objects per
  client, begin/update/end/cancel streams routed to the
  pointer-focus client with serial/time/finger semantics —
  verified by ptr_proto_test (complete gesture streams during
  the nested sweep)
- `zwp_keyboard_shortcuts_inhibit_v1` — per-surface/seat
  inhibitors: active/inactive events on focus change, active
  inhibition forwards every key to the client and suspends
  compositor bindings (quit escape hatch stays) — verified by
  inhibit_proto_test (the C-A-d binding combo reached the
  client's wl_keyboard instead of triggering the binding)
- `zwp_text_input_v3` + `zwp_input_method_v2` — the IME pair:
  text inputs enter immediately when created on an already-
  focused surface (focus transitions alone would leave them
  dangling), enter/leave never cross client boundaries,
  enable+commit activates the IM with surrounding/cause/
  content-type/cursor-rect state and done serials; commit_
  string/preedit/delete apply atomically per IM commit,
  keyboard grab keymap+key/modifier routing, popup surfaces
  anchor at the field's cursor rect in the scene — verified
  end-to-end by ti_proto_test (two connections: IM activate,
  surrounding text, commit_string back, deactivate)
- `wl_touch` — slot-tracked touch points: down hit-tests and
  grabs the surface for the slot's lifetime (implicit-grab
  semantics like the pointer), motion/up/cancel/frame batches
  per client, seat capability advertised, dead-surface slots
  pruned, slot re-use sends an implicit up first — verified
  by ptr_proto_test (down/motion/up/frame batches during the
  sweep)
- Touch-armed grabs — an xdg_toplevel.move/resize request
  quoting a wl_touch.down serial used to arm a pointer grab
  that could never release (no button release ever arrives —
  frozen pointer events, ghost drags). Serial→slot tracking
  now arms touch grabs that release on the finger's up; a
  touch serial whose slot already ended arms nothing at all;
  touch_motion drives the grab like pointer grab_motion does
- `zxdg_decoration_manager_v1` + KDE `server_decoration` —
  CSD/SSD negotiation with duplicate-object and lifetime
  enforcement, and a real SSD renderer behind it: titlebar,
  borders and a close button are compositor-drawn scene items
  (`ssd.rs`), titlebar drags start a move grab, border zones
  start resize grabs, the close button sends
  `xdg_toplevel.close()`. Focus state recolors the titlebar;
  popups/layers/IME surfaces still paint above every frame and
  keep their clicks. Negotiated per client — `Auto`/`Server`
  configure SSD, `Client` keeps CSD; the `decoration =` config
  key steers the default
- `zxdg_foreign_v2` — handle export/import, `set_parent_of`
  cross-client transient-for, importer `destroyed` on parent
  death, restack keeps children above parents; ordinary
  xdg_toplevel.set_parent now feeds the same transient_for
  field and no longer drops pre-map requests
- `wp_single_pixel_buffer_v1` — solid 1×1 buffers backed by a
  real 4-byte anonymous mmap, so attach/commit/blit/upload
  treat them as ordinary buffers
- `wp_alpha_modifier_v1` — double-buffered multiplier
  (u32::MAX = opaque) committed onto SurfaceData; CPU blit
  scales source alpha, Vulkan pushes it to the fragment stage
  at push-constant offset 32
- `wp_content_type_v1` — double-buffered content hints on
  SurfaceData for later renderer/output policy
- `wp_tearing_control_v1` — double-buffered async hint;
  tearing_active feeds PageFlipFlags::ASYNC on the DRM path
  with an automatic fallback to vsync flips on EINVAL
- `zwlr_output_power_manager_v1` — per-connector power state
  in CompState.power; the DRM frame path blanks its CRTC via
  set_crtc(None) and skips compositing, the nested backend
  stops presenting; mode events confirm state
- `xdg_dialog_v1` — set_modal/unset_modal on SurfaceData;
  modal_descendant redirects focus from ancestors to the
  topmost modal transient
- `xdg_system_bell_v1` — ring() writes BEL to the session's
  controlling tty
- `zwp_tablet_manager_v2` — manager → per-seat tablet_seat
  objects; tablets announce name/vid/pid/path on libinput
  hotplug; tools announce type/serial/capabilities on first
  proximity with proximity/motion/pressure/distance/tilt/
  rotation/slider/wheel/tip/button/frame streams routed to
  the surface-owning client; pads announce buttons plus
  ring/strip children in mode groups
- Presentation path overhaul — same-fb page flips removed
  (~27 ms per frame on virtio-gpu), dirtyfb clipped to real
  damage, readback clipped to queued-damage coverage with
  per-parity sequence tracking, render/present split with a
  mid-tick input drain (pointer latency 33 → 18 ms under a
  35-commit/s storm), wl_shm geometry validated at
  create_buffer, pool shrink rejected, screencopy/cursor/vulkan
  upload paths bounds-checked

## Known issues

- **SSD frame grab zones are the drawn 1px borders only** —
  `GRIP` (the documented 6px invisible resize-grip) is used
  solely for corner detection inside the decorated extent; the
  outer margin it describes is never applied, so edge/corner
  grabs on framed windows are nearly unhittable. An invisible
  grip band outside the visible border is owed.
- **No hover cursor-shape feedback on SSD frames** —
  `ssd_pick` only runs on button press, so the pointer never
  signals resizability over a compositor frame; a motion-time
  zone check + cursor-shape switch is owed.
- **Fullscreen-animation flicker — fix landed, verification
  pending**: the Vulkan parity retirement picked the
  last-iterated slot index as "newest", which is not the
  newest submission once `cur` wraps — presenting the older of
  two retirees (stale frame visible under continuous damage)
  and regressing `presentable`. Both scans now pick max
  cover_seq. Needs a real-session soak test (fullscreen video
  on one output); if it still flickers, next step is a
  frame-dump / parity-choice trace.
- **Session activation environment can go stale** — clients
  started by D-Bus activation (GApplication pattern) can see
  the previous session's environment and silently never map
  ("nothing launches", only a reboot cleared it);
  `dbus-update-activation-environment` is not called in the
  session wrapper path yet.
- **GLES backend is a stub** — `GlesBackend::new` bails, so
  the renderer cascade is Vulkan → CPU in practice.
- **`eglSetDamageRegion` spam** — client/driver-side
  (egl-wayland2/egl-x11 lack `EGL_KHR_partial_update`;
  function is a documented no-op on NVIDIA). Not Odin's —
  recorded here so it stops being re-diagnosed.
- **Popups land at the raw anchor position** —
  `xdg_positioner` stores size/anchor_rect/anchor/gravity/
  offset but `set_constraint_adjustment`, `set_reactive` and
  `set_anchor_point` fall through unhandled, so no
  flip/slide/resize solving exists. A menu anchored near an
  output edge opens at the unadjusted rect — reported as
  "menus open at the wrong spot". Constraint solving is owed.
- **Window-control hit zones offset on some clients** —
  on some applications the clickable area for
  minimize/maximize/close sits visibly off the drawn button
  (pointer must hover beside the X). Consistent with the CSD
  margin family: grips live inside the invisible buffer
  margin and some toolkits use asymmetric margins — but a
  coordinate-mapping mismatch in the local-space event path
  has not been excluded yet; needs a per-client audit
  (which toolkit, which margins) before it's called
  "expected".
- **`wp_cursor_shape` not reliably applied** — the device
  and `set_shape` dispatch exist, yet shapes clients set
  (e.g. I-beam over text fields) don't always reach the
  screen. The stored-shape → shown-cursor chain (focus gate,
  hwcursor fallback, shape enum mapping) needs auditing.
- **Screen capture silently skips every dmabuf client** —
  root cause of "browser/file manager missing from
  screenshots while GIMP and terminals capture fine".
  `draw_at` (the CPU composite behind all shm captures)
  needs `shown_buf.data::<BufInfo>()`, which only exists on
  `wl_shm` buffers — a dmabuf surface returns `false` and is
  left out with no fallback. `cap_src_rect` has the same
  dependency: a toplevel on a dmabuf resolves to `bw=0` →
  `None` → the capture session reports `stopped`. So both
  paths fail GPU clients identically: per-output captures
  draw everything except dmabuf windows, per-toplevel
  captures refuse them outright. dmabuf = every GPU-
  accelerated client, Wayland-native as much as XWayland/
  glamor — verified with a GTK4 file manager connecting
  natively (GDK_BACKEND=wayland): its GSK GL/Vulkan renderer
  submits dmabuf, so it stays invisible while shm-only GTK3
  apps capture fine. The dump-frame path is unaffected
  because it reads the already-rendered frame, proving the
  pixels exist — the capture path just never asks the GPU
  for them. Fixes owed: (a) resolve toplevel capture rects
  from `pos`+`win_geom` (buffer-type independent), (b) serve
  output captures from the composited frame/readback like
  dump-frame does instead of re-compositing on CPU — which
  also removes the CPU-bound cost below, (c) GPU readback
  per toplevel for the ext-image-copy-capture path.
- **Screen capture is CPU-bound** — the shm blit composites
  the full scene per frame; tools issuing per-toplevel or
  per-output captures multiply that cost (observed as
  "screenshots take noticeably long"). Serving output
  captures from the rendered frame (previous item) or the
  dmabuf/GPU capture path in the roadmap is the fix.
- **CSD shadow margins are expected, not a bug** — resize
  grips living in a client's invisible buffer margin look
  "offset" on thick-shadow sides (typically bottom/right on
  GTK); the hit-test correctly covers the whole buffer.

## Next

- Validate the Vulkan path inside a real session end-to-end
- Full pointer/keyboard focus audit against real applications
- Automated test coverage beyond the Vulkan selftest
- XWayland: XTEST via libei

## Renderer — mid-term

- Zero-copy scanout via DMABUF import and format modifiers
  (extension probing already implemented; client-buffer
  direct scanout landed — see top section)
- Hardware cursor planes
- OpenGL ES fallback backend
- `linux-dmabuf` multi-plane formats (v4 feedback + single-plane
  create landed: format table, `main_device` dev_t, scanout
  tranche — XWayland glamor depends on it)

## Protocols and desktop

- DMABUF screencopy path (SHM capture exists)
- Protocol gaps for full coverage (audit 2025): adaptive
  sync/VRR is refused in zwlr_output_manager validation —
  no VRR path exists at all, notable for gaming loads;
  wp_fifo_v1 + wp_commit_timing_v1 (frame pacing, newer
  clients request it); ext_transient_seat_v1 (nested/VM
  sessions); wp_color_manager_v1 (HDR — roadmap tier);
  drm_lease (VR headsets); wp_security_context_v1 (sandbox
  metadata); ext_pointer_warp (niche)
- Output-management follow-ups: real DRM re-modeset on apply
  (the layout records the mode; the atomic modeset lands in
  the backend), hotplug head/finished events, live primary
  selection
- D-Bus settings interface
- ~~Optional SSD renderer for CSD-incapable clients~~ —
  landed: `ssd.rs` frame renderer + decoration negotiation
  pick SSD for `Auto`/`Server`

## Open decisions

- ~~Workspace model semantics (per-output vs. global)~~ —
  decided: i3 model, per-output (implemented)
- [ ] Interactive grabs: XReq::Configure dedup'd per window per tick (raw 1kHz motion flooded the X connection -> Xwayland saturates -> ALL X11 video freezes during drag); native resize configures now coalesced via rs_pending -> one xdg configure per frame tick in flush_motion (the pacing a client can keep up with)
- [ ] Hit-test reverted to input-region/buffer bounds: window_geometry does NOT bound input per spec (CSD shadow margins carry the resize grips; the earlier geometry-strict test removed them entirely)
- [ ] Resize grab start rect now uses window GEOMETRY w/h (was buffer size incl. shadow margins -> configure sizes were off by the margin); matches spec (configure size = geometry space — resizes happen in logical space)
- [ ] grab_end() sends the final non-Resizing configure on grab release (pointer button, touch up, touch cancel) with the coalesced target size — clients previously stayed in Resizing state after release
- [ ] zwp_linux_dmabuf_v1 now advertises the GPU-ENUMERATED sampled modifier set (VkDrmFormatModifierPropertiesListEXT, B8G8R8A8 + SAMPLED_IMAGE) instead of LINEAR-only — measured: XWayland spent ~75% CPU in libpixman during window drags because its buffers fell back to shm/software. Early VkContext::probe_modifiers() runs in main before the Xwayland spawn; validate_and_map accepts the advertised set, tiled buffers skip mmap and go straight to explicit-modifier Vulkan import (import.rs uses dma.modifier now)
- [ ] xwm ConfigureRequest ack now echoes only MASKED fields — an unset x/y in the request carried 0 and the unmasked echo teleported the X window to (0,0) via its own ConfigureNotify (window "vanishes"/jumps to the corner)
- [ ] Loop stall forensics: present stats now report pump_ms (client dispatch), xwm_ms (X event drain + request apply), drain_ms (input callback) alongside input_lag_ms — names the saturating callback during a drag freeze
- [ ] dmabuf import failures log a warning (modifier/size/err) — a broken import path blanks tiled surfaces instead of failing silently; grab_motion warns when a dragged window leaves all outputs
- [ ] xdg_toplevel set_min_size/set_max_size now stored on SurfaceData and enforced by the interactive resize grab (was silently ignored); the fixed corner stays anchored when a min/max clamp moves the origin on left/top edges
- [ ] Nested selftest infrastructure: a headless Wayland host + ODIN_BACKEND=nested runs the full pipeline incl. its own XWayland with zero relogs; test-client shm pools fixed to O_RDWR fds (write-only fds failed server-side mmap → protocol death → all RESULTs were invalid). Selftest now green: relative pointer, confine, swipe, pinch, touch, keyboard-inhibit, foreign-toplevel, workspaces, dnd
- [ ] xdotool/wmctrl inside nested XWayland verified: masked ConfigureRequest resize preserves window position (teleport regression test)
- [ ] syncobj timeline imports now mint handles on Odin's own DRM render node (comp.drm_fd, lazy fallback on nested) — NVIDIA rejects fd_to_handle on the client's exported syncobj fd, which protocol-killed XWayland and every GTK4 client on first import; glxgears-in-nested-Xwayland verified green
- [ ] Vulkan texture-cache eviction gate was dead code: it sat after queue_submit+in_flight[cur]=true, so `!in_flight.any()` never held — imported dmabuf textures accumulated forever (VRAM leak matching the hard-freeze signature). Gate now exempts `cur` — its fresh submission can only sample `live` textures — and opens whenever the older parities retire
- [ ] libinput scroll axes now guarded with has_axis before scroll_value/scroll_value_v120 — unguarded reads logged "client bug: value requested for unset axis" per wheel tick (3899 lines in 4 days of stderr.log)
- [ ] Connector naming now uses interface_id (per-type index → "DP-3") instead of the DRM object id ("DP-107"), and EDID serial prefers the 0xFF string descriptor over the numeric field — the convention clients expect. Output-management clients now see the same names/serials they see elsewhere, so display-profile matching and any persisted primary-monitor setting can no longer be poisoned cross-session

## Working plan

The compositor-parity audit lives in `PLAN.md` / `PLAN_de.md` —
what every standard compositor does, Odin's status per item,
and the verification order (nested selftest → vptr injection →
xdotool in nested XWayland → instrumented stats → real session
last).
- [ ] wl_subcompositor/wl_subsurface — full core-protocol subsurfaces: sync-mode commit caching (commits hold until the parent's commit applies, spec default), desync, double-buffered set_position/place_above/place_below against a Root-marker z-stack, scene composition + input hit-testing integrated, verified live with a subsurface test client (nested EGL triangle + shm children composite correctly)
- [ ] Protocol coverage audit — wayland-info diff drove: wl_compositor v6, wl_seat v9, wl_shm v2 bumps, zxdg_exporter/importer v1 pair on the shared handle table, dmabuf v4 feedback (main_device/tranches — fixes XWayland glamor: real-session log showed "GBM Wayland interfaces not available" → full software fallback, the drag-stall root cause); then landed: ext_data_control, ext_foreign_toplevel_list, ext_image_copy_capture (+ source managers), wp_linux_drm_syncobj + implicit dmabuf-fence gating, xdg_toplevel_icon, xdg_wm_base v6 (configure_bounds/wm_capabilities/suspended/set_minimized)
- [ ] pointer.rs (524 lines) and render.rs exceed the 400-line rule — split hit-testing/scene-assembly out (structural debt, pre-existing)
- [ ] X11 ConfigureNotify now syncs xwin geometry + sd.pos — override-redirect windows (menus, Firefox doorhangers) self-configure after map; without it they rendered stuck at (0,0). Live X-event trace confirmed: doorhanger created at (0,0,1x1), configured to (359,76,415x164), position previously lost
- [ ] Override-redirect clicks no longer take keyboard focus — hit_window→set_kbd_focus fired SetInputFocus on the popup, the resulting FocusOut at the owning window rolled Firefox doorhangers closed on first click. OR popups now receive the press without focus/raise
- [ ] Output stack rebuilt to hardware truth: EDID base-block parser (make/model/serial from the connector's EDID blob property), real phys_mm + real refresh everywhere (wl_output.geometry/mode, head.physical_size) — the 275×155mm/60Hz/“odin” placeholders are gone
- [ ] Full mode lists advertised: every connector mode becomes a wl_output mode event + zwlr_output_head_v1 mode object so wlr-randr/wdisplays/kanshi see real choices
- [ ] zwlr_output_manager_v1 now reaches hardware: strict validate (mode must be advertised, transform=Normal only, adaptive_sync refused) → honest `failed` instead of silent ignore; commit queues OutOps the event loop executes — real re-modeset (buffers torn down, backend rebuilt, crtc re-armed), viewport origin move, immediate CRTC blank on disable
- [ ] Monitor hotplug: udev drm-subsystem monitor in the calloop → hotplug::rescan diffs connectors vs layout — add (right of bounding box, Output::new, wl_output global, head announce), remove (CRTC blank, drop, global withdraw, head.finished), metadata refresh in place; surviving outputs keep their positions
