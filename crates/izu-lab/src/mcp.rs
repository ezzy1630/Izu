//! Real NDJSON client: stdin remains open until the requested responses arrive.
use crate::{
    runner::Runner,
    suite::{Case, Outcome, observe_file},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, io, path::Path};

fn responses(bytes: &[u8]) -> io::Result<BTreeMap<u64, Value>> {
    let mut output = BTreeMap::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let value: Value = serde_json::from_slice(line).map_err(io::Error::other)?;
        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            if output.insert(id, value).is_some() {
                return Err(io::Error::other("duplicate MCP response ID"));
            }
        } else if value.get("method").and_then(Value::as_str).is_none() {
            return Err(io::Error::other("unexpected MCP stdout message"));
        }
    }
    Ok(output)
}

pub fn acceptance(runner: &Runner, root: &Path) -> io::Result<Case> {
    fs::create_dir_all(root.join("repo"))?;
    fs::create_dir(root.join("controller"))?;
    fs::write(root.join("repo/source.txt"), "MCP source bytes\n")?;
    let mut case = Case {
        name: "json_mcp_external_path".into(),
        outcome: Outcome::Passed,
        runs: vec![],
        file_observations: vec![],
        managed_overlap: None,
    };
    let result = (|| -> io::Result<()> {
        let run = runner.run(&["--json".into(), "init".into(), "repo".into()], root, None)?;
        let json: Value = serde_json::from_str(&run.stdout).map_err(io::Error::other)?;
        let success =
            run.succeeded() && json.pointer("/outcome/kind").and_then(Value::as_str) == Some("ok");
        case.runs.push(run);
        if !success {
            return Err(io::Error::other("MCP fixture init failed"));
        }
        let workspace = json
            .pointer("/outcome/result/data/id")
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::other("missing exact workspace ID"))?;
        let head = json
            .pointer("/outcome/result/data/expected/head")
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::other("missing exact head ID"))?;
        let request = json!({"schema_version":1,"operation":{"kind":"status","context":{"repository":root.join("repo"),"workspace":workspace}}});
        let modern = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"izu-lab","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"izu_status","arguments":request}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"_meta":modern,"name":"izu_status","arguments":request}}),
            json!({"jsonrpc":"2.0","id":5,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2099-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}),
        ];
        let mut input = Vec::new();
        for message in messages {
            serde_json::to_writer(&mut input, &message).map_err(io::Error::other)?;
            input.push(b'\n');
        }
        let run = runner.run_interactive(
            &["agent".into(), "serve".into()],
            &root.join("controller"),
            &input,
            &mut |stdout, _| {
                if !stdout.ends_with(b"\n") {
                    return false;
                }
                responses(stdout)
                    .ok()
                    .is_some_and(|responses| (1..=5).all(|id| responses.contains_key(&id)))
            },
        )?;
        let success = run.succeeded();
        let parsed = responses(run.stdout.as_bytes());
        case.runs.push(run);
        if !success {
            return Err(io::Error::other(
                "MCP process failed or did not shut down cleanly after responses",
            ));
        }
        let parsed = parsed?;
        if parsed
            .get(&1)
            .and_then(|value| value.pointer("/result/protocolVersion"))
            .and_then(Value::as_str)
            != Some("2025-11-25")
        {
            return Err(io::Error::other("legacy MCP negotiation mismatch"));
        }
        if !parsed
            .get(&2)
            .and_then(|value| value.pointer("/result/tools"))
            .and_then(Value::as_array)
            .is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| tool.get("name").and_then(Value::as_str) == Some("izu_status"))
            })
        {
            return Err(io::Error::other(
                "MCP tool discovery lacks actual status capability",
            ));
        }
        for id in [3, 4] {
            let result = parsed
                .get(&id)
                .and_then(|value| value.pointer("/result/structuredContent"))
                .ok_or_else(|| io::Error::other("MCP status missing structured response"))?;
            if result.pointer("/outcome/kind").and_then(Value::as_str) != Some("ok")
                || result
                    .pointer("/outcome/result/data/workspace")
                    .and_then(Value::as_str)
                    != Some(workspace)
                || result
                    .pointer("/outcome/result/data/head")
                    .and_then(Value::as_str)
                    != Some(head)
                || result
                    .pointer("/outcome/result/data/entries/0/path")
                    .and_then(Value::as_str)
                    != Some("source.txt")
            {
                return Err(io::Error::other(
                    "MCP returned wrong repository/workspace/head/source state",
                ));
            }
        }
        if parsed
            .get(&5)
            .and_then(|value| value.pointer("/error/code"))
            .and_then(Value::as_i64)
            != Some(-32022)
        {
            return Err(io::Error::other(
                "unknown MCP version was not explicitly rejected",
            ));
        }
        Ok(())
    })();
    case.file_observations.push(observe_file(
        root.join("repo/source.txt"),
        b"MCP source bytes\n",
    ));
    if let Err(error) = result {
        case.outcome = Outcome::Failed {
            reason: error.to_string(),
        };
    }
    if !case
        .file_observations
        .iter()
        .all(|observation| observation.matched())
    {
        case.outcome = Outcome::Failed {
            reason: "MCP changed source bytes".into(),
        };
    }
    Ok(case)
}
