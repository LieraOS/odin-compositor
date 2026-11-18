// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_workspace_v1 request dispatch — the client-facing half
//! of the workspace protocol. State, announce and the sync
//! pass live in `ws_ext`; here are the three object
//! dispatches: manager (commit/stop), group handle
//! (create_workspace/destroy) and workspace handle
//! (activate/assign/destroy, with deactivate/remove ignored
//! as unadvertised).

use tracing::info;
use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1 as eg, ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
    ext_workspace_handle_v1 as ew, ext_workspace_handle_v1::ExtWorkspaceHandleV1,
    ext_workspace_manager_v1 as ewm, ext_workspace_manager_v1::ExtWorkspaceManagerV1,
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use super::ws_ext::{GroupData, WsReq};
use super::CompState;

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for CompState {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let mgr = init.init::<ExtWorkspaceManagerV1, ()>(resource, ());
        state.ws_announce(&mgr);
        info!("workspace manager bound");
    }
}

/// Queue a request onto the manager `handle` belongs to — the
/// client whose object carried it, found by client id.
fn queue_req(state: &mut CompState, handle: &impl Resource, req: WsReq) {
    let Some(cid) = handle.client().map(|c| c.id()) else {
        return;
    };
    if let Some(wc) = state
        .ws_clients
        .iter_mut()
        .find(|wc| wc.mgr.client().map(|c| c.id()) == Some(cid.clone()))
    {
        wc.pending.push(req);
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        mgr: &ExtWorkspaceManagerV1,
        request: ewm::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ewm::Request::Commit => {
                let Some(wc) = state
                    .ws_clients
                    .iter_mut()
                    .find(|wc| wc.mgr.id() == mgr.id())
                else {
                    return;
                };
                let pending = std::mem::take(&mut wc.pending);
                state.ws_apply(pending);
                mgr.done();
            }
            // stop = finished now: the event is a destructor —
            // the object dies right after, so no further events
            // go out to it.
            ewm::Request::Stop => {
                if let Some(wc) = state
                    .ws_clients
                    .iter_mut()
                    .find(|wc| wc.mgr.id() == mgr.id())
                {
                    wc.stopped = true;
                }
                mgr.finished();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, GroupData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        g: &ExtWorkspaceGroupHandleV1,
        request: eg::Request,
        _: &GroupData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            // create_workspace isn't in our capabilities — the
            // nine are fixed — so the request is ignored per spec.
            eg::Request::CreateWorkspace { .. } => {}
            eg::Request::Destroy => {
                let gid = g.id();
                for wc in &mut state.ws_clients {
                    wc.groups.retain(|(h, _)| h.id() != gid);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, u8> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        h: &ExtWorkspaceHandleV1,
        request: ew::Request,
        n: &u8,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ew::Request::Activate => queue_req(state, h, WsReq::Activate(*n)),
            ew::Request::Assign { workspace_group } => {
                if let Some(gd) = workspace_group.data::<GroupData>() {
                    queue_req(state, h, WsReq::Assign(*n, gd.output.clone()));
                }
            }
            // deactivate/remove aren't in our capabilities — an
            // output always shows a workspace and the set is
            // fixed — so both are ignored, per spec.
            ew::Request::Deactivate | ew::Request::Remove => {}
            ew::Request::Destroy => {
                // h.id() would resolve to the `id` event, not
                // Resource::id — qualify explicitly.
                let hid = Resource::id(h);
                for wc in &mut state.ws_clients {
                    wc.handles.retain(|(x, _)| Resource::id(x) != hid);
                }
            }
            _ => {}
        }
    }
}
