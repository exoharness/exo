//! Egress decisions reported as sandbox signals.
//!
//! DNS answers are counted per name, record type, and verdict, and reported
//! when each window closes. Proxy decisions are reported as they happen, up to
//! [`MAX_REQUESTS`] per verdict per window for each sandbox; decisions over
//! that cap are counted and reported when the window closes. The cap is keyed
//! by sandbox rather than by proxy session because hosted listeners may build
//! a session per tunnel. Recording takes a short lock and never waits on I/O.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::{self, Display};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use hickory_proto::op::{Message, ResponseCode};
use hickory_proto::rr::RecordType;
use tokio::time::Instant;

use crate::sandbox_signal::sandbox_signal;

pub(super) const WINDOW: Duration = Duration::from_secs(60);
const MAX_DNS_ANSWERS: usize = 256;
const MAX_REQUESTS: u32 = 100;
const MAX_METHOD_BYTES: usize = 32;
const MAX_PATH_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Verdict {
    Allow,
    Deny,
    /// Permitted by policy, but name or credential resolution failed.
    Error,
}

impl Verdict {
    const ALL: [Self; 3] = [Self::Allow, Self::Deny, Self::Error];

    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Error => "error",
        }
    }
}

type Counts = [u64; Verdict::ALL.len()];

pub(super) struct DnsSignals {
    sandbox_id: String,
    window: Mutex<DnsWindow>,
}

#[derive(Default)]
struct DnsWindow {
    answers: HashMap<(String, RecordType, Verdict), u64>,
    dropped: Counts,
}

impl DnsSignals {
    pub(super) fn new(sandbox_id: &str) -> Self {
        Self {
            sandbox_id: sandbox_id.to_owned(),
            window: Mutex::default(),
        }
    }

    /// Count an answer once for each of its questions.
    pub(super) fn record(&self, answer: &Message) {
        let verdict = match answer.response_code() {
            ResponseCode::NoError => Verdict::Allow,
            ResponseCode::NXDomain | ResponseCode::Refused => Verdict::Deny,
            _ => Verdict::Error,
        };
        for question in answer.queries() {
            let mut name = question.name().to_ascii();
            name.truncate(name.trim_end_matches('.').len());
            name.make_ascii_lowercase();
            let mut window = self.window.lock().expect("DNS signal window poisoned");
            let window = &mut *window;
            let full = window.answers.len() >= MAX_DNS_ANSWERS;
            match window.answers.entry((name, question.query_type(), verdict)) {
                Entry::Occupied(mut entry) => *entry.get_mut() += 1,
                Entry::Vacant(entry) if !full => {
                    entry.insert(1);
                }
                Entry::Vacant(_) => window.dropped[verdict as usize] += 1,
            }
        }
    }

    pub(super) fn flush(&self) {
        let window = std::mem::take(&mut *self.window.lock().expect("DNS signal window poisoned"));
        for ((name, record_type, verdict), count) in window.answers {
            sandbox_signal!(
                signal,
                "sandbox.dns",
                sandbox_id = self.sandbox_id,
                source = "exo.egress.dns",
                name = name.as_str(),
                record_type = %record_type,
                verdict = verdict.as_str(),
                count,
                window_seconds = WINDOW.as_secs(),
            );
        }
        for verdict in Verdict::ALL {
            let count = window.dropped[verdict as usize];
            if count > 0 {
                sandbox_signal!(
                    signal,
                    "sandbox.dns_dropped",
                    sandbox_id = self.sandbox_id,
                    source = "exo.egress.dns",
                    verdict = verdict.as_str(),
                    count,
                    window_seconds = WINDOW.as_secs(),
                );
            }
        }
    }
}

/// One proxy decision. `path` is set only for hosts the proxy intercepts and
/// never includes the query string.
#[derive(Default)]
pub(super) struct EgressRequest<'a> {
    pub(super) host: Option<&'a str>,
    pub(super) sni: Option<&'a str>,
    pub(super) method: Option<&'a str>,
    pub(super) port: u16,
    pub(super) path: Option<&'a str>,
    /// Substituted `(binding name, value)` pairs; only names are reported.
    pub(super) credentials: &'a [(String, String)],
    pub(super) reason: Option<&'a dyn Display>,
}

pub(super) fn egress_request(sandbox_id: &str, verdict: Verdict, request: EgressRequest<'_>) {
    if !admit(sandbox_id, verdict) {
        return;
    }
    sandbox_signal!(
        signal,
        "sandbox.egress_request",
        sandbox_id = sandbox_id,
        source = "exo.egress.proxy",
        verdict = verdict.as_str(),
        host = request.host,
        sni = request.sni,
        method = request
            .method
            .map(|method| truncated(method, MAX_METHOD_BYTES)),
        port = request.port,
        path = request.path.map(|path| truncated(path, MAX_PATH_BYTES)),
        binding = (!request.credentials.is_empty())
            .then(|| tracing::field::display(BindingNames(request.credentials))),
        reason = request.reason.map(tracing::field::display),
    );
}

struct BindingNames<'a>(&'a [(String, String)]);

impl Display for BindingNames<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (name, _)) in self.0.iter().enumerate() {
            if self.0[..index].iter().any(|(previous, _)| previous == name) {
                continue;
            }
            if index > 0 {
                formatter.write_str(",")?;
            }
            formatter.write_str(name)?;
        }
        Ok(())
    }
}

fn truncated(value: &str, max: usize) -> &str {
    &value[..value.floor_char_boundary(max)]
}

struct RequestWindows {
    sandboxes: HashMap<String, RequestWindow>,
    swept_at: Instant,
}

struct RequestWindow {
    started: Instant,
    reported: [u32; Verdict::ALL.len()],
    dropped: Counts,
}

impl RequestWindow {
    fn new(started: Instant) -> Self {
        Self {
            started,
            reported: [0; Verdict::ALL.len()],
            dropped: [0; Verdict::ALL.len()],
        }
    }

    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= WINDOW
    }

    fn dropped(&self) -> bool {
        self.dropped.iter().any(|count| *count > 0)
    }
}

static REQUESTS: LazyLock<Mutex<RequestWindows>> = LazyLock::new(|| {
    Mutex::new(RequestWindows {
        sandboxes: HashMap::new(),
        swept_at: Instant::now(),
    })
});

fn requests() -> std::sync::MutexGuard<'static, RequestWindows> {
    REQUESTS.lock().expect("egress signal windows poisoned")
}

fn admit(sandbox_id: &str, verdict: Verdict) -> bool {
    let now = Instant::now();
    let mut closed = Vec::new();
    let admitted = {
        let mut windows = requests();
        // Forget expired windows, reporting drops their scheduled close has not.
        if now.duration_since(windows.swept_at) >= WINDOW {
            windows.swept_at = now;
            windows.sandboxes.retain(|id, window| {
                if !window.expired(now) {
                    return true;
                }
                if window.dropped() {
                    closed.push((id.clone(), window.dropped));
                }
                false
            });
        }
        if !windows.sandboxes.contains_key(sandbox_id) {
            windows
                .sandboxes
                .insert(sandbox_id.to_owned(), RequestWindow::new(now));
        }
        let window = windows
            .sandboxes
            .get_mut(sandbox_id)
            .expect("request window was just inserted");
        if window.expired(now) {
            if window.dropped() {
                closed.push((sandbox_id.to_owned(), window.dropped));
            }
            *window = RequestWindow::new(now);
        }
        let index = verdict as usize;
        if window.reported[index] < MAX_REQUESTS {
            window.reported[index] += 1;
            true
        } else {
            if !window.dropped() {
                tokio::spawn(close_after(sandbox_id.to_owned(), window.started + WINDOW));
            }
            window.dropped[index] += 1;
            false
        }
    };
    for (id, dropped) in closed {
        report_dropped(&id, dropped);
    }
    admitted
}

// Report a window's drops when it ends, even if the sandbox goes quiet.
async fn close_after(sandbox_id: String, deadline: Instant) {
    tokio::time::sleep_until(deadline).await;
    let dropped = {
        let mut windows = requests();
        match windows.sandboxes.entry(sandbox_id) {
            Entry::Occupied(entry) if entry.get().expired(Instant::now()) => {
                let (id, window) = entry.remove_entry();
                window.dropped().then_some((id, window.dropped))
            }
            _ => None,
        }
    };
    if let Some((id, dropped)) = dropped {
        report_dropped(&id, dropped);
    }
}

fn report_dropped(sandbox_id: &str, dropped: Counts) {
    for verdict in Verdict::ALL {
        let count = dropped[verdict as usize];
        if count > 0 {
            sandbox_signal!(
                signal,
                "sandbox.egress_request_dropped",
                sandbox_id = sandbox_id,
                source = "exo.egress.proxy",
                verdict = verdict.as_str(),
                count,
                window_seconds = WINDOW.as_secs(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_signal::tests::{Captured, capture};
    use hickory_proto::op::{MessageType, Query};
    use hickory_proto::rr::Name;

    fn paused_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
    }

    fn answer(name: &str, record_type: RecordType, code: ResponseCode) -> Message {
        let mut message = Message::new();
        message
            .set_message_type(MessageType::Response)
            .set_response_code(code)
            .add_query(Query::query(Name::from_ascii(name).unwrap(), record_type));
        message
    }

    fn signals<'a>(events: &'a [Captured], signal: &str) -> Vec<&'a Captured> {
        events
            .iter()
            .filter(|event| event.target == "sandbox_signal" && event.fields["signal"] == signal)
            .collect()
    }

    #[test]
    fn dns_answers_are_counted_per_name_type_and_verdict() {
        let dns = DnsSignals::new("dns-aggregate");
        let events = capture(|| {
            for _ in 0..3 {
                dns.record(&answer("API.test.", RecordType::A, ResponseCode::NoError));
            }
            dns.record(&answer("api.test", RecordType::AAAA, ResponseCode::NoError));
            dns.record(&answer(
                "blocked.test",
                RecordType::A,
                ResponseCode::NXDomain,
            ));
            dns.record(&answer(
                "broken.test",
                RecordType::A,
                ResponseCode::ServFail,
            ));
            dns.flush();
            dns.flush();
        });
        let mut counts = signals(&events, "sandbox.dns")
            .into_iter()
            .map(|event| {
                assert_eq!(event.fields["sandbox_id"], "dns-aggregate");
                assert_eq!(event.fields["source"], "exo.egress.dns");
                assert_eq!(event.fields["window_seconds"], "60");
                (
                    event.fields["name"].as_str(),
                    event.fields["record_type"].as_str(),
                    event.fields["verdict"].as_str(),
                    event.fields["count"].as_str(),
                )
            })
            .collect::<Vec<_>>();
        counts.sort_unstable();
        assert_eq!(
            counts,
            [
                ("api.test", "A", "allow", "3"),
                ("api.test", "AAAA", "allow", "1"),
                ("blocked.test", "A", "deny", "1"),
                ("broken.test", "A", "error", "1"),
            ]
        );
        assert!(signals(&events, "sandbox.dns_dropped").is_empty());
    }

    #[test]
    fn dns_names_over_the_window_cap_are_summarized() {
        let dns = DnsSignals::new("dns-cap");
        let events = capture(|| {
            for index in 0..MAX_DNS_ANSWERS + 3 {
                dns.record(&answer(
                    &format!("n{index}.exfil.test"),
                    RecordType::TXT,
                    ResponseCode::NXDomain,
                ));
            }
            dns.record(&answer(
                "n0.exfil.test",
                RecordType::TXT,
                ResponseCode::NXDomain,
            ));
            dns.record(&answer("late.test", RecordType::A, ResponseCode::NoError));
            dns.flush();
        });
        let answers = signals(&events, "sandbox.dns");
        assert_eq!(answers.len(), MAX_DNS_ANSWERS);
        assert!(
            answers.iter().any(
                |event| event.fields["name"] == "n0.exfil.test" && event.fields["count"] == "2"
            )
        );
        let mut dropped = signals(&events, "sandbox.dns_dropped")
            .into_iter()
            .map(|event| {
                (
                    event.fields["verdict"].as_str(),
                    event.fields["count"].as_str(),
                )
            })
            .collect::<Vec<_>>();
        dropped.sort_unstable();
        assert_eq!(dropped, [("allow", "1"), ("deny", "3")]);
    }

    #[test]
    fn requests_over_the_cap_are_summarized_when_the_window_closes() {
        let sandbox = "request-cap";
        let events = capture(|| {
            paused_runtime().block_on(async {
                let decide = |verdict| {
                    egress_request(
                        sandbox,
                        verdict,
                        EgressRequest {
                            host: Some("blocked.test"),
                            port: 443,
                            ..Default::default()
                        },
                    )
                };
                for _ in 0..MAX_REQUESTS + 5 {
                    decide(Verdict::Deny);
                }
                decide(Verdict::Allow);
                tokio::time::sleep(WINDOW).await;
                tokio::task::yield_now().await;
                for _ in 0..MAX_REQUESTS + 2 {
                    decide(Verdict::Deny);
                }
                tokio::time::sleep(WINDOW).await;
                decide(Verdict::Allow);
            });
        });
        let requests = signals(&events, "sandbox.egress_request");
        let denied = requests
            .iter()
            .filter(|event| event.fields["verdict"] == "deny")
            .count();
        assert_eq!(denied, 2 * MAX_REQUESTS as usize);
        assert_eq!(requests.len() - denied, 2);
        let dropped = signals(&events, "sandbox.egress_request_dropped")
            .into_iter()
            .map(|event| {
                assert_eq!(event.fields["sandbox_id"], sandbox);
                (
                    event.fields["verdict"].as_str(),
                    event.fields["count"].as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(dropped, [("deny", "5"), ("deny", "2")]);
    }

    #[test]
    fn request_fields_omit_values_and_bound_guest_strings() {
        let credentials = [
            ("github".to_owned(), "secret-one".to_owned()),
            ("github".to_owned(), "secret-one".to_owned()),
            ("npm".to_owned(), "secret-two".to_owned()),
        ];
        let long_path = format!("/{}", "a".repeat(2 * MAX_PATH_BYTES));
        let reason = "host is not allowed";
        let events = capture(|| {
            paused_runtime().block_on(async {
                egress_request(
                    "request-fields",
                    Verdict::Allow,
                    EgressRequest {
                        host: Some("api.test"),
                        sni: Some("api.test"),
                        method: Some("GET"),
                        port: 443,
                        path: Some(&long_path),
                        credentials: &credentials,
                        reason: Some(&reason),
                    },
                );
                egress_request(
                    "request-fields",
                    Verdict::Deny,
                    EgressRequest {
                        port: 443,
                        ..Default::default()
                    },
                );
            });
        });
        let [allowed, denied] = signals(&events, "sandbox.egress_request")[..] else {
            panic!("expected two request signals, got {events:?}");
        };
        assert_eq!(allowed.fields["binding"], "github,npm");
        assert_eq!(allowed.fields["path"].len(), MAX_PATH_BYTES);
        assert_eq!(allowed.fields["reason"], reason);
        assert!(
            allowed
                .fields
                .values()
                .all(|value| !value.contains("secret"))
        );
        assert_eq!(denied.fields["port"], "443");
        for field in ["host", "sni", "method", "path", "binding", "reason"] {
            assert!(
                !denied.fields.contains_key(field),
                "{field} should be absent"
            );
        }
    }
}
