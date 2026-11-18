// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

#version 450
// Sample the bound surface texture — premultiplied blending is
// configured on the pipeline. The wp_alpha_modifier multiplier
// scales the whole premultiplied vector (rgb and a together),
// pushed at offset 32 of the shared push-constant block.
layout(location = 0) in vec2 uv_in;
layout(location = 0) out vec4 color;
layout(binding = 0) uniform texture2D tex;
layout(binding = 1) uniform sampler smp;
// Same block layout as the vertex stage — ndc/uv pad out the
// first 32 bytes so `alpha` lands at offset 32, which is the
// range this stage receives.
layout(push_constant) uniform Push {
    vec4 ndc;
    vec4 uv;
    float alpha;
} p;

void main() {
    color = texture(sampler2D(tex, smp), uv_in) * p.alpha;
}
