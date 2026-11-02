use std::fs;
use tempfile::tempdir;

use cordata_floorplan::model::ComponentRole;

#[test]
fn test_component_discovery_and_generation() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Create a mock Cargo workspace
    let root_cargo = r#"
[workspace]
resolver = "2"
members = ["crates/core", "crates/ui"]
"#;
    fs::write(root.join("Cargo.toml"), root_cargo).unwrap();

    let core_dir = root.join("crates/core");
    let ui_dir = root.join("crates/ui");
    fs::create_dir_all(core_dir.join("src")).unwrap();
    fs::create_dir_all(ui_dir.join("src")).unwrap();

    let core_cargo = r#"
[package]
name = "mock-core"
version = "0.1.0"
description = "Mock core library"
"#;
    fs::write(core_dir.join("Cargo.toml"), core_cargo).unwrap();
    fs::write(
        core_dir.join("src/lib.rs"),
        "//! Core primitives\npub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();

    let ui_cargo = r#"
[package]
name = "mock-ui"
version = "0.1.0"
description = "Mock UI client"

[dependencies]
mock-core = { path = "../core" }
"#;
    fs::write(ui_dir.join("Cargo.toml"), ui_cargo).unwrap();
    fs::write(
        ui_dir.join("src/main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();

    // Test generation with active scope
    let active_scope = vec!["crates/core/src/lib.rs".to_string()];
    let fp = cordata_floorplan::generate(root, &active_scope).unwrap();

    assert_eq!(fp.components.len(), 2);
    assert_eq!(fp.metrics.total_components, 2);

    let core = fp.components.iter().find(|c| c.name == "mock-core").unwrap();
    assert_eq!(core.role, ComponentRole::Core);
    assert!(core.status.is_active());

    let ui = fp.components.iter().find(|c| c.name == "mock-ui").unwrap();
    assert_eq!(ui.role, ComponentRole::Interface);
    assert_eq!(ui.dependencies, vec!["mock-core"]);
    assert_eq!(core.dependents, vec!["mock-ui"]);

    // Test terminal rendering
    let term = cordata_floorplan::render_terminal(&fp, false);
    assert!(term.contains("mock-core"));
    assert!(term.contains("mock-ui"));
    assert!(term.contains("ACTIVE"));

    // Test HTML report generation
    let html_path = root.join(".cordata/floorplan.html");
    cordata_floorplan::write_html_report(&fp, &html_path).unwrap();
    assert!(html_path.exists());
    let html_content = fs::read_to_string(&html_path).unwrap();
    assert!(html_content.contains("mock-core"));
    assert!(html_content.contains("mock-ui"));
    assert!(html_content.contains("Architectural Floorplan"));
}
