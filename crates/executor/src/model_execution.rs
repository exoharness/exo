use web_time::Instant;

use exoharness::Result;

use crate::execution_tracing::TurnExecutionTrace;
use crate::harness_executor::ExecutorStreamMode;
use crate::shared::try_send_stream_event;
use crate::{ExecutionStreamEvent, ModelClient, ModelRequest, ModelResponse};

#[derive(Clone, Copy)]
pub(crate) enum ModelStreamOutput {
    Visible,
    Internal,
}

pub(crate) async fn complete_model_round<M: ModelClient + ?Sized>(
    model: &M,
    request: ModelRequest,
    round: usize,
    stream_mode: ExecutorStreamMode<'_>,
    output: ModelStreamOutput,
    turn_trace: Option<&dyn TurnExecutionTrace>,
) -> Result<ModelResponse> {
    let trace = match turn_trace {
        Some(trace) => trace.start_llm_round(&request, round).await,
        None => None,
    };
    let requested_model = request.model.clone();
    let started_at = Instant::now();
    let result = async {
        let mut ttft = None;
        let response = match stream_mode {
            ExecutorStreamMode::Disabled => model.complete(request).await?,
            ExecutorStreamMode::Enabled(sender) => {
                let mut stream = model.complete_stream(request).await?;
                while let Some(chunk) = stream.next_chunk().await? {
                    if chunk.is_keep_alive() {
                        continue;
                    }
                    if ttft.is_none() {
                        let measured = started_at.elapsed();
                        ttft = Some(measured);
                        try_send_stream_event(
                            sender,
                            ExecutionStreamEvent::FirstChunk { ttft: measured },
                        );
                    }
                    if matches!(output, ModelStreamOutput::Visible) {
                        try_send_stream_event(sender, ExecutionStreamEvent::Chunk(chunk));
                    }
                }
                stream.finish().await?
            }
        };
        Ok::<_, anyhow::Error>((response, ttft))
    }
    .await;
    match result {
        Ok((mut response, ttft)) => {
            response.model.get_or_insert(requested_model);
            response.ttft = response.ttft.or(ttft);
            response.duration.get_or_insert(started_at.elapsed());
            if let Some(trace) = trace {
                trace.finish_success(&response, ttft).await;
            }
            Ok(response)
        }
        Err(error) => {
            if let Some(trace) = trace {
                trace.finish_error(&error).await;
            }
            Err(error)
        }
    }
}
