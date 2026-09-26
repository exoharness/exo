use actix_web::{App, HttpServer, dev::Service, http::header::HeaderValue, web};
use anyhow::{Context, Result};
use executor::{
    LocalProvider, Runtime,
    http_service::{RuntimeHttpService, configure},
};
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, SandboxBackendRegistration, SandboxProvider,
    SecretBackendChoice,
};
use serde_json::json;
use std::{
    collections::HashMap,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, process::Command};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

pub const SOURCE: &str = "---\nname: Workflow agent\nharness: basic\nconfig:\n  model: gpt-5-mini\n  credential: model-key\n---\nReply to the user.\n";

type RequestContext = (String, Option<HeaderValue>);

pub struct Fixture {
    pub temp: TempDir,
    pub model: MockServer,
    pub root: PathBuf,
    pub agent_file: PathBuf,
    pub endpoint: String,
    pub runtime: Arc<Runtime>,
    pub server: actix_web::dev::ServerHandle,
    #[allow(dead_code)]
    pub contexts: Arc<Mutex<Vec<RequestContext>>>,
}

impl Fixture {
    #[allow(dead_code)]
    pub async fn new() -> Result<Self> {
        Self::with_sandbox(SandboxProvider::LocalProcess).await
    }

    pub async fn with_sandbox(sandbox: SandboxProvider) -> Result<Self> {
        let temp = TempDir::new()?;
        let root = temp.path().join("state");
        let agent_file = temp.path().join("agent.md");
        std::fs::write(temp.path().join("prices.json"), "{}")?;
        let model = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(model_response())
            .with_priority(10)
            .mount(&model)
            .await;
        let config = BasicExoHarnessConfig {
            root: root.join("exoharness"),
            secret_backend: SecretBackendChoice::File {
                path: Some(temp.path().join("master-key")),
            },
            sandbox_default: sandbox,
            sandbox_policy: None,
            sandbox_backends: vec![
                SandboxBackendRegistration::local_process(),
                SandboxBackendRegistration::docker(),
                SandboxBackendRegistration::apple_container(),
                #[cfg(feature = "firecracker")]
                SandboxBackendRegistration::firecracker(exoharness::FirecrackerBackendSpec {
                    config: exoharness::FirecrackerConfig::default(),
                    lima: exoharness::FirecrackerLimaConfig {
                        bridge_binary: std::env::var_os("EXO_EGRESS_BRIDGE_BINARY")
                            .map(PathBuf::from),
                        ..Default::default()
                    },
                }),
            ],
        };
        let state = Arc::new(BasicExoHarness::new(config.clone()).await?);
        let runtime = Arc::new(Runtime::new(
            LocalProvider::managed(
                state,
                config.clone(),
                HashMap::new(),
                Arc::new(cost::PricingTable::empty()),
            )?,
            None,
        ));
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = format!("http://{}/exo", listener.local_addr()?);
        let service = Arc::new(RuntimeHttpService::new(
            runtime.clone(),
            Some("workflow-token"),
        )?);
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let recorded = contexts.clone();
        let serving = HttpServer::new(move || {
            let recorded = recorded.clone();
            App::new()
                .app_data(web::Data::new(service.clone()))
                .wrap_fn(move |request, service| {
                    recorded.lock().unwrap().push((
                        request.path().to_owned(),
                        request.headers().get("x-exo-context").cloned(),
                    ));
                    service.call(request)
                })
                .configure(configure)
        })
        .listen(listener)?
        .run();
        let server = serving.handle();
        actix_web::rt::spawn(serving);
        let f = Self {
            temp,
            model,
            root,
            agent_file,
            endpoint,
            runtime,
            server,
            contexts,
        };
        f.cli(&[
            "vault",
            "secret",
            "create",
            "global",
            "model-key",
            "--token-env",
            "SMOKE_API_KEY",
        ])
        .await?;
        std::fs::write(&f.agent_file, f.source())?;
        f.cli(&[
            "provider",
            "create",
            "local",
            "--local-root",
            f.root.to_str().context("root")?,
        ])
        .await?;
        f.cli(&[
            "provider",
            "create",
            "remote",
            "--url",
            &f.endpoint,
            "--api-key-env",
            "RUNTIME_TOKEN",
        ])
        .await?;
        Ok(f)
    }

    #[allow(dead_code)]
    pub fn source(&self) -> String {
        SOURCE.replace(
            "  credential: model-key",
            &format!("  credential: model-key\n  base_url: {}", self.model.uri()),
        )
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let binary =
            std::env::var_os("EXO_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_exo").into());
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("EXO_CONFIG_DIR", self.temp.path().join("config"))
            .env("SMOKE_API_KEY", "fixture-model-key")
            .env("RUNTIME_TOKEN", "workflow-token")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .current_dir(self.temp.path())
            .kill_on_drop(true);
        if let Some(bridge) = std::env::var_os("EXO_EGRESS_BRIDGE_BINARY") {
            command.env("EXO_FIRECRACKER_LIMA_EXO_BINARY", bridge);
        }
        #[cfg(feature = "firecracker")]
        for name in ["HOME", "LIMA_HOME"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let command_index = if args.first() == Some(&"--provider") {
            2
        } else {
            0
        };
        command.args(&args[..=command_index]);
        if args[command_index] != "provider" {
            command
                .arg("--root")
                .arg(&self.root)
                .args(["--secret-backend", "file", "--master-key-path"])
                .arg(self.temp.path().join("master-key"))
                .env(
                    "EXO_LITELLM_PRICES_PATH",
                    self.temp.path().join("prices.json"),
                );
        }
        command.args(&args[command_index + 1..]);
        command
    }

    pub async fn output(
        &self,
        args: &[&str],
        cwd: Option<&Path>,
        input: Option<&str>,
    ) -> Result<Output> {
        let mut command = self.command(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = input {
            child
                .stdin
                .take()
                .context("stdin")?
                .write_all(input.as_bytes())
                .await?;
        } else {
            drop(child.stdin.take());
        }
        Ok(tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await??)
    }

    pub async fn cli(&self, args: &[&str]) -> Result<String> {
        success(self.output(args, None, None).await?)
    }

    pub async fn stop(&self) -> Result<()> {
        let result = self.runtime.shutdown().await;
        self.server.stop(false).await;
        result
    }
}

pub fn success(output: Output) -> Result<String> {
    anyhow::ensure!(
        output.status.success(),
        "CLI failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

pub fn model_response() -> ResponseTemplate {
    let message = json!({"id":"msg_fixture", "type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"Workflow reply.", "annotations":[]}]});
    let response = json!({"id":"resp_fixture", "object":"response", "created_at":1700000000, "model":"gpt-5-mini", "status":"completed", "output":[message], "usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}});
    let events = [
        json!({"type":"response.created", "response":{"id":"resp_fixture","object":"response","model":"gpt-5-mini","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added", "output_index":0, "item":{"id":"msg_fixture","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added", "item_id":"msg_fixture","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_fixture","output_index":0,"content_index":0,"delta":"Workflow reply."}),
        json!({"type":"response.output_item.done","output_index":0,"item":message}),
        json!({"type":"response.completed","response":response}),
    ];
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(
            events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>(),
        )
}

pub fn thread_slug(output: &str) -> Result<&str> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("thread: "))
        .and_then(|line| line.split_once(" ("))
        .map(|(slug, _)| slug)
        .context("thread in CLI output")
}
