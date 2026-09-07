//! Compile the vendored Modal proto slice into the gRPC client.
//!
//! `protox` is a pure-Rust protobuf compiler, so no `protoc` binary has to be
//! present on the build machine (the node builds in CI images and on hosts
//! that do not carry one). It produces the descriptor set that tonic-build
//! then turns into the client stubs.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/modal.proto";
    println!("cargo:rerun-if-changed={}", proto);

    let descriptors = protox::compile([proto], ["proto"])?;
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_fds(descriptors)?;
    Ok(())
}
