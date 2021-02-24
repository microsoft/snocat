extern crate tonic_build;
fn main() -> Result<(), Box<dyn std::error::Error>> {
  tonic_build::configure().build_server(false).compile(
    &[
      "src/proto/delegation.proto",
      "src/proto/eventing.proto",
      "src/proto/ffi.proto",
    ],
    &["src/proto"],
  )?;
  Ok(())
}
