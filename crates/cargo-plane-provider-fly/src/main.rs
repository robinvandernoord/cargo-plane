//! The fly provider as a plugin binary; cargo-plane also has it built in.

fn main() -> std::process::ExitCode {
    cargo_plane_provider::run(cargo_plane_provider_fly::Fly)
}
