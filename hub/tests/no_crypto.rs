//! The hub never links the end-to-end crypto (v1.1, docs/PLANO-v1.1-E2EE.md "Threat model"): the
//! envelope lives in `vox-e2ee`, which only the apps use. This asks cargo for the hub's dependency
//! tree (`cargo tree -p voxhub-server`, every target, offline, from Cargo.lock) and fails if
//! X25519, its curve or `vox-e2ee` is in it, so a dependency added by mistake is caught like any
//! other failing test.

use std::process::Command;

#[test]
fn the_hub_has_no_x25519() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let out = Command::new(cargo)
        .args(["tree", "--offline", "--locked", "--prefix", "none", "--target", "all", "-p", "voxhub-server", "--manifest-path"])
        .arg(&manifest)
        .output()
        .expect("running cargo tree");
    let tree = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "cargo tree failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(tree.lines().any(|l| l.starts_with("voxhub-server ")), "not the hub's tree:\n{tree}");
    assert!(tree.lines().any(|l| l.starts_with("rustls ")), "the tree looks incomplete:\n{tree}");
    for banned in ["x25519", "curve25519", "vox-e2ee"] {
        let hits: Vec<&str> = tree.lines().filter(|l| l.contains(banned)).collect();
        assert!(hits.is_empty(), "the hub links {banned}: {hits:?}");
    }
}
