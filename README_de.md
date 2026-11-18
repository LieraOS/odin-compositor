# Odin

Ein eigenständiger Wayland-Compositor in Rust — **kein wlroots,
kein Smithay**. Der komplette Compositor-Stack ist direkt
implementiert: DRM/KMS-Session-Handling, Vulkan-Rendering,
Scene-Modell, Input- und Fokus-Semantik, Fenstermanagement,
Workspaces und ein eigener X-Window-Manager für XWayland. (Die
`wlr-*`-Protokollerweiterungen, die Odin spricht — Layer-Shell,
Screencopy, Output-Management und Freunde — sind öffentliche
Wayland-Protokollspezifikationen, die Odin selbst implementiert,
kein wlroots-Bibliothekscode.)

> **Status: experimentell.** Odin führt echte Wayland- und
> X11-Clients auf echter Hardware aus — ist aber eine
> weiterentwickelte Codebasis, kein Daily Driver. Rechne mit
> rauen Kanten, unbehandelten Randfällen und Breaking Changes.
> `TODO_de.md` enthält die ehrliche Liste dessen, was fertig
> ist und was nicht.

> **Getestete Hardware: NVIDIA** (proprietärer Treiber,
> 580.x-Serie). Der Renderer nutzt keine herstellerspezifischen
> Vulkan-Features — aber kein anderer GPU-Anbieter wurde bisher
> getestet, AMD-/Intel-Berichte sind willkommen.

![Eine laufende Odin-Session — Master/Stack-Tiling auf dem
linken Output, Floating auf dem rechten](docs/screenshot.png)

[English version](README.md) ·
[Protokoll-Unterstützung](PROTOCOLS_de.md) · [TODO](TODO_de.md)

## Highlights

- **Direkte DRM/KMS-Session** — läuft auf einem echten VT über
  libseat/seatd, kein Eltern-Compositor nötig; kann parallel zu
  einer bestehenden Session auf einem zweiten VT sitzen
- **Vulkan-Renderer** — Damage-Tracking pro Surface, Retained
  Rendering, Direct Scanout der Renderer-Parities und
  geeigneter Vollbild-Client-Buffer; CPU-Fallback, wenn kein
  GPU-Pfad existiert (der OpenGL-ES-Slot in der Kaskade ist
  reserviert, aber noch nicht implementiert)
- **Eigener XWM** — XWayland-Clients sind gleichberechtigte
  Bürger: Fensterung, Fokus, Override-Redirects, EWMH-Basics
  und Clipboard-Bridging in beide Richtungen
- **Layouts pro Monitor** — jeder Output fährt unabhängig
  Floating (Standard) oder Master/Stack-Tiling, live
  umschaltbar ohne Neustart; ein über Monitorgrenzen gezogenes
  Fenster übernimmt die Policy des Ziels; Vollbild-Clients
  sitzen dem Grid grundsätzlich fern
- **Workspaces** — i3-Modell: Workspaces besitzen Outputs,
  Fenster besitzen Workspaces
- **~40 Wayland-Protokolle** — Layer-Shell, Session-Lock,
  Screencopy und Image-Copy-Capture, explizite Synchronisation
  via drm-syncobj, Output-Management, IME, Tablet-Input und
  mehr: die volle Liste mit Implementierungsnotizen steht in
  [PROTOCOLS_de.md](PROTOCOLS_de.md)
- **Hardware-Cursor-Plane** — Pointer-Bewegung ist ein
  `MOVE_CURSOR`-Ioctl statt eines Composite; Treiber ohne
  Cursor-Plane fallen automatisch auf den Software-Cursor
  zurück

## Voraussetzungen

- Linux mit `seatd` (oder einem anderen funktionierenden
  libseat-Backend)
- Systembibliotheken: `libinput`, `libudev`, `xkbcommon`,
  `libdrm`
- Eine Vulkan-fähige GPU samt Treiber für den beschleunigten
  Pfad (entwickelt und getestet auf NVIDIA; der Code nutzt
  keine herstellerspezifischen Features — alles, was die
  benötigten Vulkan-/dmabuf-Schnittstellen anbietet, sollte
  funktionieren)
- Rust-Toolchain (Edition 2021) — Shader werden zur Build-Zeit
  via `naga` nach SPIR-V kompiliert, keine externe Shader-
  Toolchain nötig

## Installation und Start

### 1. Bauen

```sh
cargo build --release
```

### 2. Session-Dateien installieren

```sh
sudo install -Dm755 target/release/odin /usr/bin/odin
sudo install -Dm755 session/startodin /usr/bin/startodin
sudo install -Dm644 session/odin.desktop \
    /usr/share/wayland-sessions/odin.desktop
```

`startodin` nimmt die Session-Shell entgegen, die gestartet
wird, sobald der Compositor läuft — die mitgelieferte
Desktop-Datei fährt eine nackte Session (nur Compositor);
Desktop-Umgebungen übergeben ihr eigenes Session-Kommando,
z.B. `startodin my-session`. Ausloggen, im Display-Manager
**Odin** wählen, einloggen.

Gut zu wissen vor dem ersten Versuch:

- **Beenden:** `C-A-q` oder `C-A-Backspace` — beim ersten
  Test im Kopf behalten
- **Terminal:** Binding in `~/.config/odin/bindings.conf`
  setzen (wird beim ersten Start mit Defaults angelegt) — die
  Default-Datei bindet `C-A-t` auf `weston-terminal`; auf das
  eigene Terminal zeigen
- **Autostart:** `~/.config/odin/autostart` führt pro Zeile
  ein Shell-Kommando aus, sobald die Session steht (Panels,
  Hintergrund, Session-Komponenten). `odin -S "<Kommando>"`
  ist das Kommandozeilen-Äquivalent
- **Logs:** `~/.local/state/odin/` — `odin.log` plus
  `stderr.log` für Client-Ausgaben; bei Fehlverhalten zuerst
  hier schauen

### 3. Ohne Logout ausprobieren

Auf einem freien VT läuft Odin neben einer bestehenden
Session — libseat arbitriert die Geräte. Oder verschachtelt
als Fenster starten:

```sh
# Verschachtelt in einer laufenden Wayland-Session:
ODIN_BACKEND=nested ./target/release/odin

# Skriptgesteuerter Selbsttest — startet echte Clients, treibt
# Input, dumppt den komponierten Frame für Pixelchecks:
ODIN_BACKEND=nested ODIN_NESTED_TEST=1 ./target/release/odin

# Vulkan-Device/Kontext auf dieser Maschine prüfen:
./target/release/odin --vk-probe /dev/dri/card1

# Voller Pipeline-Test — Upload, Draw, Readback, Pixelcheck:
./target/release/odin --vk-render-test /dev/dri/card1
```

## Konfiguration

`~/.config/odin/bindings.conf`, ein Binding pro Zeile:

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

Modifikatoren: `C` Strg, `A` Alt, `S` Shift, `W` Super.
`primary` benennt den primären Output, `policy` setzt das
Layout pro Output oder global, `decoration` wählt den
Standard der CSD/SSD-Verhandlung. Die Datei wird überwacht
und live neu angewendet.

### Binding-Aktionen

Der komplette aktuelle Satz — bewusst klein; Odin ist ein
Compositor, keine Shell. Er wächst, wenn echte Bedürfnisse
auftauchen:

| Aktion | Wirkung |
|--------|---------|
| `spawn <Kommando>` | Client starten (erbt `WAYLAND_DISPLAY`, plus `DISPLAY`, sobald Xwayland läuft) |
| `cycle-focus` | Tastaturfokus durch die Fenster rotieren |
| `toggle-layout` | Output unter dem Pointer zwischen Floating und Tiling umschalten |
| `ws-<n>` | Fokussierten Output auf Workspace `n` schalten (1–9) |
| `mv-<n>` | Fokussiertes Fenster auf Workspace `n` schicken (1–9) |
| `dump-frame` | Komponierten Frame nach `~/.local/state/odin/` dumpen |
| `quit` | Session beenden (greift auch bei gesperrtem Bildschirm) |

Alles andere — Lautstärketasten, Launcher, Screenshots — ist
ein `spawn <Kommando>` vom Lieblingswerkzeug entfernt.

## Architektur

| Modul | Zuständigkeit |
|-------|---------------|
| `src/compositor/` | Wayland-Protokoll-Implementierung: Globals, Surface-Lebenszyklus, Layer-Shell, Seat-Input, Pointer-Erweiterungen, Cursor-Shapes, Zwischenablage, Render-Reihenfolge, Fenster-Operationen |
| `src/wm.rs` | Layout-Policies und Output-Layout (Floating, Master/Stack-Tiling, primärer Output) |
| `src/backend/` | Plattformseite: DRM/KMS-Outputs, libinput, Event-Loop, GPU-Probing, verschachteltes Backend |
| `src/renderer/` | Austauschbare Render-Backends: `vulkan/` (primär), `gles/` (reserviert, nicht implementiert), `cpu/` (Fallback) |
| `src/xwm/` | X-Window-Manager für XWayland: Verbindungs-Handshake, Event-Dispatch, EWMH, Selection-Bridging, X-Socket-Lebenszyklus |
| `src/session.rs` | Seat-/VT-Verwaltung via libseat |
| `src/xcursor.rs`, `src/cursor.rs` | XCursor-Theme-Laden und der Software-Pointer-Cursor |
| `src/config.rs` | Keybinding- und Einstellungs-Parser |
| `src/log.rs` | Persistentes Tracing nach `~/.local/state/odin/` |
| `shaders/` | Shader-Quellen, kompiliert von `build.rs` |

Die Renderer-Wahl läuft pro Output: Vulkan zuerst, CPU, wenn
kein GPU-Pfad existiert.

## Roadmap

Der volle Stand steht in `TODO.md` / `TODO_de.md` — erledigt,
offen und bekannte Probleme. Die Kurzversion dessen, was noch
kommt:

- OpenGL-ES-Fallback-Backend (Slot existiert; meldet
  unavailable)
- DMABUF-Screencopy-Pfad (SHM-Capture existiert)
- Output-Management-Folgepunkte (Adaptive Sync, Primary-
  Selection am Output-Manager)
- XWayland-XTEST via libei
- Breitere automatisierte Abdeckung jenseits der
  verschachtelten und Vulkan-Selbsttests
- Real-Session-Validierung — Odin ist durch verschachtelte
  Selbsttests und instrumentierte Läufe verifiziert, noch
  nicht durch Jahre täglicher Nutzung

## Ziele

- **Vollständig, nicht nur funktionsfähig** — alles, was
  einen Compositor ausmacht, lebt in diesem Baum:
  Protokoll-Handling, Scene-Graph, Renderer, Input, Fokus,
  Fenstermanagement, Workspaces, Multi-Monitor, XWayland.
  Ziel ist volle Feature-Abdeckung, nicht volle Bugfreiheit.
- **Kein geliehener Kern** — kein wlroots, kein Smithay.
  Jedes Frame und jedes Event läuft durch Code, den dieses
  Projekt von Ende zu Ende versteht — genau deshalb werden
  Bugs hier gefixt statt upstream gemeldet.
- **Performance als Feature** — Vulkan-first-Rendering,
  Direct Scanout, damage-gesteuertes Frame-Pacing, Zero-Copy-
  Pfade, wo immer die Hardware sie hergibt.

## Lizenz

Copyright (C) 2026 Jörg Schröder.

GPL-3.0-or-later — siehe `LICENSE`.
