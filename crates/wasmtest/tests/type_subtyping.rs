//! The subtype-declaration fixture must stay judged, not parked.
//!
//! `waspec/test/core/gc/type-subtyping.wast` cannot be swept — its `multiple
//! supertypes` case uses text the pinned `wast` grammar rejects, so the file is
//! excluded from the corpus — which would leave the rules it covers ungated.
//! `fixtures/type-subtyping.wast` pins them in a form the converter accepts.

use std::process::Command;

#[test]
fn subtype_declarations_are_validated() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = format!("{manifest}/fixtures/type-subtyping.wast");
    let output = Command::new(env!("CARGO_BIN_EXE_wasmtest"))
        .current_dir(manifest)
        .args(["run", "--strict", &fixture])
        .output()
        .expect("failed to spawn wasmtest");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wasmtest run --strict exited {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code()
    );

    let total = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("total:"))
        .unwrap_or_else(|| panic!("no `total:` line in:\n{stdout}"));

    // Every case must be judged: a fail means a rule is missing, a pending that
    // the module is parked. The pass count is pinned so a case cannot quietly
    // stop being converted.
    assert!(
        total.contains("11 pass, 0 fail, 0 pending"),
        "expected `11 pass, 0 fail, 0 pending`, got: {total}"
    );
}
