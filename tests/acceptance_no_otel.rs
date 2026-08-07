//! The feature-gating acceptance criterion.
//!
//! Every crate in the fleet takes this one as a dependency, so a crate that leaked into a
//! default build would be paid for by all eighteen of them, and by every desktop install
//! that runs `cargo install`.

use std::process::Command;

/// `cargo tree` with default features shows no `opentelemetry*` crate.
#[test]
fn default_build_pulls_no_opentelemetry() {
    if cfg!(feature = "otel") {
        // This test binary was compiled with the feature on, so it cannot say anything
        // about a default build. `just check` runs the same assertion with default
        // features, and `just check-all` runs both.
        return;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");

    let output = Command::new(cargo)
        .args(["tree", "--edges", "normal", "--prefix", "none", "--manifest-path", manifest])
        .output()
        .expect("cargo tree must run");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8_lossy(&output.stdout);
    let leaked: Vec<&str> = tree
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("opentelemetry"))
        .collect();

    assert!(
        leaked.is_empty(),
        "a default build must resolve no opentelemetry crate, found: {leaked:?}"
    );
}
