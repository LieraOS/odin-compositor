// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

// Frame drain — reads the finished color attachment on the
// graphics engine and packs it BGRA into a host-mappable buffer.
// Runs on the compute engine: no copy-engine sysmem traffic.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<storage, read_write> dst_buf: array<u32>;

// Per-dispatch clip offset — a small uniform buffer holds one
// vec2 slot per damage rect; the descriptor is re-bound with a
// dynamic offset before each rect's dispatch. vkCmdDispatchBase
// proved unreliable (silently ignored as a plain dispatch on
// NVIDIA 580) and naga's WGSL front-end has no push constants.
struct DrainParams {
    offset: vec2<u32>,
}
@group(0) @binding(3) var<uniform> params: DrainParams;

fn to_u8(f: f32) -> u32 {
    let c = clamp(f, 0.0, 1.0);
    return u32(c * 255.0 + 0.5);
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = params.offset + gid.xy;
    let dims = textureDimensions(src);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let c = textureLoad(src, vec2<i32>(p.xy), 0);
    // Sampled B8G8R8A8 arrives as (r,g,b,a); pack to B,G,R,A byte
    // order to match the compositor's framebuffer layout.
    let px = to_u8(c.b) | (to_u8(c.g) << 8u) | (to_u8(c.r) << 16u) | (to_u8(c.a) << 24u);
    dst_buf[p.y * dims.x + p.x] = px;
}
