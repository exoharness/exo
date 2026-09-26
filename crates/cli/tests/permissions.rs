mod support;

use anyhow::Result;
use support::{Fixture, success};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

#[actix_web::test]
async fn local_and_http_cli_prompt_before_executing_and_continue_after_denial() -> Result<()> {
    for provider in ["local", "remote"] {
        for (mcp, answer, approved) in [
            (false, "y\n", true),
            (false, "n\n", false),
            (true, "y\n", true),
            (true, "n\n", false),
        ] {
            let f = Fixture::new().await?;
            f.cli(&["provider", "switch", provider]).await?;
            let marker = f.temp.path().join("approved");
            let tool = if mcp {
                "exo_mcp__notes__write"
            } else {
                "shell"
            };
            let source = if mcp {
                let marker = marker.clone();
                Mock::given(method("POST")).and(path("/mcp")).respond_with(move |request: &wiremock::Request| {
                    #[derive(serde::Deserialize)]
                    struct Rpc { id: Option<u64>, method: String }
                    let rpc: Rpc = request.body_json().unwrap();
                    let result = match rpc.method.as_str() {
                        "initialize" => serde_json::json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{}}, "serverInfo":{"name":"notes","version":"1"}}),
                        "notifications/initialized" => return ResponseTemplate::new(202),
                        "tools/list" => serde_json::json!({"tools":[{"name":"write","inputSchema":{"type":"object","properties":{}}}]}),
                        "tools/call" => {
                            std::fs::write(&marker, "approved").unwrap();
                            serde_json::json!({"content":[{"type":"text","text":"Written"}],"isError":false})
                        },
                        other => panic!("unexpected MCP request {other}"),
                    };
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"jsonrpc":"2.0","id":rpc.id,"result":result}))
                }).mount(&f.model).await;
                support::SOURCE.replace(
                    "config:",
                    &format!(
                        "mcp_servers:\n  - type: url\n    name: notes\n    url: {}/mcp\nconfig:",
                        f.model.uri()
                    ),
                )
            } else {
                support::SOURCE.replace("config:", "permission_policy: {type: always_ask}\nconfig:")
            };
            std::fs::write(&f.agent_file, source)?;
            let arguments = if mcp {
                "{}".to_owned()
            } else {
                serde_json::json!({"command": format!("printf approved > '{}'", marker.display())})
                    .to_string()
            };
            let call = serde_json::json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":tool, "arguments":arguments, "status":"completed"});
            let events = [
                serde_json::json!({"type":"response.created", "response":{"id":"resp_tool","object":"response","model":"gpt-5-mini","status":"in_progress","output":[]}}),
                serde_json::json!({"type":"response.output_item.added", "output_index":0, "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":tool,"arguments":"","status":"in_progress"}}),
                serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":arguments}),
                serde_json::json!({"type":"response.output_item.done","output_index":0,"item":call}),
                serde_json::json!({"type":"response.completed","response":{"id":"resp_tool","object":"response","created_at":1700000000,"model":"gpt-5-mini","status":"completed","output":[call],"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}}),
            ];
            Mock::given(method("POST"))
                .and(path("/responses"))
                .and(|request: &wiremock::Request| {
                    !String::from_utf8_lossy(&request.body).contains("function_call_output")
                })
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(
                            events
                                .iter()
                                .map(|event| format!("data: {event}\n\n"))
                                .collect::<String>(),
                        ),
                )
                .with_priority(1)
                .mount(&f.model)
                .await;
            let mut args = vec![
                "agent",
                "run",
                "--agent-file",
                f.agent_file.to_str().unwrap(),
                "--prompt",
                "Use the requested tool",
            ];
            if provider == "local" {
                args.extend(["--sandbox", "local-process"]);
            }
            let output = success(f.output(&args, None, Some(answer)).await?)?;
            assert!(
                output.contains(&format!("Permission required: {tool}")),
                "{output}"
            );
            assert!(output.contains("Workflow reply."), "{output}");
            support::thread_slug(&output)?;
            assert_eq!(marker.exists(), approved, "{output}");
            f.stop().await?;
        }
    }
    Ok(())
}

#[actix_web::test]
async fn unsupported_native_approvals_fail_before_model_execution() -> Result<()> {
    for provider in ["local", "remote"] {
        let f = Fixture::new().await?;
        f.cli(&["provider", "switch", provider]).await?;
        for (harness, policy, expected) in [
            (
                "codex",
                "permission_policy: {type: always_ask}",
                "cannot enforce always_ask",
            ),
            (
                "cursor",
                "permission_policy: {type: always_ask}",
                "cannot enforce always_ask",
            ),
            (
                "codex",
                "tool_policies:\n  shell: {type: always_ask}",
                "unknown tool in tool_policies: shell",
            ),
            (
                "codex",
                "tool_policies:\n  codex.shell: {type: always_ask}",
                "unknown tool in tool_policies: codex.shell",
            ),
        ] {
            std::fs::write(
                &f.agent_file,
                support::SOURCE.replace("harness: basic", &format!("harness: {harness}\n{policy}")),
            )?;
            let output = f
                .output(
                    &[
                        "agent",
                        "run",
                        "--agent-file",
                        f.agent_file.to_str().unwrap(),
                        "--prompt",
                        "Use tools",
                    ],
                    None,
                    None,
                )
                .await?;
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(expected),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(f.model.received_requests().await.unwrap().is_empty());
        f.stop().await?;
    }
    Ok(())
}

#[actix_web::test]
async fn renamed_and_undeclared_harnesses_validate_approval_support() -> Result<()> {
    for provider in ["local", "remote"] {
        let f = Fixture::new().await?;
        f.cli(&["provider", "switch", provider]).await?;
        let module = f.temp.path().join("renamed-wrapper.js");
        for declaration in ["nativeToolApprovals: false,", ""] {
            std::fs::write(
                &module,
                format!(
                    r#"export default {{
  {declaration}
  async runTurn(context) {{
    await context.stream.text("Wrapper ran.");
    await context.exoharness.current.turn.addEvents([{{
      type: "messages", messages: [{{ role: "assistant", content: "Wrapper ran." }}]
    }}]);
  }}
}}"#
                ),
            )?;
            for (policy, expected) in [
                (
                    "permission_policy: {type: always_ask}",
                    Some("cannot enforce always_ask"),
                ),
                (
                    "tool_policies:\n  shlel: {type: always_ask}",
                    Some("unknown tool in tool_policies: shlel"),
                ),
                ("tool_policies:\n  shell: {type: always_ask}", None),
            ] {
                std::fs::write(
                    &f.agent_file,
                    support::SOURCE.replace(
                        "harness: basic",
                        &format!("harness: {}\n{policy}", module.display()),
                    ),
                )?;
                let output = f
                    .output(
                        &[
                            "agent",
                            "run",
                            "--agent-file",
                            f.agent_file.to_str().unwrap(),
                            "--prompt",
                            "Use tools",
                        ],
                        None,
                        None,
                    )
                    .await?;
                if let Some(expected) = expected {
                    assert!(!output.status.success());
                    let error = String::from_utf8_lossy(&output.stderr);
                    assert!(error.contains(expected), "{error}");
                } else {
                    let output = success(output)?;
                    assert!(
                        output.contains("Wrapper ran."),
                        "{provider} ({declaration}): {output}"
                    );
                }
            }
        }
        assert!(f.model.received_requests().await.unwrap().is_empty());
        f.stop().await?;
    }
    Ok(())
}

#[actix_web::test]
async fn unknown_tool_policies_fail_before_model_execution() -> Result<()> {
    for provider in ["local", "remote"] {
        let f = Fixture::new().await?;
        f.cli(&["provider", "switch", provider]).await?;
        Mock::given(method("POST")).and(path("/mcp")).respond_with(|request: &wiremock::Request| {
            #[derive(serde::Deserialize)]
            struct Rpc { id: Option<u64>, method: String }
            let rpc: Rpc = request.body_json().unwrap();
            let result = match rpc.method.as_str() {
                "initialize" => serde_json::json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{}}, "serverInfo":{"name":"notes","version":"1"}}),
                "notifications/initialized" => return ResponseTemplate::new(202),
                "tools/list" => serde_json::json!({"tools":[{"name":"write","inputSchema":{"type":"object","properties":{}}}]}),
                other => panic!("unexpected MCP request {other}"),
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"jsonrpc":"2.0","id":rpc.id,"result":result}))
        }).mount(&f.model).await;
        for (policy, expected) in [
            (
                "tool_policies:\n  shlel: {type: always_ask}".to_owned(),
                "unknown tool in tool_policies: shlel",
            ),
            (
                format!(
                    "mcp_servers:\n  - type: url\n    name: notes\n    url: {}/mcp\n    tool_policies:\n      exo_mcp__notes__write: {{type: always_ask}}",
                    f.model.uri()
                ),
                "MCP server notes has no tool named exo_mcp__notes__write",
            ),
        ] {
            std::fs::write(
                &f.agent_file,
                support::SOURCE.replace("config:", &format!("{policy}\nconfig:")),
            )?;
            let output = f
                .output(
                    &[
                        "agent",
                        "run",
                        "--agent-file",
                        f.agent_file.to_str().unwrap(),
                        "--prompt",
                        "Use tools",
                    ],
                    None,
                    None,
                )
                .await?;
            assert!(!output.status.success());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains(expected), "{error}");
        }
        assert!(
            f.model
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.url.path() == "/mcp")
        );
        f.stop().await?;
    }
    Ok(())
}
