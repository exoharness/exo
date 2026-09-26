use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use executor::{
    ConversationHandle, EventData, EventId, EventKind, EventQuery, EventQueryDirection, TurnId,
    UsageRecord,
};

pub(crate) async fn interruptible<T>(future: impl Future<Output = Result<T>>) -> Result<Option<T>> {
    tokio::select! {
        result = future => result.map(Some),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            println!("\nInterrupted");
            Ok(None)
        }
    }
}

pub(crate) struct TurnProgress {
    enabled: bool,
    status: Option<String>,
    frame: usize,
    rendered: bool,
}

impl TurnProgress {
    pub(crate) fn new() -> Self {
        Self {
            enabled: io::stdout().is_terminal(),
            status: Some("Starting turn".to_string()),
            frame: 0,
            rendered: false,
        }
    }

    pub(crate) fn set_status(&mut self, status: Option<String>) {
        self.status = status;
    }

    pub(crate) async fn wait<T>(&mut self, future: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::pin!(future);
        let mut interval = tokio::time::interval(Duration::from_millis(120));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                result = &mut future => {
                    self.clear()?;
                    return result;
                }
                signal = tokio::signal::ctrl_c() => {
                    self.clear()?;
                    signal?;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "turn interrupted").into());
                }
                _ = interval.tick(), if self.enabled && self.status.is_some() => {
                    let frame = ["-", "\\", "|", "/"][self.frame % 4];
                    self.frame = self.frame.wrapping_add(1);
                    print!("\r\x1b[2K{frame} {}", self.status.as_deref().unwrap_or_default());
                    io::stdout().flush()?;
                    self.rendered = true;
                }
            }
        }
    }

    fn clear(&mut self) -> Result<()> {
        if self.rendered {
            print!("\r\x1b[2K");
            io::stdout().flush()?;
            self.rendered = false;
        }
        Ok(())
    }
}

impl Drop for TurnProgress {
    fn drop(&mut self) {
        if let Err(error) = self.clear() {
            tracing::debug!(%error, "failed to clear turn progress");
        }
    }
}

#[derive(Default)]
pub(crate) struct UsageTracker {
    cursor: Option<EventId>,
    groups: HashMap<TurnId, UsageGroup>,
    models: BTreeMap<String, UsageTotals>,
    pub(crate) total: UsageTotals,
}

impl UsageTracker {
    pub(crate) fn cost_lines(&self) -> Vec<String> {
        if self.total.records == 0 {
            return vec!["no recorded model usage in this conversation yet".into()];
        }
        let mut lines = vec![format!(
            "{:<28} {:>6} {:>11} {:>11} {:>11} {:>12}",
            "MODEL", "CALLS", "PROMPT", "CACHED", "OUT", "COST"
        )];
        lines.extend(self.models.iter().map(|(model, usage)| usage.row(model)));
        lines.push(self.total.row("TOTAL"));
        lines
    }

    pub(crate) async fn refresh(
        &mut self,
        conversation: &dyn ConversationHandle,
        turn_id: Option<TurnId>,
    ) -> Result<UsageTotals> {
        loop {
            let result = conversation
                .get_events(Some(EventQuery {
                    cursor: self.cursor,
                    direction: Some(EventQueryDirection::Asc),
                    limit: None,
                    types: Some(vec![EventKind::MESSAGES]),
                    ..Default::default()
                }))
                .await?;
            if result.events.is_empty() {
                break;
            }
            anyhow::ensure!(
                result.events.last().map(|event| event.id) > self.cursor,
                "usage history cursor did not advance"
            );
            for event in result.events {
                if let EventData::Messages {
                    usage, messages, ..
                } = event.data
                {
                    let group = self
                        .groups
                        .entry(event.turn_id.unwrap_or(event.id))
                        .or_default();
                    if let Some(usage) = usage {
                        group.totals.add(&usage);
                        self.models
                            .entry(usage.model.clone())
                            .or_default()
                            .add(&usage);
                        group.separate_usage |= messages.is_empty();
                    } else {
                        group.unmetered_messages |= messages
                            .iter()
                            .any(|message| matches!(message, lingua::Message::Assistant { .. }));
                    }
                }
                self.cursor = Some(event.id);
            }
            if result.cursor.is_none() {
                break;
            }
        }
        let mut turn = UsageTotals::default();
        self.total = UsageTotals::default();
        for (id, group) in &self.groups {
            let totals = group.finish();
            self.total.merge(&totals);
            if Some(*id) == turn_id {
                turn = totals;
            }
        }
        Ok(turn)
    }
}

#[derive(Default)]
struct UsageGroup {
    totals: UsageTotals,
    unmetered_messages: bool,
    separate_usage: bool,
}

impl UsageGroup {
    fn finish(&self) -> UsageTotals {
        let mut totals = self.totals.clone();
        if self.unmetered_messages && !self.separate_usage {
            totals.add(&UsageRecord::default());
        }
        totals
    }
}

#[derive(Clone, Default)]
pub(crate) struct UsageTotals {
    records: usize,
    prompt: Option<i64>,
    cached: Option<i64>,
    completion: Option<i64>,
    cost: Option<f64>,
}

impl UsageTotals {
    fn merge(&mut self, other: &Self) {
        if other.records == 0 {
            return;
        }
        if self.records == 0 {
            *self = other.clone();
            return;
        }
        self.records += other.records;
        self.prompt = self.prompt.zip(other.prompt).map(|(a, b)| a + b);
        self.cached = self.cached.zip(other.cached).map(|(a, b)| a + b);
        self.completion = self.completion.zip(other.completion).map(|(a, b)| a + b);
        self.cost = self.cost.zip(other.cost).map(|(a, b)| a + b);
    }

    fn add(&mut self, usage: &UsageRecord) {
        self.merge(&Self {
            records: 1,
            prompt: usage.prompt_tokens,
            cached: usage.prompt_cached_tokens,
            completion: usage.completion_tokens,
            cost: usage.cost_usd,
        });
    }

    fn tokens(&self) -> Option<i64> {
        self.prompt.zip(self.completion).map(|(a, b)| a + b)
    }

    fn row(&self, model: &str) -> String {
        let [prompt, cached, completion] =
            [self.prompt, self.cached, self.completion].map(|count| {
                count
                    .map(format_count)
                    .unwrap_or_else(|| "unavailable".into())
            });
        let cost = self
            .cost
            .map(format_cost)
            .unwrap_or_else(|| "unavailable".into());
        format!(
            "{:<28} {:>6} {prompt:>11} {cached:>11} {completion:>11} {cost:>12}",
            model.chars().take(28).collect::<String>(),
            self.records
        )
    }

    pub(crate) fn display(
        &self,
        total: &Self,
        ttft: Option<Duration>,
        elapsed: Duration,
    ) -> String {
        let mut parts = Vec::new();
        if let Some(ttft) = ttft {
            parts.push(format!("ttft: {}", format_duration(ttft)));
        }
        let mut duration = format!("duration: {}", format_duration(elapsed));
        if let Some(tokens) = self.tokens()
            && !elapsed.is_zero()
        {
            let rate = (tokens as f64 / elapsed.as_secs_f64()).round() as i64;
            duration.push_str(&format!(" ({} tok/s)", format_count(rate)));
        }
        parts.push(duration);
        parts.push(match self.tokens() {
            Some(tokens) => match total.tokens() {
                Some(total) => format!(
                    "{} [{} total] tok",
                    format_count(tokens),
                    format_count(total)
                ),
                None => format!("{} tok", format_count(tokens)),
            },
            None => "tokens: unavailable".into(),
        });
        parts.push(match self.cost {
            Some(cost) => match total.cost {
                Some(total) => format!("{} [{} total]", format_cost(cost), format_cost(total)),
                None => format_cost(cost),
            },
            None => "cost: unavailable".into(),
        });
        parts.join(", ")
    }
}

fn format_count(value: i64) -> String {
    let digits = value.to_string();
    let mut result = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

fn format_cost(value: f64) -> String {
    if value >= 0.01 {
        format!("${value:.2}")
    } else {
        format!("${value:.6}")
    }
}

fn format_duration(value: Duration) -> String {
    if value.as_secs() >= 60 {
        format!(
            "{}m{:04.1}s",
            value.as_secs() / 60,
            value.as_secs_f64() % 60.0
        )
    } else if value.as_secs() == 0 {
        format!("{}ms", value.as_millis())
    } else {
        format!("{:.2}s", value.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn separate_usage_covers_late_records_without_hiding_missing_usage() -> Result<()> {
        use exoharness::{
            BasicExoHarness, BasicExoHarnessConfig, ExoHarness, NewAgentRequest,
            SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
        };
        use lingua::{Message, universal::AssistantContent};

        let temp = tempfile::TempDir::new()?;
        let store = BasicExoHarness::in_memory(BasicExoHarnessConfig {
            root: temp.path().into(),
            secret_backend: SecretBackendChoice::File { path: None },
            sandbox_default: SandboxProvider::LocalProcess,
            sandbox_policy: None,
            sandbox_backends: vec![SandboxBackendRegistration::local_process()],
        })
        .await?;
        let agent = store
            .new_agent(NewAgentRequest {
                name: "Usage test".into(),
                slug: "usage-test".into(),
                vaults: vec![],
            })
            .await?;
        let thread = agent.new_thread(Default::default()).await?;
        let mut tracker = UsageTracker::default();
        let text = EventData::Messages {
            messages: vec![Message::Assistant {
                content: AssistantContent::String("Assistant text".into()),
                id: None,
            }],
            response_id: None,
            usage: None,
        };
        let usage = UsageRecord {
            prompt_tokens: Some(1000),
            completion_tokens: Some(100),
            prompt_cached_tokens: Some(800),
            completion_reasoning_tokens: Some(20),
            cost_usd: Some(0.01),
            ..Default::default()
        };
        for usage_first in [false, true] {
            let turn = thread.begin_turn(Default::default()).await?;
            let mut events = vec![text.clone(); 101];
            events.insert(
                if usage_first { 0 } else { 101 },
                EventData::Messages {
                    messages: vec![],
                    response_id: None,
                    usage: Some(Box::new(usage.clone())),
                },
            );
            if !usage_first {
                let late_usage = events.pop().unwrap();
                turn.add_events(events).await?;
                let partial = tracker
                    .refresh(thread.as_ref(), Some(turn.record().id))
                    .await?;
                assert!(partial.tokens().is_none() && tracker.total.cost.is_none());
                turn.add_events(vec![late_usage]).await?;
            } else {
                turn.add_events(events).await?;
            }
            turn.finish().await?;
            let current = tracker
                .refresh(thread.as_ref(), Some(turn.record().id))
                .await?;
            assert_eq!(current.tokens(), Some(1100));
            assert!(current.cost.is_some());
            let total = if usage_first { 2200 } else { 1100 };
            assert_eq!(tracker.total.tokens(), Some(total));
            tracker.refresh(thread.as_ref(), None).await?;
            assert_eq!(tracker.total.tokens(), Some(total));
        }
        let turn = thread.begin_turn(Default::default()).await?;
        let mut attached = text.clone();
        if let EventData::Messages { usage: record, .. } = &mut attached {
            *record = Some(Box::new(usage));
        }
        turn.add_events(vec![attached, text]).await?;
        turn.finish().await?;
        let current = tracker
            .refresh(thread.as_ref(), Some(turn.record().id))
            .await?;
        assert!(current.tokens().is_none() && current.cost.is_none());
        assert!(tracker.total.tokens().is_none() && tracker.total.cost.is_none());
        let mut reopened = UsageTracker::default();
        reopened.refresh(thread.as_ref(), None).await?;
        assert_eq!(reopened.total.tokens(), tracker.total.tokens());
        assert!(reopened.total.tokens().is_none() && reopened.total.cost.is_none());
        assert!(
            reopened
                .cost_lines()
                .last()
                .unwrap()
                .contains("unavailable")
        );
        Ok(())
    }

    #[test]
    fn summarizes_multiple_calls_without_double_counting_cached_or_reasoning_tokens() {
        let usage = UsageRecord {
            prompt_tokens: Some(1000),
            completion_tokens: Some(100),
            prompt_cached_tokens: Some(800),
            completion_reasoning_tokens: Some(20),
            cost_usd: Some(0.01),
            ..Default::default()
        };
        let mut turn = UsageTotals::default();
        turn.add(&usage);
        turn.add(&usage);
        let mut total = UsageTotals::default();
        total.add(&usage);
        total.add(&usage);
        total.add(&usage);
        assert_eq!(
            turn.display(
                &total,
                Some(Duration::from_millis(250)),
                Duration::from_secs(2)
            ),
            "ttft: 250ms, duration: 2.00s (1,100 tok/s), 2,200 [3,300 total] tok, $0.02 [$0.03 total]"
        );
    }

    #[test]
    fn does_not_report_incomplete_usage_or_pricing_as_zero() {
        let mut turn = UsageTotals::default();
        turn.add(&UsageRecord::default());
        turn.add(&UsageRecord {
            prompt_tokens: Some(100),
            completion_tokens: Some(20),
            cost_usd: Some(0.01),
            ..Default::default()
        });
        assert_eq!(
            turn.display(&turn, None, Duration::from_secs(1)),
            "duration: 1.00s, tokens: unavailable, cost: unavailable"
        );
    }
}
