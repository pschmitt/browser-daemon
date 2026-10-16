//! Who is on the other end of a loopback connection. The SOCKS relays cannot
//! use credentials (Chromium does not do SOCKS5 authentication), so a relay
//! accepts a connection only from the session's own browser: the socket of the
//! peer port is looked up in `/proc/net/tcp{,6}`, matched to a process through
//! `/proc/<pid>/fd`, and that process must be the browser's main process or a
//! descendant of it.
//!
//! Every failure to find out counts as "not the browser". Other users' processes
//! cannot be inspected (their fd directories are unreadable); the browsers run as
//! the daemon's user, so they always can.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the list of the browser's processes is reused. It is only a list of
/// candidates: a hit is verified again through the process tree.
const CANDIDATES_TTL: Duration = Duration::from_secs(2);
/// Deeper trees than this are not browsers.
const MAX_DEPTH: usize = 64;

/// The session's browser: its main process, identified by pid and start time
/// (a recycled pid is a different process).
#[derive(Default)]
pub struct Owner {
    process: Mutex<Option<(u32, u64)>>,
    candidates: Mutex<Option<(Instant, Vec<u32>)>>,
}

impl Owner {
    /// Called once the browser has been started (or adopted); until then every
    /// connection is refused.
    pub fn set(&self, pid: u32, start: u64) {
        *self.process.lock().unwrap() = Some((pid, start));
        *self.candidates.lock().unwrap() = None;
    }

    fn get(&self) -> Option<(u32, u64)> {
        *self.process.lock().unwrap()
    }

    /// Whether the process that holds the client end of `peer_port` -> `relay_port`
    /// belongs to the browser.
    pub fn allows(&self, proc_root: &Path, peer_port: u16, relay_port: u16) -> bool {
        let Some((owner, start)) = self.get() else {
            return false;
        };
        // The browser must still be the process we started.
        match read_stat(proc_root, owner) {
            Some(st) if st.start == start => {}
            _ => return false,
        }
        let inodes = client_inodes(proc_root, peer_port, relay_port);
        if inodes.is_empty() {
            return false;
        }
        let targets: HashSet<String> = inodes.iter().map(|i| format!("socket:[{i}]")).collect();

        let cached = {
            let c = self.candidates.lock().unwrap();
            c.as_ref()
                .filter(|(at, _)| at.elapsed() < CANDIDATES_TTL)
                .map(|(_, pids)| pids.clone())
        };
        if let Some(pids) = cached {
            if let Some(holder) = pids
                .into_iter()
                .find(|p| holds_socket(proc_root, *p, &targets))
            {
                if is_descendant(proc_root, holder, owner) {
                    return true;
                }
            }
        }
        // Not among the known processes (a new one, or a stale list): look again.
        let fresh = descendants(proc_root, owner);
        let holder = fresh
            .iter()
            .copied()
            .find(|p| holds_socket(proc_root, *p, &targets));
        *self.candidates.lock().unwrap() = Some((Instant::now(), fresh));
        holder.is_some_and(|h| is_descendant(proc_root, h, owner))
    }
}

/// Inodes of the client sockets of the connection `peer_port` -> `relay_port`
/// (the relay's own end has the ports the other way round).
fn client_inodes(proc_root: &Path, peer_port: u16, relay_port: u16) -> Vec<u64> {
    ["net/tcp", "net/tcp6"]
        .iter()
        .filter_map(|t| fs::read_to_string(proc_root.join(t)).ok())
        .flat_map(|text| parse_inodes(&text, peer_port, relay_port))
        .collect()
}

/// Sockets in a `/proc/net/tcp` or `tcp6` table whose local port is `local` and
/// remote port `remote`.
pub fn parse_inodes(table: &str, local: u16, remote: u16) -> Vec<u64> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (_sl, laddr, raddr) = (f.next()?, f.next()?, f.next()?);
            if hex_port(laddr)? != local || hex_port(raddr)? != remote {
                return None;
            }
            // st, queues, timer, retransmits, uid, timeout, inode.
            let inode: u64 = f.nth(6)?.parse().ok()?;
            (inode != 0).then_some(inode)
        })
        .collect()
}

/// The port of an `ADDR:PORT` pair in a table (the port is hex).
fn hex_port(addr: &str) -> Option<u16> {
    u16::from_str_radix(addr.rsplit_once(':')?.1, 16).ok()
}

#[derive(Debug, PartialEq, Eq)]
struct Stat {
    ppid: u32,
    start: u64,
}

/// Parent and start time from `/proc/<pid>/stat`. The command name is in
/// parentheses and may hold spaces and parentheses itself: parse from the last `)`.
fn parse_stat(text: &str) -> Option<Stat> {
    let rest = text.get(text.rfind(')')? + 1..)?;
    let mut f = rest.split_whitespace();
    // state, ppid, pgrp, session, tty_nr, tpgid, flags, minflt, cminflt, majflt,
    // cmajflt, utime, stime, cutime, cstime, priority, nice, threads, itrealvalue, starttime
    let ppid = f.nth(1)?.parse().ok()?;
    let start = f.nth(17)?.parse().ok()?;
    Some(Stat { ppid, start })
}

fn read_stat(proc_root: &Path, pid: u32) -> Option<Stat> {
    parse_stat(&fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?)
}

/// Whether `pid` is `owner` or below it in the process tree.
pub fn is_descendant(proc_root: &Path, pid: u32, owner: u32) -> bool {
    let mut cur = pid;
    for _ in 0..MAX_DEPTH {
        if cur == owner {
            return true;
        }
        if cur <= 1 {
            return false;
        }
        match read_stat(proc_root, cur) {
            Some(st) => cur = st.ppid,
            None => return false,
        }
    }
    false
}

/// `owner` and every process below it.
fn descendants(proc_root: &Path, owner: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(dir) = fs::read_dir(proc_root) {
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some(st) = read_stat(proc_root, pid) {
                children.entry(st.ppid).or_default().push(pid);
            }
        }
    }
    let mut out = vec![owner];
    let mut i = 0;
    while i < out.len() {
        if let Some(kids) = children.get(&out[i]) {
            out.extend(kids);
        }
        i += 1;
    }
    out
}

/// Whether one of the process's fds is one of the sockets (`socket:[inode]`).
fn holds_socket(proc_root: &Path, pid: u32, targets: &HashSet<String>) -> bool {
    let Ok(dir) = fs::read_dir(proc_root.join(pid.to_string()).join("fd")) else {
        return false;
    };
    dir.flatten().any(|e| {
        fs::read_link(e.path())
            .ok()
            .and_then(|l| l.to_str().map(|s| targets.contains(s)))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
0: 0100007F:C350 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 11111 1 0000000000000000 20 4 30 10 -1\n   \
1: 0100007F:1F90 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 22222 1 0000000000000000 20 4 30 10 -1\n   \
2: 0100007F:C351 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 33333 1 0000000000000000 20 4 30 10 -1\n";

    #[test]
    fn parses_the_client_end_only() {
        assert_eq!(parse_inodes(TABLE, 0xC350, 0x1F90), vec![11111]);
        assert_eq!(parse_inodes(TABLE, 0x1F90, 0xC350), vec![22222]);
        assert!(parse_inodes(TABLE, 1, 2).is_empty());
        assert!(parse_inodes("garbage\nnot a table", 1, 2).is_empty());
    }

    #[test]
    fn parses_tcp6_mapped_rows() {
        let t = "header\n 0: 0000000000000000FFFF00000100007F:C350 0000000000000000FFFF00000100007F:1F90 01 0:0 0:0 0 1000 0 44444 1\n";
        assert_eq!(parse_inodes(t, 0xC350, 0x1F90), vec![44444]);
    }

    #[test]
    fn stat_with_awkward_command_name() {
        let ok = "123 (a (b) c) S 77 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 4242 0 0";
        assert_eq!(
            parse_stat(ok),
            Some(Stat {
                ppid: 77,
                start: 4242
            })
        );
        assert_eq!(parse_stat("no parens"), None);
        assert_eq!(parse_stat("1 (x) S 2"), None);
    }

    /// A fake /proc: (pid, ppid, start, socket inodes held).
    fn fake_proc(procs: &[(u32, u32, u64, &[u64])], tables: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("net")).unwrap();
        for (name, text) in tables {
            fs::write(d.path().join("net").join(name), text).unwrap();
        }
        for (pid, ppid, start, socks) in procs {
            let p = d.path().join(pid.to_string());
            fs::create_dir_all(p.join("fd")).unwrap();
            fs::write(
                p.join("stat"),
                format!(
                    "{pid} (some proc) S {ppid} 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0"
                ),
            )
            .unwrap();
            symlink("/dev/null", p.join("fd/0")).unwrap();
            for (i, s) in socks.iter().enumerate() {
                symlink(format!("socket:[{s}]"), p.join(format!("fd/{}", i + 3))).unwrap();
            }
        }
        d
    }

    #[test]
    fn ancestry_walk() {
        // 100 (browser) -> 101 -> 102; 200 unrelated; 300 child of 200.
        let d = fake_proc(
            &[
                (1, 0, 1, &[]),
                (100, 1, 500, &[]),
                (101, 100, 501, &[]),
                (102, 101, 502, &[]),
                (200, 1, 600, &[]),
                (300, 200, 601, &[]),
            ],
            &[],
        );
        let r = d.path();
        assert!(is_descendant(r, 100, 100));
        assert!(is_descendant(r, 102, 100));
        assert!(!is_descendant(r, 300, 100));
        assert!(!is_descendant(r, 200, 100));
        assert!(!is_descendant(r, 999, 100));
        let mut ds = descendants(r, 100);
        ds.sort();
        assert_eq!(ds, vec![100, 101, 102]);
    }

    #[test]
    fn owner_decides() {
        let d = fake_proc(
            &[
                (1, 0, 1, &[]),
                (100, 1, 500, &[]),
                (102, 100, 502, &[11111]),
                (300, 1, 601, &[33333]),
            ],
            &[("tcp", TABLE)],
        );
        let r = d.path();
        let owner = Owner::default();
        // Not set yet: refused.
        assert!(!owner.allows(r, 0xC350, 0x1F90));
        owner.set(100, 500);
        assert!(owner.allows(r, 0xC350, 0x1F90));
        // Held by a process outside the browser.
        assert!(!owner.allows(r, 0xC351, 0x1F90));
        // Unknown connection.
        assert!(!owner.allows(r, 0xC352, 0x1F90));
        // Recycled pid (other start time).
        let stale = Owner::default();
        stale.set(100, 499);
        assert!(!stale.allows(r, 0xC350, 0x1F90));
    }

    #[test]
    fn unreadable_tables_fail_closed() {
        let d = fake_proc(&[(100, 1, 500, &[11111])], &[]);
        let owner = Owner::default();
        owner.set(100, 500);
        assert!(!owner.allows(d.path(), 0xC350, 0x1F90));
    }
}
