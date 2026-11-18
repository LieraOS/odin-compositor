// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! ext_workspace_v1 end-to-end — real taskbar-style client.
//!
//! Binds the manager plus one wl_output, checks the initial
//! announce burst (one group per enabled output, nine
//! workspaces, ids/names/coordinates/capabilities/state,
//! workspace_enter + output_enter, done), then exercises the
//! request path: activate ws 5 → commit → expect it to become
//! active; assign ws 7 to the group → commit; stop → finished.
//!
//!   ODIN_BACKEND=nested ./odin &
//!   ./ws_proto_test

use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1 as ewg, ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
    ext_workspace_handle_v1 as ewh, ext_workspace_handle_v1::ExtWorkspaceHandleV1,
    ext_workspace_manager_v1 as ewm, ext_workspace_manager_v1::ExtWorkspaceManagerV1,
};

#[derive(Default)]
struct App {
    mgr: Option<ExtWorkspaceManagerV1>,
    group: Option<ExtWorkspaceGroupHandleV1>,
    groups: u32,
    handles: Vec<ExtWorkspaceHandleV1>,
    output: Option<wl_output::WlOutput>,
    /// (id, name, state-bits as reported) per workspace.
    ws_info: Vec<(String, String, u32)>,
    enters: u32,
    leaves: u32,
    out_enter: u32,
    out_leave: u32,
    dones: u32,
    finished: bool,
}

fn bits(w: WEnum<ewh::State>) -> u32 {
    match w {
        WEnum::Value(v) => v.bits(),
        WEnum::Unknown(u) => u,
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        reg: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_output" => app.output = Some(reg.bind(name, version.min(4), qh, ())),
                "ext_workspace_manager_v1" => app.mgr = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for App {
    /// Both announce events carry a new_id — opcode 0 is
    /// `workspace_group`, opcode 1 is `workspace`.
    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            0 => qh.make_data::<ExtWorkspaceGroupHandleV1, ()>(()),
            1 => qh.make_data::<ExtWorkspaceHandleV1, ()>(()),
            _ => unreachable!(),
        }
    }

    fn event(
        app: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ewm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ewm::Event::WorkspaceGroup { workspace_group } => {
                app.groups += 1;
                if app.group.is_none() {
                    app.group = Some(workspace_group);
                }
            }
            ewm::Event::Workspace { workspace } => {
                app.handles.push(workspace);
                app.ws_info.push((String::new(), String::new(), 0));
            }
            ewm::Event::Done => app.dones += 1,
            ewm::Event::Finished => app.finished = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ExtWorkspaceGroupHandleV1,
        event: ewg::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ewg::Event::OutputEnter { .. } => app.out_enter += 1,
            ewg::Event::OutputLeave { .. } => app.out_leave += 1,
            ewg::Event::WorkspaceEnter { .. } => app.enters += 1,
            ewg::Event::WorkspaceLeave { .. } => app.leaves += 1,
            ewg::Event::Capabilities { capabilities } => {
                println!("GROUP-CAPS {capabilities:?}");
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        h: &ExtWorkspaceHandleV1,
        event: ewh::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Events arrive per handle — find this handle's slot by
        // proxy id; announce order already pushed the entry.
        let idx = app.handles.iter().position(|x| x.id() == h.id());
        let Some(i) = idx else { return };
        match event {
            ewh::Event::Id { id } => app.ws_info[i].0 = id,
            ewh::Event::Name { name } => app.ws_info[i].1 = name,
            ewh::Event::State { state } => {
                let b = bits(state);
                app.ws_info[i].2 = b;
                println!("WS-STATE i={i} active={}", b & 1);
            }
            ewh::Event::Capabilities { capabilities } => {
                println!("WS-CAPS {capabilities:?}");
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let mut app = App::default();
    let _reg = conn.display().get_registry(&qh, ());
    eq.roundtrip(&mut app).unwrap();
    // Second roundtrip: the manager bind's announce burst plus
    // the per-tick output_enter retry both land here.
    eq.roundtrip(&mut app).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    eq.roundtrip(&mut app).unwrap();

    println!(
        "ANNOUNCE groups={} ws={} enters={} out_enter={} dones={}",
        app.groups,
        app.handles.len(),
        app.enters,
        app.out_enter,
        app.dones
    );
    for (id, name, st) in &app.ws_info {
        println!("WS {id} name={name} state={st}");
    }

    // activate workspace 5 (index 4) — commit applies it.
    app.handles[4].activate();
    app.mgr.as_ref().unwrap().commit();
    eq.roundtrip(&mut app).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    eq.roundtrip(&mut app).unwrap();
    let active5 = app
        .ws_info
        .iter()
        .find(|(id, ..)| id == "ws-4")
        .map(|e| e.2 & 1 == 1)
        .unwrap_or(false);
    println!("ACTIVATE-5 active={active5} dones={}", app.dones);

    // assign workspace 7 (index 6) to our group, then commit.
    if let Some(g) = &app.group {
        app.handles[6].assign(g);
        app.mgr.as_ref().unwrap().commit();
        eq.roundtrip(&mut app).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        eq.roundtrip(&mut app).unwrap();
    }
    let active7 = app
        .ws_info
        .iter()
        .find(|(id, ..)| id == "ws-6")
        .map(|e| e.2 & 1 == 1)
        .unwrap_or(false);
    println!("ASSIGN-7 active={active7} dones={}", app.dones);

    app.mgr.as_ref().unwrap().stop();
    eq.roundtrip(&mut app).unwrap();
    println!(
        "RESULT groups={} ws={} enters={} leaves={} out_enter={} dones={} finished={}",
        app.groups,
        app.handles.len(),
        app.enters,
        app.leaves,
        app.out_enter,
        app.dones,
        app.finished
    );
}
