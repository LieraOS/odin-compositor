// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `zwlr_output_power_manager_v1` — per-output DPMS for shell
//! clients (idle-blanking panels, lock screens). `set_mode`
//! records the desired power per connector name in
//! `state.power`; the backends apply it on the next frame —
//! legacy KMS blanks via `set_crtc(None)`, the nested backend
//! stops presenting — and the mode event confirms back.

use wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1 as opm, zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1 as op, zwlr_output_power_v1::ZwlrOutputPowerV1,
};
use wayland_server::protocol::wl_output;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource, WEnum};

use super::CompState;

pub use wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1 as ZwlrPowerManager;

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrOutputPowerManagerV1,
        request: opm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            opm::Request::GetOutputPower { id, output } => {
                let obj = init.init::<ZwlrOutputPowerV1, wl_output::WlOutput>(id, output.clone());
                // The initial mode event reports the live state —
                // unknown outputs default to on, matching
                // `CompState::powered`.
                let name = state
                    .outputs
                    .iter()
                    .find(|(o, _)| o.id() == output.id())
                    .map(|(_, n)| n.clone());
                let on = name.map(|n| state.powered(&n)).unwrap_or(true);
                obj.mode(if on { op::Mode::On } else { op::Mode::Off });
            }
            opm::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputPowerV1, wl_output::WlOutput> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        obj: &ZwlrOutputPowerV1,
        request: op::Request,
        output: &wl_output::WlOutput,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            op::Request::SetMode { mode } => {
                // Resolve the connector name the way gamma does —
                // `state.outputs` pairs each wl_output with its
                // kernel name.
                let name = state
                    .outputs
                    .iter()
                    .find(|(o, _)| o.id() == output.id())
                    .map(|(_, n)| n.clone());
                if let (WEnum::Value(m), Some(name)) = (mode, name) {
                    let on = matches!(m, op::Mode::On);
                    state.power.insert(name, on);
                    // Confirm the accepted mode back — backends
                    // apply it on their next frame tick.
                    obj.mode(m);
                }
            }
            op::Request::Destroy => {}
            _ => {}
        }
    }
}

impl CompState {
    /// DPMS state for an output — unset means powered on.
    pub fn powered(&self, name: &str) -> bool {
        self.power.get(name).copied().unwrap_or(true)
    }
}
