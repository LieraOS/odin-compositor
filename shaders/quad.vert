// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

#version 450
// odin compositor quad vertex shader — six vertices generated from
// gl_VertexIndex; dst rect (ndc) and src rect (uv) come from push
// constants.
layout(push_constant) uniform Push {
    vec4 ndc;   // x0,y0,x1,y1 clip space
    vec4 uv;    // u0,v0,u1,v1 texel space
} p;

layout(location = 0) out vec2 uv_out;

void main() {
    const vec2 quad[6] = vec2[](
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0));
    vec2 c = quad[gl_VertexIndex];
    gl_Position = vec4(mix(p.ndc.xy, p.ndc.zw, c), 0.0, 1.0);
    uv_out = mix(p.uv.xy, p.uv.zw, c);
}
