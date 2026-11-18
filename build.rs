// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Build-time shader compile — GLSL → SPIR-V via naga (pure Rust,
//! no external glslang). One module per stage, both embedded via
//! include_bytes! in the vulkan pipeline.

use std::path::Path;

fn compile(path: &str, stage: naga::ShaderStage, out_name: &str) {
    println!("cargo:rerun-if-changed={}", path);
    let src = std::fs::read_to_string(path).expect("shader readable");
    let module = match naga::front::glsl::Frontend::default()
        .parse(&naga::front::glsl::Options::from(stage), &src)
    {
        Ok(m) => m,
        Err(e) => panic!("{} parse failed: {:?}", path, e),
    };
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap_or_else(|e| panic!("{} validation failed: {}", path, e));
    let spv =
        naga::back::spv::write_vec(&module, &info, &naga::back::spv::Options::default(), None)
            .unwrap_or_else(|e| panic!("{} spirv emit failed: {:?}", path, e));
    let mut bytes = Vec::with_capacity(spv.len() * 4);
    for w in spv {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join(out_name);
    std::fs::write(out, bytes).expect("spv written");
}

fn main() {
    compile(
        "shaders/quad.vert",
        naga::ShaderStage::Vertex,
        "quad_vs.spv",
    );
    compile(
        "shaders/quad.frag",
        naga::ShaderStage::Fragment,
        "quad_fs.spv",
    );
    println!("cargo:rerun-if-changed=shaders/drain.wgsl");
    // WGSL drain — naga's GLSL frontend lacks storage buffers,
    // WGSL handles them natively (no push constants needed; the
    // shader derives dims from textureDimensions).
    let src = std::fs::read_to_string("shaders/drain.wgsl").expect("drain shader readable");
    let module = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| {
        panic!("shaders/drain.wgsl parse failed: {}", e);
    });
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap_or_else(|e| panic!("drain validation failed: {}", e));
    let spv =
        naga::back::spv::write_vec(&module, &info, &naga::back::spv::Options::default(), None)
            .unwrap_or_else(|e| panic!("drain spirv emit failed: {:?}", e));
    let mut bytes = Vec::with_capacity(spv.len() * 4);
    for w in spv {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    std::fs::write(
        Path::new(&std::env::var("OUT_DIR").unwrap()).join("drain_cs.spv"),
        bytes,
    )
    .expect("drain spv written");
}
