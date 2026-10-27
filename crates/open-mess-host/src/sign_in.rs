//! Sign-in imports (spec 8.3, 9.1). The configuration window that shows them comes with a later
//! milestone (E/F2); until then they refuse. A sign-in import that waits for the user runs with the
//! call deadline paused, so the user's time never counts against the call limit (spec 8.3, R4).

use std::future::Future;

use crate::actor::Deadline;

/// Test support: answers `wam-token` in place of Windows. It gets the scopes, whether the call is
/// interactive and the claims asked for, and returns a token or the reason it refuses
/// (`sign-in-error::refused`).
#[cfg(any(test, feature = "test-support"))]
pub type FakeWam = std::sync::Arc<dyn Fn(&[String], bool, Option<&str>) -> Result<String, String> + Send + Sync>;

/// The refusal every sign-in import answers with for now.
pub(crate) const UNAVAILABLE: &str = "sign-in UI not available";

/// Runs a sign-in step that waits for the user, with the call deadline paused meanwhile.
pub(crate) async fn wait_for_user<T>(deadline: &Deadline, step: impl Future<Output = T>) -> T {
    let _paused = deadline.pause();
    step.await
}
