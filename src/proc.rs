//! Long-lived session processes (browser, Xvfb) that outlive the daemon: they
//! run in a process group of their own, are identified by pid + start time
//! (so a reused pid is never mistaken for them) and are signalled by group.
//! A restarted daemon adopts them from their saved records (see `persist`).

use std::time::Duration;
use tokio::process::{Child, Command};

/// Seconds since boot (in clock ticks) the process started at, from /proc/<pid>/stat.
pub fn start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ..." where comm may contain spaces and parentheses.
    let rest = &stat[stat.rfind(')')? + 1..];
    // Fields after the comm start at "state" (3rd): starttime is the 22nd.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// A process the daemon started, now or in a previous run.
pub struct Proc {
    pub pid: u32,
    pub start: u64,
    /// Present when this daemon spawned it: reaped through it.
    child: Option<Child>,
    done: bool,
}

impl Proc {
    pub fn spawned(child: Child) -> Option<Self> {
        let pid = child.id()?;
        Some(Self {
            pid,
            start: start_ticks(pid)?,
            child: Some(child),
            done: false,
        })
    }

    /// A process of an earlier daemon run, if it is still the one recorded.
    pub fn adopt(pid: u32, start: u64) -> Option<Self> {
        (start_ticks(pid) == Some(start)).then_some(Self {
            pid,
            start,
            child: None,
            done: false,
        })
    }

    pub fn alive(&mut self) -> bool {
        if self.done {
            return false;
        }
        if let Some(child) = self.child.as_mut() {
            return matches!(child.try_wait(), Ok(None));
        }
        start_ticks(self.pid) == Some(self.start)
    }

    fn signal(&self, sig: i32) {
        // Its own process group: the whole browser (renderers, helpers) at once.
        // SAFETY: plain kill(2) on a group this daemon created or adopted.
        unsafe {
            libc::kill(-(self.pid as i32), sig);
        }
    }

    /// SIGTERM, then SIGKILL once `grace` is over; returns when it is gone.
    pub async fn terminate(&mut self, grace: Duration) {
        if self.done {
            return;
        }
        self.signal(libc::SIGTERM);
        let deadline = tokio::time::Instant::now() + grace;
        while self.alive() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if self.alive() {
            self.signal(libc::SIGKILL);
            if let Some(child) = self.child.as_mut() {
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }
        }
        // Stragglers of the group (renderers outliving the main process).
        self.signal(libc::SIGKILL);
        self.done = true;
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if !self.done {
            self.signal(libc::SIGKILL);
        }
    }
}

/// Put the child in a process group of its own (pgid = pid).
pub fn own_group(cmd: &mut Command) {
    cmd.process_group(0);
}

/// Kill the child when the daemon dies: helpers (ffmpeg encoders, ...) must not
/// outlive it now that the service no longer kills its whole control group.
pub fn die_with_parent(cmd: &mut Command) {
    // SAFETY: prctl(2) between fork and exec, async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_start_ticks_are_readable() {
        let pid = std::process::id();
        assert!(start_ticks(pid).is_some());
        assert_eq!(start_ticks(pid), start_ticks(pid));
        assert!(Proc::adopt(pid, 0).is_none());
    }
}
