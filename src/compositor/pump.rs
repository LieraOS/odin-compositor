// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Client accept + request pump — split from mod.rs for the
//! file-size budget.

use super::{CData, CompState};
use tracing::{info, warn};
use wayland_server::ListeningSocket;

/// Accept pending clients and pump their messages — called by the
/// backend loop whenever the socket fd is readable.
pub fn pump(state: &mut CompState, listening: &ListeningSocket) -> anyhow::Result<()> {
    loop {
        match listening.accept() {
            Ok(Some(stream)) => {
                // Identify the peer before insert — a client that
                // connects and then stalls before its first request
                // must still be nameable in the log.
                let pid = rustix::net::sockopt::socket_peercred(&stream)
                    .map(|c| c.pid.as_raw_nonzero().get())
                    .unwrap_or(0);
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .map(|s| s.trim_end().to_string())
                    .unwrap_or_default();
                let mut handle = state.display.as_ref().unwrap().handle();
                if let Err(err) = handle.insert_client(
                    stream,
                    std::sync::Arc::new(CData {
                        pid,
                        comm: comm.clone(),
                    }),
                ) {
                    warn!(?err, pid, "client insert failed");
                } else {
                    info!(pid, comm, "wayland client connected");
                }
            }
            _ => break,
        }
    }
    // Take the display out while dispatching — it borrows state.
    let mut display = state.display.take().unwrap();
    let res = display.dispatch_clients(state);
    match &res {
        Ok(n) if *n > 0 => info!(dispatched = n, "client requests dispatched"),
        Ok(_) => {}
        Err(e) => warn!(?e, "dispatch_clients failed"),
    }
    let flush = display.flush_clients();
    state.display = Some(display);
    res.map_err(|e| anyhow::anyhow!("dispatch_clients: {e}"))?;
    flush?;
    Ok(())
}
