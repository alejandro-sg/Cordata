//! Generate the Rust side of `proto/harness.proto` (the daemon generates its Python side with
//! `scripts/gen-proto.sh`). One schema, two generated halves, nothing hand-written to drift.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../proto");
    let proto = proto_dir.join("harness.proto");
    println!("cargo:rerun-if-changed={}", proto.display());

    // A system protoc (e.g. Homebrew's) wins if PROTOC says so; otherwise use the vendored one,
    // so a fresh checkout builds with no protobuf installed.
    if std::env::var_os("PROTOC").is_none() {
        // SAFETY: build scripts are single-threaded at this point.
        unsafe { std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
    }

    tonic_prost_build::configure()
        .build_client(true)
        // The server half is for the in-process fake daemon the client's tests run against.
        .build_server(true)
        .compile_protos(&[proto], &[proto_dir])?;
    Ok(())
}
