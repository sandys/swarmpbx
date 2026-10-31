fn main() -> std::io::Result<()> {
    let proto = "../../proto/agentdialer/session/v1/session.proto";
    println!("cargo:rerun-if-changed={proto}");
    tonic_prost_build::configure().compile_protos(&[proto], &["../../proto"])
}
