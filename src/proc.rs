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

/// The command line of a process, arguments joined by spaces.
pub fn cmdline(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    Some(text.trim_end_matches('\0').replace('\0', " "))
}

fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// Is this the main process of a session browser or its X server, by command line?
fn is_session_main(args: &str) -> bool {
    let browser = args.contains("--remote-debugging-port")
        && (args.contains("--user-data-dir=") || args.contains("--profile "));
    let xvfb = args.split(' ').next().is_some_and(|a| a.ends_with("Xvfb"))
        && args
            .split(' ')
            .nth(1)
            .and_then(|d| d.strip_prefix(':'))
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| n >= 100);
    browser || xvfb
}

/// Kill browsers and session displays of this user that no session record
/// claims (a crash, or a daemon from before sessions survived restarts left
/// them): they hold profiles and memory and nothing can reach them.
pub fn sweep_orphans(keep: &std::collections::HashSet<u32>) -> usize {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid(2) has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut killed = 0;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if keep.contains(&pid) || entry.metadata().map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        // Group leaders reparented to init: what an earlier daemon left behind.
        let Some(fields) = stat_fields(pid) else {
            continue;
        };
        let (ppid, pgid) = (fields.get(1), fields.get(2));
        if ppid.map(String::as_str) != Some("1")
            || pgid.map(String::as_str) != Some(&pid.to_string())
        {
            continue;
        }
        if cmdline(pid).is_some_and(|a| is_session_main(&a)) {
            // SAFETY: plain kill(2) on the orphan's own process group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            killed += 1;
        }
    }
    killed
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
    fn session_main_processes_are_recognised() {
        assert!(is_session_main(
            "/nix/store/x-chromium/bin/chromium --remote-debugging-port=0 --user-data-dir=/var/lib/p/1"
        ));
        assert!(is_session_main(
            "/nix/store/x-firefox/bin/firefox --new-instance --profile /var/lib/p/1 --remote-debugging-port=0"
        ));
        assert!(is_session_main(
            "/nix/store/x/bin/Xvfb :100 -screen 0 3840x2160x24"
        ));
        // The shared display and renderer helpers are not sessions.
        assert!(!is_session_main(
            "/nix/store/x/bin/Xvfb :98 -screen 0 1x1x24"
        ));
        assert!(!is_session_main(
            "/nix/store/x-chromium/bin/chromium --type=renderer --user-data-dir=/p"
        ));
    }

    #[test]
    fn own_start_ticks_are_readable() {
        let pid = std::process::id();
        assert!(start_ticks(pid).is_some());
        assert_eq!(start_ticks(pid), start_ticks(pid));
        assert!(Proc::adopt(pid, 0).is_none());
    }
}
