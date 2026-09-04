//! A request to stop, as something a tick can watch without owning signals.
//!
//! Only one thing in a tick is slow enough to be worth interrupting: the
//! gzip of a large generation, which runs on a blocking thread. Everything
//! else is a stat, a rename or a socket request, and a tick that is
//! interrupted between a rename and the reopen that follows it leaves shep
//! writing into a file with the wrong name, which is worse than the wait.
//! So the tick asks this type two things, and only from its tidy loop: is a
//! stop requested before this base, and did one arrive while this base's
//! gzip was running.
//!
//! The poll loop owns the signal. It turns ctrl-c into a request here, and
//! watches the same request while it sleeps between ticks.

use tokio::sync::watch;

/// Where a stop request is read.
#[derive(Debug)]
pub struct Stop(watch::Receiver<bool>);

/// Where a stop request is made. Dropping it without requesting means no
/// request ever comes, which is what a test that never stops wants.
#[derive(Debug)]
pub struct Request(watch::Sender<bool>);

impl Stop {
    /// A stop and the handle that requests it.
    pub fn new() -> (Self, Request) {
        let (sender, receiver) = watch::channel(false);
        (Self(receiver), Request(sender))
    }

    /// A stop nothing will ever request.
    #[cfg(test)]
    pub fn never() -> Self {
        Self::new().0
    }

    /// A stop that ctrl-c requests.
    ///
    /// The listener is a spawned task, so this needs a runtime. If the
    /// handler cannot be installed the dog runs until the shepherd stops it,
    /// which is how it ran before signals were watched at all, and a ctrl-c
    /// then ends it the way the OS does with no handler in place: at once,
    /// without the tidy loop's chance to skip or abandon a gzip. Requesting
    /// a stop on that failure instead would exit a dog nobody asked to stop,
    /// and the shepherd would restart it into the same failure.
    pub fn on_ctrl_c() -> Self {
        let (stop, request) = Self::new();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                request.request();
            }
        });
        stop
    }

    /// Whether a stop has been requested.
    pub fn requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolve once a stop is requested, immediately if it already was, and
    /// never if the [`Request`] was dropped without one.
    pub async fn wait(&mut self) {
        // `wait_for` checks the value before it checks for a dropped
        // sender, so a request made before the wait is still seen. It
        // errors only when the sender is gone and the value is still
        // false, which is the "nobody will ever ask" case.
        if self.0.wait_for(|&requested| requested).await.is_err() {
            core::future::pending::<()>().await;
        }
    }
}

impl Request {
    /// Ask every [`Stop`] made with this to stop.
    pub fn request(self) {
        // Nothing to do if every Stop is already gone.
        let _ = self.0.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn a_request_wakes_a_waiter() {
        let (mut stop, request) = Stop::new();
        assert!(!stop.requested());
        let waiter = tokio::spawn(async move {
            stop.wait().await;
            stop.requested()
        });
        request.request();
        assert!(
            timeout(Duration::from_secs(5), waiter)
                .await
                .expect("woke")
                .expect("joined")
        );
    }

    #[tokio::test]
    async fn a_request_made_before_the_wait_still_wakes_it() {
        let (mut stop, request) = Stop::new();
        request.request();
        assert!(stop.requested());
        timeout(Duration::from_secs(5), stop.wait())
            .await
            .expect("a request already made resolves the wait at once");
    }

    #[tokio::test]
    async fn no_request_never_wakes() {
        // Including once the Request is gone: a dropped handle is "nobody
        // will ever ask", not "asked".
        let mut stop = Stop::never();
        assert!(!stop.requested());
        assert!(
            timeout(Duration::from_millis(50), stop.wait())
                .await
                .is_err(),
            "nothing requested a stop, so the wait must not resolve"
        );
        assert!(!stop.requested());
    }
}
