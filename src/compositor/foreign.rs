// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! `zxdg_foreign_v2` — transient-for across client boundaries.
//! A toolkit (or a portal/desktop helper) exports a toplevel
//! handle; another client imports the handle and claims one of
//! its own surfaces as a transient child — file dialogs that
//! belong to a different process's window. Shares the same
//! `transient_for` field xdg_toplevel.set_parent uses, so the
//! stacking rule ("children ride above parents") is uniform.

use std::sync::Mutex;
pub use wayland_protocols::xdg::foreign::zv1::server::zxdg_exported_v1::ZxdgExportedV1;
pub use wayland_protocols::xdg::foreign::zv1::server::zxdg_exporter_v1::ZxdgExporterV1;
pub use wayland_protocols::xdg::foreign::zv1::server::zxdg_imported_v1::ZxdgImportedV1;
pub use wayland_protocols::xdg::foreign::zv1::server::zxdg_importer_v1::ZxdgImporterV1;
use wayland_protocols::xdg::foreign::zv1::server::{
    zxdg_exported_v1 as ex1, zxdg_exporter_v1 as exm1, zxdg_imported_v1 as im1,
    zxdg_importer_v1 as imm1,
};
pub use wayland_protocols::xdg::foreign::zv2::server::zxdg_exported_v2::ZxdgExportedV2;
pub use wayland_protocols::xdg::foreign::zv2::server::zxdg_exporter_v2::ZxdgExporterV2;
pub use wayland_protocols::xdg::foreign::zv2::server::zxdg_imported_v2::ZxdgImportedV2;
pub use wayland_protocols::xdg::foreign::zv2::server::zxdg_importer_v2::ZxdgImporterV2;
use wayland_protocols::xdg::foreign::zv2::server::{
    zxdg_exported_v2 as ex, zxdg_exporter_v2 as exm, zxdg_imported_v2 as im,
    zxdg_importer_v2 as imm,
};
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::{CompState, SurfaceData};

/// Exported surface an `zxdg_imported_v2` resolves against.
#[derive(Default)]
pub struct ImportedData {
    pub parent: Mutex<Option<wl_surface::WlSurface>>,
}

/// Random session token — 64 bits of urandom hex; handles must
/// not be guessable across clients.
fn new_handle() -> String {
    use std::io::Read;
    let mut b = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    } else {
        // Fallback entropy — time + counter; still unique
        // within the session even if urandom is blocked.
        static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        b = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ CTR
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                .wrapping_mul(0x9E3779B97F4A7C15))
        .to_ne_bytes();
    }
    format!("odin-fgn-{:016x}", u64::from_ne_bytes(b))
}

impl Dispatch<ZxdgExporterV2, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgExporterV2,
        request: exm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            exm::Request::ExportToplevel { id, surface } => {
                let exported =
                    init.init::<ZxdgExportedV2, wl_surface::WlSurface>(id, surface.clone());
                let handle = new_handle();
                state.exported.insert(handle.clone(), surface);
                exported.handle(handle);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgExportedV2, wl_surface::WlSurface> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgExportedV2,
        request: ex::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Only `destroy` — the export record dies with it.
        if let ex::Request::Destroy = request {
            state.exported.retain(|_, s| s.id() != surf.id());
        }
    }
}

impl Dispatch<ZxdgImporterV2, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgImporterV2,
        request: imm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            imm::Request::ImportToplevel { id, handle } => {
                // Unknown handles still produce a live object —
                // per spec the import is simply inert, and a
                // late `destroyed` event can't arrive for a
                // parent that never existed.
                let parent = state.exported.get(&handle).cloned();
                let imported = init.init::<ZxdgImportedV2, ImportedData>(
                    id,
                    ImportedData {
                        parent: Mutex::new(parent),
                    },
                );
                state.imported.push(imported);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgImportedV2, ImportedData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgImportedV2,
        request: im::Request,
        data: &ImportedData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            im::Request::SetParentOf { surface } => {
                let Some(parent) = data.parent.lock().unwrap().clone() else {
                    return;
                };
                state.set_transient(&surface, Some(&parent));
            }
            _ => {}
        }
    }
}

// ---- v1 protocol — same machinery, older interface names ----
// Clients written before v2 bind zxdg_exporter_v1/importer_v1
// first (GTK4's portal helper still tries v1). Handle spaces
// are shared — a v1 export is importable from v2 and back;
// one handle table serves both interface generations.

impl Dispatch<ZxdgExporterV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgExporterV1,
        request: exm1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            exm1::Request::Export { id, surface } => {
                let exported =
                    init.init::<ZxdgExportedV1, wl_surface::WlSurface>(id, surface.clone());
                let handle = new_handle();
                state.exported.insert(handle.clone(), surface);
                exported.handle(handle);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgExportedV1, wl_surface::WlSurface> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgExportedV1,
        request: ex1::Request,
        surf: &wl_surface::WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ex1::Request::Destroy = request {
            state.exported.retain(|_, s| s.id() != surf.id());
        }
    }
}

impl Dispatch<ZxdgImporterV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgImporterV1,
        request: imm1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            imm1::Request::Import { id, handle } => {
                let parent = state.exported.get(&handle).cloned();
                let imported = init.init::<ZxdgImportedV1, ImportedData>(
                    id,
                    ImportedData {
                        parent: Mutex::new(parent),
                    },
                );
                state.imported_v1.push(imported);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgImportedV1, ImportedData> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgImportedV1,
        request: im1::Request,
        data: &ImportedData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            im1::Request::SetParentOf { surface } => {
                let Some(parent) = data.parent.lock().unwrap().clone() else {
                    return;
                };
                state.set_transient(&surface, Some(&parent));
            }
            _ => {}
        }
    }
}

impl CompState {
    /// Shared transient-for setter — xdg_toplevel.set_parent and
    /// xdg_foreign set_parent_of funnel here. `None` clears.
    /// Restacks so the child lands directly above the parent.
    pub fn set_transient(
        &mut self,
        child: &wl_surface::WlSurface,
        parent: Option<&wl_surface::WlSurface>,
    ) {
        let Some(sd) = child.data::<SurfaceData>() else {
            return;
        };
        *sd.transient_for.lock().unwrap() = parent.cloned();
        self.restack_transients();
        self.damage_everything();
    }

    /// Pull every transient directly above its parent in the
    /// stacking order — repeats until stable so chains
    /// (grandchild→child→parent) settle in one pass.
    pub fn restack_transients(&mut self) {
        for _ in 0..self.order.len() {
            let mut moved = false;
            for i in 0..self.order.len() {
                let Some(par) = self.order[i]
                    .data::<SurfaceData>()
                    .and_then(|sd| sd.transient_for.lock().unwrap().clone())
                else {
                    continue;
                };
                let Some(pidx) = self.order.iter().position(|s| s.id() == par.id()) else {
                    continue;
                };
                if pidx > i {
                    let s = self.order.remove(i);
                    let pidx = self.order.iter().position(|x| x.id() == par.id()).unwrap();
                    self.order.insert(pidx + 1, s);
                    moved = true;
                    break;
                }
            }
            if !moved {
                break;
            }
        }
    }

    /// Exported surfaces that died: every imported object gets
    /// `destroyed`, and children of the dead parent lose their
    /// transient binding — part of the dead-client sweep.
    pub fn foreign_prune(&mut self) {
        let dead: Vec<String> = self
            .exported
            .iter()
            .filter(|(_, s)| !s.is_alive())
            .map(|(h, _)| h.clone())
            .collect();
        if dead.is_empty() {
            self.imported.retain(|o| o.is_alive());
            return;
        }
        let dead_surfs: Vec<wl_surface::WlSurface> = dead
            .iter()
            .filter_map(|h| self.exported.get(h))
            .cloned()
            .collect();
        for h in &dead {
            self.exported.remove(h);
        }
        self.imported.retain(|o| o.is_alive());
        self.imported_v1.retain(|o| o.is_alive());
        for obj in &self.imported {
            let Some(d) = obj.data::<ImportedData>() else {
                continue;
            };
            let mut par = d.parent.lock().unwrap();
            if par
                .as_ref()
                .is_some_and(|p| dead_surfs.iter().any(|d| d.id() == p.id()))
            {
                *par = None;
                obj.destroyed();
            }
        }
        for obj in &self.imported_v1 {
            let Some(d) = obj.data::<ImportedData>() else {
                continue;
            };
            let mut par = d.parent.lock().unwrap();
            if par
                .as_ref()
                .is_some_and(|p| dead_surfs.iter().any(|d| d.id() == p.id()))
            {
                *par = None;
                obj.destroyed();
            }
        }
        // Children whose parent died fall back to free-floating.
        for s in &self.order {
            let Some(sd) = s.data::<SurfaceData>() else {
                continue;
            };
            let mut tf = sd.transient_for.lock().unwrap();
            if tf.as_ref().is_some_and(|p| !p.is_alive()) {
                *tf = None;
            }
        }
    }
}
