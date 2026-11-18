// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! XWM — the X11 window-manager half of XWayland support.
//!
//! Architecture (the standard model): we spawn `Xwayland :N
//! -rootless -wm <fd>` — the fd is one end of a socketpair. We
//! connect to it with x11rb as an X11 CLIENT and take
//! SubstructureRedirect on the root window: every X map/configure
//! request now arrives here instead of being executed by the
//! server. Xwayland itself is a normal Wayland client on odin-0;
//! it creates a wl_surface per X window and tells us the pairing
//! via a WL_SURFACE_ID ClientMessage on the root window.
//!
//! Startup is async: Xwayland only answers the wm socket once its
//! Wayland side is up — which needs our event loop pumping. A
//! synchronous handshake at spawn would deadlock, so `spawn`
//! returns a Pending state the event loop drives until the X11
//! setup reply completes and the connection becomes Ready.

pub(crate) mod assoc;
pub mod client_msg;
pub mod events;
mod handshake;
pub mod selection;
pub mod state;
pub mod xsel;
pub mod xsock;

pub use handshake::drive_handshake;

use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use tracing::{info, warn};
use wayland_backend::server::ClientData;
use wayland_server::Display;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, EventMask, Window, CLIENT_MESSAGE_EVENT};
use x11rb::reexports::x11rb_protocol::connect::Connect;
use x11rb::rust_connection::RustConnection;

/// XWM lifecycle — Pending until the X11 handshake completes,
/// then Ready with the live connection.
pub enum XwmState {
    Pending(Pending),
    Ready(Xwm),
}

impl XwmState {
    /// Terminate the Xwayland process — `std::process::Child`'s
    /// Drop never kills, so an orphaned X server would outlive the
    /// compositor inside the session scope (holding X sockets and
    /// keeping stale display state alive for the next session).
    /// SIGTERM gives it a clean X11 shutdown; SIGKILL catches a
    /// wedged process after the grace period.
    pub fn kill_child(&mut self) {
        let child = match self {
            Self::Pending(p) => match p.child.as_mut() {
                Some(c) => c,
                None => return,
            },
            Self::Ready(x) => &mut x.child,
        };
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        for _ in 0..50 {
            match child.try_wait() {
                Ok(Some(status)) => {
                    info!(pid = child.id(), ?status, "xwayland exited");
                    return;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        warn!(pid = child.id(), "xwayland ignored SIGTERM — SIGKILL");
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Handshake in flight — the setup request is written once,
/// the reply accumulates in `connect`'s buffer per readable wake.
pub struct Pending {
    pub child: Option<Child>,
    /// Option so the completed handshake can take ownership of
    /// the stream into DefaultStream — no fd juggling.
    pub stream: Option<std::os::unix::net::UnixStream>,
    pub display: u32,
    /// Option so into_setup() can take it on completion.
    pub connect: Option<Connect>,
    pub request_sent: bool,
    /// Fatal socket error (EPIPE, EOF) — the handshake can never
    /// finish; callers drop the pending state and its poll source
    /// instead of retrying the dead stream every tick.
    pub dead: bool,
    /// Display lease — Option so Ready can take it.
    pub lease: Option<xsock::Lease>,
    /// The Wayland client handle of Xwayland itself — protocol ids
    /// in WL_SURFACE_ID messages are client-local, so surface
    /// lookup must be restricted to this client's objects.
    pub client: wayland_server::Client,
}

/// Everything the running XWM needs — the connection, the
/// Xwayland child handle, display number and interned atoms.
pub struct Xwm {
    pub conn: RustConnection,
    /// RAII — the Xwayland process must outlive the XWM; the
    /// handle is also what reaps it on shutdown.
    #[allow(dead_code)]
    pub child: Child,
    pub root: Window,
    pub atoms: Atoms,
    /// RAII — owns the X listen sockets; dropping it closes the
    /// display sockets Xwayland and X clients connect through.
    #[allow(dead_code)]
    pub lease: xsock::Lease,
    /// The small window we own — WM_S0 selection owner, EWMH
    /// supporting-wm-check anchor, and CLIPBOARD selection owner
    /// whenever a Wayland client holds the selection.
    pub wm_win: Window,
    /// In-flight Wayland→X clipboard transfers — each waits on
    /// the source client's pipe; pumped once per tick.
    pub pending: Vec<selection::PendingXfer>,
    /// In-flight X→Wayland fetches — TARGETS queries on owner
    /// change plus paste requests awaiting SelectionNotify.
    pub xsel_pending: Vec<xsel::XFetch>,
}

/// Interned atoms we reference — intern once at startup instead
/// of string-lookup per event.
#[derive(Debug)]
pub struct Atoms {
    pub wl_surface_id: u32,
    pub wm_change_state: u32,
    pub net_wm_state: u32,
    pub net_wm_name: u32,
    pub wm_protocols: u32,
    pub wm_delete_window: u32,
    pub net_wm_window_type: u32,
    pub motif_wm_hints: u32,
    /// EWMH active-window tracking — the X-side mirror of our
    /// wl_keyboard focus.
    pub net_active_window: u32,
    /// _NET_WM_MOVERESIZE — client-initiated window move/resize;
    /// Steam's own titlebar drags travel exactly this path.
    pub net_moveresize: u32,
    /// Clipboard bridging: CLIPBOARD selection + the TARGETS /
    /// UTF8_STRING atoms used in selection transfers.
    pub clipboard: u32,
    pub targets: u32,
    pub utf8_string: u32,
    /// _NET_WM_STATE values — interned so client state messages
    /// and our property writes share one name→atom map.
    pub net_wm_state_fullscreen: u32,
    pub net_wm_state_max_vert: u32,
    pub net_wm_state_max_horz: u32,
    pub net_wm_state_hidden: u32,
}

/// Requests the compositor queues for the XWM — compositor code
/// stays free of X11 types; the backend loop drains these onto
/// the X connection once per tick.
pub enum XReq {
    /// Focus/unfocus an X window: SetInputFocus plus
    /// _NET_ACTIVE_WINDOW on the root. 0 clears — focus moved
    /// to a Wayland window, so no X window may stay lit.
    Activate(u32),
    /// Push compositor-side placement back to the X window —
    /// move grabs drag the wl_surface, the X window must follow.
    /// `w`/`h` Some = a real resize (tiles, maximize,
    /// fullscreen); None = position only.
    Configure {
        win: u32,
        x: i32,
        y: i32,
        w: Option<u32>,
        h: Option<u32>,
    },
    /// Write the _NET_WM_STATE property after a state change —
    /// X clients read their real state from there.
    SetState {
        win: u32,
        fullscreen: bool,
        maximized: bool,
        minimized: bool,
    },
    /// Stacking: raise in X terms (StackMode::Above).
    Raise(u32),
    /// A Wayland client owns the clipboard — take the X CLIPBOARD
    /// selection so X clients can paste from it.
    OwnClipboard,
    /// Wayland selection cleared — release the X selection.
    ClearClipboard,
}

fn intern(conn: &RustConnection, name: &str) -> u32 {
    conn.intern_atom(false, name.as_bytes())
        .ok()
        .and_then(|c| c.reply().ok().map(|r| r.atom))
        .unwrap_or(0)
}

/// Socketpair helper — returns (ours, child's). Both plain fds;
/// CLOEXEC on ours is set by the caller.
fn spair() -> Option<[i32; 2]> {
    let mut fds = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        warn!("xwm: socketpair failed");
        return None;
    }
    Some(fds)
}

/// Spawn Xwayland and return the Pending handshake + a pollable
/// fd. `disp` is our Wayland display — Xwayland becomes a client
/// via insert_client + WAYLAND_SOCKET — the standard handoff.
/// Returns None when Xwayland isn't installed — a missing X stack
/// must never keep the compositor itself from starting.
pub fn spawn<D: 'static>(
    disp: &Display<D>,
    client_data: Arc<dyn ClientData>,
) -> Option<(Pending, OwnedFd)> {
    let path = which("Xwayland")?;
    // Claim a display number + bind both listen sockets ourselves —
    // Xwayland gets them via -listenfd and never touches /tmp.
    let lease = xsock::open_display_sockets()?;
    let display_num = lease.display;

    let [wl_ours, wl_child] = spair()?;
    let [wm_ours, wm_child] = spair()?;
    unsafe { libc::fcntl(wl_ours, libc::F_SETFD, libc::FD_CLOEXEC) };
    unsafe { libc::fcntl(wm_ours, libc::F_SETFD, libc::FD_CLOEXEC) };

    // Xwayland's Wayland connection — a real client on our display,
    // fed through the socketpair. insert_client hands our end to
    // the server; the child end travels as WAYLAND_SOCKET.
    let wl_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(wl_ours) };
    let client = match disp.handle().insert_client(wl_stream, client_data) {
        Ok(id) => id,
        Err(e) => {
            warn!(?e, "xwm: insert_client failed");
            for fd in [wl_child, wm_ours, wm_child, lease.fds[0], lease.fds[1]] {
                xsock::close(fd);
            }
            return None;
        }
    };

    let (la, lf) = (lease.fds[0], lease.fds[1]);
    let mut cmd = Command::new(path);
    cmd.arg(format!(":{display_num}"))
        .args(["-rootless", "-core", "-terminate"])
        .args(["-listenfd", &la.to_string()])
        .args(["-listenfd", &lf.to_string()])
        .args(["-wm", &wm_child.to_string()])
        // Diagnostics — ODIN_XW_ARGS="-verbose 3" surfaces the
        // server's internal DebugF decisions (allow_commits,
        // viewport, grabs) on our stderr.
        .args(
            std::env::var("ODIN_XW_ARGS")
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
        .env("WAYLAND_SOCKET", wl_child.to_string())
        // Protocol trace lands in our stderr log — the Xwayland
        // startup sequence stalls silently without it.
        .env("WAYLAND_DEBUG", std::env::var("ODIN_XW_DEBUG").unwrap_or_default())
        .stdout(Stdio::null())
        // stderr stays inherited — Xwayland reports failures there.
        ;
    // Rust's Command uses posix_spawn(CLOEXEC_DEFAULT) which would
    // close every non-stdio fd. A pre_exec closure forces the
    // fork+exec path; there we clear CLOEXEC on the fds Xwayland
    // must inherit so they survive the exec with their numbers.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
            for fd in [wl_child, wm_child, la, lf] {
                libc::fcntl(fd, libc::F_SETFD, 0);
            }
            Ok(())
        });
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "xwm: Xwayland spawn failed");
            for fd in [wl_child, wm_ours, wm_child, la, lf] {
                xsock::close(fd);
            }
            return None;
        }
    };
    info!(
        x_display = display_num,
        pid = child.id(),
        "xwm: Xwayland spawned"
    );
    // Parent's copies of the child-side fds die here — only the
    // forked Xwayland holds them now.
    for fd in [wl_child, wm_child, la, lf] {
        xsock::close(fd);
    }
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(wm_ours) };
    stream.set_nonblocking(true).ok();
    // The Connect state machine parses the setup reply; our own
    // 12-byte request is written once the socket takes writes.
    let (connect, _setup_request) = Connect::with_authorization(Vec::new(), Vec::new());
    let loop_fd = rustix::io::dup(&stream).ok()?;
    Some((
        Pending {
            child: Some(child),
            stream: Some(stream),

            connect: Some(connect),
            display: display_num,
            request_sent: false,
            dead: false,
            lease: Some(lease),
            client,
        },
        loop_fd,
    ))
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let p = std::path::Path::new(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    warn!("xwm: Xwayland not on PATH — X11 clients disabled");
    None
}

/// Client-message convenience — used for WM_PROTOCOLS delete
/// and the occasional synthetic notify.
pub fn client_message(conn: &RustConnection, win: Window, ty: u32, data: [u32; 5]) {
    let ev = x11rb::protocol::xproto::ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: win,
        type_: ty,
        data: data.into(),
    };
    let _ = conn.send_event(false, win, EventMask::NO_EVENT, ev);
    let _ = conn.flush();
}
