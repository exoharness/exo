use anyhow::{Context, Result};
use executor::runtime_host::RuntimeHost;
use exo_managed_agents::http::sse;
use exoharness::EventStream;
use futures::StreamExt;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub(crate) struct Watch {
    events: EventStream,
    sse: bool,
    ended: bool,
}
#[derive(Default)]
pub(crate) struct Watches(Mutex<HashMap<u32, Arc<tokio::sync::Mutex<Watch>>>>);
impl Watches {
    pub fn open(&self, id: u32, events: EventStream, sse: bool) -> u32 {
        self.0.lock().expect("watches poisoned").insert(
            id,
            Arc::new(tokio::sync::Mutex::new(Watch {
                events,
                sse,
                ended: false,
            })),
        );
        id
    }
    pub fn close(&self, id: u32) {
        self.0.lock().expect("watches poisoned").remove(&id);
    }
    pub async fn next(&self, host: &dyn RuntimeHost, id: u32) -> Result<Option<String>> {
        let watch = self
            .0
            .lock()
            .expect("watches poisoned")
            .get(&id)
            .cloned()
            .context("watch is closed")?;
        let mut watch = watch.lock().await;
        if watch.ended {
            self.close(id);
            return Ok(None);
        }
        // The caller pulls a single frame; no history or live-event push queue
        // sits between Rust and a slow SSE/RPC consumer.
        let event = if watch.sse {
            tokio::select! {
                event = watch.events.next() => event,
                result = host.sleep(sse::HEARTBEAT_INTERVAL) => {
                    result?;
                    return Ok(Some(String::from_utf8(sse::HEARTBEAT.to_vec())?));
                }
            }
        } else {
            watch.events.next().await
        };
        match event {
            Some(Ok(event)) if watch.sse => Ok(Some(String::from_utf8(
                sse::encode_event(&event)?.to_vec(),
            )?)),
            Some(Ok(event)) => Ok(Some(serde_json::to_string(&event)?)),
            Some(Err(error)) if watch.sse => {
                watch.ended = true;
                Ok(Some(String::from_utf8(
                    sse::encode_error(&error.to_string()).to_vec(),
                )?))
            }
            Some(Err(error)) => {
                self.close(id);
                Err(error)
            }
            None => {
                self.close(id);
                Ok(None)
            }
        }
    }
}
