#![cfg(not(target_os = "windows"))]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use codex_core::config::types::McpServerConfig;
use codex_core::config::types::McpServerTransportConfig;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::ReviewDecision;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

fn write_workmux_stub(dir: &TempDir) -> anyhow::Result<(PathBuf, PathBuf)> {
    let calls_dir = dir.path().join("calls");
    fs::create_dir_all(&calls_dir)?;

    let bin_path = dir.path().join("workmux");
    let calls_dir_str = calls_dir.to_string_lossy();
    fs::write(
        &bin_path,
        format!(
            r#"#!/bin/bash
set -euo pipefail
out_dir="{calls_dir_str}"
mkdir -p "$out_dir"
seq="${{CODEX_HOOK_SEQ:-unset}}"
event="${{CODEX_HOOK_EVENT:-unset}}"
echo "${{event}} $*" > "${{out_dir}}/${{seq}}.txt"
"#,
        ),
    )?;
    fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755))?;
    Ok((bin_path, calls_dir))
}

async fn wait_for_call_files(calls_dir: &Path, expected: usize) -> anyhow::Result<Vec<PathBuf>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut files = Vec::new();
        for entry in fs::read_dir(calls_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.push(entry.path());
            }
        }
        if files.len() >= expected {
            return Ok(files);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for {expected} hook files in {} (found {})",
                calls_dir.display(),
                files.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn read_hook_calls(files: Vec<PathBuf>) -> anyhow::Result<Vec<(String, String)>> {
    let mut entries = files
        .into_iter()
        .filter_map(|path| {
            let stem = path.file_stem()?.to_string_lossy().parse::<u64>().ok()?;
            Some((stem, path))
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|(seq, _)| *seq);

    let mut calls = Vec::with_capacity(entries.len());
    for (_, path) in entries {
        let line = fs::read_to_string(path)?.trim().to_string();
        let parts = line.split_whitespace().collect::<Vec<_>>();
        let (event, status) = match parts.as_slice() {
            [event, .., status] => ((*event).to_string(), (*status).to_string()),
            _ => anyhow::bail!("unexpected hook output: {line:?}"),
        };
        calls.push((event, status));
    }
    Ok(calls)
}

#[tokio::test(flavor = "current_thread")]
async fn updates_workmux_status_for_turn_lifecycle() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let hooks_tmp = TempDir::new()?;
    let (workmux_bin, calls_dir) = write_workmux_stub(&hooks_tmp)?;
    let workmux_bin_str = workmux_bin.to_string_lossy().to_string();

    let mut hooks = HashMap::new();
    hooks.insert(
        "turn_started".to_string(),
        vec![vec![
            workmux_bin_str.clone(),
            "set-window-status".to_string(),
            "working".to_string(),
        ]],
    );
    hooks.insert(
        "exec_approval_request".to_string(),
        vec![vec![
            workmux_bin_str.clone(),
            "set-window-status".to_string(),
            "waiting".to_string(),
        ]],
    );
    hooks.insert(
        "exec_command_end".to_string(),
        vec![vec![
            workmux_bin_str.clone(),
            "set-window-status".to_string(),
            "working".to_string(),
        ]],
    );
    hooks.insert(
        "turn_complete".to_string(),
        vec![vec![
            workmux_bin_str.clone(),
            "set-window-status".to_string(),
            "done".to_string(),
        ]],
    );

    let test = test_codex()
        .with_config(move |cfg| cfg.hooks = hooks)
        .build(&server)
        .await?;

    let target = test.cwd.path().join("workmux-status.txt");
    let _ = fs::remove_file(&target);
    let command = format!("printf \"hook-test\" > {target:?}");

    let args = serde_json::to_string(&json!({
        "command": command,
        "timeout_ms": 1_000,
    }))?;

    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call("call-1", "shell_command", &args),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let session_model = test.session_configured.model.clone();
    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "update status".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd.path().to_path_buf(),
            approval_policy: AskForApproval::UnlessTrusted,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: session_model,
            effort: None,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
        })
        .await?;

    wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::ExecApprovalRequest(_))
    })
    .await;
    test.codex
        .submit(Op::ExecApproval {
            id: "0".into(),
            decision: ReviewDecision::Approved,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let files = wait_for_call_files(&calls_dir, 4).await?;
    let calls = read_hook_calls(files)?;
    assert_eq!(
        calls,
        vec![
            ("turn_started".to_string(), "working".to_string()),
            ("exec_approval_request".to_string(), "waiting".to_string()),
            ("exec_command_end".to_string(), "working".to_string()),
            ("turn_complete".to_string(), "done".to_string()),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn runs_hooks_for_direct_event_channel_emits() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let hooks_tmp = TempDir::new()?;
    let (workmux_bin, calls_dir) = write_workmux_stub(&hooks_tmp)?;
    let workmux_bin_str = workmux_bin.to_string_lossy().to_string();

    let failing_server = hooks_tmp.path().join("failing_mcp_server.sh");
    fs::write(&failing_server, "#!/bin/bash\nexit 1\n")?;
    fs::set_permissions(&failing_server, fs::Permissions::from_mode(0o755))?;
    let failing_server_str = failing_server.to_string_lossy().to_string();

    let mut hooks = HashMap::new();
    hooks.insert(
        "mcp_startup_update".to_string(),
        vec![vec![
            workmux_bin_str,
            "set-window-status".to_string(),
            "working".to_string(),
        ]],
    );

    let server_name = "test-mcp";

    let _test = test_codex()
        .with_config(move |config| {
            config.hooks = hooks;

            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                server_name.to_string(),
                McpServerConfig {
                    transport: McpServerTransportConfig::Stdio {
                        command: failing_server_str,
                        args: Vec::new(),
                        env: None,
                        env_vars: Vec::new(),
                        cwd: None,
                    },
                    enabled: true,
                    disabled_reason: None,
                    startup_timeout_sec: Some(Duration::from_secs(10)),
                    tool_timeout_sec: None,
                    enabled_tools: None,
                    disabled_tools: None,
                },
            );
            config
                .mcp_servers
                .set(servers)
                .expect("test mcp servers should accept any configuration");
        })
        .build(&server)
        .await?;

    let files = wait_for_call_files(&calls_dir, 1).await?;
    let calls = read_hook_calls(files)?;

    assert_eq!(
        calls.first().map(|(event, _)| event.as_str()),
        Some("mcp_startup_update")
    );

    Ok(())
}
