//! Idle tracking: sessions nobody has used for a while are stopped.

use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Longest idle timeout that can be asked for: a year. Anything more is a
/// "never" in all but name, and must not overflow when turned into seconds.
pub const MAX_TIMEOUT_MINUTES: u64 = 366 * 24 * 60;

/// An idle timeout given in minutes, as seconds; 0 stays "never".
pub fn minutes_to_secs(minutes: u64) -> u64 {
    minutes.min(MAX_TIMEOUT_MINUTES) * 60
}

pub struct Activity {
    last: AtomicU64,
    /// 0 disables the idle stop for this session.
    timeout_secs: AtomicU64,
}

impl Activity {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            last: AtomicU64::new(now_secs()),
            timeout_secs: AtomicU64::new(timeout_secs),
        }
    }

    pub fn touch(&self) {
        self.last.store(now_secs(), Ordering::Relaxed);
    }

    pub fn idle_secs(&self) -> u64 {
        now_secs().saturating_sub(self.last.load(Ordering::Relaxed))
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs.load(Ordering::Relaxed)
    }

    pub fn set_timeout_secs(&self, secs: u64) {
        self.timeout_secs.store(secs, Ordering::Relaxed);
        self.touch();
    }

    pub fn expired(&self) -> bool {
        let timeout = self.timeout_secs();
        timeout > 0 && self.idle_secs() >= timeout
    }

    /// Every command from an automation client counts. An observer (the
    /// handoff page) streams screenshots and polls page info all the time,
    /// so only its input and navigation count.
    pub fn note_command(&self, text: &str, observer: bool) {
        if !observer || is_interactive(text) {
            self.touch();
        }
    }
}

#[derive(Deserialize)]
struct MethodOnly<'a> {
    #[serde(borrow)]
    method: Option<&'a str>,
}

fn is_interactive(text: &str) -> bool {
    let Ok(MethodOnly {
        method: Some(method),
    }) = serde_json::from_str::<MethodOnly>(text)
    else {
        return false;
    };
    const INTERACTIVE: &[&str] = &[
        "Input.",
        "input.",
        "Page.navigate",
        "Page.reload",
        "Target.createTarget",
        "browsingContext.navigate",
        "browsingContext.reload",
        "browsingContext.traverseHistory",
        "browsingContext.create",
        "browsingContext.handleUserPrompt",
        "Page.handleJavaScriptDialog",
    ];
    INTERACTIVE.iter().any(|p| method.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_polling_is_not_activity() {
        assert!(!is_interactive(
            r#"{"id":1,"method":"Page.screencastFrameAck","params":{"sessionId":1}}"#
        ));
        assert!(!is_interactive(
            r#"{"id":2,"method":"browsingContext.captureScreenshot","params":{}}"#
        ));
        assert!(!is_interactive(r#"{"id":3,"method":"Runtime.evaluate"}"#));
        assert!(is_interactive(
            r#"{"id":4,"method":"Input.dispatchMouseEvent"}"#
        ));
        assert!(is_interactive(
            r#"{"id":5,"method":"input.performActions"}"#
        ));
        assert!(is_interactive(r#"{"id":6,"method":"Page.navigate"}"#));
        assert!(!is_interactive("not json"));
    }

    #[test]
    fn huge_timeouts_do_not_wrap() {
        assert_eq!(minutes_to_secs(0), 0);
        assert_eq!(minutes_to_secs(30), 1800);
        assert_eq!(minutes_to_secs(u64::MAX), MAX_TIMEOUT_MINUTES * 60);
        assert!(minutes_to_secs(u64::MAX / 60 + 1) > 0);
    }

    #[test]
    fn zero_timeout_never_expires() {
        let a = Activity::new(0);
        a.last.store(0, Ordering::Relaxed);
        assert!(!a.expired());
        a.set_timeout_secs(60);
        a.last.store(0, Ordering::Relaxed);
        assert!(a.expired());
    }
}
