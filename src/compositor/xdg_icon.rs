// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! xdg_toplevel_icon_v1 — per-window icon assignment so
//! taskbars and window switchers can show real app icons
//! instead of guessing desktop-entry files.
//!
//! Contract details that matter (staging protocol):
//! - `set_icon` is DOUBLE-BUFFERED — the assignment applies on
//!   the toplevel surface's next wl_surface.commit. We snapshot
//!   the icon (name + tightly packed pixel copies) at request
//!   time into `icon_pending`; commit_apply moves it to `icon`.
//! - After `set_icon` the icon object is immutable — further
//!   `set_name`/`add_buffer` raise the `immutable` error.
//! - `add_buffer` accepts wl_shm SQUARE buffers only; anything
//!   else is `invalid_buffer`. Same (size, scale) overrides.
//! - Snapshotting pixels (not buffer refs) is what keeps the
//!   icon alive after the client destroys the icon object —
//!   the spec lets it free the wl_buffer right after.
//!
//! Preferred sizes are advertised at bind so clients with
//! scalable artwork render exactly the squares a taskbar asks.

use wayland_protocols::xdg::toplevel_icon::v1::server::{
    xdg_toplevel_icon_manager_v1 as im, xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
    xdg_toplevel_icon_v1 as ico, xdg_toplevel_icon_v1::XdgToplevelIconV1,
};
use wayland_server::protocol::{wl_buffer, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};

use super::shm::BufInfo;
use super::{CompState, SurfaceData};

pub use wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1 as XdgIconManager;

/// The square sizes a taskbar realistically asks for —
/// advertised via icon_size so scalable clients render sharp.
const PREFERRED_SIZES: &[i32] = &[32, 48, 64, 128];

/// One icon buffer as an owned, tightly packed pixel copy —
/// w*h*4 bytes in the buffer's native format. Outlives the
/// client's wl_buffer by contract. Fields are the data surface
/// for icon consumers (taskbar/switcher renderers) — the
/// compositor itself only produces them.
#[derive(Clone)]
#[allow(dead_code)]
pub struct IconBuf {
    pub w: i32,
    pub h: i32,
    pub scale: i32,
    /// wl_shm::Format value — consumers swizzle by it.
    pub format: u32,
    pub pixels: Vec<u8>,
}

/// The toplevel's assigned icon — name for theme lookup plus
/// every buffer the client supplied, best first.
#[derive(Clone, Default)]
pub struct IconData {
    pub name: String,
    pub bufs: Vec<IconBuf>,
}

/// Per-icon-object state — mutable until set_icon freezes it.
#[derive(Default)]
pub struct IconObjData {
    pub name: std::sync::Mutex<String>,
    /// Pending buffers as (wl_buffer, scale) — resolved to
    /// pixel copies at set_icon time.
    pub bufs: std::sync::Mutex<Vec<(wl_buffer::WlBuffer, i32)>>,
    /// Flipped by the first set_icon — later mutations are the
    /// protocol's `immutable` error.
    pub assigned: std::sync::atomic::AtomicBool,
}

/// Tightly pack one shm buffer's visible rows into an owned
/// copy — stride padding is dropped; nil maps yield nothing.
fn snapshot(buf: &wl_buffer::WlBuffer, scale: i32) -> Option<IconBuf> {
    let bi = buf.data::<BufInfo>()?;
    let m = bi.map.lock().unwrap();
    if m.ptr.is_null() || m.len == 0 {
        return None;
    }
    let row = (bi.w * 4) as usize;
    let mut pixels = Vec::with_capacity(row * bi.h as usize);
    for y in 0..bi.h as usize {
        let start = bi.offset as usize + y * bi.stride as usize;
        let end = start + row;
        if end > m.len {
            return None;
        }
        pixels.extend_from_slice(unsafe { std::slice::from_raw_parts(m.ptr.add(start), row) });
    }
    Some(IconBuf {
        w: bi.w,
        h: bi.h,
        scale,
        format: bi.format,
        pixels,
    })
}

impl Dispatch<XdgToplevelIconManagerV1, ()> for CompState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgToplevelIconManagerV1,
        request: im::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            im::Request::CreateIcon { id } => {
                init.init::<XdgToplevelIconV1, IconObjData>(id, IconObjData::default());
            }
            im::Request::SetIcon { toplevel, icon } => {
                let Some(surf) = toplevel.data::<wl_surface::WlSurface>() else {
                    return;
                };
                let Some(sd) = surf.data::<SurfaceData>() else {
                    return;
                };
                let pending = match icon {
                    Some(icon) => {
                        let Some(data) = icon.data::<IconObjData>() else {
                            return;
                        };
                        // Assignment freezes the icon — the spec
                        // makes it immutable from this point.
                        data.assigned
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        let mut ic = IconData {
                            name: data.name.lock().unwrap().clone(),
                            bufs: Vec::new(),
                        };
                        for (buf, scale) in data.bufs.lock().unwrap().iter() {
                            if let Some(b) = snapshot(buf, *scale) {
                                ic.bufs.push(b);
                            }
                        }
                        // Empty name + no buffers means "reset to
                        // default", same as a null icon.
                        if ic.name.is_empty() && ic.bufs.is_empty() {
                            None
                        } else {
                            Some(ic)
                        }
                    }
                    None => None,
                };
                *sd.icon_pending.lock().unwrap() = Some(pending);
                state.dirty = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgToplevelIconV1, IconObjData> for CompState {
    fn request(
        _: &mut Self,
        _: &Client,
        icon: &XdgToplevelIconV1,
        request: ico::Request,
        data: &IconObjData,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Mutating an assigned icon is the protocol's `immutable`
        // error — but `destroy` must always pass: clients are
        // expected to drop the icon object right after set_icon
        // (the icon stays set), and erroring on destroy kills the
        // connection — that was the Electron crash ("error 2:
        // icon is assigned to a toplevel" on a destroy request).
        let mutating = matches!(
            request,
            ico::Request::SetName { .. } | ico::Request::AddBuffer { .. }
        );
        if mutating && data.assigned.load(std::sync::atomic::Ordering::SeqCst) {
            dh.post_error(
                icon,
                ico::Error::Immutable as u32,
                "icon is assigned to a toplevel".to_string(),
            );
            return;
        }
        match request {
            ico::Request::SetName { icon_name } => {
                *data.name.lock().unwrap() = icon_name;
            }
            ico::Request::AddBuffer { buffer, scale } => {
                let valid = buffer
                    .data::<BufInfo>()
                    .map(|bi| bi.dma.is_none() && bi.w == bi.h)
                    .unwrap_or(false);
                if !valid {
                    dh.post_error(
                        icon,
                        ico::Error::InvalidBuffer as u32,
                        "icon buffers must be square wl_shm buffers".to_string(),
                    );
                    return;
                }
                let mut bufs = data.bufs.lock().unwrap();
                let w = buffer.data::<BufInfo>().map(|bi| bi.w).unwrap_or(0);
                // Same size + scale overrides the earlier entry.
                bufs.retain(|(b, s)| {
                    !(b.data::<BufInfo>().map(|bi| bi.w).unwrap_or(0) == w && *s == scale)
                });
                bufs.push((buffer, scale));
            }
            _ => {}
        }
    }
}

impl CompState {
    /// Bind-time burst: preferred square sizes, then `done` —
    /// required by spec even with no preferences.
    pub(crate) fn xdg_icon_hello(mgr: &XdgToplevelIconManagerV1) {
        for s in PREFERRED_SIZES {
            mgr.icon_size(*s);
        }
        mgr.done();
    }
}
