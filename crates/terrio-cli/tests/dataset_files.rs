//! Dataset file-set safety and MCP write classification.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).output().unwrap()
}
#[test]
fn copies_renames_and_preflights_batch_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.csv");
    let target = directory.path().join("copy.csv");
    let moved = directory.path().join("moved.csv");
    std::fs::write(&source, b"x,y\n1,2\n").unwrap();
    std::fs::write(source.with_extension("csvt"), b"Real,Real").unwrap();
    let output = run(&["dataset-copy", source.to_str().unwrap(), target.to_str().unwrap(), "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["files"].as_array().unwrap().len(), 2);
    assert_eq!(report["bytes"], 17);
    assert!(run(&["dataset-rename", target.to_str().unwrap(), moved.to_str().unwrap()]).status.success());
    assert!(!target.exists());
    let missing = directory.path().join("missing.csv");
    assert!(!run(&["dataset-delete", moved.to_str().unwrap(), missing.to_str().unwrap()]).status.success());
    assert!(moved.exists());
    assert!(run(&["dataset-delete", source.to_str().unwrap(), moved.to_str().unwrap()]).status.success());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}
#[test]
fn mcp_discovers_file_operations_and_marks_mutations_as_writes() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.csv");
    let target = directory.path().join("copy.csv");
    std::fs::write(&source, b"x,y\n1,2\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"dataset-copy","arguments":{"args":[source.to_str().unwrap(),target.to_str().unwrap(),"--json"]}}}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    let replies: Vec<Value> =
        BufReader::new(child.stdout.take().unwrap()).lines().take(2).map(|line| serde_json::from_str(&line.unwrap()).unwrap()).collect();
    drop(input);
    assert!(child.wait().unwrap().success());
    let tools = replies.iter().find(|r| r["id"] == 1).unwrap()["result"]["tools"].as_array().unwrap();
    for (name, read_only) in [
        ("dataset-files", true),
        ("dataset-copy", false),
        ("dataset-rename", false),
        ("dataset-delete", false),
        ("parquet-create-metadata", false),
    ] {
        assert_eq!(tools.iter().find(|t| t["name"] == name).unwrap()["annotations"]["readOnlyHint"], read_only);
    }
    let reply = &replies.iter().find(|r| r["id"] == 2).unwrap()["result"];
    assert_eq!(reply["isError"], false);
    assert_eq!(std::fs::read(&target).unwrap(), b"x,y\n1,2\n");
}

#[test]
fn external_vrt_reference_flag_is_explicit_and_recovery_status_is_structured() {
    let directory = tempfile::tempdir().unwrap();
    let inputs = directory.path().join("inputs");
    std::fs::create_dir(&inputs).unwrap();
    std::fs::write(directory.path().join("source.raw"), [1, 2, 3, 4]).unwrap();
    let source = inputs.join("source.vrt");
    let output = directory.path().join("copy.vrt");
    std::fs::write(&source, b"<VRTDataset rasterXSize='2' rasterYSize='2'><VRTRasterBand band='1' dataType='Byte' subClass='VRTRawRasterBand'><SourceFilename relativeToVRT='1'>../source.raw</SourceFilename><ImageOffset>0</ImageOffset><PixelOffset>1</PixelOffset><LineOffset>2</LineOffset></VRTRasterBand></VRTDataset>").unwrap();
    let rejected = run(&["dataset-copy", source.to_str().unwrap(), output.to_str().unwrap(), "--json"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("source directory tree"));
    assert!(!output.exists());
    let accepted = run(&["dataset-copy", source.to_str().unwrap(), output.to_str().unwrap(), "--allow-external-references", "--json"]);
    assert!(accepted.status.success(), "{}", String::from_utf8_lossy(&accepted.stderr));
    let report: Value = serde_json::from_slice(&accepted.stdout).unwrap();
    assert_eq!(report["recovery_files"], json!([]));
    assert_eq!(std::fs::read(output.with_extension("raw")).unwrap(), [1, 2, 3, 4]);
}
