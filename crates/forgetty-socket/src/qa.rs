//! Test-only extension point (feature `qa-tools`).
//!
//! The server binary registers one hook. `dispatch` offers it every request
//! the normal table does not know, so the protocol harness can read counters
//! and inject faults over the local socket. The feature is off in release
//! builds, so a shipped server has no such methods.

use std::sync::{Arc, OnceLock};

use forgetty_session::SessionManager;

use crate::protocol::{Request, Response};

/// Handler for extra `qa_*` methods. Returns `None` for a method it does not know.
pub type Hook = Arc<dyn Fn(&Request, &Arc<SessionManager>) -> Option<Response> + Send + Sync>;

static HOOK: OnceLock<Hook> = OnceLock::new();

/// Install the hook. The first call wins.
pub fn set_hook(hook: Hook) {
    let _ = HOOK.set(hook);
}

pub(crate) fn call(request: &Request, sm: &Arc<SessionManager>) -> Option<Response> {
    HOOK.get().and_then(|h| h(request, sm))
}
