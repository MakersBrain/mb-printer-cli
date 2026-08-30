// SPDX-License-Identifier: AGPL-3.0-or-later
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // SAFETY: Cargo executes this single-threaded build script before generated
    // code is compiled; no other thread can observe a partially changed value.
    unsafe { std::env::set_var("PROTOC", protoc) };
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(
            &["proto/makersbrain/print/agent/v1/agent.proto"],
            &["proto"],
        )?;
    println!("cargo:rerun-if-changed=proto/makersbrain/print/agent/v1/agent.proto");
    Ok(())
}
