//! Process control with real child processes (spec section 18: R04-R06 at
//! the process level).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use testry_adapters::process::unix::{
    ProcessOutcome, ProcessSpec, processes_of_owner, run, stop_processes_of_owner,
};
use testry_application::ports::{Cancellation, NeverCancelled, PortError};
use testry_domain::results::ProcessExit;
use tokio::sync::Notify;

/// A cancellation source a test can trigger.
#[derive(Default)]
struct Trigger {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Trigger {
    fn fire(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

impl Cancellation for Trigger {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn cancelled<'a>(&'a self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}

struct Sandbox {
    directory: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn spec(&self, script: &str) -> ProcessSpec {
        ProcessSpec {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            owner: "00000000-0000-4000-8000-00000000test".to_owned(),
            cwd: self.directory.path().to_path_buf(),
            env: BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]),
            timeout: Duration::from_secs(20),
            grace: Duration::from_millis(300),
            stdout_path: self.directory.path().join("stdout.log"),
            stderr_path: self.directory.path().join("stderr.log"),
            max_bytes_per_stream: 1024 * 1024,
        }
    }

    fn stdout(&self) -> String {
        std::fs::read_to_string(self.directory.path().join("stdout.log")).unwrap()
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(self.directory.path().join("stderr.log")).unwrap()
    }
}

async fn run_ok(spec: &ProcessSpec) -> ProcessOutcome {
    run(spec, &NeverCancelled).await.unwrap()
}

/// Whether any process has `marker` as one of its arguments. Each test uses
/// its own marker (an unusual sleep duration), so tests do not see each other.
fn process_with_argument_exists(marker: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        panic!("/proc must be readable in the test container");
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let is_pid = name
            .to_str()
            .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()));
        if !is_pid {
            return false;
        }
        // A zombie has an empty command line and is not a running process.
        std::fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| {
            cmdline
                .split(|byte| *byte == 0)
                .any(|argument| argument == marker.as_bytes())
        })
    })
}

fn wait_until_absent(marker: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !process_with_argument_exists(marker) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[tokio::test]
async fn captures_exit_code_and_both_streams() {
    let sandbox = Sandbox::new();
    let outcome = run_ok(&sandbox.spec("echo out; echo err >&2; exit 7")).await;
    assert_eq!(outcome.exit, ProcessExit::Exited { code: 7 });
    assert_eq!(sandbox.stdout(), "out\n");
    assert_eq!(sandbox.stderr(), "err\n");
    assert!(!outcome.stdout_truncated && !outcome.stderr_truncated);
    assert!(!outcome.stopped_survivors && !outcome.residual_group && !outcome.output_incomplete);
    assert!(outcome.finished_at >= outcome.started_at);

    let success = run_ok(&sandbox.spec("true")).await;
    assert_eq!(success.exit, ProcessExit::Exited { code: 0 });
    assert!(success.exit.is_success());
}

#[tokio::test]
async fn the_environment_is_exactly_what_was_given() {
    let sandbox = Sandbox::new();
    let mut spec = sandbox.spec("env");
    spec.env.insert("TESTRY_GIVEN".to_owned(), "yes".to_owned());
    run_ok(&spec).await;
    let environment = sandbox.stdout();
    assert!(environment.contains("TESTRY_GIVEN=yes"));
    for inherited in ["HOME=", "CARGO", "USER=", "HOSTNAME="] {
        assert!(
            !environment.contains(inherited),
            "{inherited} leaked into the child:\n{environment}"
        );
    }
}

#[tokio::test]
async fn runs_in_the_given_directory_without_a_shell_for_arguments() {
    let sandbox = Sandbox::new();
    let mut spec = sandbox.spec("");
    spec.program = "/bin/pwd".to_owned();
    spec.args = Vec::new();
    run_ok(&spec).await;
    assert_eq!(
        Path::new(sandbox.stdout().trim()).canonicalize().unwrap(),
        sandbox.directory.path().canonicalize().unwrap()
    );

    // Arguments are passed as an argv array; shell metacharacters are data.
    spec.program = "/bin/echo".to_owned();
    spec.args = vec![
        "$(touch injected); `touch injected`; a b".to_owned(),
        "*".to_owned(),
    ];
    run_ok(&spec).await;
    assert_eq!(
        sandbox.stdout(),
        "$(touch injected); `touch injected`; a b *\n"
    );
    assert!(!sandbox.directory.path().join("injected").exists());
}

#[tokio::test]
async fn a_missing_program_is_a_spawn_error() {
    let sandbox = Sandbox::new();
    let mut spec = sandbox.spec("");
    spec.program = "/nonexistent/program".to_owned();
    let error = run(&spec, &NeverCancelled).await.unwrap_err();
    assert!(matches!(error, PortError::Spawn { .. }));
    assert_eq!(error.exit_condition().exit_code(), 3);
}

#[tokio::test]
async fn a_signal_is_not_an_exit_code() {
    let sandbox = Sandbox::new();
    let outcome = run_ok(&sandbox.spec("kill -9 $$")).await;
    assert_eq!(outcome.exit, ProcessExit::Signaled { signal: 9 });
    assert!(!outcome.exit.is_success());
}

/// R04: large output on both streams neither blocks nor grows without bound.
#[tokio::test]
async fn r04_large_output_is_drained_and_stored_up_to_the_limit() {
    let sandbox = Sandbox::new();
    // 20 MiB on each stream, interleaved, far beyond any pipe buffer.
    let script = "i=0; while [ $i -lt 20 ]; do head -c 1048576 /dev/zero | tr '\\0' 'o'; head -c 1048576 /dev/zero | tr '\\0' 'e' >&2; i=$((i+1)); done; echo done";
    let mut spec = sandbox.spec(script);
    spec.max_bytes_per_stream = 64 * 1024;
    let outcome = run_ok(&spec).await;

    assert_eq!(
        outcome.exit,
        ProcessExit::Exited { code: 0 },
        "the child must finish, not block"
    );
    assert!(outcome.stdout_truncated && outcome.stderr_truncated);
    assert!(!outcome.output_incomplete);
    assert_eq!(
        std::fs::metadata(&spec.stdout_path).unwrap().len(),
        64 * 1024
    );
    assert_eq!(
        std::fs::metadata(&spec.stderr_path).unwrap().len(),
        64 * 1024
    );

    // Under the limit nothing is dropped.
    spec.max_bytes_per_stream = 64 * 1024 * 1024;
    let complete = run_ok(&spec).await;
    assert!(!complete.stdout_truncated && !complete.stderr_truncated);
    assert_eq!(
        std::fs::metadata(&spec.stdout_path).unwrap().len(),
        20 * 1024 * 1024 + 5
    );
}

#[tokio::test]
async fn output_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new();
    let spec = sandbox.spec("echo secret");
    run_ok(&spec).await;
    for path in [&spec.stdout_path, &spec.stderr_path] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

/// R05: a timeout stops the whole process group, including grandchildren.
#[tokio::test]
async fn r05_timeout_stops_the_whole_process_group() {
    let sandbox = Sandbox::new();
    let mut spec = sandbox.spec("sleep 31337 & sleep 31337 & (sleep 31337 &) ; echo started; wait");
    spec.timeout = Duration::from_millis(300);
    assert!(!process_with_argument_exists("31337"));

    let started = Instant::now();
    let outcome = run_ok(&spec).await;
    assert_eq!(outcome.exit, ProcessExit::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert!(!outcome.residual_group);
    assert_eq!(sandbox.stdout(), "started\n");
    assert!(
        wait_until_absent("31337"),
        "grandchildren survived the timeout"
    );
}

/// R05: a process that ignores SIGTERM is killed after the grace period.
#[tokio::test]
async fn r05_sigterm_is_escalated_to_sigkill() {
    let sandbox = Sandbox::new();
    let mut spec =
        sandbox.spec("trap '' TERM; (trap '' TERM; sleep 31338) & echo ready; sleep 31338");
    spec.timeout = Duration::from_millis(300);
    spec.grace = Duration::from_millis(200);

    let started = Instant::now();
    let outcome = run_ok(&spec).await;
    assert_eq!(outcome.exit, ProcessExit::TimedOut);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(450),
        "SIGKILL came before the grace period: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    assert!(!outcome.residual_group);
    assert!(
        wait_until_absent("31338"),
        "a process ignoring SIGTERM survived"
    );
}

/// R06 (process level): cancellation stops the group and is reported as such.
#[tokio::test]
async fn r06_cancellation_stops_the_process_group() {
    let sandbox = Sandbox::new();
    let spec = sandbox.spec("sleep 31339 & echo started; wait");
    let trigger = Arc::new(Trigger::default());
    let firing = Arc::clone(&trigger);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        firing.fire();
    });

    let started = Instant::now();
    let outcome = run(&spec, trigger.as_ref()).await.unwrap();
    assert_eq!(outcome.exit, ProcessExit::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!outcome.residual_group);
    assert!(wait_until_absent("31339"));

    // Nothing is started once cancellation was requested.
    let error = run(
        &sandbox.spec("touch started-after-cancel"),
        trigger.as_ref(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, PortError::Cancelled);
    assert!(
        !sandbox
            .directory
            .path()
            .join("started-after-cancel")
            .exists()
    );
}

/// R05: background processes left behind by a command that exits normally
/// are stopped, and the fact is reported.
#[tokio::test]
async fn processes_left_behind_after_a_normal_exit_are_stopped() {
    let sandbox = Sandbox::new();
    let outcome = run_ok(&sandbox.spec("sleep 31340 >/dev/null 2>&1 & exit 0")).await;
    assert_eq!(outcome.exit, ProcessExit::Exited { code: 0 });
    assert!(
        outcome.stopped_survivors,
        "the leftover process must be reported"
    );
    assert!(!outcome.residual_group);
    assert!(wait_until_absent("31340"));

    let clean = run_ok(&sandbox.spec("exit 0")).await;
    assert!(!clean.stopped_survivors);
}

/// A process that left the group of its command (a browser, a detached
/// server) is found through the marker in its environment and stopped.
#[tokio::test]
async fn processes_that_left_the_group_are_stopped_after_a_normal_exit() {
    let sandbox = Sandbox::new();
    let outcome =
        run_ok(&sandbox.spec("setsid sleep 31350 >/dev/null 2>&1 </dev/null & sleep 0.2; exit 0"))
            .await;
    assert_eq!(outcome.exit, ProcessExit::Exited { code: 0 });
    assert!(
        outcome.stopped_survivors,
        "the process in another session must be reported"
    );
    assert!(!outcome.residual_group);
    assert!(wait_until_absent("31350"));
}

/// R05: a timeout also stops what the command started in another session,
/// even when that ignores SIGTERM.
#[tokio::test]
async fn r05_a_timeout_stops_processes_that_left_the_group() {
    let sandbox = Sandbox::new();
    let mut spec = sandbox
        .spec("setsid sh -c 'trap \"\" TERM; sleep 31351' >/dev/null 2>&1 </dev/null & sleep 600");
    spec.timeout = Duration::from_millis(500);
    let started = Instant::now();
    let outcome = run_ok(&spec).await;
    assert_eq!(outcome.exit, ProcessExit::TimedOut);
    assert!(!outcome.residual_group, "everything was confirmed gone");
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(wait_until_absent("31351"));
}

/// The marker belongs to one command: a process of another command that runs
/// at the same time is never touched.
#[tokio::test]
async fn the_marker_of_one_command_does_not_reach_another_command() {
    let sandbox = Sandbox::new();
    let other = Sandbox::new();
    let long = other.spec("sleep 31352");
    let long_running = tokio::spawn(async move {
        let mut spec = long;
        spec.timeout = Duration::from_secs(3);
        run(&spec, &NeverCancelled).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let quick =
        run_ok(&sandbox.spec("setsid sleep 31353 >/dev/null 2>&1 </dev/null & sleep 0.2; exit 0"))
            .await;
    assert!(quick.stopped_survivors);
    assert!(wait_until_absent("31353"));
    assert!(
        process_with_argument_exists("31352"),
        "the other command must still be running"
    );

    let other_outcome = long_running.await.unwrap().unwrap();
    assert_eq!(other_outcome.exit, ProcessExit::TimedOut);
    assert!(wait_until_absent("31352"));
}

/// R15: what a run left behind when testry itself was killed can still be
/// attributed to that run, and only to it, and stopped later.
#[tokio::test]
async fn r15_processes_of_a_run_can_be_found_and_stopped_after_testry_is_gone() {
    let sandbox = Sandbox::new();
    let mine = "11111111-1111-4111-8111-111111111111";
    let other = "22222222-2222-4222-8222-222222222222";
    assert!(processes_of_owner(mine).is_empty());

    // testry was killed while its command ran: the command is never stopped
    // by its own run. A dropped run future is the closest a test can get,
    // so the processes are started in a session of their own.
    let start = |owner: &str, marker: &str| {
        let mut spec = sandbox.spec(&format!(
            "setsid sh -c 'trap \"\" TERM; sleep {marker}' >/dev/null 2>&1 </dev/null & sleep 600"
        ));
        spec.owner = owner.to_owned();
        spec.stdout_path = sandbox.directory.path().join(format!("{marker}.out"));
        spec.stderr_path = sandbox.directory.path().join(format!("{marker}.err"));
        tokio::spawn(async move { run(&spec, &NeverCancelled).await })
    };
    let mine_run = start(mine, "31360");
    let other_run = start(other, "31361");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(process_with_argument_exists("31360") && process_with_argument_exists("31361")) {
        assert!(Instant::now() < deadline, "the commands did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let found = processes_of_owner(mine);
    assert!(found.len() >= 2, "the shell and its sleep: {found:?}");
    assert!(
        found.iter().any(|process| process.name == "sleep"),
        "{found:?}"
    );
    assert!(processes_of_owner("33333333-3333-4333-8333-333333333333").is_empty());
    // A prefix of an owner is another owner.
    assert!(processes_of_owner(&mine[..mine.len() - 1]).is_empty());

    let left = stop_processes_of_owner(mine, Duration::from_millis(300)).await;
    assert!(left.is_empty(), "{left:?}");
    assert!(wait_until_absent("31360"));
    assert!(
        process_with_argument_exists("31361"),
        "the processes of another run must not be touched"
    );

    let left = stop_processes_of_owner(other, Duration::from_millis(300)).await;
    assert!(left.is_empty(), "{left:?}");
    assert!(wait_until_absent("31361"));
    mine_run.abort();
    other_run.abort();
}

#[tokio::test]
async fn a_fast_command_is_not_affected_by_a_long_timeout() {
    let sandbox = Sandbox::new();
    let mut spec = sandbox.spec("echo quick");
    spec.timeout = Duration::from_secs(3600);
    let started = Instant::now();
    let outcome = run_ok(&spec).await;
    assert_eq!(outcome.exit, ProcessExit::Exited { code: 0 });
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(outcome.duration < Duration::from_secs(5));
}
