# TODO — Odin

[English version](README.md) · [English TODO](TODO.md)

## Erledigt

- `dump-frame` friert die Session nicht mehr ein — der Dump
  las ~9 MB pixelweise aus einem write-combined gemappten
  Dumb-BO und schrieb die PPM synchron mitten in `present()`:
  ~8 s Event-Loop-Blockade pro Output (~16 s toter Input bei
  zwei Monitoren). Der Frame wird jetzt einmal als Bulk in
  gecachtes RAM kopiert; BGRA→PPM und Datei-Write laufen auf
  einem Worker-Thread — der Stall schrumpft auf ein memcpy.
- `zwlr_screencopy_v1` erfasst den ANGEFRAGTEN Output —
  `CaptureOutput`/`CaptureOutputRegion` ignorierten das
  `output`-Argument und captureten immer `primary_rect()`:
  Tools, die alle Monitore durchgehen, bekamen N-mal den
  primären Bildschirm. Beide Requests lösen den Connector-
  Namen der gebundenen wl_output jetzt auf das Rect des
  jeweiligen Outputs auf (ext-image-copy-capture machte das
  schon richtig).
- `dump-frame` erfasst jetzt alle Outputs — das Binding setzte
  bisher ein Bool, das der erste presentierende Output
  verbrauchte: In einer Multi-Monitor-Session wurde genau ein
  Bildschirm gedumppt, und welcher hing von der Present-
  Reihenfolge ab. Das Flag ist jetzt eine `dump_seq`-Generation
  in `CompState`; jeder `Output` merkt sich `dump_done` und
  schreibt pro Generation einmal seine eigene
  `dump-<x>-<y>.ppm` beim nächsten Present — alle Monitore
  landen im State-Dir, auch ein inaktiver Output, der erst
  später presentiert.
- Vulkan-Parity-Retirement wählt jetzt nach cover_seq statt
  Slot-Index — `take_retired`/der Render-Zeit-Retire-Scan
  iterierte die Parities 0..3 und hielt den zuletzt
  signalisierten Index für den "neuesten". Sobald `cur`
  umwickelt, ist Index-Reihenfolge aber nicht mehr
  Submission-Reihenfolge: der ältere von zwei Retirees wurde
  als aktuellster Frame präsentiert (sichtbares Flackern bei
  kontinuierlichem Damage, z.B. Fullscreen-Video) und
  `presentable` konnte regressieren, was die Settle-Damage-
  Queue durcheinanderbrachte. Beide Scans wählen jetzt den
  max-cover_seq-Retiree und heben die Schranke nur an.
- Vulkan-Mid-Flight-Destruction gefixt — importierte dmabuf-
  Texturen bei Buffer-ID-Reuse und shm-Texturen bei Commit
  wurden sofort zerstört/re-poolt, obwohl der Descriptor-Set
  einer vorherigen Submission sie noch sampeln konnte
  (GPU-Use-After-Free → VK_ERROR_DEVICE_LOST unter NVIDIA,
  dann zwangsweise CPU-Renderer-Demotion). Beide laufen jetzt
  über `tex_retire`/`imp_retire`-Listen, die der bestehende
  Alle-Parities-retired-Sweep drained.
- `xdg_toplevel_icon_v1`: `immutable`-Error nur bei mutierenden
  Requests (`set_name`, `add_buffer`) — `destroy` muss immer
  durchgehen, da Clients das Icon-Objekt direkt nach `set_icon`
  abräumen (Spec: das Icon bleibt gesetzt). Ein Error auf
  `destroy` tötete Electron-Apps (Discord/devin-desktop) mit
  "error 2: icon is assigned to a toplevel" mitten im
  Fenster-Setup.
- Bounds-Guard im CPU-Composite `fill_rect` — Clip-Rects werden
  gegen Surface-Grenzen geschnitten, nie gegen den Output; ein
  Off-Screen-Rect (x0 ≥ x1) slicete `frame[start..end]`
  rückwärts und panickte den Compositor — getroffen bei
  GPU-Device-Loss-Demotion während ein 1025×882-Electron-
  Fenster neu zeichnete.
- Geordnetes Session-Teardown — nach dem Event-Loop-Stop beendet
  `backend::teardown::shutdown` das Xwayland-Kind (SIGTERM→SIGKILL-
  Grace; `Child::drop` killt allein nie), schließt Listener+Display
  (echte Disconnects statt toter Sockets), gibt den Seat via
  `Seat::disable` frei und armiert einen 10-s-Watchdog (`_exit`)
  um die Drop-Kaskade — ein hängendes `vkDeviceWaitIdle` kann den
  Session-Leader nicht mehr ~50 min blockieren, während der
  Display-Manager wartet (beobachteter Hang).
- `new_id`-Requests initialisieren ihr Objekt auf jedem Pfad —
  Error/`failed`/`stopped`-Antworten hinterlassen jetzt ein
  gültiges serverseitiges Objekt (Audit über dmabuf/shm/decoration/
  syncobj/session_lock/fractional-scale/alpha/content-type/
  tearing/input-inhibit/copy-capture/spb). Vorher übersprang ein
  früher Error-Return das `init`, und ein späterer Request auf
  diese ID panickte im Backend — der prozessweite Abort, der die
  Session bei einem Electron-`create_immed` tötete.
- Client-Identitäts-Logging — Accepts loggen `pid`+`comm` via
  `SO_PEERCRED`; Disconnects loggen pid/comm/`DisconnectReason`
  und unterscheiden clientseitige Exits (`ConnectionClosed`) von
  Compositor-Protocol-Kills (`ProtocolError`).
- Pointer-Motion-Koaleszierung — rohe libinput-Events setzen ein
  Pending-Flag; `flush_motion` liefert ein `wl_pointer.motion`
  (plus ein akkumuliertes `relative_motion`) pro Frame-Tick,
  geflusht vor geordneten Button-/Axis-Events. Ein 1-kHz-Gerät
  flutet Clients nicht mehr in Repaint-Staus (Eingabe-Hänger beim
  Hover über Menüs).
- Damage-geclippte Textur-Uploads nur bei Same-Buffer-Commits
  (reine Damage-Commits) — laut Spec sind `damage` und
  `damage_buffer` Diffs gegenüber dem vorherigen Frame, nie
  gegenüber dem eigenen Pufferinhalt, daher laden Attaches
  immer voll; pro-Buffer-Damage akkumuliert über Commits, die
  Frames überholen (>24 Rects kollabiert zu Voll-Upload).
- Offscreen-Surface-Culling — Surfaces außerhalb des Output-
  Rechts überspringen Upload+Draw, statt in jedem Output-
  Textur-Cache zu landen; Upload-Volumen und -Anzahl pro Frame
  loggen als `up_kb`/`up_texs` in den Present-Stats.
- Client-dmabuf → Vulkan-Import: `zwp_linux_dmabuf_v1`-Buffer
  (LINEAR, einplanar XR24/AR24) importieren als Sampled-Images
  via `VkImportMemoryFdInfoKHR` + explizitem Modifier — null
  memcpy, wo Clients GPU-Speicher liefern; XRGB-Views swizzlen
  Alpha auf 1. Fehlgeschlagene Importe markieren die Buffer-ID
  und zeichnen über den gemmapten Fallback. `up_big` in den
  Present-Stats nennt den größten Upload.
- Verzögertes `wl_buffer.release` — getauschte Buffer releasen
  zwei präsentierte Frames nach dem Tausch (pending→next→ready),
  nicht beim Attach; dmabuf-Clients dürfen einen freigegebenen
  Buffer sofort neu beschreiben, und ein frühes Release ließ sie
  über Pixel malen, die ein CRTC noch scannte (Flackern).
- Pro-Surface-SHM-Texturen — der Cache keyed auf wl_surface,
  nicht wl_buffer: Commit-Damage ist per Spec ein Frame-Diff,
  also darf jeder Commit (inkl. Attach) die persistente Textur
  patchen; Buffer-Wechsel kosten einen Rect-Patch statt eines
  Voll-Uploads. `set_window_geometry`-w/h begrenzt jetzt den
  Hit-Test — Schattenränder fressen keine Klicks mehr an den
  rechten/unteren Fensterkanten.
- Render-Paritäts-Kohärenz: Retire->Pick->Reset->Record->Submit
  einer einzigen Parität; Blocked-Maske = gescannt + flip-pending;
  ohne freie Parität wird der letzte pensionierte Frame erneut
  präsentiert. Zurückgesetzte Dead-Parities können die Queue nicht
  mehr in die CPU-Demotion wedgen.
- `zxdg_output_v1` sendet jetzt das abschließende
  `wl_output.done` nach dem Logical-Size-Burst (v3-
  Deprecation-Regel) — Xwayland berechnet seine RANDR-/
  Screen-Geometrie nur bei wl_output.done neu, dadurch blieb
  der X-Screen 0×0 und klemmte jedes X11-Pointer-Event am
  Ursprung fest (Steam, xev, XTEST komplett tot); End-to-end
  verifiziert, dass ButtonPress jetzt mit exakten Koordinaten
  in X-Fenstern ankommt
- `zxdg_output_v1` löst den angefragten wl_output über dessen
  Connector-Namen auf, statt immer den Primary-Rect zu melden
- Hardware-Cursor-Plane auf DRM — `set_cursor2`/`move_cursor`
  mit automatischem Software-Fallback pro Output; Pointer-
  Bewegung berührt die Render-Pipeline nicht mehr
- `wl_surface.set_input_region` in jedem Hit-Test-Pfad —
  XShape-Masken und transparente Overlays reichen Klicks durch
- `_NET_WM_MOVERESIZE` clientinitiiertes Move/Resize für
  X11-Titlebar-Drags (Steam)
- Vulkan-Texturcache invalidiert bei Buffer-Rewrite-Commits —
  Surfaces, die unter derselben wl_buffer-ID neu zeichnen,
  werden frisch hochgeladen
- CPU-Blitter iteriert direkt über Damage ∩ Surface ∩ Frame —
  keine Vollbild-Scans mit Pixel-Skip-Clipping mehr

- libseat-Session mit DRM/KMS-Multi-Output — disjunkte CRTCs,
  globaler Scene-Koordinatenraum, Compositing pro Output
- xdg-shell-Clients mit korrektem SHM-Buffer-Lebenszyklus
  (`wl_buffer.release`) und Frame-Callbacks
- Client-Pointer-Cursor mit Hotspot-Unterstützung plus
  Compositor-Fallback-Zeiger
- Layout-Policies pro Monitor — Floating (Standard) und
  Master/Stack-Tiling je Bildschirm, umgeschaltet auf dem Output
  unter dem Pointer; Fenster folgen der Policy des Monitors, auf
  dem ihr Zentrum landet (Drop auf Tiling faltet ins Grid, Drop
  auf Floating stellt die normale Größe wieder her), Fullscreen-
  Clients sind ausgenommen; `primary = <Connector>`-, `policy =`-
  (alle Monitore) und `policy.<name> =`-Config-Keys, Live-Reload
  der Config via inotify aufs Config-Verzeichnis, Master/Stack-
  Tile-Mathematik per Unit-Test abgesichert
- Konfigurierbare Tastenkombinationen über
  `~/.config/odin/bindings.conf`
- Damage-gesteuertes Frame-Scheduling — Frames nur bei
  Scene-Änderungen
- Direct-Scanout via DMABUF-Export: die drei Readback-Paritäten
  tragen DMA_BUF-Handles, werden zu DRM-Framebuffern und
  Presents page-flippen ganz ohne CPU-Kopien (CPU-Pfad bleibt
  als automatischer Fallback); CRTC-/Retired-Paritätendisziplin
  hält Schreibzugriffe vom gescannten Buffer fern
- **Client-Buffer-Direct-Scanout** — eine Fullscreen-Surface,
  die den ganzen Output bedeckt, einen dmabuf-Buffer trägt und
  in einer sonst leeren Szene steht, importiert ihr dmabuf
  direkt als DRM-Framebuffer (`direct.rs`): der eigene
  Client-Buffer wird ohne jede Kompositor-Berührung
  page-geflippt — kein Render, kein Upload, kein Readback.
  Szenen-Sicherheit schließt Popups, Layer-Surfaces,
  Drag-Icons, Software-Cursor und Screencopy aus; Fehler beim
  FB-Import oder Flip fallen auf den Composite-Pfad zurück.
  Das importierte FB hält eine Referenz auf den Client-Buffer,
  bis der CRTC ihn ausmustert — wl_buffer.release wird genauso
  zurückgehalten wie bei den Readback-Paritäten
- **Tearing-Control** — `wp_tearing_control_v1` Async-Toggle
  pro Surface honoriert: eine Tearing-erlaubte Surface
  schaltet den Page-Flip auf `PageFlipFlags::ASYNC`
  (treiberseitig erlaubt), damit Fullscreen-Games Frames ohne
  Vsync-Warte durchdrücken können
- Damage-geclippte Surface-Uploads — In-Place-wl_buffer-
  Rewrites patchen nur die Client-Damage-Rects in die
  gecachte Textur statt die ganze Surface neu hochzuladen
- Kooperative Render-Yields — Input-Drain + Hardware-Cursor-
  Moves laufen mitten in langen Uploads, Kopien und
  Fence-Waits und begrenzen die Pointer-Latenz auf ~8 ms
- Vulkan-Renderer End-to-End: Device-Kontext, Graphics-Pipeline,
  Surface-Textur-Uploads, Compositing, Readback — verifiziert
  durch den automatisierten `--vk-render-test` auf echter Hardware
- Renderer-Kaskade Vulkan → GLES → CPU pro Output
- Session-Integration: `startodin`-Wrapper (nimmt den
  Session-Shell-Befehl entgegen und reicht ihn als `-S`
  durch), Wayland-Session-Eintrag über `startodin`
- XWayland-Kern: asynchroner `-wm`-Socket-Handshake,
  Substructure-Redirect, `WM_S0`-Selection-Claim,
  Composite-Off-Screen-Redirect, `WL_SURFACE_ID`-Assoziation
  (clientbezogen, mit Auflösung des Channel-Racings), X-Geometrie
  für die Scene-Platzierung, echte X11-Clients verifiziert
  (`xeyes`, `xclock`), Click-to-Focus und Tastaturfokus-
  Zustellung an X-Surfaces, EWMH-Grundlagen (`_NET_SUPPORTED`,
  `_NET_SUPPORTING_WM_CHECK`, `_NET_ACTIVE_WINDOW` in beide
  Richtungen: Compositor-Fokus spiegelt nach X, X-Aktivierungs-
  requests fokussieren die Surface), ConfigureWindow-Rückmeldung
  bei Move-Grabs, X-seitiges Raise-Mirroring, Wayland→X-
  Zwischenablage-Bridging (CLIPBOARD-Claim, TARGETS, asynchrone
  Pipe-Transfers — verifiziert mit wl-copy und X11-
  SelectionRequest/convert_selection-Roundtrips),
  X→Wayland-Zwischenablage via XFIXES-SelectionNotify-Tracking
  (TARGETS-Negotiation, SelectionRequest-Pumpen in Paste-Pipe-FDs —
  End-to-End verifiziert mit wl-paste gegen einen echten X11-
  Clipboard-Besitzer), `_NET_WM_STATE`-Anwendung
  (Fullscreen/Maximiert auf Fensterzustand gemappt), X-Fenster
  in Tiling-Layouts
- Focus-on-Map (neu gemappte Toplevels bekommen Tastaturfokus —
  nötig für serial-abhängige Clients wie Clipboard-Tools),
  Protokoll-Versionsprüfung (`wl_keyboard.repeat_info` nur bei
  v4+-Binds), Display-Handle-Sicherheit in Request-Handlern
- Pointer-Protokoll-Erweiterungen: `zwp_relative_pointer_v1`
  (unbeschleunigte Deltas für Spiele), `zwp_pointer_constraints_v1`
  (Lock/Confine inkl. Regions, Lifetime, Cursor-Hint-Rückstellung),
  `wp_cursor_shape_v1` (Xcursor-Theme-Loading, hotspot-korrektes
  Compositing) — End-to-End verifiziert durch einen
  automatisierten Client im Nested-Selftest
- `wlr-layer-shell` v4 — Anker, Ränder, Exclusive-Zonen und
  -Kanten, Tastatur-Interaktivität, Configure/Ack-Platzierung,
  alle vier Ebenen in Hit-Test und Compositing-Reihenfolge,
  Exclusive-Zonen verkleinern die Tiling-Fläche — End-to-End
  verifiziert mit swaybg als bildschirmfüllendem Hintergrund
- Surface-Rollen-Gate: nur xdg-toplevel-/X-Fenster-Surfaces
  kommen in die Stacking-Order — Aux-Surfaces (Pointer-Cursor,
  Drag-Icons) mit Buffern bekommen keine Kacheln und keinen
  Tastaturfokus mehr (das Enter ließ Clients wie
  echte Clients segfaulten)
- Reine Damage-Commits behalten den gezeigten Buffer
  (Tri-State-Pending-Attach) — nötig für Clients wie swaybg
- Session-Autostart — `~/.config/odin/autostart` (ein Befehl
  pro Zeile) und das `-S`/`--session`-Flag starten
  Session-Komponenten auf dem Compositor-Socket; im
  Nested-Modus verifiziert und im Skript-Selbsttest korrekt
  übersprungen
- Ein `wl_output`-Global pro Layout-Output — Clients sehen
  jeden physischen Monitor als eigenes Objekt mit echter
  Position/Modus (vorher versteckte ein einziges Global alles
  außer dem Primary)
- `zwlr_output_manager_v1` v4 — Heads mit Name/Beschreibung/
  phys. Größe/Modus/Position/Transform/Scale/Adaptive-Sync
  werden beim Binden replayed, Konfigurations-Transaktionen
  mit Serial-Gating (veraltet → cancelled), Enable/Disable/
  Position/Modus werden live ins Layout angewendet, die
  Validierung lehnt Konfigurationen ohne verbleibende Outputs
  ab — End-to-End verifiziert mit wlr-randr (Head-Listing,
  Live-Positions-Apply, --off am einzigen Output korrekt
  abgelehnt)
- `zwlr_screencopy_manager_v1` — Output-Capture in
  Client-SHM-Buffer inkl. Damage-/Frame-Events — End-to-End
  verifiziert mit grim, das ein 1280×800-PNG mit den
  Layer-Shell-Wallpaper-Pixeln erzeugt
- `zxdg_output_manager_v1` — logische Position/Größe/Name für
  den wl_output; Voraussetzung, bevor Capture-Tools den Output
  akzeptieren
- `zwlr_foreign_toplevel_management_v1` — ein Handle-Objekt pro
  gemapptem Toplevel mit Titel/App-ID/Output/State (maximiert,
  minimiert, aktiviert, Vollbild), Replay für Manager, die sich
  spät binden; alle Fenster-Ops implementiert: Aktivieren,
  Schließen, Maximieren, Vollbild und echtes Minimieren
  (ikonifizierte Fenster verschwinden aus Rendering + Hit-Test
  bis zur Wiederherstellung) — End-to-End verifiziert durch
  einen automatisierten Client im Nested-Selftest
  (Minimieren/Aktivieren/Schließen-Roundtrips)
- `zwlr_gamma_control_manager_v1` — Gamma-Rampen pro Output:
  wl_output-Auflösung zum Connector, 256er-Rampe annonciert,
  set_gamma-FD validiert + via pread gelesen (geteilter
  FD-Offset), pro Output als LUT beim Present wie ein CRTC —
  Screencopy bleibt pre-Gamma; Rampe stirbt mit ihrem Control —
  verifiziert durch block_proto_test
- `zwlr_input_inhibit_manager_v1` — Input-Inhibition für
  Lock-Screens: ein aktiver Inhibitor, erzwungen über den
  already_inhibited-Protokollfehler; Pointer-Hit-Test und
  Tastatur-Routing dunkeln für alle anderen Clients ab,
  Freigabe bei Destroy oder Client-Tod — verifiziert durch
  block_proto_test inkl. Disconnect-Cleanup
- `zwlr_virtual_pointer_manager_v1` — virtuelle Pointer-
  Injektion (ydotool/wayvnc-Klasse): relative + absolute
  Bewegung auf den Compositor-Raum gemappt, Buttons und Axis
  über die echten Input-Pfade — verifiziert durch
  block_proto_test
- `zwp_virtual_keyboard_manager_v1` — virtuelle Tastatur-
  Injektion: Keymap-FD validiert, Tasten/Modifier über
  key_event wie Hardware-Input (Bindings, Repeat, Inhibitor-
  Gating greifen) — verifiziert durch block_proto_test
- `zwlr_data_control_manager_v1` v2 — Clipboard-Manager-Zugriff:
  Device/Source/Offer-Objekte, Selection + Primary-Selection-
  Beobachtung, einheitliches SelSrc-Modell — wl_data_device-
  und Data-Control-Owner teilen eine Zwischenablage inkl.
  X11-Brücke — verifiziert mit wl-copy/wl-paste-Round-Trips
- Drag & Drop (`wl_data_device` v3) — komplettes DnD-Routing:
  start_drag unter der Implicit-Grab-Regel (ein Button muss
  gehalten sein), Pointer-Events während des Drags stumm,
  Re-Targeting pro Motion mit drag_enter/motion/leave,
  Mime- + Action-Negotiation (accept→target,
  set_actions→action), drop→receive→send→finish-Pipeline,
  Drag-Icon rendert am Cursor und nie in Captures —
  verifiziert End-to-End durch dnd_proto_test mit skript-
  gesteuerter Press/Drag/Release-Sequenz im Nested-Selftest,
  echte Payload-Bytes zurückgelesen
- `zwp_idle_inhibit_manager_v1` — Idle-Inhibition pro Surface,
  mit Liveness-Pruning getrackt; die Abfrage für den späteren
  DPMS-/Lock-Pfad (idle_inhibited)
- `ext_idle_notifier_v1` + `ext_session_lock_v1` +
  `xdg_activation_v1` — Session-Block: Seat-Timeout-Idle-
  Benachrichtigungen, per-Output-Lock-Surfaces die exklusiv
  rendern (per Screencopy pixelverifiziert), Eingabe-
  Beschränkung während der Sperre, Unlock + Re-Lock-Zyklen,
  Launch-Activation-Token-Fokus — alles durch
  session_proto_test verifiziert
- Nested-Backend: Client-Request-Dispatch hängt nicht mehr am
  Display-Poll-FD (erwies sich als unzuverlässig — Requests
  lagen einmal sekundenlang ungelesen); Pump läuft jetzt bei
  jedem Poll-Durchlauf
- shm-Pool-mmap-Fehler (z.B. ein Write-Only-FD) warnen jetzt
  bei create_pool statt still schwarz zu zeichnen
- **`zwp_linux_dmabuf_v1` v3** — GPU-teilbare Client-Buffer:
  lineare XRGB8888/ARGB8888-Einplane-Dmabufs werden in den
  normalen Blit-Pfad gemmapt; `create_params`/`create`/
  `create_immed` plus ehrliches `failed` bei nicht
  unterstützten Planes/Modifizierern/Formaten. v4+-
  Feedback-Objekte werden bewusst noch nicht beworben.
  End-to-End verifiziert: memfd-Dmabuf in einem echten
  Toplevel, `created` + `failed` korrekt.
- **`wp_presentation` v2** — Present-Feedback pro Surface:
  Einmal-`feedback`-Objekte feuern `presented` mit echtem
  CLOCK_MONOTONIC-Zeitstempel, dem wl_output des jeweiligen
  Clients (Client-übergreifende Objekte sind ein Protokoll-
  Fehler), Refresh-Intervall und Sequenznummer; `discarded`
  bei Surface-Destroy. Verifiziert mit einem Presentation-Feedback-Testclient
  (757 Feedback-Events).
- Workspaces (i3-Modell) — Workspaces gehören Outputs,
  Fenster gehören Workspaces: sichtbare Workspace pro Output,
  ein Visibility-Gate geteilt von Tiling/Render/Hit-Test/
  Focus-Cycle, Steal-Fallback über prev_ws damit kein Output
  leer wird, ws-N/mv-N-Bindings (Super+1..9 / Super+Shift+1..9),
  Fokus folgt dem obersten sichtbaren Fenster — verifiziert
  durch den Selftest: der ws2-Frame-Dump enthält keine
  Fensterpixel, zurückgeschaltet sind alle wieder da
- `zwp_primary_selection_device_manager_v1` — Primary
  Selection: Source-/Offer-/Device-Objekte, MIME-Offers, Send
  in Requester-FDs, teilt das SelSrc-Modell mit
  wl_data_device + Data-Control — verifiziert mit
  wl-copy --primary / wl-paste --primary Round-Trips
- `ext_workspace_manager_v1` — Workspace-Protokoll für
  Taskbars/Docks: eine Gruppe pro aktivem Output
  (output_enter löst die eigene wl_output des Clients auf,
  pro Tick wiederholt bis gebunden), neun Workspaces mit
  stabilen IDs, Namen, 1-D-Koordinaten, Capabilities
  (activate|assign) und State; Requests werden bis `commit`
  gequeued, greifen über die switch_ws/show_ws-Steal-Semantik,
  dann `done`; `ws_sync` pusht Per-Tick-Diffs (State-Flips,
  Group-Enter/Leave, Hotplug-Gruppen); `stop` → `finished` —
  End-to-End verifiziert durch ws_proto_test
- GPLv3-or-later-Lizenzierung, englische/deutsche Dokumentation
- Layer-Popups (GTK-Menüs): `xdg_surface.get_popup` mit
  NULL-Parent hinterließ das Popup-Objekt ohne ObjectData —
  ein harter Server-Panic beim nächsten Request, reproduzierbar
  durch das Startmenü der Taskbar ausgelöst. Das Objekt wird
  jetzt immer initialisiert und `zwlr_layer_surface.get_popup`
  führt das Reparenting durch — End-to-End verifiziert durch
  lpop_proto_test
- Geteilte Szenenliste (`CompState::scene`) — ein Stacking-/
  Sichtbarkeits-Modell für alle Render-Backends: Layer-Bänder,
  Popups, Session-Lock-Gate, Minimize-/Workspace-Filter. Behebt
  eine echte Lücke, bei der der Vulkan-Pfad nur Toplevels
  zeichnete (unsichtbare Taskbar/Panels auf Hardware), und
  bringt Theme-Cursor-Texturen in den Vulkan-Pointer-Pfad
- Vulkan-Frame-Pipelining — das Backend blockierte den
  Eventloop nach jedem Submit auf einen kompletten
  GPU-Roundtrip; Clients mit Commit pro Frame (GTK
  Menü-Hover) verhungerten die Input-Verarbeitung und der
  Pointer hing sichtbar. Der Drain doppelpuffert jetzt den
  Readback und liefert die fertige Parität des vorherigen
  Frames — Submits bremsen den Loop nie. Korrektheitshälfte
  der Pipeline: Der In-Flight-Frame wird per Fence verfolgt
  und `Backend::take_retired` übergibt ihn an einen
  Settle-Pass pro Tick (`Output::settle`), der ihn beim
  Retieren präsentiert — der letzte committete Frame landet
  immer, auch wenn kein weiterer Damage ankommt. Texturen toter
  Buffer recyceln über einen größenindizierten Pool, was
  den vkImage-Allokations-Churn pro Commit und ein
  GPU-Resource-Leak beendet (evickierte Texturen wurden nie
  zerstört; tex_cache, Pool und Cursor-Texturen werden jetzt
  beim Backend-Drop zerstört)
- Damage im Present-Pfad — der udev-Output kopierte pro
  Present ~9 MB Vollbild in einen write-combined Dumb-Buffer
  und mappte ihn jedes Mal neu; genau das verhungerte die
  Input-Verarbeitung bei Clients mit Commit pro Frame. Die
  Kopie clippt jetzt auf gemergte Damage-Rects über eine
  pro-Output-Queue, die die Pipeline-Latenz des Backends
  respektiert (`Backend::latency`), die Dumb-Map ist
  sessionweit gecacht, unberührte Outputs überspringen nur
  Copy+Flip (der Render läuft immer — jedes queued
  Damage-Set ist mit genau einem Submit gepaart, sodass ein
  retirter Frame nachweisbar alle enthält), und
  Cursor-Updates damagen nur ein
  128-px-Rect statt des ganzen Bildschirms. Der
  Theme-Cursor-Pfad greift außerdem nicht mehr pro SetShape
  aufs Dateisystem zu — Theme-Bilder cachen pro (Name,
  Größe) im CompState
- Cursor-Ownership-Fix — set_cursor/set_shape gelten nur noch
  vom Client, über dem der Pointer wirklich liegt (die
  eigene Gültigkeitsregel des Protokolls), und ein
  Pointer-Fokuswechsel verwirft den Cursor der vorherigen
  Surface. Ohne das blieb das letzte Shape (Resize-Griff,
  DnD-Feedback) über Fenstern hängen, die nie eines setzten —
  und über dem Desktop selbst
- Ghost-Surface-Fix — `prune_dead` lief nur innerhalb der
  Input-Event-Verarbeitung und entfernte tote Surfaces, ohne
  die freigewordenen Pixel zu damagen — Fenster von Clients,
  die zwischen Input-Events starben, blieben ewig als Geister
  auf dem Schirm. Der Sweep läuft jetzt jeden Frame-Tick und
  damaget die Szene, sobald er tatsächlich ein Objekt entfernt
- Nested-Backend respektiert Host-Größe — xdg_toplevel.configure
  baut jetzt den Memfd-Frame und den virtuellen Output in der
  zugewiesenen Größe neu, statt stur einen 1280x800-Buffer zu
  attachen, den der Host dann auf die Tile-Fläche clippte
  (das Fenster zeigte unter Tiling-Hosts nur eine Ecke)
- Socket-Instanz-Auswahl — der Compositor bindet den ersten
  freien odin-N-Namen, nachdem ein Connect-Probe live
  Instanzen von stale Dateien trennt. Bisher löschte eine
  zweite Instanz (Nested-Dev-Lauf) den Socket einer laufenden
  Session und verwaiste still alle künftigen Clients.
  Gespawnte Clients bekommen den echten Namen per
  WAYLAND_DISPLAY — live verifiziert: ein nested Odin band
  odin-1 innerhalb einer laufenden odin-0-Session und lief
  den kompletten Selftest ohne sie anzufassen
- `wp_viewporter` — Surface-Crop/-Scale: doppelt gepufferter
  Pending-State beim Commit angewendet, Buffer-Space-Damage
  durch die src/dst-Rect-Mathematik transformiert (Defaults aus
  den Buffer-Maßen abgeleitet), effektive Viewport-Größe speist
  Hit-Tests und Whole-Surface-Damage, CPU-blit_scaled und
  Vulkan-UV/Destination-Draw teilen ein Modell — verifiziert
  durch vp_proto_test mit Pixelchecks über Screencopy
  (Crop+Stretch, Reset)
- xdg_toplevel-Owner-Auflösung — Requests vor dem ersten
  Commit (set_fullscreen, set_title, set_app_id, maximize)
  wurden still gedroppt, weil die Surface erst beim ersten
  Buffer-Attach in die Stacking-Order kommt. Der Toplevel
  trägt seine besitzende wl_surface jetzt als Objektdaten —
  echte Client-Titel/App-IDs kommen wieder bei
  Foreign-Toplevel-Taskbars an
- `wp_fractional_scale_v1` — Manager-Global, pro-Surface
  preferred_scale aus dem Output der Surface gespeist;
  Output-Scale ist ein echtes wm::Output-Feld, das der
  Output-Management-Pfad anwendet — verifiziert via
  vp_proto_test (Scale-120-Event)
- `zwp_pointer_gestures_v1` — Swipe-/Pinch-Gesture-Objekte pro
  Client, Begin/Update/End/Cancel-Streams an den
  Pointer-Fokus-Client geroutet mit Serial-/Time-/Finger-
  Semantik — verifiziert durch ptr_proto_test (komplette
  Gesture-Streams während des Nested-Sweeps)
- `zwp_keyboard_shortcuts_inhibit_v1` — Inhibitoren pro
  Surface/Seat: active/inactive-Events bei Fokuswechsel,
  aktive Inhibition leitet jede Taste an den Client und
  suspendiert Compositor-Bindings (die Quit-Notbremse bleibt)
  — verifiziert durch inhibit_proto_test (die C-A-d-Binding-
  Combo erreichte das wl_keyboard des Clients statt das
  Binding auszulösen)
- `zwp_text_input_v3` + `zwp_input_method_v2` — das IME-Paar:
  Text-Inputs entern sofort, wenn sie auf einer bereits
  fokussierten Surface erstellt werden (Fokus-Übergänge allein
  würden sie hängen lassen), enter/leave überschreiten nie
  Client-Grenzen, enable+commit aktiviert die IM mit
  Surrounding-/Cause-/Content-Type-/Cursor-Rect-State und
  Done-Serials; commit_string/preedit/delete werden atomar
  pro IM-Commit angewendet, Keyboard-Grab-Keymap+Key/Modifier-
  Routing, Popup-Surfaces ankern am Cursor-Rect des Felds in
  der Scene — verifiziert End-to-End durch ti_proto_test
  (zwei Verbindungen: IM-Activate, Surrounding-Text,
  commit_string zurück, Deactivate)
- `wl_touch` — slot-getrackte Touch-Points: down hit-testet
  und grabbed die Surface für die Lebensdauer des Slots
  (Implicit-Grab-Semantik wie beim Pointer), Motion-/Up-/
  Cancel-/Frame-Batches pro Client, Seat-Capability
  advertised, tote Surface-Slots gepruned, Slot-Wiedervergabe
  schickt erst ein implizites Up — verifiziert durch
  ptr_proto_test (down/motion/up/frame-Batches während des
  Sweeps)
- Touch-armierte Grabs — ein xdg_toplevel.move/resize-Request,
  der eine wl_touch.down-Serial zitiert, armierte früher einen
  Pointer-Grab, der nie loslassen konnte (kein Button-Release
  kommt je — eingefrorene Pointer-Events, Ghost-Drags).
  Serial→Slot-Tracking armiert jetzt Touch-Grabs, die beim Up
  des Fingers loslassen; eine Touch-Serial, deren Slot schon
  endete, armiert gar nichts; touch_motion treibt den Grab
  wie pointer grab_motion
- `zxdg_decoration_manager_v1` + KDE `server_decoration` —
  CSD-/SSD-Verhandlung mit Duplikat- und Lebenszeit-Prüfung,
  und ein echter SSD-Renderer dahinter: Titelleiste, Ränder
  und Close-Button sind kompositor-gezeichnete Szene-Items
  (`ssd.rs`); Ziehen auf der Titelleiste startet einen
  Move-Grab, die Randzonen Resize-Grabs, der Close-Button
  schickt `xdg_toplevel.close()`. Fokus färbt die Titelleiste
  um; Popups/Layer/IME-Surfaces bleiben über jedem Frame und
  behalten ihre Klicks. Pro Client verhandelt — `Auto`/`Server`
  konfiguriert SSD, `Client` bleibt CSD; der Config-Key
  `decoration =` steuert den Default
- `zxdg_foreign_v2` — Handle-Export/-Import, `set_parent_of`
  als Cross-Client-Transient-For, `destroyed` am Importer bei
  Eltern-Tod, Restack hält Kinder über Eltern; das normale
  xdg_toplevel.set_parent füttert jetzt dasselbe
  transient_for-Feld und droppt keine Pre-Map-Requests mehr
- `wp_single_pixel_buffer_v1` — 1×1-Farbflächen als echte
  4-Byte-Anon-Mmaps; Attach/Commit/Blit/Upload behandeln sie
  wie gewöhnliche Buffer
- `wp_alpha_modifier_v1` — double-gepufferter Multiplikator
  (u32::MAX = opak) auf SurfaceData; der CPU-Blit skaliert
  das Quell-Alpha, Vulkan pusht es an die Fragment-Stufe bei
  Push-Constant-Offset 32
- `wp_content_type_v1` — double-gepufferte Content-Hints auf
  SurfaceData für spätere Renderer-/Output-Policy
- `wp_tearing_control_v1` — double-gepufferter Async-Hint;
  tearing_active speist PageFlipFlags::ASYNC im DRM-Pfad mit
  automatischem Fallback auf Vsync-Flips bei EINVAL
- `zwlr_output_power_manager_v1` — Power-State pro Connector
  in CompState.power; der DRM-Frame-Pfad blankt den CRTC via
  set_crtc(None) und skippt Compositing, das Nested-Backend
  stoppt das Presenten; Mode-Events bestätigen den Zustand
- `xdg_dialog_v1` — set_modal/unset_modal auf SurfaceData;
  modal_descendant leitet Fokus von Vorfahren auf den
  obersten modalen Transient um
- `xdg_system_bell_v1` — ring() schreibt BEL auf die Tty der
  Session
- `zwp_tablet_manager_v2` — Manager → tablet_seat-Objekte pro
  Seat; Tablets melden Name/VID/PID/Pfad beim libinput-
  Hotplug; Tools melden Typ/Seriennummer/Fähigkeiten bei
  erster Proximity mit Proximity-/Motion-/Druck-/Distanz-/
  Neigung-/Rotation-/Slider-/Wheel-/Tip-/Button-/Frame-
  Streams zum Surface-Eigner; Pads melden Buttons plus
  Ring-/Strip-Kinder in Modusgruppen
- Überarbeitung des Present-Pfads — redundante Page-Flips auf
  demselben fb entfernt (~27 ms pro Frame auf virtio-gpu),
  dirtyfb auf echten Schaden geclippt, Readback auf die
  Coverage der Damage-Queue mit Sequenz-Tracking pro Parität
  begrenzt, Render/Present-Split mit Input-Drain mitten im
  Tick (Pointer-Latenz 33 → 18 ms unter 35-Commits/s-Sturm),
  wl_shm-Geometrie bei create_buffer validiert, Pool-Shrink
  abgelehnt, Screencopy-/Cursor-/Vulkan-Upload-Pfade mit
  Bounds-Checks

## Bekannte Probleme

- **SSD-Rahmen-Grabzonen sind nur die gezeichneten
  1px-Borders** — `GRIP` (der dokumentierte 6px-unsichtbare
  Resize-Grip) wird ausschließlich zur Eck-Erkennung innerhalb
  des dekorierten Extents benutzt; die Außenmarge, die er
  beschreibt, wird nie angewendet. Kanten-/Eck-Grabs auf
  gerahmten Fenstern sind dadurch fast nicht treffbar. Ein
  unsichtbares Grip-Band außerhalb des sichtbaren Randes
  fehlt.
- **Kein Hover-Cursor-Feedback auf SSD-Rahmen** — `ssd_pick`
  läuft nur beim Tastendruck, der Pointer zeigt über einem
  Compositor-Rahmen nie Änderbarkeit an; ein Zonen-Check zur
  Motion-Zeit + Cursor-Shape-Wechsel fehlt.
- **Vollbild-Animations-Flackern — Fix gelandet, Verifikation
  offen**: Das Vulkan-Parity-Retirement hielt den zuletzt
  iterierten Slot-Index für den "neuesten" — was nach dem
  Umwickeln von `cur` nicht die neueste Submission ist. Es
  präsentierte den älteren von zwei Retirees (alter Frame
  sichtbar bei kontinuierlichem Damage) und ließ `presentable`
  regressieren. Beide Scans wählen jetzt max cover_seq.
  Braucht einen Real-Session-Dauertest (Vollbild-Video auf
  einem Output); falls es weiter flackert, ist der nächste
  Schritt Frame-Dump/Parity-Wahl-Tracing.
- **Session-Aktivierungs-Umgebung kann veralten** — per
  D-Bus-Aktivierung gestartete Clients (GApplication-Muster)
  können die Umgebung der vorherigen Session sehen und mappen
  still nie ("nichts startet", nur ein Reboot half);
  `dbus-update-activation-environment` fehlt im Session-
  Wrapper-Pfad noch.
- **GLES-Backend ist ein Stub** — `GlesBackend::new` bricht
  ab; die Renderer-Kaskade ist faktisch Vulkan → CPU.
- **`eglSetDamageRegion`-Spam** — client-/treiberseitig
  (egl-wayland2/egl-x11 fehlt `EGL_KHR_partial_update`; die
  Funktion ist auf NVIDIA dokumentiert ein No-op). Nicht
  Odins Problem — hier festgehalten, damit es nicht erneut
  diagnostiziert wird.
- **Popups landen auf der rohen Ankerposition** —
  `xdg_positioner` speichert size/anchor_rect/anchor/gravity/
  offset, aber `set_constraint_adjustment`, `set_reactive`
  und `set_anchor_point` fallen unbehandelt durch — es gibt
  keine Flip-/Slide-/Resize-Auflösung. Ein Menü nahe einer
  Output-Kante öffnet am unangepassten Rect — berichtet als
  "Menüs öffnen an der falschen Stelle". Constraint-Lösung
  fehlt.
- **Fensterknopf-Hitzonen bei manchen Clients versetzt** —
  bei manchen Anwendungen sitzt der klickbare Bereich für
  Minimieren/Maximieren/Schließen sichtbar neben dem
  gezeichneten Knopf (Pointer muss neben dem X schweben).
  Passt zur CSD-Margin-Familie: Grips liegen in der
  unsichtbaren Buffer-Marge und manche Toolkits nutzen
  asymmetrische Ränder — ein Mapping-Fehler im
  lokalkoordinaten-Eventpfad ist aber nicht ausgeschlossen;
  braucht ein Audit pro Client (welches Toolkit, welche
  Ränder), bevor es "erwartet" heißen darf.
- **`wp_cursor_shape` nicht zuverlässig angewendet** — Device
  und `set_shape`-Dispatch existieren, aber clientgesetzte
  Shapes (z.B. I-Beam über Textfeldern) kommen nicht immer
  auf dem Schirm an. Die Kette gespeicherte Shape →
  angezeigter Cursor (Fokus-Gate, HW-Cursor-Fallback, Shape-
  Enum-Mapping) braucht ein Audit.
- **Screen-Capture überspringt still jeden dmabuf-Client** —
  Root Cause von "Browser/Dateimanager fehlen in
  Screenshots, während GIMP und Terminals gehen". `draw_at`
  (der CPU-Kompositierer hinter allen shm-Captures) braucht
  `shown_buf.data::<BufInfo>()` — existiert nur auf
  `wl_shm`-Buffern; eine dmabuf-Surface liefert `false` und
  wird ohne Fallback ausgelassen. `cap_src_rect` hat dieselbe
  Abhängigkeit: ein Toplevel auf dmabuf löst zu `bw=0` →
  `None` → die Capture-Session meldet `stopped`. Beide Pfade
  scheitern also identisch an GPU-Clients: Per-Output-
  Captures zeichnen alles außer dmabuf-Fenstern, Per-
  Toplevel-Captures verweigern sie ganz. dmabuf = jeder
  GL-/XWayland-glamor-Client. Der dump-frame-Pfad ist
  unbetroffen — er liest den bereits gerenderten Frame, die
  Pixel existieren also; der Capture-Pfad fragt die GPU nur
  nie danach. Geschuldete Fixes: (a) Toplevel-Capture-Rects
  aus `pos`+`win_geom` auflösen (buffertyp-unabhängig),
  (b) Output-Captures wie dump-frame aus dem komponierten
  Frame/Readback bedienen statt CPU-seitig neu zu
  kompositieren — behebt nebenbei die CPU-Kosten unten,
  (c) GPU-Readback pro Toplevel für den ext-image-copy-
  capture-Pfad.
- **Screen-Capture ist CPU-gebunden** — der shm-Blit
  kompositiert die volle Szene pro Frame; Tools mit Per-
  Toplevel- oder Per-Output-Captures multiplizieren die
  Kosten (beobachtet als "Screenshots dauern spürbar lang").
  Output-Captures aus dem gerenderten Frame zu bedienen
  (Punkt davor) oder der dmabuf-/GPU-Capture-Pfad aus der
  Roadmap ist die Antwort.
- **CSD-Schattenränder sind erwartetes Verhalten, kein Bug** —
  Resize-Grips in der unsichtbaren Buffer-Marge eines Clients
  wirken auf dick beschatteten Seiten "versetzt" (typisch
  unten/rechts bei GTK); der Hit-Test deckt den ganzen Buffer
  korrekt ab.

## Als Nächstes

- Den Vulkan-Pfad in einer echten Session End-to-End validieren
- Vollständiges Pointer-/Keyboard-Fokus-Audit gegen echte Anwendungen
- Automatisierte Testabdeckung über den Vulkan-Selftest hinaus
- XWayland: XTEST via libei

## Renderer — mittelfristig

- Zero-Copy-Scanout via DMABUF-Import und Format-Modifiern
  (Extension-Probing bereits implementiert; Client-Buffer-
  Direct-Scanout gelandet — siehe oben)
- Hardware-Cursor-Planes
- OpenGL-ES-Fallback-Backend
- `linux-dmabuf` Multi-Plane-Formate (v4-Feedback + Single-Plane-
  Create gelandet: Format-Tabelle, `main_device` dev_t, Scanout-
  Tranche — XWayland-glamor hängt daran)

## Protokolle und Desktop

- DMABUF-Screencopy-Pfad (SHM-Capture existiert)
- Output-Management-Folgearbeiten: echtes DRM-Re-Modeset beim
  Apply (das Layout speichert den Modus; das atomare Modeset
  landet im Backend), Hotplug-Head-/Finished-Events, live
  wählbarer Primary-Output
- D-Bus-Settings-Schnittstelle
- ~~Optionaler SSD-Renderer für CSD-unfähige Clients~~ —
  gelandet: `ssd.rs` Frame-Renderer + Dekorations-Verhandlung
  wählt SSD für `Auto`/`Server`

## Offene Entscheidungen

- ~~Workspace-Modell-Semantik (pro Output vs. global)~~ —
  entschieden: i3-Modell, pro Output (implementiert)
- [ ] Interaktive Grabs: XReq::Configure pro Fenster/Tick dedupliziert (rohe 1kHz-Motion flutete die X-Verbindung -> Xwayland saturiert -> JEDES X11-Video friert beim Ziehen ein); native Resize-Configures jetzt via rs_pending gebündelt -> ein xdg-configure pro Frame-Tick in flush_motion (das Pacing, das ein Client verkraftet)
- [ ] Hit-Test zurück auf input-region/Buffer-Grenzen: window_geometry begrenzt laut Spec KEINE Eingabe (CSD-Schattenränder tragen die Resize-Griffe; der frühere Geometrie-strikte Test hatte sie komplett entfernt)
- [ ] Resize-Grab-Startrect nutzt jetzt die Window-GEOMETRY w/h (vorher Buffergröße inkl. Schattenränder -> Configure-Größen um den Rand daneben); entspricht Spec (Configure-Size = Geometry-Space — Resizes laufen im logischen Raum)
- [ ] grab_end() sendet beim Grab-Ende (Pointer-Release, Touch-Up, Touch-Cancel) die finale Configure OHNE Resizing-State mit der gebündelten Zielgröße — Clients blieben bisher im Resizing-State nach dem Loslassen
- [ ] zwp_linux_dmabuf_v1 advertiset jetzt das GPU-ENUMERIERTE Sampled-Modifier-Set (VkDrmFormatModifierPropertiesListEXT, B8G8R8A8 + SAMPLED_IMAGE) statt nur LINEAR — gemessen: XWayland verbrannte ~75% CPU in libpixman beim Fensterziehen, weil seine Buffer auf shm/Software zurückfielen. Früher VkContext::probe_modifiers() in main vor dem Xwayland-Spawn; validate_and_map akzeptiert das advertisete Set, getilete Buffer ohne mmap direkt in den Explicit-Modifier-Vulkan-Import (import.rs nutzt dma.modifier)
- [ ] xwm ConfigureRequest-Ack echot jetzt nur MASKIERTE Felder — ein ungesetztes x/y im Request trug 0 und das unmaskierte Echo teleportierte das X-Fenster via eigenem ConfigureNotify nach (0,0) (Fenster „verschwindet"/springt in die Ecke)
- [ ] Loop-Stall-Forensik: present stats zeigen jetzt pump_ms (Client-Dispatch), xwm_ms (X-Event-Drain + Request-Apply), drain_ms (Input-Callback) neben input_lag_ms — benennt den saturierenden Callback beim Drag-Freeze
- [ ] dmabuf-Import-Fehler loggen eine Warnung (Modifier/Größe/Fehler) — ein kaputter Import-Pfad blendet getilete Surfaces aus statt still zu scheitern; grab_motion warnt, wenn ein gezogenes Fenster alle Outputs verlässt
- [ ] xdg_toplevel set_min_size/set_max_size werden jetzt auf SurfaceData gespeichert und vom interaktiven Resize-Grab erzwungen (wurde bisher still ignoriert); die fixierte Ecke bleibt verankert, wenn ein Min/Max-Clamp den Ursprung an linken/oberen Kanten verschiebt
- [ ] Nested-Selftest-Infrastruktur: ein headless-Wayland-Host + ODIN_BACKEND=nested fährt die volle Pipeline inkl. eigenem XWayland — null Relogs. Testclient-shm-pools auf O_RDWR-FDs korrigiert (Write-only-FDs ließen das serverseitige mmap scheitern → Protokoll-Tod → alle RESULTs ungültig). Selftest jetzt grün: relativer Pointer, Confine, Swipe, Pinch, Touch, Keyboard-Inhibit, Foreign-Toplevel, Workspaces, DnD
- [ ] xdotool/wmctrl im verschachtelten XWayland verifiziert: maskierter ConfigureRequest-Resize erhält die Fensterposition (Teleport-Regressionstest)
- [ ] Syncobj-Timeline-Imports prägen Handles jetzt auf Odins eigenem DRM-Render-Node (comp.drm_fd, Lazy-Fallback unter nested) — NVIDIA lehnt fd_to_handle auf dem exportierten Syncobj-FD des Clients ab, was XWayland und alle GTK4-Clients beim ersten Import per Protokollfehler gekillt hat; glxgears in nested XWayland verifiziert grün
- [ ] Vulkan-Texture-Cache-Eviction war toter Code: das Gate lag hinter queue_submit+in_flight[cur]=true, `!in_flight.any()` war nie wahr — importierte dmabuf-Texturen sammelten sich endlos (VRAM-Leak passend zur Freeze-Signatur). Das Gate befreit jetzt `cur` — seine frische Submission kann nur `live`-Texturen samplen — und öffnet, sobald die älteren Parities retiriert sind
- [ ] libinput-Scroll-Achsen jetzt mit has_axis geguardet vor scroll_value/scroll_value_v120 — ungeschützte Reads loggten „client bug: value requested for unset axis" pro Wheel-Tick (3899 Zeilen in 4 Tagen stderr.log)
- [ ] Connector-Namen nutzen jetzt interface_id (Per-Type-Index → „DP-3") statt der DRM-Objekt-ID („DP-107"), und die EDID-Serial bevorzugt den 0xFF-String-Descriptor vor dem numerischen Feld — die Konvention, die Clients erwarten. Output-Management-Clients sehen jetzt dieselben Namen/Serials wie anderswo, sodass Profil-Matching und eine persistierte Primary-Monitor-Einstellung nicht mehr sessionsübergreifend vergiftet werden können

## Arbeitsplan

Der Kompositor-Paritäts-Audit liegt in `PLAN.md` / `PLAN_de.md` —
was jeder Standard-Kompositor macht, Odins Stand pro Punkt und
die Verifikationsreihenfolge (Nested-Selftest → vptr-Injection →
xdotool im verschachtelten XWayland → instrumentierte Stats →
echte Session zuletzt).
- [ ] wl_subcompositor/wl_subsurface — vollständige Core-Subsurfaces: Sync-Modus-Commit-Caching (Commits halten bis der Parent-Commit anwendet, Spec-Default), Desync, double-buffered set_position/place_above/place_below gegen einen Root-Marker-Z-Stack, Scene-Komposition + Input-Hit-Test integriert, live verifiziert mit einem Subsurface-Testclient (verschachteltes EGL-Dreieck + shm-Kinder kompositieren korrekt)
- [ ] Protokoll-Abdeckungs-Audit — wayland-info-Diff ergab: wl_compositor v6, wl_seat v9, wl_shm v2 gehoben, zxdg_exporter/importer v1-Paar auf geteilter Handle-Tabelle, dmabuf-v4-Feedback (main_device/Tranchen — behebt XWayland-glamor: Real-Session-Log zeigte „GBM Wayland interfaces not available" → kompletter Software-Fallback, die Drag-Stall-Ursache); dann gelandet: ext_data_control, ext_foreign_toplevel_list, ext_image_copy_capture (+ Source-Manager), wp_linux_drm_syncobj + implizites dmabuf-Fence-Gating, xdg_toplevel_icon, xdg_wm_base v6 (configure_bounds/wm_capabilities/suspended/set_minimized)
- [ ] X11-ConfigureNotify synct jetzt Fenstergeometrie + sd.pos — Override-Redirect-Fenster (Menüs, Firefox-Doorhanger) konfigurieren sich nach dem Map selbst; ohne den Sync renderten sie fest bei (0,0). Live-Event-Trace bestätigt: Doorhanger bei (0,0,1x1) erstellt, auf (359,76,415x164) konfiguriert, Position ging vorher verloren
- [ ] Override-Redirect-Klicks nehmen keinen Tastaturfokus mehr — hit_window→set_kbd_focus feuerte SetInputFocus aufs Popup, das FocusOut am Hauptfenster rollte Firefox-Doorhanger beim ersten Klick sofort ein. OR-Popups bekommen jetzt den Klick ohne Fokus/Raise
- [ ] pointer.rs (524 Zeilen) und render.rs überschreiten die 400-Zeilen-Regel — Hit-Testing/Scene-Assembly auslagern (strukturelle Schuld, vorbestehend)
- [ ] Output-Stack auf Hardware-Wahrheit umgebaut: EDID-Base-Block-Parser (make/model/serial aus dem EDID-Blob-Property des Connectors), echte phys_mm + echte Hz überall (wl_output.geometry/mode, head.physical_size) — die 275×155mm/60Hz/„odin"-Platzhalter sind weg
- [ ] Volle Modus-Listen advertised: jeder Connector-Modus wird ein wl_output-Mode-Event + zwlr_output_head_v1-Modus-Objekt, damit wlr-randr/wdisplays/kanshi echte Auswahl sehen
- [ ] zwlr_output_manager_v1 reicht jetzt bis zur Hardware: strikte Validierung (Modus muss advertised sein, nur Transform=Normal, adaptive_sync abgelehnt) → ehrliches `failed` statt stillem Ignorieren; commit queueht OutOps, die der Event-Loop ausführt — echtes Re-Modeset (Buffer abgebaut, Backend neu gebaut, CRTC neu gesetzt), Viewport-Origin-Move, sofortiges CRTC-Blank bei Disable
- [ ] Monitor-Hotplug: udev-drm-Subsystem-Monitor im calloop → hotplug::rescan difft Connectoren gegen Layout — Add (rechts der Bounding-Box, Output::new, wl_output-Global, Head-Announce), Remove (CRTC-Blank, Drop, Global-Entzug, head.finished), Metadaten-Refresh in-place; überlebende Outputs behalten ihre Positionen
