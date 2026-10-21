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

/// Environment variable carrying the id of the daemon that started a session process.
pub const INSTANCE_VAR: &str = "BROWSER_DAEMON_INSTANCE";

static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Stable id of the daemon owning `state_dir` (FNV-1a of the path): the same
/// after a restart, different for a daemon with another state directory.
pub fn instance_id(state_dir: &std::path::Path) -> String {
    let dir = std::fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in dir.as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Set once at startup, before any session process is spawned.
pub fn set_instance(id: String) {
    let _ = INSTANCE.set(id);
}

/// What the sweep may rely on to tell this daemon's leftovers from other daemons'.
pub struct SweepScope<'a> {
    pub instance: &'a str,
    /// Directories whose mention in a command line marks a process as ours
    /// (profiles, state) -- only used for processes without an instance tag.
    pub dirs: &'a [std::path::PathBuf],
    /// Prefix of temporary profile paths (`<tmp>/browserd-`).
    pub temp_prefix: &'a str,
    /// Display numbers our own session records mention.
    pub displays: &'a std::collections::HashSet<u32>,
}

/// Command-line switch carrying the same tag: Chromium blanks its environment
/// (it reuses the memory for its process title), so the variable is unreadable.
pub const INSTANCE_ARG: &str = "--browser-daemon-instance=";

/// The instance this daemon tags its browsers with, as a switch for Chromium.
pub fn instance_arg() -> Option<String> {
    INSTANCE.get().map(|id| format!("{INSTANCE_ARG}{id}"))
}

fn args_instance(args: &str) -> Option<String> {
    args.split(' ')
        .find_map(|a| a.strip_prefix(INSTANCE_ARG))
        .map(str::to_string)
}

/// The instance tag in a `/proc/<pid>/environ` blob, if any.
fn environ_instance(raw: &[u8]) -> Option<String> {
    let prefix = format!("{INSTANCE_VAR}=");
    raw.split(|b| *b == 0)
        .filter_map(|e| std::str::from_utf8(e).ok())
        .find_map(|e| e.strip_prefix(&prefix))
        .map(str::to_string)
}

fn xvfb_display(args: &str) -> Option<u32> {
    let mut it = args.split(' ');
    it.next().filter(|a| a.ends_with("Xvfb"))?;
    it.next()?.strip_prefix(':')?.parse().ok()
}

/// Is this (session main) process left behind by this daemon, as opposed to a
/// live process of another daemon of the same user?
///
/// - Tagged with an instance id (every process started by this version): ours
///   only when the id is ours.
/// - Untagged (started by an older version): a browser is ours when its command
///   line names a path under our own directories (profiles, state, `browserd-*`
///   temp profiles); an Xvfb only when its display number appears in our session
///   records. Anything else is left alone.
fn is_ours(tag: Option<&str>, args: &str, scope: &SweepScope) -> bool {
    if let Some(tag) = tag {
        return tag == scope.instance;
    }
    if let Some(n) = xvfb_display(args) {
        return scope.displays.contains(&n);
    }
    mentions_path(args, scope.temp_prefix, false)
        || scope.dirs.iter().any(|d| {
            let d = d.to_string_lossy();
            !d.is_empty() && mentions_path(args, &d, true)
        })
}

/// Does `args` name `path` (or something below it)? With `whole`, `/a/b` does not
/// match `/a/bc`; a prefix like `<tmp>/browserd-` is meant to match more.
fn mentions_path(args: &str, path: &str, whole: bool) -> bool {
    let path = if whole {
        path.trim_end_matches('/')
    } else {
        path
    };
    args.match_indices(path).any(|(i, m)| {
        !whole
            || args[i + m.len()..]
                .chars()
                .next()
                .is_none_or(|c| c == '/' || c == ' ')
    })
}

/// Kill browsers and session displays of this daemon that no session record
/// claims (a crash, or a daemon from before sessions survived restarts left
/// them): they hold profiles and memory and nothing can reach them. Processes
/// of other daemons of the same user are left alone (see [`is_ours`]).
pub fn sweep_orphans(keep: &std::collections::HashSet<u32>, scope: &SweepScope) -> usize {
    sweep_orphans_in(std::path::Path::new("/proc"), keep, scope, |pid| {
        // SAFETY: plain kill(2) on the orphan's own process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    })
}

fn sweep_orphans_in(
    root: &std::path::Path,
    keep: &std::collections::HashSet<u32>,
    scope: &SweepScope,
    kill: impl Fn(u32),
) -> usize {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid(2) has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut killed = 0;
    let Ok(entries) = std::fs::read_dir(root) else {
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
        let dir = entry.path();
        // Group leaders reparented to init: what an earlier daemon left behind.
        let Some(stat) = std::fs::read_to_string(dir.join("stat")).ok() else {
            continue;
        };
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.get(1) != Some(&"1")
            || fields.get(2).and_then(|p| p.parse::<u32>().ok()) != Some(pid)
        {
            continue;
        }
        let Some(args) = std::fs::read(dir.join("cmdline")).ok().map(|raw| {
            String::from_utf8_lossy(&raw)
                .trim_end_matches('\0')
                .replace('\0', " ")
        }) else {
            continue;
        };
        if !is_session_main(&args) {
            continue;
        }
        let tag = args_instance(&args).or_else(|| {
            std::fs::read(dir.join("environ"))
                .ok()
                .and_then(|raw| environ_instance(&raw))
        });
        if is_ours(tag.as_deref(), &args, scope) {
            kill(pid);
            killed += 1;
        }
    }
    killed
}

/// Remove the temporary profiles (`browserd-*` in the temp dir) and Firefox logs
/// (`<sessions dir>/*.log`) no live session claims: a crash, a kill or a daemon
/// of an earlier version left them. Returns how many were removed.
pub fn sweep_leftover_files(
    sessions_dir: &std::path::Path,
    keep: &std::collections::HashSet<std::path::PathBuf>,
) -> usize {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid(2) has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut removed = 0;
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let ours = entry.file_name().to_string_lossy().starts_with("browserd-")
                && entry.metadata().is_ok_and(|m| m.is_dir() && m.uid() == uid);
            if ours && !keep.contains(&path) && std::fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "log")
                && !keep.contains(&path)
                && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
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
        // The main process is gone for good once its pid belongs to another process
        // (a reused pid may even lead a group of someone else's): leave the group
        // alone then. A pid that is simply gone cannot be reused while stragglers of
        // its group are left, so those are still reached.
        if !group_is_ours(start_ticks(self.pid), self.start) {
            return;
        }
        let Ok(pid) = i32::try_from(self.pid) else {
            return;
        };
        // Its own process group: the whole browser (renderers, helpers) at once.
        // SAFETY: plain kill(2) on a group this daemon created or adopted.
        unsafe {
            libc::kill(-pid, sig);
        }
    }

    /// Wait up to `max` for the main process to exit; whether it did.
    pub async fn wait_exit(&mut self, max: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + max;
        while self.alive() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
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

/// Whether the group led by the recorded process may be signalled, given the
/// start time `/proc` shows for that pid now: nothing there (the leader is gone,
/// maybe with stragglers behind) or the very process; not another one.
fn group_is_ours(current_start: Option<u64>, recorded_start: u64) -> bool {
    current_start.is_none_or(|now| now == recorded_start)
}

/// Put the child in a process group of its own (pgid = pid).
/// Tag it with this daemon's instance id, so another daemon's sweep leaves it alone.
pub fn own_group(cmd: &mut Command) {
    cmd.process_group(0);
    if let Some(id) = INSTANCE.get() {
        cmd.env(INSTANCE_VAR, id);
    }
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

    fn scope<'a>(
        dirs: &'a [std::path::PathBuf],
        displays: &'a std::collections::HashSet<u32>,
    ) -> SweepScope<'a> {
        SweepScope {
            instance: "aaaa",
            dirs,
            temp_prefix: "/tmp/browserd-",
            displays,
        }
    }

    #[test]
    fn the_instance_tag_is_read_from_environ() {
        let raw = b"A=1\0BROWSER_DAEMON_INSTANCE=abc\0B=2\0";
        assert_eq!(environ_instance(raw).as_deref(), Some("abc"));
        assert_eq!(environ_instance(b"A=1\0"), None);
        assert_eq!(
            args_instance("/x/chromium --a --browser-daemon-instance=abc --b").as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn instance_ids_follow_the_directory() {
        let a = instance_id(std::path::Path::new("/nonexistent/a"));
        assert_eq!(a, instance_id(std::path::Path::new("/nonexistent/a")));
        assert_ne!(a, instance_id(std::path::Path::new("/nonexistent/b")));
    }

    #[test]
    fn tagged_processes_belong_to_their_daemon_only() {
        let dirs = vec![std::path::PathBuf::from("/var/lib/bd")];
        let displays = std::collections::HashSet::new();
        let sc = scope(&dirs, &displays);
        let args = "/x/chromium --remote-debugging-port=0 --user-data-dir=/var/lib/bd/profiles/p";
        assert!(is_ours(Some("aaaa"), args, &sc));
        // Another daemon's browser, even one whose path looks like ours.
        assert!(!is_ours(Some("bbbb"), args, &sc));
    }

    #[test]
    fn untagged_processes_need_a_path_or_display_of_ours() {
        let dirs = vec![std::path::PathBuf::from("/var/lib/bd")];
        let displays = std::collections::HashSet::from([101]);
        let sc = scope(&dirs, &displays);
        assert!(is_ours(
            None,
            "/x/chromium --remote-debugging-port=0 --user-data-dir=/var/lib/bd/profiles/p",
            &sc
        ));
        assert!(is_ours(
            None,
            "/x/firefox --profile /tmp/browserd-1 --remote-debugging-port=0",
            &sc
        ));
        assert!(!is_ours(
            None,
            "/x/chromium --remote-debugging-port=0 --user-data-dir=/other/profiles/p",
            &sc
        ));
        assert!(!is_ours(
            None,
            "/x/chromium --remote-debugging-port=0 --user-data-dir=/var/lib/bd-test/profiles/p",
            &sc
        ));
        assert!(is_ours(None, "/x/Xvfb :101 -screen 0 1x1x24", &sc));
        assert!(!is_ours(None, "/x/Xvfb :102 -screen 0 1x1x24", &sc));
    }

    #[test]
    fn the_sweep_kills_only_matching_orphans_in_a_fake_proc() {
        use std::cell::RefCell;
        let root = std::env::temp_dir().join(format!("bd-fakeproc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |pid: u32, ppid: u32, pgid: u32, cmd: &str, env: &str| {
            let d = root.join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("stat"), format!("{pid} (x y) S {ppid} {pgid} 0")).unwrap();
            std::fs::write(d.join("cmdline"), cmd.replace(' ', "\0")).unwrap();
            std::fs::write(d.join("environ"), env).unwrap();
        };
        let chrome = "/x/chromium --remote-debugging-port=0 --user-data-dir=/other/p";
        mk(5001, 1, 5001, chrome, "BROWSER_DAEMON_INSTANCE=aaaa\0");
        mk(5002, 1, 5002, chrome, "BROWSER_DAEMON_INSTANCE=bbbb\0");
        mk(5003, 1, 5003, chrome, "");
        mk(5004, 77, 5004, chrome, "BROWSER_DAEMON_INSTANCE=aaaa\0");
        mk(5005, 1, 5005, chrome, "BROWSER_DAEMON_INSTANCE=aaaa\0");
        // Chromium: tag on the command line, environment blanked.
        let tagged = format!("{chrome} --browser-daemon-instance=aaaa");
        let other = format!("{chrome} --browser-daemon-instance=bbbb");
        mk(5006, 1, 5006, &tagged, "");
        mk(5007, 1, 5007, &other, "");
        let dirs = vec![];
        let displays = std::collections::HashSet::new();
        let sc = scope(&dirs, &displays);
        let keep = std::collections::HashSet::from([5005]);
        let killed = RefCell::new(Vec::new());
        let n = sweep_orphans_in(&root, &keep, &sc, |p| killed.borrow_mut().push(p));
        let _ = std::fs::remove_dir_all(&root);
        // Only the orphan tagged with our id: not another daemon's, not an
        // untagged one outside our dirs, not a child, not a kept one.
        let mut killed = killed.into_inner();
        killed.sort_unstable();
        assert_eq!(killed, vec![5001, 5006]);
        assert_eq!(n, 2);
    }

    #[test]
    fn a_reused_pid_is_not_signalled() {
        assert!(group_is_ours(Some(42), 42));
        assert!(group_is_ours(None, 42));
        assert!(!group_is_ours(Some(43), 42));
    }

    #[test]
    fn own_start_ticks_are_readable() {
        let pid = std::process::id();
        assert!(start_ticks(pid).is_some());
        assert_eq!(start_ticks(pid), start_ticks(pid));
        assert!(Proc::adopt(pid, 0).is_none());
    }
}
