//! The engine-owned decoder-classification fixtures must stay judged, not
//! parked.
//!
//! `wasmtest run` exits zero on `pending`, so nothing else in the suite
//! enforces the wasm sweep's "0 pendings" claim; this test does it for the
//! decoder's malformed/unsupported boundary.

use std::process::Command;

#[test]
fn reserved_encodings_are_malformed_not_pending() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = format!("{manifest}/fixtures/decoder-classification.wast");
    let output = Command::new(env!("CARGO_BIN_EXE_wasmtest"))
        .current_dir(manifest)
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("failed to spawn wasmtest");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wasmtest run exited {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code()
    );

    let total = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("total:"))
        .unwrap_or_else(|| panic!("no `total:` line in:\n{stdout}"));

    // Every case must decode to Malformed. A pending means one is being
    // reported as a feature a later cut owns, which the runner accepts
    // silently; the pass count is pinned so a case cannot quietly stop being
    // converted.
    assert!(
        total.contains("6 pass, 0 fail, 0 pending"),
        "expected `6 pass, 0 fail, 0 pending`, got: {total}"
    );
}
