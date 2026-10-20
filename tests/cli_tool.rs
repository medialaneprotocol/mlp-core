//! The command-line tool's own suite: one test per command, a test that the
//! JSON shape is stable, and a test that the exit-status contract holds.
//!
//! The suite drives the library's dispatcher, so it asserts on exactly what
//! the binary prints. The one test that needs the binary itself is the one
//! that reads standard input.

use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Run the tool with an argument vector and return its outcome.
fn run(arguments: &[&str]) -> mlp_core::cli::Outcome {
    let args: Vec<String> = std::iter::once("mlp-core".to_string())
        .chain(arguments.iter().map(|value| (*value).to_string()))
        .collect();
    mlp_core::cli::run(&args)
}

/// The JSON the tool printed.
fn json_of(outcome: &mlp_core::cli::Outcome) -> Value {
    serde_json::from_str(&outcome.stdout).expect("the tool printed JSON")
}

/// A fragment's bytes, as the tool builds them.
const FRAGMENT_HEX: &str =
    "4d4c5000010201060700000000000100040000000a000108070000000000000000010203";

#[test]
fn fragment_build_prints_the_bytes_and_the_fields() {
    let outcome = run(&[
        "fragment",
        "build",
        "--class",
        "media",
        "--priority",
        "6",
        "--flags",
        "fin",
        "--object-id",
        "7",
        "--tlv",
        "UPDATE_ID=0700000000000000",
        "--body",
        "00010203",
    ]);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outcome.stdout.contains("fragment built"));
    assert!(outcome.stdout.contains(FRAGMENT_HEX));
    assert!(outcome.stdout.contains("UPDATE_ID"));
}

#[test]
fn fragment_inspect_reads_what_fragment_build_wrote() {
    let outcome = run(&["fragment", "inspect", "--hex", FRAGMENT_HEX]);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outcome.stdout.contains("fragment inspected"));
    assert!(outcome.stdout.contains("class            media"));
    assert!(outcome.stdout.contains("priority         6"));
    assert!(outcome.stdout.contains("body             4 bytes"));
}

#[test]
fn object_assemble_reports_what_the_assembler_decided() {
    let first = "4d4c5000010200060700000000000200000000000000";
    let second = "4d4c5000010201060700000001000200000000000000";
    let outcome = run(&[
        "object", "assemble", "--hex", first, "--hex", second, "--json",
    ]);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let document = json_of(&outcome);
    assert_eq!(document["status"], "complete");
    assert_eq!(document["object"]["object_id"], 7);
    assert_eq!(document["reason"], Value::Null);
}

#[test]
fn ack_build_and_ack_inspect_round_trip() {
    let built = run(&[
        "ack",
        "build",
        "--highest-applied-update-id",
        "10",
        "--ack-bitmap",
        "15",
        "--applied-cell-count",
        "510",
        "--converged",
        "true",
        "--json",
    ]);
    assert_eq!(built.code, 0, "{}", built.stderr);
    let document = json_of(&built);
    let bytes = document["bytes_hex"].as_str().expect("the bytes");

    let read = run(&["ack", "inspect", "--hex", bytes, "--json"]);
    assert_eq!(read.code, 0, "{}", read.stderr);
    let document = json_of(&read);
    assert_eq!(document["ack"]["highest_applied_update_id"], 10);
    assert_eq!(document["window"]["applied_update_ids"], json!([10, 8, 6]));
    assert_eq!(document["window"]["size"], 64);
    assert_eq!(document["cell_counts"]["applied_cell_count"], 510);
}

#[test]
fn limits_lists_the_caps_the_timeouts_and_the_invariants() {
    let outcome = run(&["limits"]);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outcome.stdout.contains("MAX_OPEN_OBJECTS"));
    assert!(outcome.stdout.contains("TARGET_FRAGMENT_PAYLOAD"));
    assert!(outcome.stdout.contains("assembly timeout"));
    assert!(outcome.stdout.contains("INV-10"));
}

#[test]
fn conformance_runs_the_published_corpus() {
    let corpus = corpus_directory();
    let Some(corpus) = corpus else {
        // The corpus is a sibling repository. When it is not next to this one,
        // the command must say so with its own exit status, not crash.
        let outcome = run(&["conformance"]);
        assert_eq!(outcome.code, 3, "{}", outcome.stderr);
        assert!(outcome.stderr.contains("corpus"));
        return;
    };
    let outcome = run(&["conformance", "--corpus", corpus.as_str(), "--json"]);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let document = json_of(&outcome);
    assert_eq!(document["summary"]["failed"], 0);
    assert!(document["summary"]["total"].as_u64().unwrap_or(0) > 0);
    for scenario in document["scenarios"].as_array().expect("scenarios") {
        assert_eq!(scenario["verdict"], "match", "{scenario}");
    }
}

/// The corpus directory, when it is next to this repository.
fn corpus_directory() -> Option<String> {
    if let Ok(path) = std::env::var("MLP_CONFORMANCE_DIR")
        && !path.is_empty()
        && std::path::Path::new(&path).is_dir()
    {
        return Some(path);
    }
    let sibling = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .join("mlp-conformance");
    if sibling.is_dir() {
        return Some(sibling.to_string_lossy().to_string());
    }
    None
}

#[test]
fn the_json_shape_is_stable() {
    // The shape a script may depend on: every command's JSON, exactly.
    let built = run(&[
        "fragment",
        "build",
        "--class",
        "media",
        "--priority",
        "6",
        "--flags",
        "fin",
        "--object-id",
        "7",
        "--tlv",
        "UPDATE_ID=0700000000000000",
        "--body",
        "00010203",
        "--json",
    ]);
    assert_eq!(built.code, 0, "{}", built.stderr);
    assert_eq!(
        json_of(&built),
        json!({
            "command": "fragment.build",
            "bytes_hex": FRAGMENT_HEX,
            "bytes_len": 36,
            "fragment": {
                "version": 1,
                "class": "media",
                "flags": ["fin"],
                "priority": 6,
                "object_id": 7,
                "fragment_index": 0,
                "fragment_count": 1,
                "payload_len": 4,
                "header_ext_len": 10,
            },
            "tlvs": [{
                "kind": 1,
                "name": "UPDATE_ID",
                "classification": "named",
                "length": 8,
                "value_hex": "0700000000000000",
                "value": { "update_id": 7 },
            }],
            "body": {
                "length": 4,
                "sha256": "054edec1d0211f624fed0cbca9d4f9400b0e491c43742af2c5b0abebf0c990d8",
            },
        })
    );

    let inspected = run(&["fragment", "inspect", "--hex", FRAGMENT_HEX, "--json"]);
    assert_eq!(inspected.code, 0, "{}", inspected.stderr);
    let document = json_of(&inspected);
    assert_eq!(document["command"], "fragment.inspect");
    assert_eq!(document["carrier"], "ws");
    assert_eq!(document["bytes_len"], 36);
    assert_eq!(document["header"]["class"], "media");
    assert_eq!(document["tlvs"][0]["name"], "UPDATE_ID");
    assert_eq!(document["body"]["length"], 4);

    let assembled = run(&[
        "object",
        "assemble",
        "--hex",
        "4d4c5000010201060700000000000100000000000000",
        "--json",
    ]);
    assert_eq!(assembled.code, 0, "{}", assembled.stderr);
    let document = json_of(&assembled);
    assert_eq!(document["command"], "object.assemble");
    assert_eq!(document["status"], "complete");
    assert_eq!(document["fragments"], 1);
    assert_eq!(document["dropped_fragments"], 0);
    assert_eq!(document["open_objects"], 0);
    assert_eq!(document["open_bytes"], 0);
    assert_eq!(document["object"]["object_id"], 7);
    assert_eq!(document["object"]["class"], "media");
    assert_eq!(document["object"]["payload_len"], 0);
    assert_eq!(document["object"]["flags"], json!(["fin"]));
    assert_eq!(document["object"]["is_anchor"], false);
    assert_eq!(document["object"]["is_duplicate"], false);
    assert_eq!(document["object"]["echo_of_object_id"], Value::Null);
    assert_eq!(document["object"]["deadline_epoch_ms"], Value::Null);
    assert_eq!(document["object"]["update"], Value::Null);
    assert_eq!(document["object"]["tlvs"], json!([]));

    let acknowledged = run(&[
        "ack",
        "build",
        "--highest-applied-update-id",
        "10",
        "--ack-bitmap",
        "15",
        "--applied-cell-count",
        "510",
        "--converged",
        "true",
        "--json",
    ]);
    assert_eq!(acknowledged.code, 0, "{}", acknowledged.stderr);
    let document = json_of(&acknowledged);
    assert_eq!(document["command"], "ack.build");
    assert_eq!(document["ack"]["highest_applied_update_id"], 10);
    assert_eq!(document["ack"]["ack_bitmap"], "0000000000000015");
    assert_eq!(document["ack"]["chain_epoch"], 0);
    assert_eq!(document["ack"]["applied_cell_count"], 510);
    assert_eq!(document["ack"]["stale_cells"], 0);
    assert_eq!(document["ack"]["converged"], true);
    assert_eq!(document["ack"]["render_latency_ms"], 0.0);
    assert_eq!(document["ack"]["has_render_latency_ms"], false);
    assert_eq!(document["ack"]["echo_of_input_seq"], 0);
    assert_eq!(document["ack"]["ack_epoch_ms"], 0);
    assert_eq!(document["window"]["size"], 64);
    assert_eq!(document["window"]["applied_count"], 3);
    assert_eq!(document["window"]["applied_update_ids"], json!([10, 8, 6]));
    assert_eq!(document["window"]["empty"], false);
    assert_eq!(document["cell_counts"]["applied_cell_count"], 510);
    assert_eq!(document["cell_counts"]["stale_cells"], 0);
    assert_eq!(document["cell_counts"]["converged"], true);
    assert_eq!(document["bytes_hex"], "080a101520fe033001");

    let limits = run(&["limits", "--json"]);
    assert_eq!(limits.code, 0, "{}", limits.stderr);
    let document = json_of(&limits);
    assert_eq!(document["command"], "limits");
    assert_eq!(document["protocol_version"], 1);
    let groups = document["groups"].as_array().expect("groups");
    assert!(!groups.is_empty());
    for group in groups {
        assert!(group["name"].is_string());
        assert!(group["spec"].is_string());
        for entry in group["entries"].as_array().expect("entries") {
            assert!(entry["name"].is_string());
            assert!(entry["value"].is_number() || entry["value"].is_string());
            assert!(entry["unit"].is_string() || entry["unit"].is_null());
            assert!(entry["spec"].is_string());
        }
    }
}

#[test]
fn the_exit_status_contract_holds() {
    // 0: the command succeeded.
    assert_eq!(run(&["limits"]).code, 0);
    // 2: the usage was wrong.
    assert_eq!(run(&["nonsense"]).code, 2);
    assert_eq!(run(&["limits", "--nonsense"]).code, 2);
    assert_eq!(run(&["fragment", "inspect"]).code, 2);
    assert_eq!(run(&["fragment", "build", "--class"]).code, 2);
    // 3: the input could not be read.
    assert_eq!(run(&["fragment", "inspect", "--hex", "zz"]).code, 3);
    assert_eq!(run(&["fragment", "inspect", "--hex", "4d4c5000"]).code, 3);
    assert_eq!(run(&["ack", "inspect", "--hex", "ffff"]).code, 3);
    // 1: the check failed. A fragment the assembler refuses is a rejection of
    // the input, not of a check; a check fails when a verdict does not match.
    let duplicate = run(&[
        "object",
        "assemble",
        "--hex",
        "4d4c5000010201060700000000000100000000000000",
        "--hex",
        "4d4c5000010201060700000000000100000000000000",
        "--json",
    ]);
    assert_eq!(duplicate.code, 0);
    assert_eq!(json_of(&duplicate)["status"], "rejected");
}

#[test]
fn the_tool_reads_standard_input() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mlp-core"))
        .args(["object", "assemble", "--stdin", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the tool starts");
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().expect("standard input");
        stdin
            .write_all(b"4d4c5000010201060700000000000100000000000000\n")
            .expect("the fragment is written");
    }
    let output = child.wait_with_output().expect("the tool finishes");
    assert_eq!(output.status.code(), Some(0));
    let document: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(document["status"], "complete");
}

#[test]
fn the_help_documents_the_verbs_the_flags_and_the_exit_status() {
    let outcome = run(&["--help"]);
    assert_eq!(outcome.code, 0);
    for verb in [
        "fragment build",
        "fragment inspect",
        "object assemble",
        "ack build",
        "ack inspect",
        "limits",
        "conformance",
    ] {
        assert!(outcome.stdout.contains(verb), "{verb} is missing");
    }
    assert!(outcome.stdout.contains("0  the command succeeded"));
    assert!(outcome.stdout.contains("1  the check failed"));
    assert!(outcome.stdout.contains("2  the usage was wrong"));
    assert!(outcome.stdout.contains("3  the input could not be read"));

    for command in [
        &["fragment", "build"][..],
        &["fragment", "inspect"][..],
        &["object", "assemble"][..],
        &["ack", "build"][..],
        &["ack", "inspect"][..],
        &["limits"][..],
        &["conformance"][..],
    ] {
        let mut arguments = command.to_vec();
        arguments.push("--help");
        let outcome = run(&arguments);
        assert_eq!(outcome.code, 0);
        assert!(
            outcome.stdout.contains("Flags:"),
            "{command:?} has no flags"
        );
        assert!(
            outcome.stdout.contains("Example:"),
            "{command:?} has no example"
        );
    }
}
