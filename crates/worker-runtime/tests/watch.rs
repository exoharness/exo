// Compile the portable watch adapter on native test runners; the Worker crate
// itself is wasm-only. This includes the production implementation unchanged.
#[path = "../src/watch.rs"]
mod watch;
use anyhow::Result;
use executor::runtime_host::RuntimeHost;
use futures::future::BoxFuture;
use watch::Watches;
struct Timer;
impl RuntimeHost for Timer {
    fn spawn(&self, _: BoxFuture<'static, ()>) {
        panic!("watch must not spawn a push task")
    }
    fn sleep(&self, _: std::time::Duration) -> BoxFuture<'static, Result<()>> {
        Box::pin(futures::future::pending())
    }
}

mod tests {
    use super::*;

    #[test]
    fn stream_failure_emits_an_sse_error_then_releases_the_handle() -> Result<()> {
        let watches = Watches::default();
        let host = Timer;
        watches.open(
            7,
            Box::pin(futures::stream::iter([Err(anyhow::anyhow!(
                "journal unavailable"
            ))])),
            true,
        );
        let frame = futures::executor::block_on(watches.next(&host, 7))?.expect("error frame");
        assert!(frame.starts_with("event: error\n"));
        assert!(frame.contains("journal unavailable"));
        assert!(futures::executor::block_on(watches.next(&host, 7))?.is_none());
        assert!(futures::executor::block_on(watches.next(&host, 7)).is_err());
        Ok(())
    }
}
