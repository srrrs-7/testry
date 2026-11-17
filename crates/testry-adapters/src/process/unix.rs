//! Child process control on POSIX systems (spec section 10).
//!
//! Every command runs in its own process group. Stopping it means stopping
//! the whole group: `SIGTERM`, a grace period, `SIGKILL`, then waiting until
//! the group is gone. Killing only the direct child, or relying on
//! kill-on-drop, would leave grandchildren behind.
//!
//! The group is also checked after a normal exit: a command that leaves
//! background processes behind has them stopped, and the fact is reported.
//!
//! stdout and stderr are drained concurrently into files. Output beyond the
//! size limit is still read (so the child never blocks on a full pipe) but is
//! not stored, and the truncation is recorded.
//!
//! A process can leave its group: browsers and development servers are
//! commonly started in a session of their own, so a signal to the group of
//! their runner does not reach them. Every command therefore gets a unique
//! marker in its environment, which its descendants inherit. After the group
//! is gone, processes that still carry the marker are looked up in `/proc`
//! and stopped as well. A process that cleared its environment, or one on a
//! system without `/proc`, is outside this control. testry v0.1 runs trusted
//! code; it does not claim to contain hostile processes.

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use testry_application::ports::{Cancellation, PortError};
use testry_domain::results::ProcessExit;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

/// How long the group is given to disappear after `SIGKILL`.
const KILL_CONFIRMATION: Duration = Duration::from_secs(5);
/// How long output draining may continue after the group was stopped. Only a
/// process that escaped the group can keep a pipe open that long.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long a process outside the group may take to end on its own after the
/// group is gone (a browser that notices its closed pipe) before it counts as
/// left behind.
const ESCAPED_SETTLE: Duration = Duration::from_millis(500);

/// The environment variable that marks every process of one command.
pub const PROCESS_COOKIE_VARIABLE: &str = "TESTRY_PROCESS_COOKIE";

static COOKIE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A value no other command of this or another testry process uses. It
/// starts with the owner (the run), so what a run left behind can still be
/// attributed to it after testry itself is gone.
fn new_cookie(owner: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{owner}{OWNER_SEPARATOR}{}-{}-{nanos}",
        std::process::id(),
        COOKIE_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Separates the owner from the rest of a marker. It cannot occur in a run ID.
const OWNER_SEPARATOR: char = '/';

/// A process that carries the marker of an owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedProcess {
    pub pid: i32,
    /// The short name of the process, for display.
    pub name: String,
}

fn owner_of(environment: &[u8]) -> Option<&[u8]> {
    let prefix = format!("{PROCESS_COOKIE_VARIABLE}=");
    environment
        .split(|byte| *byte == 0)
        .find_map(|variable| variable.strip_prefix(prefix.as_bytes()))
        .and_then(|cookie| {
            cookie
                .iter()
                .position(|byte| *byte == OWNER_SEPARATOR as u8)
                .and_then(|end| cookie.get(..end))
        })
}

/// The live processes that were started for `owner`. Only processes of the
/// same user are visible; on a system without `/proc` nothing is found.
pub fn processes_of_owner(owner: &str) -> Vec<OwnedProcess> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes: Vec<OwnedProcess> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            let environment = std::fs::read(entry.path().join("environ")).ok()?;
            (owner_of(&environment)? == owner.as_bytes()).then(|| OwnedProcess {
                pid,
                name: std::fs::read_to_string(entry.path().join("comm"))
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            })
        })
        .collect();
    processes.sort_by_key(|process| process.pid);
    processes
}

/// Stops what is left of an owner: `SIGTERM`, the grace period, `SIGKILL`.
/// Returns the processes that could not be confirmed gone.
pub async fn stop_processes_of_owner(owner: &str, grace: Duration) -> Vec<OwnedProcess> {
    let signal_all = |signal: Signal| {
        for process in processes_of_owner(owner) {
            let _ = kill(Pid::from_raw(process.pid), signal);
        }
    };
    let gone_within = async |limit: Duration| {
        let deadline = Instant::now() + limit;
        loop {
            if processes_of_owner(owner).is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };
    if processes_of_owner(owner).is_empty() {
        return Vec::new();
    }
    signal_all(Signal::SIGTERM);
    if !gone_within(grace).await {
        signal_all(Signal::SIGKILL);
        gone_within(KILL_CONFIRMATION).await;
    }
    processes_of_owner(owner)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Who the command belongs to: the run ID. It is part of the marker
    /// every process of the command carries. It must not contain `/`.
    pub owner: String,
    /// Absolute working directory.
    pub cwd: PathBuf,
    /// The complete environment of the child. Nothing is inherited.
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    /// Time between `SIGTERM` and `SIGKILL`.
    pub grace: Duration,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub max_bytes_per_stream: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutcome {
    pub exit: ProcessExit,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    /// Measured with a monotonic clock.
    pub duration: Duration,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// The command exited but left processes behind, in its group or outside
    /// of it; they were stopped.
    pub stopped_survivors: bool,
    /// Processes of the command still existed after `SIGKILL` and the
    /// confirmation period.
    pub residual_group: bool,
    /// A pipe stayed open after the group was gone, so output may be missing.
    pub output_incomplete: bool,
}

/// Runs a command to completion, to its timeout, or until cancellation.
pub async fn run(
    spec: &ProcessSpec,
    cancellation: &dyn Cancellation,
) -> Result<ProcessOutcome, PortError> {
    if cancellation.is_cancelled() {
        return Err(PortError::Cancelled);
    }
    let spawn_error = |message: String| PortError::Spawn {
        program: spec.program.clone(),
        message,
    };

    let cookie = new_cookie(&spec.owner);
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .env(PROCESS_COOKIE_VARIABLE, &cookie)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A new process group whose ID is the PID of the child.
        .process_group(0)
        // Last line of defence if this future is dropped; the normal path
        // below stops the whole group explicitly.
        .kill_on_drop(true);

    let started_at = SystemTime::now();
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(error.kind().to_string()))?;
    let group = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .map(Pid::from_raw)
        .ok_or_else(|| spawn_error("the child has no process id".to_owned()))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut stdout_task = tokio::spawn(drain(
        stdout,
        spec.stdout_path.clone(),
        spec.max_bytes_per_stream,
    ));
    let mut stderr_task = tokio::spawn(drain(
        stderr,
        spec.stderr_path.clone(),
        spec.max_bytes_per_stream,
    ));

    let exit = tokio::select! {
        status = child.wait() => match status {
            Ok(status) => match (status.code(), status.signal()) {
                (Some(code), _) => ProcessExit::Exited { code },
                (None, Some(signal)) => ProcessExit::Signaled { signal },
                (None, None) => ProcessExit::Signaled { signal: 0 },
            },
            Err(error) => {
                stop_group(group, spec.grace, &mut child).await;
                return Err(spawn_error(format!("waiting for the child failed: {}", error.kind())));
            }
        },
        () = tokio::time::sleep(spec.timeout) => ProcessExit::TimedOut,
        () = cancellation.cancelled() => ProcessExit::Cancelled,
    };

    // Whatever happened, nothing of this command may keep running.
    let stop = stop_group(group, spec.grace, &mut child).await;
    let escaped = stop_escaped(&cookie, spec.grace).await;
    let duration = started.elapsed();
    let finished_at = SystemTime::now();

    let mut output_incomplete = false;
    let mut finish = async |task: &mut tokio::task::JoinHandle<std::io::Result<bool>>| {
        match tokio::time::timeout(DRAIN_AFTER_EXIT, &mut *task).await {
            Ok(Ok(Ok(truncated))) => truncated,
            Ok(_) => {
                output_incomplete = true;
                false
            }
            Err(_) => {
                task.abort();
                output_incomplete = true;
                false
            }
        }
    };
    let stdout_truncated = finish(&mut stdout_task).await;
    let stderr_truncated = finish(&mut stderr_task).await;

    Ok(ProcessOutcome {
        exit,
        started_at,
        finished_at,
        duration,
        stdout_truncated,
        stderr_truncated,
        stopped_survivors: (stop.had_survivors || escaped.found)
            && matches!(
                exit,
                ProcessExit::Exited { .. } | ProcessExit::Signaled { .. }
            ),
        residual_group: stop.residual || escaped.residual,
        output_incomplete,
    })
}

struct GroupStop {
    /// Processes were still in the group when stopping began.
    had_survivors: bool,
    /// The group could not be confirmed empty.
    residual: bool,
}

fn group_exists(group: Pid) -> bool {
    // Signal 0 only checks for existence. EPERM means it exists but may not
    // be signalled, which still counts as existing.
    !matches!(killpg(group, None), Err(Errno::ESRCH))
}

/// Reaps the direct child if it has exited, so it does not linger as a zombie
/// member of the group.
fn reap(child: &mut Child) {
    let _ = child.try_wait();
}

async fn wait_until_gone(group: Pid, child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        reap(child);
        if !group_exists(group) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// SIGTERM, grace period, SIGKILL, wait and reap.
async fn stop_group(group: Pid, grace: Duration, child: &mut Child) -> GroupStop {
    reap(child);
    if !group_exists(group) {
        return GroupStop {
            had_survivors: false,
            residual: false,
        };
    }
    let _ = killpg(group, Signal::SIGTERM);
    let mut gone = wait_until_gone(group, child, grace).await;
    if !gone {
        let _ = killpg(group, Signal::SIGKILL);
        gone = wait_until_gone(group, child, KILL_CONFIRMATION).await;
    }
    // The direct child is always reaped, even when the group lingers.
    let _ = tokio::time::timeout(KILL_CONFIRMATION, child.wait()).await;
    GroupStop {
        had_survivors: true,
        residual: !gone && group_exists(group),
    }
}

struct EscapedStop {
    /// Processes of the command were found outside its group.
    found: bool,
    /// Some of them could not be confirmed gone.
    residual: bool,
}

/// The processes that carry the marker of a command. Only processes of the
/// same user are visible; a zombie has no environment and is not listed.
fn processes_with_cookie(cookie: &str) -> Vec<Pid> {
    let marker = format!("{PROCESS_COOKIE_VARIABLE}={cookie}");
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            let environment = std::fs::read(entry.path().join("environ")).ok()?;
            environment
                .split(|byte| *byte == 0)
                .any(|variable| variable == marker.as_bytes())
                .then(|| Pid::from_raw(pid))
        })
        .collect()
}

async fn wait_until_no_cookie(cookie: &str, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if processes_with_cookie(cookie).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Stops what the command started outside of its process group.
async fn stop_escaped(cookie: &str, grace: Duration) -> EscapedStop {
    // Most of the time there is nothing, or it ends by itself right away.
    if wait_until_no_cookie(cookie, ESCAPED_SETTLE).await {
        return EscapedStop {
            found: false,
            residual: false,
        };
    }
    for process in processes_with_cookie(cookie) {
        let _ = kill(process, Signal::SIGTERM);
    }
    let mut gone = wait_until_no_cookie(cookie, grace).await;
    if !gone {
        // New processes may have appeared in the meantime: list again.
        for process in processes_with_cookie(cookie) {
            let _ = kill(process, Signal::SIGKILL);
        }
        gone = wait_until_no_cookie(cookie, KILL_CONFIRMATION).await;
    }
    EscapedStop {
        found: true,
        residual: !gone,
    }
}

/// Copies a pipe into a file that only the owner can read. Returns whether
/// output was dropped because of the size limit.
async fn drain<R>(reader: Option<R>, path: PathBuf, limit: u64) -> std::io::Result<bool>
where
    R: AsyncRead + Unpin,
{
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .await?;
    let Some(mut reader) = reader else {
        return Ok(false);
    };
    let mut buffer = vec![0u8; 64 * 1024];
    let mut stored: u64 = 0;
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = usize::try_from(limit.saturating_sub(stored)).unwrap_or(usize::MAX);
        let keep = read.min(remaining);
        if keep > 0 {
            file.write_all(buffer.get(..keep).unwrap_or_default())
                .await?;
            stored += keep as u64;
        }
        if keep < read {
            // Keep reading so the child is never blocked by a full pipe.
            truncated = true;
        }
    }
    file.flush().await?;
    Ok(truncated)
}
