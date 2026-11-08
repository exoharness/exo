//! Security signals observed outside the guest.
//!
//! A signal is a `tracing` event on [`TARGET`] carrying `signal`, `sandbox_id`,
//! `source`, `observed_at`, and `severity`, followed by fields specific to the
//! signal. Sinks select signals by target, so emitters never depend on the sink.
//! Emitters bound their own volume per sandbox.
//!
//! Severity is `signal` for anything that needs triage, and `exploit` only for
//! evidence of code running in the VMM that Firecracker never runs.

pub(crate) const TARGET: &str = "sandbox_signal";

pub(crate) fn observed_at() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

macro_rules! sandbox_signal {
    (signal, $signal:literal, sandbox_id = $sandbox_id:expr, source = $source:literal $(, $($fields:tt)+)?) => {
        ::tracing::info!(
            target: $crate::sandbox_signal::TARGET,
            signal = $signal,
            sandbox_id = %$sandbox_id,
            source = $source,
            observed_at = %$crate::sandbox_signal::observed_at(),
            severity = "signal",
            $($($fields)+)?
        )
    };
    (exploit, $signal:literal, sandbox_id = $sandbox_id:expr, source = $source:literal $(, $($fields:tt)+)?) => {
        ::tracing::error!(
            target: $crate::sandbox_signal::TARGET,
            signal = $signal,
            sandbox_id = %$sandbox_id,
            source = $source,
            observed_at = %$crate::sandbox_signal::observed_at(),
            severity = "exploit",
            $($($fields)+)?
        )
    };
}

pub(crate) use sandbox_signal;

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    #[derive(Debug)]
    pub(crate) struct Captured {
        pub(crate) target: String,
        pub(crate) level: tracing::Level,
        pub(crate) fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<Captured>>>);

    struct FieldVisitor<'a>(&'a mut BTreeMap<String, String>);

    impl Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            let mut fields = BTreeMap::new();
            event.record(&mut FieldVisitor(&mut fields));
            self.0.lock().unwrap().push(Captured {
                target: event.metadata().target().to_string(),
                level: *event.metadata().level(),
                fields,
            });
        }
    }

    pub(crate) fn capture(emit: impl FnOnce()) -> Vec<Captured> {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, emit);
        std::mem::take(&mut *capture.0.lock().unwrap())
    }

    #[test]
    fn signal_carries_the_common_fields() {
        let events = capture(|| {
            sandbox_signal!(
                signal,
                "sandbox.test",
                sandbox_id = "sbx_1",
                source = "exo.test",
                verdict = "deny",
                count = 3_u64,
            );
        });
        let [event] = events.as_slice() else {
            panic!("expected one event, got {events:?}");
        };
        assert_eq!(event.target, "sandbox_signal");
        assert_eq!(event.level, tracing::Level::INFO);
        assert_eq!(event.fields["signal"], "sandbox.test");
        assert_eq!(event.fields["sandbox_id"], "sbx_1");
        assert_eq!(event.fields["source"], "exo.test");
        assert_eq!(event.fields["severity"], "signal");
        assert_eq!(event.fields["verdict"], "deny");
        assert_eq!(event.fields["count"], "3");
        chrono::DateTime::parse_from_rfc3339(&event.fields["observed_at"])
            .expect("observed_at is RFC 3339");
    }

    #[test]
    fn exploit_is_an_error_event() {
        let events = capture(|| {
            sandbox_signal!(
                exploit,
                "sandbox.test",
                sandbox_id = "sbx_1",
                source = "exo.test"
            );
        });
        let [event] = events.as_slice() else {
            panic!("expected one event, got {events:?}");
        };
        assert_eq!(event.level, tracing::Level::ERROR);
        assert_eq!(event.fields["severity"], "exploit");
    }
}
