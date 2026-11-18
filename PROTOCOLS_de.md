# Wayland-Protokoll-Unterstützung — Odin

[English version](PROTOCOLS.md)

Jedes Protokoll unten ist im Baum implementiert — kein wlroots,
kein Smithay. Die Notizen beschreiben Verhalten jenseits des
nackten Globals: Lebenszyklen, Randfälle und wie die
Implementierung verifiziert wurde. Der Status ist ehrlich:
Als *verifiziert* markierte Einträge liefen gegen echte
Client-Programme im verschachtelten Selbsttest oder in einer
Live-Session.

## Core-Shell

- **xdg-shell (xdg_wm_base v6)** — Toplevels mit State
  (aktiviert/maximiert/Vollbild), `xdg_popup`-Menüs via
  `xdg_positioner`, korrekter SHM-Buffer-Lebenszyklus
  (`wl_buffer.release`, reine Damage-Commits behalten den
  gezeigten Buffer); `configure_bounds` meldet das nutzbare
  Output-Rechteck des Fensters, `wm_capabilities` die ehrliche
  Liste (maximieren/Vollbild/minimieren — kein Fenstermenü),
  der `suspended`-State markiert unsichtbare/minimierte
  Toplevels und `set_minimized` steuert den echten
  Iconify-Pfad
- **`wlr-layer-shell` v4** — Panels, Hintergründe, Launcher,
  Benachrichtigungen: Anker, Ränder, Exclusive-Zonen und
  -Kanten, Tastatur-Interaktivität, Configure/Ack-Handshake,
  alle vier Bänder in Hit-Test und Compositing; Exclusive-
  Zonen verkleinern die Tiling-Fläche. Verifiziert mit einem
  echten Layer-Shell-Client
- **`zxdg_decoration_manager_v1`** + KDE `server_decoration` —
  CSD/SSD-Verhandlung plus echter SSD-Renderer: Titelleiste,
  Ränder, Close-Button, Move-Grab an der Titelleiste und
  Resize-Grabs an den Rändern (`decoration =` in der Config
  wählt den Standardmodus)
- **`xdg_dialog_v1`** + **`xdg_system_bell_v1`** — modale
  Dialoge blockieren den Fokus des Vorfahren; Bells laufen auf
  das Session-TTY
- **`xdg_toplevel_icon_manager_v1`** — Fenster-Icons: benannte
  Icons und SHM-Pixel-Snapshots (quadratisch, pro-Scale-
  Buffer), beim Surface-Commit double-buffered zugewiesen
- **`zxdg_foreign_v2`** — exportierte/importierte Toplevel-
  Handles, transient-for über Client-Grenzen; Kinder stapeln
  über ihren Eltern
- **`xdg_activation_v1`** — Launch-Aktivierungs-Tokens;
  `activate`-Requests laufen durch den echten Fokus-Pfad

## Seat, Eingabe, Zwischenablage

- **Seat-Input** — Pointer Motion/Buttons/Axis mit
  `wl_pointer.frame`, Click-to-Focus, Move/Resize-Grabs;
  Tastatur mit Keymap, Live-Modifikatoren und Repeat-Info
  (versionsgestuft für ältere Client-Binds)
- **`zwp_relative_pointer_v1`** — rohe, unbeschleunigte Deltas
  für Spiele neben dem beschleunigten Paar
- **`zwp_pointer_constraints_v1`** — Pointer-Lock und Confine
  mit echten `wl_region`-Rechtecken, Oneshot-/Persistent-
  Lebenszyklen, Cursor-Positions-Hints beim Unlock
- **`wp_cursor_shape_v1`** — eigener XCursor-Theme-Loader,
  hotspot-korrekte thematisierte Cursor, plus Client-
  `set_cursor`-Surfaces und ein Compositor-Fallback-Pointer
- **`zwp_pointer_gestures_v1`** — Touchpad-Swipe/Pinch-Streams
  an den Pointer-Fokus-Client
- **`zwp_keyboard_shortcuts_inhibit_v1`** — Spiele und VMs
  greifen jede Taste; Compositor-Bindings pausieren, solange
  es aktiv ist
- **`zwp_tablet_manager_v2`** — Tablet-Seats, Tablets, Tools
  (Proximity/Motion/Pressure/Tilt/Tip/Buttons) und Pads
  (Buttons/Rings/Strips in Mode-Gruppen) aus libinput
- **`wl_touch`** — slot-getracktes Multi-Touch mit
  Implicit-Grab-Semantik; touch-initiierte Move/Resize-Grabs
  lösen beim Finger-Lift
- **Zwischenablage** — `wl_data_device`-Selection, Copy/Paste
  zwischen Wayland-Clients
- **Drag & Drop** — volles `wl_data_device`-v3-Drag-Routing:
  Implicit-Grab-Semantik, Enter/Motion/Leave-Retargeting,
  Mime- + Action-Verhandlung, Drop→Receive→Finish-Pipeline
  und ein Drag-Icon, das am Cursor hängt — End-to-End durch
  einen skriptgesteuerten Selbsttest-Client verifiziert
- **`zwlr_data_control_manager_v1`** — Clipboard-Manager-
  Zugriff (mit wl-copy/wl-paste verifiziert), teilt eine
  Zwischenablage mit wl_data_device und der X11-Bridge
- **`ext_data_control_manager_v1`** — das stabilisierte
  Data-Control-Protokoll; dieselbe Clipboard-Oberfläche wie
  zwlr_data_control für moderne GTK4-/KDE-Clients
- **`zwp_primary_selection_device_manager_v1`** — Primary
  Selection (Middle-Click-Paste): Sources, Offers, Sends —
  mit wl-copy --primary / wl-paste --primary verifiziert
- **`zwp_text_input_v3`** + **`zwp_input_method_v2`** — das
  IME-Paar: Feld-State an die Input-Methode, Preedit/Commit-
  Strings zurück, Keyboard-Grabs und Popup-Surfaces
- **`zwlr_virtual_pointer_manager_v1`** /
  **`zwp_virtual_keyboard_manager_v1`** — virtuelle
  Input-Injektion durch die echten Input-Pfade
- **`zwlr_input_inhibit_manager_v1`** — Input-Inhibition
  eines einzelnen Clients für Lock-Screens

## Buffer, Surfaces, Präsentation

- **`zwp_linux_dmabuf_v1` v3** — GPU-teilbare Buffer, in den
  Blit-Pfad gemmapt (LINEAR, XRGB/ARGB8888, Single-Plane)
- **`wp_linux_drm_syncobj_v1`** — explizite Synchronisation
  für dmabuf-Clients: Acquire-Points halten ein Attach, bis
  sein Fence signalisiert (nicht-blockierend am Frame-Tick
  gepollt), Release-Points signalisieren zwei präsentierte
  Frames später — über dieselbe Staging-Strecke wie
  wl_buffer.release. Implizite dmabuf-Fences parken das
  Attach identisch — kein Sample-Early-Flackern. Timeline-
  Imports prägen ihre Handles auf Odins eigenem DRM-Render-
  Node — Treiber lehnen `fd_to_handle` auf dem exportierten
  Client-Syncobj-fd ab, was XWayland- und GTK4-Clients beim
  Import tötete
- **`wp_viewporter`** — clientgesteuertes Surface-Crop/Scale,
  per Screencopy pixelverifiziert
- **`wp_fractional_scale_v1`** — pro-Surface Preferred-Scale,
  gespeist vom Output der Surface
- **`wp_single_pixel_buffer_v1`** — einfarbige 1×1-Buffer als
  echte anonyme Mmaps durch den normalen Buffer-Pfad
- **`wp_alpha_modifier_v1`** — pro-Surface Alpha-Multiplikator
  in beiden Compositing-Pfaden (CPU-Blit, Vulkan-Push-
  Konstante)
- **`wp_content_type_v1`** — Content-Hints (none/photo/video/
  game) auf SurfaceData für Renderer-/Output-Policy
- **`wp_tearing_control_v1`** — Async-Präsentations-Hint →
  ASYNC_PAGE_FLIP auf dem DRM-Pfad mit Treiber-Fallback
- **`wp_presentation`** — pro-Surface Present-Feedback mit
  echten Zeitstempeln, Refresh-Intervall und Sequenznummern
- **`zwp_idle_inhibit_manager_v1`** — pro-Surface Idle-
  Inhibition
- **`ext_idle_notifier_v1`** — Idle/Resumed-Benachrichtigungen
  mit Per-Seat-Timeouts; speist die künftige
  DPMS/Lock-Pipeline

## Outputs, Capture, Session

- **`zxdg_output_manager_v1`** — logischer Output-Name/
  Position/Größe für Capture-Tools
- **`zwlr_output_manager_v1`** — Live-Monitor-Konfiguration
  für wlr-randr/wdisplays/kanshi: ein `wl_output`-Global und
  ein Head-Objekt pro Output, serial-gesteuerte
  Konfigurations-Transaktionen, Enable/Disable/Position/Mode
  live angewendet — mit wlr-randr verifiziert
- **`zwlr_output_power_manager_v1`** — echtes DPMS: Legacy-
  KMS-CRTC-Blank auf dem DRM-Backend, Present-Stop auf
  verschachtelt
- **`zwlr_gamma_control_manager_v1`** — pro-Output Gamma-
  Ramps beim Present wie eine CRTC-LUT; Captures bleiben
  Pre-Gamma
- **`zwlr_screencopy_manager_v1`** — Output-Capture in
  Client-SHM-Buffer mit Damage-/Frame-Events. Mit grim
  verifiziert, das ein korrektes 1280×800-PNG der
  komponierten Szene erzeugte
- **`ext_image_copy_capture_v1`** +
  **`ext_image_capture_source_v1`** — der stabilisierte
  Screen-Capture-Nachfolger, den neuere Portale/OBS nutzen:
  Output- und Toplevel-Sources, Session-/Frame-Objekte,
  SHM-Blit in den Client-Pool, ehrliches `stopped`, wenn
  eine Source stirbt
- **`ext_session_lock_v1`** — Session-Locking: Lock-Surfaces
  werden pro Output konfiguriert und exklusiv gerendert (per
  Screencopy-Pixel-Capture verifiziert), Input auf den Lock-
  Client beschränkt, Unlock-/Re-Lock-Zyklen End-to-End
  verifiziert
- **`zwlr_foreign_toplevel_management_v1`** — Taskbars und
  Fensterlisten: Per-Toplevel-Handles mit Title/App_Id/State,
  Replay für spät bindende Manager und funktionierende
  Fenster-Ops (aktivieren, schließen, maximieren, Vollbild,
  echtes Minimieren, das das Fenster aus dem Rendern nimmt,
  bis es zurückkehrt). Von einem automatisierten Client im
  verschachtelten Selbsttest verifiziert
- **`ext_foreign_toplevel_list_v1`** — schreibgeschützte
  Toplevel-Liste: Title, App_Id, Identifier, State und
  Output-Zuordnung, gespiegelt am Lebenszyklus der
  Management-Handles
- **`ext_workspace_manager_v1`** — Workspaces für Taskbars
  und Docks: eine Gruppe pro aktivem Output (output_enter auf
  dem eigenen wl_output des Clients), neun Workspaces mit
  Id/Name/Koordinaten/Capabilities/State, Activate + Assign
  gebündelt per `commit`, Live-Diffs nach compositorseitigen
  Wechseln — mit ws_proto_test verifiziert
