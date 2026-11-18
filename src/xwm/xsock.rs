// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! X display socket lease — claims a display number the same way
//! a native X server does: an O_EXCL lock file plus the two
//! listen sockets (abstract + filesystem) we later hand to
//! Xwayland via `-listenfd`, so it never needs to bind (and can
//! never fail EADDRINUSE on a display another X server owns in
//! the abstract namespace only).

use std::io::Write;
use std::os::fd::RawFd;
use tracing::warn;

fn lock_path(n: u32) -> String {
    format!("/tmp/.X{n}-lock")
}

fn sock_path(n: u32) -> String {
    format!("/tmp/.X11-unix/X{n}")
}

/// A claimed X display — holds the bound listen sockets until the
/// child takes them, and unlinks lock + socket files on drop.
pub struct Lease {
    pub display: u32,
    /// Listen fds in canonical order: [0] abstract, [1] filesystem.
    pub fds: [RawFd; 2],
}

impl Drop for Lease {
    fn drop(&mut self) {
        let sock = sock_path(self.display);
        let lock = lock_path(self.display);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&lock);
    }
}

/// sockaddr_un bytes for a unix path; leading \0 marks the Linux
/// abstract namespace. Returns (addr, used_len).
fn unix_addr(path: &str, abstract_: bool) -> (libc::sockaddr_un, usize) {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as u16;
    let base = if abstract_ { 1 } else { 0 };
    for (i, &b) in path.as_bytes().iter().enumerate() {
        addr.sun_path[base + i] = b as i8;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + base + path.len();
    (addr, len)
}

/// Create + bind + listen one unix socket. `path == None` means
/// the abstract-namespace twin whose name starts with \0.
fn open_socket(path: &str, abstract_: bool) -> Option<RawFd> {
    let (addr, len) = unix_addr(path, abstract_);
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        warn!("xsock: socket() failed");
        return None;
    }
    if !abstract_ {
        let _ = std::fs::remove_file(path); // stale socket file
    }
    if unsafe { libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len as u32) } != 0 {
        unsafe { libc::close(fd) };
        return None; // display taken — most likely abstract ns
    }
    if unsafe { libc::listen(fd, 1) } != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    Some(fd)
}

/// Take the O_EXCL lock for display `n`. Returns false when the
/// lock is held by a live process; stale locks get reclaimed.
fn try_lock(n: u32) -> bool {
    let path = lock_path(n);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            // 10-char right-padded pid, like every X server writes.
            let _ = f.write_all(format!("{:>10}", std::process::id()).as_bytes());
            true
        }
        Err(_) => {
            // Lock exists — stale if the recorded pid is gone.
            let pid_s = std::fs::read_to_string(&path).unwrap_or_default();
            let pid: i32 = pid_s.trim().parse().unwrap_or(-1);
            if pid > 0 {
                let alive = unsafe { libc::kill(pid, 0) } == 0;
                if alive {
                    return false;
                }
            }
            let _ = std::fs::remove_file(&path);
            try_lock(n) // retry once the stale lock is gone
        }
    }
}

/// Claim the first free display: lock file first, then both
/// listen sockets. Fails the whole attempt when either socket
/// bind fails so a half-claimed display never lingers.
pub fn open_display_sockets() -> Option<Lease> {
    for n in 0..=32u32 {
        if !try_lock(n) {
            continue;
        }
        let path = sock_path(n);
        let (a, f) = (open_socket(&path, true), open_socket(&path, false));
        match (a, f) {
            (Some(a), Some(f)) => {
                return Some(Lease {
                    display: n,
                    fds: [a, f],
                });
            }
            (a, f) => {
                // Half-bound — release whatever succeeded and the lock.
                if let Some(fd) = a {
                    unsafe { libc::close(fd) };
                }
                if let Some(fd) = f {
                    unsafe { libc::close(fd) };
                    let _ = std::fs::remove_file(&path);
                }
                let _ = std::fs::remove_file(lock_path(n));
            }
        }
    }
    warn!("xsock: no free X display in 0..=32");
    None
}

/// RawFd helper — close a fd without the OwnedFd wrapper when the
/// child takes it anyway.
pub fn close(fd: RawFd) {
    unsafe { libc::close(fd) };
}
