//! Worker adapter for the shared managed-agent route dispatcher.
use crate::host::Host;
pub(crate) use api::Request;
use async_trait::async_trait;
use executor::{
    managed_agents::{http as api, service::Service},
    runtime_host::RuntimeHost,
};
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Response {
    Json { status: u16, body: String },
    Watch { watch: u32 },
}
struct Platform<'a> {
    host: &'a Arc<Host>,
    account_id: &'a str,
}
#[async_trait]
impl api::Platform for Platform<'_> {
    fn exposes_store_protocol(&self) -> bool {
        true
    }
    fn account_id(&self) -> &str {
        self.account_id
    }
    fn host(&self) -> Arc<dyn RuntimeHost> {
        self.host.clone()
    }
    fn streaming(&self, config: &executor::AgentConfig) -> bool {
        // WorkerModel completes whole responses; TS harnesses supply a stream.
        config.harness == executor::AgentHarnessKind::TypeScript
    }
}
pub(crate) async fn handle(
    service: &Service<'_>,
    host: &Arc<Host>,
    progress: &tokio::sync::broadcast::Sender<exoharness::Event>,
    request: Request,
    account_id: &str,
    id: u32,
) -> Response {
    match api::handle(service, &Platform { host, account_id }, progress, &request).await {
        api::Response::Json { status, body } => Response::Json { status, body },
        api::Response::Error { status, message } => {
            #[derive(Serialize)]
            struct ErrorBody {
                error: String,
            }
            Response::Json {
                status,
                body: serde_json::to_string(&ErrorBody { error: message })
                    .expect("serialize error"),
            }
        }
        api::Response::Watch(events) => Response::Watch {
            watch: host.watches.open(id, events, true),
        },
    }
}
