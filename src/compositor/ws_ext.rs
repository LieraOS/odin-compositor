// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_workspace_v1 — workspaces for taskbars and docks.
//!
//! Odin's model is i3's: a workspace belongs to the output
//! showing it, and each enabled output is its own workspace
//! group. The protocol is published as one
//! `ext_workspace_group_handle_v1` per enabled output and one
//! `ext_workspace_handle_v1` per workspace; a workspace joins
//! a group while some output shows it and leaves when it is
//! stolen or its output goes away.
//!
//! Clients batch requests and end them with `commit`: pending
//! activations and assignments apply atomically, then `done`
//! confirms. Compositor-driven changes (keybind switches,
//! outputs coming and going) are pushed by `ws_sync`, a
//! per-tick diff that emits only what moved plus one `done`.

use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{ExtWorkspaceGroupHandleV1, GroupCapabilities},
    ext_workspace_handle_v1::{ExtWorkspaceHandleV1, State, WorkspaceCapabilities},
    ext_workspace_manager_v1::ExtWorkspaceManagerV1,
};
use wayland_server::protocol::wl_output::WlOutput;
use wayland_server::{Client, Resource};

use super::workspace::WS_COUNT;
use super::CompState;

/// Global type re-export — globals.rs registers it under the
/// module path like every other protocol here.
pub use wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1 as ExtWorkspaceManager;

/// A queued request a client sent before its `commit` —
/// applied atomically on commit in the order received.
#[derive(Clone)]
pub enum WsReq {
    Activate(u8),
    Assign(u8, String),
}

/// Per-bind state of one ext_workspace_manager client: its
/// group and workspace handle objects, the last-published
/// group and state of each workspace (for diffing), the
/// pending-request queue, and whether `stop` silenced it.
pub struct WsClient {
    pub mgr: ExtWorkspaceManagerV1,
    pub groups: Vec<(ExtWorkspaceGroupHandleV1, String)>,
    pub handles: Vec<(ExtWorkspaceHandleV1, u8)>,
    pub ws_group: [Option<String>; WS_COUNT as usize],
    pub ws_state: [State; WS_COUNT as usize],
    pub pending: Vec<WsReq>,
    pub stopped: bool,
}

/// Group-handle udata: the output this group stands for, and
/// whether its `output_enter` has been sent (it needs a
/// wl_output resource the client may not have bound yet).
pub struct GroupData {
    pub output: String,
    pub announced: std::sync::atomic::AtomicBool,
}

/// The wl_output resource this client bound for connector
/// `name` — group output_enter/leave must name a resource
/// that client actually holds. Searches the caller's snapshot
/// of `CompState::outputs`.
fn client_output(outs: &[(WlOutput, String)], client: &Client, name: &str) -> Option<WlOutput> {
    let cid = client.id();
    outs.iter()
        .find(|(o, n)| n == name && o.client().map(|c| c.id()) == Some(cid.clone()))
        .map(|(o, _)| o.clone())
}

/// The output currently showing workspace `n`, if any —
/// a workspace's group is the output displaying it.
fn group_of_ws(state: &CompState, n: u8) -> Option<String> {
    state
        .layout
        .outputs
        .values()
        .find(|o| o.enabled && o.ws == n)
        .map(|o| o.name.clone())
}

/// Active = an enabled output shows it. Urgent has no source
/// yet and every workspace stays displayable — none of the
/// nine is ever hidden in the i3 model.
fn state_of_ws(state: &CompState, n: u8) -> State {
    if group_of_ws(state, n).is_some() {
        State::Active
    } else {
        State::empty()
    }
}

impl CompState {
    /// One workspace group's descriptor — sent once after the
    /// `workspace_group` announce event. Takes the output
    /// snapshot rather than `self` so `ws_sync` can call it
    /// while holding `&mut ws_clients`.
    fn ws_group_details(
        outs: &[(WlOutput, String)],
        client: &Client,
        g: &ExtWorkspaceGroupHandleV1,
    ) {
        // create_workspace is not offered — the set of nine is
        // fixed, so the capabilities bitfield stays empty.
        g.capabilities(GroupCapabilities::empty());
        let Some(gd) = g.data::<GroupData>() else {
            return;
        };
        if let Some(out) = client_output(outs, client, &gd.output) {
            g.output_enter(&out);
            gd.announced
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// One workspace's full descriptor — sent once after the
    /// `workspace` announce event. The id is stable across
    /// sessions (workspaces are fixed), the name is for
    /// display, coordinates give it its 1-D slot.
    fn ws_handle_details(&self, h: &ExtWorkspaceHandleV1, n: u8) {
        h.id(format!("ws-{n}"));
        h.name(format!("{}", n + 1));
        h.coordinates((n as u32).to_le_bytes().to_vec());
        // activate and assign are real; deactivate/remove are
        // not meaningful where an output always shows one
        // workspace, so they are not advertised and their
        // requests are ignored.
        h.capabilities(WorkspaceCapabilities::Activate | WorkspaceCapabilities::Assign);
        h.state(state_of_ws(self, n));
    }

    /// Announce groups, workspaces and assignments to a freshly
    /// bound manager, ending with `done` — the initial state
    /// dump the spec asks for after bind.
    pub(crate) fn ws_announce(&mut self, mgr: &ExtWorkspaceManagerV1) {
        let Some(client) = mgr.client() else { return };
        let mut wc = WsClient {
            mgr: mgr.clone(),
            groups: Vec::new(),
            handles: Vec::new(),
            ws_group: [const { None }; WS_COUNT as usize],
            ws_state: [State::empty(); WS_COUNT as usize],
            pending: Vec::new(),
            stopped: false,
        };
        for name in self
            .layout
            .outputs
            .values()
            .filter(|o| o.enabled)
            .map(|o| o.name.clone())
            .collect::<Vec<_>>()
        {
            let Ok(g) = client.create_resource::<ExtWorkspaceGroupHandleV1, GroupData, CompState>(
                &self.dh,
                mgr.version(),
                GroupData {
                    output: name.clone(),
                    announced: false.into(),
                },
            ) else {
                continue;
            };
            mgr.workspace_group(&g);
            Self::ws_group_details(&self.outputs, &client, &g);
            wc.groups.push((g, name));
        }
        for n in 0..WS_COUNT {
            let Ok(h) = client.create_resource::<ExtWorkspaceHandleV1, u8, CompState>(
                &self.dh,
                mgr.version(),
                n,
            ) else {
                continue;
            };
            mgr.workspace(&h);
            self.ws_handle_details(&h, n);
            wc.ws_state[n as usize] = state_of_ws(self, n);
            wc.ws_group[n as usize] = group_of_ws(self, n);
            wc.handles.push((h, n));
        }
        // Workspaces announced unassigned, per spec — now join
        // each to the group showing it.
        for (h, n) in wc.handles.iter() {
            if let Some(group) = &wc.ws_group[*n as usize] {
                if let Some((g, _)) = wc.groups.iter().find(|(_, o)| o == group) {
                    g.workspace_enter(h);
                }
            }
        }
        mgr.done();
        self.ws_clients.push(wc);
    }

    /// Diff real workspace/group state against what each client
    /// was last told; emit only the deltas and one `done` per
    /// client that received anything. Runs every tick. All
    /// `self` reads are snapshotted first — the loop holds
    /// `&mut ws_clients`, so `&self` helpers can't run inside.
    pub fn ws_sync(&mut self) {
        let enabled: Vec<String> = self
            .layout
            .outputs
            .values()
            .filter(|o| o.enabled)
            .map(|o| o.name.clone())
            .collect();
        let grp_of: Vec<Option<String>> = (0..WS_COUNT).map(|n| group_of_ws(self, n)).collect();
        let st_of: Vec<State> = (0..WS_COUNT).map(|n| state_of_ws(self, n)).collect();
        let outs = self.outputs.clone();
        let dh = self.dh.clone();
        self.ws_clients
            .retain(|wc| wc.mgr.is_alive() && !wc.stopped);
        for wc in &mut self.ws_clients {
            let Some(client) = wc.mgr.client() else {
                continue;
            };
            let mut emitted = false;
            // Groups for outputs that appeared since last sync.
            for name in &enabled {
                if wc.groups.iter().any(|(_, o)| o == name) {
                    continue;
                }
                let Ok(g) = client
                    .create_resource::<ExtWorkspaceGroupHandleV1, GroupData, CompState>(
                        &dh,
                        wc.mgr.version(),
                        GroupData {
                            output: name.clone(),
                            announced: false.into(),
                        },
                    )
                else {
                    continue;
                };
                wc.mgr.workspace_group(&g);
                Self::ws_group_details(&outs, &client, &g);
                wc.groups.push((g, name.clone()));
                emitted = true;
            }
            // Groups whose output went away leave cleanly:
            // workspaces out first, then the group itself.
            let gone: Vec<String> = wc
                .groups
                .iter()
                .map(|(_, o)| o.clone())
                .filter(|o| !enabled.contains(o))
                .collect();
            for name in gone {
                if let Some(pos) = wc.groups.iter().position(|(_, o)| o == &name) {
                    let (g, _) = wc.groups.remove(pos);
                    for (h, _) in wc.handles.iter() {
                        g.workspace_leave(h);
                    }
                    g.removed();
                    for slot in wc.ws_group.iter_mut() {
                        if slot.as_deref() == Some(name.as_str()) {
                            *slot = None;
                        }
                    }
                    emitted = true;
                }
            }
            // Workspace assignments and states.
            for (h, n) in &wc.handles {
                if !h.is_alive() {
                    continue;
                }
                let n = *n as usize;
                let grp = grp_of[n].clone();
                if wc.ws_group[n] != grp {
                    if let Some(old) = &wc.ws_group[n] {
                        if let Some((g, _)) = wc.groups.iter().find(|(_, o)| o == old) {
                            g.workspace_leave(h);
                        }
                    }
                    if let Some(new) = &grp {
                        if let Some((g, _)) = wc.groups.iter().find(|(_, o)| o == new) {
                            g.workspace_enter(h);
                        }
                    }
                    wc.ws_group[n] = grp;
                    emitted = true;
                }
                let st = st_of[n];
                if wc.ws_state[n] != st {
                    h.state(st);
                    wc.ws_state[n] = st;
                    emitted = true;
                }
            }
            wc.handles.retain(|(h, _)| h.is_alive());
            // output_enter retries — sent late once the client
            // has bound the matching wl_output.
            for (g, name) in &wc.groups {
                let Some(gd) = g.data::<GroupData>() else {
                    continue;
                };
                if gd.announced.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                if let Some(out) = client_output(&outs, &client, name) {
                    g.output_enter(&out);
                    gd.announced
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    emitted = true;
                }
            }
            if emitted {
                wc.mgr.done();
            }
        }
    }

    /// Apply one client's queued requests — activation switches
    /// the workspace on its natural output; assignment puts the
    /// workspace on the named output with the same steal
    /// semantics as a keybind switch.
    pub(crate) fn ws_apply(&mut self, reqs: Vec<WsReq>) {
        for r in reqs {
            match r {
                WsReq::Activate(n) if n < WS_COUNT => self.switch_ws(n),
                WsReq::Assign(n, out) if n < WS_COUNT => self.show_ws(&out, n),
                _ => {}
            }
        }
    }
}
