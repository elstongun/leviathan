use std::path::Path;
use std::process::Command;

use leviathan::config::Config;
use serde_json::{Value, json};

fn run(index: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_leviathan"))
        .env_remove("LEVIATHAN_INDEX")
        .env_remove("LEVIATHAN_CONFIG")
        .current_dir(index.parent().unwrap())
        .arg("--index")
        .arg(index)
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn github_issue_export_recipe_works_through_the_cli_offline() {
    let directory = tempfile::tempdir().unwrap();
    let index = directory.path().join("issues.db");
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/github-issues");
    let fixture = example.join("issues.json");
    let config = example.join("leviathan.toml");
    let original: Vec<Value> = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    let inferred = run(&index, &["init", fixture.to_str().unwrap(), "-o", "-"]);
    assert_eq!(inferred["profile"]["sampled"], 3);
    let _: Config = serde_json::from_value(inferred["config"].clone()).unwrap();

    let report = run(&index, &["index", "--config", config.to_str().unwrap()]);
    assert_eq!(report["record_count"], 3);
    assert_eq!(report["skipped_lines"], 0);
    let search = run(&index, &["search", "-g", "Release 1.0", "acknowledged chunk"]);
    assert_eq!(search["results"].as_array().unwrap().len(), 1);
    assert_eq!(search["results"][0]["id"], original[0]["url"]);

    let filtered = run(&index, &["search", "bug", "--where", "state=OPEN", "--where", "labels[].name=bug"]);
    assert_eq!(filtered["results"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["results"][0]["id"], original[0]["url"]);
    let unassigned = run(&index, &["search", "offline cache"]);
    assert_eq!(unassigned["results"][0]["id"], original[2]["url"]);
    assert_eq!(original[2]["milestone"], Value::Null);
    assert_eq!(original[2]["labels"], json!([]));

    let retrieved = run(&index, &["get", original[0]["url"].as_str().unwrap()]);
    assert_eq!(retrieved, original[0], "the original export record is retained");
}
