#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the real clock and the files and directories that they and the code under test write"
)]
#![expect(
    clippy::disallowed_macros,
    reason = "the tests print on standard error why they skip, which the test runner shows"
)]

use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener};
use std::num::NonZeroU32;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::{
    Attempt, Check, CommandEnd, Failure, Goal, HostExec, LogLines, LogWatch, MOST_BODY_BYTES,
    Outcome, ProbeExec, READY_INTERVAL, Tally, Verdict, run_bounded, wait,
};
use crate::cancel::Cancel;
use crate::clock::pause;
use crate::env::AppliedEnvironment;
use crate::plan::probe::{
    BadLogPattern, JsonExpectation, JsonPointer, JsonScalar, LogPattern, Probe, ProbeCheck,
};

/// A generous upper limit for an attempt or a wait that should end much
/// sooner, so a slow machine does not fail a timing test.
const SLACK: Duration = Duration::from_secs(5);

/// Binds a listener on 127.0.0.1 at a port the system picks.
fn listener() -> (TcpListener, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// Binds a listener only on ::1 at a port the system picks, or returns
/// `None` when this machine has no IPv6 loopback address.
fn ipv6_listener() -> Option<(TcpListener, u16)> {
    let listener = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    Some((listener, port))
}

/// Returns a port on 127.0.0.1 on which nothing listens.
fn closed_port() -> u16 {
    let (listener, port) = listener();
    drop(listener);
    port
}

/// The requests that a test server has read, each as its head.
type Requests = Arc<std::sync::Mutex<Vec<String>>>;

/// Serves each connection to a fresh listener on 127.0.0.1 with
/// `response`, after reading the request's head, and returns the port and
/// the requests read.
fn serve(response: &'static [u8]) -> (u16, Requests) {
    let (listener, port) = listener();
    serve_on(listener, port, response)
}

/// Serves each connection to `listener`, which listens on `port`, as
/// [`serve`] does.
fn serve_on(listener: TcpListener, port: u16, response: &'static [u8]) -> (u16, Requests) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&head).into_owned());
            let _ = stream.write_all(response);
        }
    });
    (port, requests)
}

/// A probe of `check` with a short period and timeout, so tests run fast.
fn quick(check: ProbeCheck) -> Probe {
    let mut probe = Probe::new(check);
    probe.period = Duration::from_millis(50);
    probe.timeout = Duration::from_millis(500);
    probe
}

fn never_ended() -> impl FnMut() -> bool {
    || false
}

#[test]
fn an_http_check_passes_on_a_2xx_status() {
    let (port, requests) = serve(b"HTTP/1.1 204 No Content\r\n\r\n");
    let mut check = Check::Http {
        port,
        path: "/health?deep=1",
        status: None,
        json: &[],
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Passed
    );
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests[0],
        format!(
            "GET /health?deep=1 HTTP/1.1\r\nHost: localhost:{port}\r\nUser-Agent: phase-probe\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        )
    );
}

#[test]
fn an_http_check_fails_on_a_status_outside_2xx() {
    let (port, _) = serve(b"HTTP/1.1 503 Service Unavailable\r\n\r\nbusy");
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Failed(Failure::Status(503))
    );
}

#[test]
fn an_http_check_follows_no_redirect_so_a_3xx_status_fails() {
    let (port, requests) =
        serve(b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n");
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Failed(Failure::Status(302))
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn an_http_check_fails_on_a_response_that_is_not_http() {
    let (port, _) = serve(b"SSH-2.0-OpenSSH_9.6\r\n");
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Failed(Failure::BadResponse)
    );
}

#[test]
fn an_http_check_reads_no_more_than_its_bound_of_a_status_line() {
    static ENDLESS: [u8; 64 * 1024] = [b'x'; 64 * 1024];
    let (port, _) = serve(&ENDLESS);
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Failed(Failure::BadResponse)
    );
}

#[test]
fn an_http_check_fails_when_nothing_listens() {
    let mut check = Check::Http {
        port: closed_port(),
        path: "/",
        status: None,
        json: &[],
    };
    assert!(matches!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Failed(Failure::Connect(_))
    ));
}

#[test]
fn an_http_check_passes_on_a_server_that_listens_only_on_ipv6_loopback() {
    let Some((listener, port)) = ipv6_listener() else {
        eprintln!("skipped: this machine has no IPv6 loopback address");
        return;
    };
    let (port, requests) = serve_on(listener, port, b"HTTP/1.1 200 OK\r\n\r\n");
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Passed
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains(&format!("\r\nHost: localhost:{port}\r\n")),
        "{}",
        requests[0]
    );
}

#[test]
fn a_tcp_check_passes_on_a_server_that_listens_only_on_ipv6_loopback() {
    let Some((listener, port)) = ipv6_listener() else {
        eprintln!("skipped: this machine has no IPv6 loopback address");
        return;
    };
    let mut check = Check::Tcp { port };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Passed
    );
    drop(listener);
}

#[test]
fn a_tcp_check_reaches_ipv4_loopback_without_trying_ipv6() {
    let (listener, port) = listener();
    let mut check = Check::Tcp { port };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Passed
    );
    let (accepted, peer) = listener.accept().unwrap();
    assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    drop(accepted);
}

#[test]
fn a_check_that_finds_nothing_on_either_loopback_address_fails_within_its_timeout() {
    // A port that is free on 127.0.0.1 may still be held on ::1, so the test
    // takes a port that is free on both.
    let port = loop {
        let port = closed_port();
        if TcpListener::bind((Ipv6Addr::LOCALHOST, port)).is_ok()
            || TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_err()
        {
            break port;
        }
    };
    let started = Instant::now();
    for mut check in [
        Check::Tcp { port },
        Check::Http {
            port,
            path: "/",
            status: None,
            json: &[],
        },
    ] {
        assert_eq!(
            check.attempt(Duration::from_secs(2), &Cancel::new()),
            Attempt::Failed(Failure::Connect(io::ErrorKind::ConnectionRefused)),
            "{check:?}"
        );
    }
    assert!(started.elapsed() < SLACK);
}

#[test]
fn an_http_check_times_out_on_a_server_that_never_answers() {
    let (listener, port) = listener();
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    let started = Instant::now();
    assert_eq!(
        check.attempt(Duration::from_millis(200), &Cancel::new()),
        Attempt::Failed(Failure::TimedOut)
    );
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(150), "took {took:?}");
    assert!(took < SLACK, "took {took:?}");
    drop(listener);
}

#[test]
fn an_http_check_refuses_a_path_with_a_space_or_a_control_character() {
    let (port, requests) = serve(b"HTTP/1.1 200 OK\r\n\r\n");
    for path in ["/a b", "/a\r\nX-Injected: 1", "/caf\u{e9}", "/page#top"] {
        let mut check = Check::Http {
            port,
            path,
            status: None,
            json: &[],
        };
        assert_eq!(
            check.attempt(Duration::from_secs(2), &Cancel::new()),
            Attempt::Failed(Failure::BadPath)
        );
    }
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn a_tcp_check_passes_when_the_port_accepts() {
    let (listener, port) = listener();
    let mut check = Check::Tcp { port };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Passed
    );
    drop(listener);
}

#[test]
fn a_tcp_check_fails_when_nothing_listens() {
    let mut check = Check::Tcp {
        port: closed_port(),
    };
    assert!(matches!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Failed(Failure::Connect(_))
    ));
}

/// An environment that holds its values in a list.
struct Fixed(Vec<(&'static str, &'static str)>);

impl AppliedEnvironment for Fixed {
    fn names(&self) -> BTreeSet<String> {
        self.0.iter().map(|(name, _)| (*name).to_owned()).collect()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, command: &mut Command, reference: &dyn Fn(&str) -> Option<String>) {
        for (name, value) in &self.0 {
            if let Some(reference) = reference(name) {
                command.env(reference, value);
            }
        }
    }
}

fn host_exec<'a>(environment: &'a Fixed, dir: &Path) -> HostExec<'a> {
    HostExec {
        environment,
        path: "/usr/bin:/bin".into(),
        home: dir.as_os_str().to_owned(),
        dir: dir.to_owned(),
    }
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

#[test]
fn a_command_check_passes_when_the_command_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let environment = Fixed(vec![]);
    let exec = host_exec(&environment, dir.path());
    let run = argv(&["true"]);
    let mut check = Check::Command {
        run: &run,
        exec: &exec,
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Passed
    );
}

#[test]
fn a_command_check_fails_with_the_command_s_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let environment = Fixed(vec![]);
    let exec = host_exec(&environment, dir.path());
    let run = argv(&["sh", "-c", "exit 3"]);
    let mut check = Check::Command {
        run: &run,
        exec: &exec,
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Failed(Failure::Exited { code: Some(3) })
    );
}

#[test]
fn a_host_command_runs_with_only_the_process_s_environment_in_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let environment = Fixed(vec![("GREETING", "hello")]);
    let exec = host_exec(&environment, dir.path());
    let script = format!(
        r#"test "$GREETING" = hello && test "$HOME" = "{home}" && test "$(pwd -P)" = "$(cd "{home}" && pwd -P)" && test -z "${{CARGO:-}}" && test -z "${{RUST_TEST_THREADS:-}}${{NEXTEST:-}}""#,
        home = dir.path().display()
    );
    assert_eq!(
        exec.exec(
            &argv(&["sh", "-c", &script]),
            Duration::from_secs(2),
            &Cancel::new()
        )
        .unwrap(),
        CommandEnd::Exited { code: Some(0) }
    );
}

#[test]
fn a_command_that_outlasts_its_timeout_is_killed_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    let environment = Fixed(vec![]);
    let exec = host_exec(&environment, dir.path());
    let run = argv(&["sleep", "30"]);
    let mut check = Check::Command {
        run: &run,
        exec: &exec,
    };
    let started = Instant::now();
    assert_eq!(
        check.attempt(Duration::from_millis(200), &Cancel::new()),
        Attempt::Failed(Failure::TimedOut)
    );
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(200), "took {took:?}");
    assert!(took < SLACK, "took {took:?}");
}

#[test]
fn a_bounded_command_that_a_signal_ends_reports_no_exit_code() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test prepares a command for the probe's bounded runner"
    )]
    let mut command = Command::new("sh");
    command.args(["-c", "kill -KILL $$"]);
    assert_eq!(
        run_bounded(command, Duration::from_secs(2), &Cancel::new()).unwrap(),
        CommandEnd::Exited { code: None }
    );
}

#[test]
fn a_command_that_cannot_start_fails() {
    let dir = tempfile::tempdir().unwrap();
    let environment = Fixed(vec![]);
    let exec = host_exec(&environment, dir.path());
    let run = argv(&["phase-probe-no-such-program"]);
    let mut check = Check::Command {
        run: &run,
        exec: &exec,
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Failed(Failure::Unstarted(io::ErrorKind::NotFound))
    );
}

/// Log lines that a test hands over in batches, one batch for each read.
#[derive(Default)]
struct Batches(VecDeque<io::Result<Vec<String>>>);

impl Batches {
    fn of(batches: Vec<io::Result<Vec<&str>>>) -> Self {
        Self(
            batches
                .into_iter()
                .map(|batch| batch.map(|lines| lines.into_iter().map(str::to_owned).collect()))
                .collect(),
        )
    }
}

impl LogLines for Batches {
    fn new_lines(&mut self) -> io::Result<Vec<String>> {
        self.0.pop_front().unwrap_or_else(|| Ok(Vec::new()))
    }
}

#[test]
fn a_log_pattern_matches_any_part_of_a_line() {
    let pattern = LogPattern::new("ready on queue \\w+").unwrap();
    assert!(pattern.is_match("worker ready on queue jobs"));
    assert!(!pattern.is_match("compiling worker ready"));
}

#[test]
fn an_empty_log_pattern_is_refused() {
    assert_eq!(LogPattern::new("").unwrap_err(), BadLogPattern::Empty);
}

#[test]
fn a_log_pattern_that_is_not_a_regular_expression_is_refused_with_a_one_line_reason() {
    let BadLogPattern::Invalid(reason) = LogPattern::new("ready (").unwrap_err() else {
        panic!("the pattern was refused for another reason");
    };
    assert!(!reason.is_empty());
    assert!(!reason.contains('\n'), "{reason}");
}

#[test]
fn a_log_check_passes_once_a_line_matches_and_on_every_later_attempt() {
    let mut lines = Batches::of(vec![
        Ok(vec!["starting"]),
        Ok(vec!["listening", "ready"]),
        Ok(vec![]),
    ]);
    let mut check = Check::Log {
        watch: LogWatch::new(LogPattern::new("^ready$").unwrap()),
        lines: &mut lines,
    };
    let timeout = Duration::from_secs(1);
    assert_eq!(
        check.attempt(timeout, &Cancel::new()),
        Attempt::Failed(Failure::NoMatch)
    );
    assert_eq!(check.attempt(timeout, &Cancel::new()), Attempt::Passed);
    assert_eq!(check.attempt(timeout, &Cancel::new()), Attempt::Passed);
}

#[test]
fn a_log_check_fails_when_the_log_cannot_be_read() {
    let mut lines = Batches::of(vec![Err(io::Error::from(io::ErrorKind::PermissionDenied))]);
    let mut check = Check::Log {
        watch: LogWatch::new(LogPattern::new("ready").unwrap()),
        lines: &mut lines,
    };
    assert_eq!(
        check.attempt(Duration::from_secs(1), &Cancel::new()),
        Attempt::Failed(Failure::Unreadable(io::ErrorKind::PermissionDenied))
    );
}

#[test]
fn a_log_watch_observes_lines_that_its_caller_reads() {
    let mut watch = LogWatch::new(LogPattern::new("ready").unwrap());
    assert!(!watch.observe(["starting"]));
    assert!(watch.observe(["now ready"]));
    assert!(watch.observe([]));
}

fn thresholds(success: u32, failure: u32) -> Probe {
    let mut probe = quick(ProbeCheck::tcp("http".parse().unwrap()));
    probe.success_threshold = NonZeroU32::new(success).unwrap();
    probe.failure_threshold = NonZeroU32::new(failure).unwrap();
    probe
}

#[test]
fn a_tally_passes_after_the_success_threshold_of_passes_in_a_row() {
    let mut tally = Tally::new(&thresholds(2, 3));
    assert_eq!(tally.record(true), Verdict::Undecided);
    assert_eq!(tally.record(false), Verdict::Undecided);
    assert_eq!(tally.record(true), Verdict::Undecided);
    assert_eq!(tally.record(true), Verdict::Passing);
}

#[test]
fn a_tally_fails_after_the_failure_threshold_of_failures_in_a_row() {
    let mut tally = Tally::new(&thresholds(1, 2));
    assert_eq!(tally.record(false), Verdict::Undecided);
    assert_eq!(tally.record(true), Verdict::Passing);
    assert_eq!(tally.record(false), Verdict::Undecided);
    assert_eq!(tally.record(false), Verdict::Failing);
}

#[test]
fn a_readiness_wait_ends_when_a_late_server_answers() {
    let (listener, port) = listener();
    let server = thread::spawn(move || {
        pause(Duration::from_millis(200));
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = [0_u8; 256];
        let _ = stream.read(&mut head).unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
    });
    let probe = quick(ProbeCheck::http("/".to_owned()));
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + SLACK,
        &mut never_ended(),
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ready);
    assert!(started.elapsed() >= Duration::from_millis(150));
    server.join().unwrap();
}

#[test]
fn a_readiness_wait_needs_the_success_threshold_of_passes_one_period_apart() {
    let (listener, port) = listener();
    let mut probe = quick(ProbeCheck::tcp("http".parse().unwrap()));
    probe.period = Duration::from_millis(100);
    probe.success_threshold = NonZeroU32::new(3).unwrap();
    let mut check = Check::Tcp { port };
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + SLACK,
        &mut never_ended(),
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ready);
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(200), "took {took:?}");
    assert!(took < SLACK, "took {took:?}");
    drop(listener);
}

#[test]
fn a_readiness_wait_keeps_trying_past_the_failure_threshold_until_its_deadline() {
    let probe = thresholds(1, 1);
    let mut check = Check::Tcp {
        port: closed_port(),
    };
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + Duration::from_millis(300),
        &mut never_ended(),
        &Cancel::new(),
    );
    let Outcome::NotReady { waited, last } = outcome else {
        panic!("the wait ended with {outcome:?}");
    };
    assert!(waited >= Duration::from_millis(250), "waited {waited:?}");
    assert!(started.elapsed() < SLACK);
    assert!(matches!(last, Some(Failure::Connect(_))), "{last:?}");
}

#[test]
fn a_liveness_wait_ends_after_the_failure_threshold_of_failures() {
    let mut probe = thresholds(1, 3);
    probe.period = Duration::from_millis(100);
    let mut check = Check::Tcp {
        port: closed_port(),
    };
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Failing,
        Instant::now() + SLACK,
        &mut never_ended(),
        &Cancel::new(),
    );
    assert!(
        matches!(
            outcome,
            Outcome::Failing {
                last: Failure::Connect(_)
            }
        ),
        "{outcome:?}"
    );
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(200), "took {took:?}");
    assert!(took < SLACK, "took {took:?}");
}

#[test]
fn a_liveness_wait_with_a_matched_log_keeps_its_period() {
    let mut lines = Batches::of(vec![Ok(vec!["ready"])]);
    let mut probe = quick(ProbeCheck::log("ready".to_owned()));
    probe.period = Duration::from_millis(100);
    let mut check = Check::Log {
        watch: LogWatch::new(LogPattern::new("ready").unwrap()),
        lines: &mut lines,
    };
    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = Arc::clone(&attempts);
    let mut ended = move || counted.fetch_add(1, Ordering::SeqCst) >= 5;
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Failing,
        Instant::now() + SLACK,
        &mut ended,
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ended);
    assert!(started.elapsed() >= Duration::from_millis(60));
}

#[test]
fn a_readiness_wait_attempts_a_log_check_as_soon_as_a_matching_line_arrives() {
    let mut lines = Batches::of(vec![
        Ok(vec!["starting"]),
        Ok(vec![]),
        Ok(vec![]),
        Ok(vec!["ready"]),
    ]);
    let mut probe = quick(ProbeCheck::log("ready".to_owned()));
    probe.period = Duration::from_secs(60);
    let mut check = Check::Log {
        watch: LogWatch::new(LogPattern::new("ready").unwrap()),
        lines: &mut lines,
    };
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + Duration::from_secs(30),
        &mut never_ended(),
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ready);
    assert!(started.elapsed() < SLACK);
}

#[test]
fn a_wait_ends_when_the_process_ends() {
    let probe = quick(ProbeCheck::tcp("http".parse().unwrap()));
    let mut check = Check::Tcp {
        port: closed_port(),
    };
    let ended = Arc::new(AtomicBool::new(false));
    let ender = Arc::clone(&ended);
    let setter = thread::spawn(move || {
        pause(Duration::from_millis(150));
        ender.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + SLACK,
        &mut || ended.load(Ordering::SeqCst),
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ended);
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(started.elapsed() < SLACK);
    setter.join().unwrap();
}

#[test]
fn a_wait_ends_when_it_is_cancelled() {
    let mut probe = quick(ProbeCheck::tcp("http".parse().unwrap()));
    probe.period = Duration::from_secs(60);
    let mut check = Check::Tcp {
        port: closed_port(),
    };
    let cancel = Cancel::new();
    let canceller = cancel.clone();
    let setter = thread::spawn(move || {
        pause(Duration::from_millis(150));
        canceller.cancel();
    });
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + Duration::from_secs(60),
        &mut never_ended(),
        &cancel,
    );
    assert_eq!(outcome, Outcome::Cancelled);
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(started.elapsed() < SLACK);
    setter.join().unwrap();
}

#[test]
fn no_attempt_outlasts_the_wait_s_deadline() {
    let (listener, port) = listener();
    let mut probe = quick(ProbeCheck::http("/".to_owned()));
    probe.timeout = Duration::from_secs(30);
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + Duration::from_millis(300),
        &mut never_ended(),
        &Cancel::new(),
    );
    assert!(
        matches!(
            outcome,
            Outcome::NotReady {
                last: Some(Failure::TimedOut),
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(started.elapsed() < SLACK);
    drop(listener);
}

#[test]
fn an_http_check_reads_a_status_line_written_in_pauses() {
    let (listener, port) = listener();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = [0_u8; 256];
        let _ = stream.read(&mut head).unwrap();
        for part in [&b"HTT"[..], b"P/1.1 2", b"00 OK\r", b"\n\r\n"] {
            stream.write_all(part).unwrap();
            stream.flush().unwrap();
            pause(Duration::from_millis(60));
        }
    });
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    assert_eq!(
        check.attempt(Duration::from_secs(2), &Cancel::new()),
        Attempt::Passed
    );
    server.join().unwrap();
}

#[test]
fn an_http_check_stops_reading_when_it_is_cancelled() {
    let (listener, port) = listener();
    let cancel = Cancel::new();
    let canceller = cancel.clone();
    let setter = thread::spawn(move || {
        pause(Duration::from_millis(150));
        canceller.cancel();
    });
    let mut check = Check::Http {
        port,
        path: "/",
        status: None,
        json: &[],
    };
    let started = Instant::now();
    assert_eq!(
        check.attempt(Duration::from_secs(30), &cancel),
        Attempt::Failed(Failure::Cancelled)
    );
    assert!(started.elapsed() < SLACK);
    setter.join().unwrap();
    drop(listener);
}

/// Reports whether the process `pid` still exists, waiting up to
/// [`SLACK`] for the system to reap it once it has no parent.
fn gone(pid: &str) -> bool {
    let pid = rustix::process::Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
    let deadline = Instant::now() + SLACK;
    while Instant::now() < deadline {
        if rustix::process::test_kill_process(pid).is_err() {
            return true;
        }
        pause(Duration::from_millis(20));
    }
    false
}

/// Runs `script` through `sh` under [`run_bounded`] with `timeout`, after
/// it starts `sleep` in the background and writes its pid to a file, and
/// returns how the command ended and the background pid.
fn with_background_sleep(script: &str, timeout: Duration) -> (CommandEnd, String) {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test prepares a command for the probe's bounded runner"
    )]
    let mut command = Command::new("sh");
    command.args([
        "-c",
        &format!("sleep 30 & echo $! > '{}'; {script}", pid_file.display()),
    ]);
    let end = run_bounded(command, timeout, &Cancel::new()).unwrap();
    (end, std::fs::read_to_string(pid_file).unwrap())
}

#[test]
fn a_bounded_command_s_group_is_killed_after_a_timeout() {
    let (end, pid) = with_background_sleep("sleep 30", Duration::from_millis(300));
    assert_eq!(end, CommandEnd::TimedOut);
    assert!(gone(&pid), "the background sleep {pid} is still running");
}

#[test]
fn a_bounded_command_s_group_is_killed_after_it_exits() {
    let (end, pid) = with_background_sleep("exit 0", Duration::from_secs(5));
    assert_eq!(end, CommandEnd::Exited { code: Some(0) });
    assert!(gone(&pid), "the background sleep {pid} is still running");
}

#[test]
fn a_bounded_command_ends_when_it_is_cancelled() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test prepares a command for the probe's bounded runner"
    )]
    let mut command = Command::new("sleep");
    command.arg("30");
    let cancel = Cancel::new();
    let canceller = cancel.clone();
    let setter = thread::spawn(move || {
        pause(Duration::from_millis(150));
        canceller.cancel();
    });
    let started = Instant::now();
    assert_eq!(
        run_bounded(command, Duration::from_secs(30), &cancel).unwrap(),
        CommandEnd::Cancelled
    );
    assert!(started.elapsed() < SLACK);
    setter.join().unwrap();
}

#[test]
fn a_bounded_command_whose_background_process_holds_its_output_leaves_nothing_running() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test prepares a command for the probe's bounded runner"
    )]
    let mut command = Command::new("sh");
    command.args([
        "-c",
        &format!("sleep 30 & echo $! > '{}'", pid_file.display()),
    ]);
    let started = Instant::now();
    assert_eq!(
        run_bounded(command, Duration::from_secs(30), &Cancel::new()).unwrap(),
        CommandEnd::Exited { code: Some(0) }
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    let pid = std::fs::read_to_string(pid_file).unwrap();
    assert!(gone(&pid), "the background sleep {pid} is still running");
}

#[test]
fn a_cancelled_bounded_command_returns_within_200_ms_of_the_cancel() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test prepares a command for the probe's bounded runner"
    )]
    let mut command = Command::new("sleep");
    command.arg("30");
    let cancel = Cancel::new();
    let canceller = cancel.clone();
    let setter = thread::spawn(move || {
        pause(Duration::from_millis(300));
        canceller.cancel();
    });
    let started = Instant::now();
    assert_eq!(
        run_bounded(command, Duration::from_secs(30), &cancel).unwrap(),
        CommandEnd::Cancelled
    );
    let took = started.elapsed();
    setter.join().unwrap();
    assert!(took < Duration::from_millis(300 + 200), "took {took:?}");
}

#[test]
fn a_bounded_command_whose_wait_fails_reports_the_failure() {
    // With SIGCHLD ignored, the system reaps each child of this process as
    // it ends, so the runner's wait for the command fails with ECHILD. A
    // test that reaped the command itself would race the runner, which
    // reaps it too once it sees it end. nextest runs each test in a
    // process of its own, so no other test's child is reaped this way.
    let end = {
        let _ignored = ChildSignalIgnored::new();
        #[expect(
            clippy::disallowed_methods,
            reason = "the test prepares a command for the probe's bounded runner"
        )]
        let mut command = Command::new("sleep");
        command.arg("0.1");
        run_bounded(command, SLACK, &Cancel::new()).unwrap()
    };

    assert!(matches!(end, CommandEnd::WaitFailed { .. }), "{end:?}");
}

/// Ignores `SIGCHLD` in this process until it is dropped, when the default
/// disposition returns, also when the test panics.
struct ChildSignalIgnored;

impl ChildSignalIgnored {
    fn new() -> Self {
        set_child_signal(libc::SIG_IGN);
        Self
    }
}

impl Drop for ChildSignalIgnored {
    fn drop(&mut self) {
        set_child_signal(libc::SIG_DFL);
    }
}

/// Sets this process's disposition of `SIGCHLD` to `disposition`.
fn set_child_signal(disposition: libc::sighandler_t) {
    #[expect(
        unsafe_code,
        reason = "the test sets the SIGCHLD disposition, which only libc can set"
    )]
    // SAFETY: `signal` only changes the process's disposition of `SIGCHLD`
    // to `SIG_IGN` or `SIG_DFL`, which are valid dispositions for it, and
    // touches no memory of this program. The disposition applies to every
    // thread of the process; nextest runs this test in a process of its
    // own, so no other test waits on a child meanwhile.
    let previous = unsafe { libc::signal(libc::SIGCHLD, disposition) };
    assert_ne!(previous, libc::SIG_ERR, "SIGCHLD's disposition was not set");
}

/// A probe command that fails until its `passes_from`th run.
struct Counted {
    runs: std::sync::atomic::AtomicU32,
    passes_from: u32,
}

impl ProbeExec for Counted {
    fn exec(&self, _: &[String], _: Duration, _: &Cancel) -> io::Result<CommandEnd> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(CommandEnd::Exited {
            code: Some(if run >= self.passes_from { 0 } else { 1 }),
        })
    }
}

#[test]
fn a_readiness_wait_attempts_at_the_ready_interval_whatever_the_period() {
    let exec = Counted {
        runs: 0.into(),
        passes_from: 3,
    };
    let run = argv(&["check"]);
    let mut check = Check::Command {
        run: &run,
        exec: &exec,
    };
    let mut probe = quick(ProbeCheck::command(run.clone()));
    probe.period = Duration::from_secs(60);
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + Duration::from_secs(30),
        &mut never_ended(),
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ready);
    let took = started.elapsed();
    assert!(
        took >= READY_INTERVAL * 2 - Duration::from_millis(20),
        "took {took:?}"
    );
    assert!(took < SLACK, "took {took:?}");
}

#[test]
fn a_liveness_wait_attempts_once_each_period() {
    let exec = Counted {
        runs: 0.into(),
        passes_from: u32::MAX,
    };
    let run = argv(&["check"]);
    let mut check = Check::Command {
        run: &run,
        exec: &exec,
    };
    let mut probe = thresholds(1, 3);
    probe.period = Duration::from_millis(250);
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Failing,
        Instant::now() + SLACK,
        &mut never_ended(),
        &Cancel::new(),
    );
    assert!(matches!(outcome, Outcome::Failing { .. }), "{outcome:?}");
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(480), "took {took:?}");
    assert!(took < SLACK, "took {took:?}");
}

#[test]
fn a_process_that_logs_its_ready_line_and_ends_counts_as_ready() {
    let mut lines = Batches::of(vec![Ok(vec!["ready"])]);
    let probe = quick(ProbeCheck::log("ready".to_owned()));
    let mut check = Check::Log {
        watch: LogWatch::new(LogPattern::new("ready").unwrap()),
        lines: &mut lines,
    };
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + SLACK,
        &mut || true,
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ready);
}

#[test]
fn a_readiness_wait_makes_one_last_attempt_when_a_matching_line_arrives_at_its_deadline() {
    let mut lines = Batches::of(vec![Ok(vec!["ready"])]);
    let probe = quick(ProbeCheck::log("ready".to_owned()));
    let mut check = Check::Log {
        watch: LogWatch::new(LogPattern::new("ready").unwrap()),
        lines: &mut lines,
    };
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now(),
        &mut never_ended(),
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ready);
}

#[test]
fn a_readiness_wait_without_a_matching_line_at_its_deadline_is_not_ready() {
    let mut lines = Batches::of(vec![Ok(vec!["starting"])]);
    let probe = quick(ProbeCheck::log("ready".to_owned()));
    let mut check = Check::Log {
        watch: LogWatch::new(LogPattern::new("ready").unwrap()),
        lines: &mut lines,
    };
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now(),
        &mut never_ended(),
        &Cancel::new(),
    );
    assert!(matches!(outcome, Outcome::NotReady { .. }), "{outcome:?}");
}

#[test]
fn a_matched_log_reaches_the_success_threshold_without_waiting_an_interval() {
    let mut lines = Batches::of(vec![Ok(vec!["ready"])]);
    let mut probe = quick(ProbeCheck::log("ready".to_owned()));
    probe.period = Duration::from_secs(60);
    probe.success_threshold = NonZeroU32::new(3).unwrap();
    let mut check = Check::Log {
        watch: LogWatch::new(LogPattern::new("ready").unwrap()),
        lines: &mut lines,
    };
    let started = Instant::now();
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + SLACK,
        &mut never_ended(),
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ready);
    assert!(
        started.elapsed() < READY_INTERVAL,
        "took {:?}",
        started.elapsed()
    );
}

/// Returns the expectation that the field at `pointer` is present.
fn present(pointer: &str) -> JsonExpectation {
    JsonExpectation::present(JsonPointer::new(pointer.to_owned()))
}

/// Returns the expectation that the field at `pointer` equals `value`.
fn equal(pointer: &str, value: JsonScalar) -> JsonExpectation {
    JsonExpectation::equals(JsonPointer::new(pointer.to_owned()), value)
}

/// Returns an HTTP check of `/` at `port` that expects `status` and `json`.
fn expecting(port: u16, status: Option<u16>, json: &[JsonExpectation]) -> Check<'_> {
    Check::Http {
        port,
        path: "/",
        status,
        json,
    }
}

/// Makes one attempt of `check` with a timeout of two seconds.
fn attempt_once(mut check: Check<'_>) -> Attempt {
    check.attempt(Duration::from_secs(2), &Cancel::new())
}

/// Serves `response`, which the test builds at run time, as [`serve`] does.
fn serve_built(response: Vec<u8>) -> u16 {
    serve(response.leak()).0
}

/// Serves the connections to a fresh listener in turn with `responses`,
/// and every later connection with the last response, and returns the
/// port and the count of connections served.
fn serve_each(responses: Vec<&'static [u8]>) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    let (listener, port) = listener();
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = Arc::clone(&served);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let index = count.fetch_add(1, Ordering::SeqCst);
            let response = responses
                .get(index)
                .or_else(|| responses.last())
                .copied()
                .unwrap_or_default();
            let _ = stream.write_all(response);
        }
    });
    (port, served)
}

#[test]
fn an_http_check_with_an_expected_status_fails_on_another_2xx_status() {
    let (port, _) = serve(b"HTTP/1.1 204 No Content\r\n\r\n");
    assert_eq!(
        attempt_once(expecting(port, Some(200), &[])),
        Attempt::Failed(Failure::StatusDiffers {
            expected: 200,
            got: 204
        })
    );
}

#[test]
fn an_http_check_with_an_expected_status_passes_on_that_status_even_outside_2xx() {
    let (port, _) = serve(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(
        attempt_once(expecting(port, Some(401), &[])),
        Attempt::Passed
    );
}

#[test]
fn an_http_check_without_an_expected_status_still_passes_on_any_2xx() {
    let (port, _) = serve(b"HTTP/1.1 202 Accepted\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");
    assert_eq!(attempt_once(expecting(port, None, &[])), Attempt::Passed);
    assert_eq!(
        attempt_once(expecting(
            port,
            None,
            &[equal("/ok", JsonScalar::Bool(true))]
        )),
        Attempt::Passed
    );
}

#[test]
fn an_http_check_judges_the_status_before_it_reads_the_body() {
    let (port, _) = serve(b"HTTP/1.1 503 Service Unavailable\r\n\r\nbusy");
    assert_eq!(
        attempt_once(expecting(port, None, &[present("/ok")])),
        Attempt::Failed(Failure::Status(503))
    );
    assert_eq!(
        attempt_once(expecting(port, Some(200), &[present("/ok")])),
        Attempt::Failed(Failure::StatusDiffers {
            expected: 200,
            got: 503
        })
    );
}

#[test]
fn an_http_check_fails_when_an_expected_json_field_is_missing() {
    let (port, _) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");
    assert_eq!(
        attempt_once(expecting(
            port,
            None,
            &[present("/ok"), present("/revision")]
        )),
        Attempt::Failed(Failure::FieldMissing(JsonPointer::new(
            "/revision".to_owned()
        )))
    );
}

#[test]
fn an_http_check_fails_when_a_json_boolean_is_false_and_true_is_expected() {
    let (port, _) = serve(b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\n\r\n{\"ok\":false}");
    assert_eq!(
        attempt_once(expecting(
            port,
            None,
            &[equal("/ok", JsonScalar::Bool(true))]
        )),
        Attempt::Failed(Failure::FieldDiffers(JsonPointer::new("/ok".to_owned())))
    );
}

#[test]
fn an_http_check_fails_when_the_body_is_not_json() {
    let (port, _) = serve(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html>ok</html>");
    assert_eq!(
        attempt_once(expecting(port, None, &[present("/ok")])),
        Attempt::Failed(Failure::NotJson)
    );
}

#[test]
fn an_http_check_passes_when_every_json_field_matches() {
    // The response has no length, so the body ends when the server closes
    // the connection.
    let (port, _) = serve(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true,\"revision\":\"4f2a9c1\",\"paths\":{\"a/b\":[1]},\"count\":3,\"gone\":null,\"build\":{\"n\":-1}}",
    );
    let json = [
        equal("/ok", JsonScalar::Bool(true)),
        equal("/revision", JsonScalar::String("4f2a9c1".to_owned())),
        present("/paths"),
        present("/paths/a~1b/0"),
        equal("/count", JsonScalar::Integer(3)),
        equal("/gone", JsonScalar::Null),
        equal("/build/n", JsonScalar::Integer(-1)),
        present(""),
    ];
    assert_eq!(
        attempt_once(expecting(port, Some(200), &json)),
        Attempt::Passed
    );
}

#[test]
fn an_http_check_compares_an_integer_only_with_a_whole_number_of_the_same_value() {
    let (port, _) = serve(b"HTTP/1.1 200 OK\r\n\r\n{\"a\":3.5,\"b\":\"3\",\"c\":4}");
    for pointer in ["/a", "/b", "/c"] {
        assert_eq!(
            attempt_once(expecting(
                port,
                None,
                &[equal(pointer, JsonScalar::Integer(3))]
            )),
            Attempt::Failed(Failure::FieldDiffers(JsonPointer::new(pointer.to_owned()))),
            "{pointer}"
        );
    }
}

#[test]
fn an_http_check_reads_a_chunked_body() {
    let (port, _) = serve(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"ok\":t\r\n5;ext=1\r\nrue}\r\n0\r\n\r\n",
    );
    assert_eq!(
        attempt_once(expecting(
            port,
            None,
            &[equal("/ok", JsonScalar::Bool(true))]
        )),
        Attempt::Passed
    );
}

#[test]
fn an_http_check_fails_a_chunked_body_with_a_bad_chunk_size() {
    let (port, _) =
        serve(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n{}\r\n0\r\n\r\n");
    assert_eq!(
        attempt_once(expecting(port, None, &[present("/ok")])),
        Attempt::Failed(Failure::BadBody)
    );
}

#[test]
fn an_http_check_fails_a_body_longer_than_its_limit_without_reading_past_it() {
    let long = vec![b' '; MOST_BODY_BYTES + 1];
    let mut closed = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    closed.extend_from_slice(&long);
    let port = serve_built(closed);
    assert_eq!(
        attempt_once(expecting(port, None, &[present("/ok")])),
        Attempt::Failed(Failure::BodyTooLong)
    );

    let mut chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..2 {
        chunked.extend_from_slice(format!("{:x}\r\n", MOST_BODY_BYTES / 2).as_bytes());
        chunked.extend_from_slice(&long[..MOST_BODY_BYTES / 2]);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"1\r\n \r\n0\r\n\r\n");
    let port = serve_built(chunked);
    assert_eq!(
        attempt_once(expecting(port, None, &[present("/ok")])),
        Attempt::Failed(Failure::BodyTooLong)
    );
}

#[test]
fn an_http_check_fails_before_reading_a_body_whose_content_length_is_over_the_limit() {
    // The server sends no body and closes, so a check that read the body
    // would fail for a short body instead.
    let port = serve_built(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MOST_BODY_BYTES + 1
        )
        .into_bytes(),
    );
    assert_eq!(
        attempt_once(expecting(port, None, &[present("/ok")])),
        Attempt::Failed(Failure::BodyTooLong)
    );
}

#[test]
fn an_http_check_fails_a_body_shorter_than_its_content_length() {
    let (port, _) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n{\"ok\":true}");
    assert_eq!(
        attempt_once(expecting(port, None, &[present("/ok")])),
        Attempt::Failed(Failure::BadBody)
    );
}

#[test]
fn a_readiness_wait_passes_before_its_deadline_once_the_body_matches() {
    let (port, served) = serve_each(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n{\"ok\":false}",
        b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n{\"ok\":false}",
        b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
    ]);
    let json = [equal("/ok", JsonScalar::Bool(true))];
    let probe = quick(ProbeCheck::http_expecting(
        "/".to_owned(),
        None,
        json.to_vec(),
    ));
    let mut check = expecting(port, None, &json);
    let outcome = wait(
        &probe,
        &mut check,
        Goal::Ready,
        Instant::now() + SLACK,
        &mut never_ended(),
        &Cancel::new(),
    );
    assert_eq!(outcome, Outcome::Ready);
    assert!(served.load(Ordering::SeqCst) >= 3);
}

#[test]
fn no_failure_text_quotes_the_response_body() {
    const CANARY: &str = "canary-5d1e";
    const EXPECTED: &str = "expected-9b04";
    let cases: [(&'static [u8], JsonExpectation); 4] = [
        (
            b"HTTP/1.1 200 OK\r\n\r\nnot json canary-5d1e",
            present("/ok"),
        ),
        (
            b"HTTP/1.1 200 OK\r\n\r\n{\"ok\":\"canary-5d1e\"}",
            equal("/ok", JsonScalar::Bool(true)),
        ),
        (
            b"HTTP/1.1 200 OK\r\n\r\n{\"canary-5d1e\":1}",
            present("/ok"),
        ),
        (
            b"HTTP/1.1 200 OK\r\n\r\n{\"ok\":\"canary-5d1e\"}",
            equal("/ok", JsonScalar::String(EXPECTED.to_owned())),
        ),
    ];
    for (response, expectation) in cases {
        let (port, _) = serve(response);
        let json = [expectation];
        let Attempt::Failed(failure) = attempt_once(expecting(port, None, &json)) else {
            panic!("the attempt passed");
        };
        let probe = quick(ProbeCheck::http_expecting(
            "/".to_owned(),
            None,
            json.to_vec(),
        ));
        let outcome = wait(
            &probe,
            &mut expecting(port, None, &json),
            Goal::Ready,
            Instant::now() + Duration::from_millis(200),
            &mut never_ended(),
            &Cancel::new(),
        );
        for text in [
            failure.to_string(),
            format!("{failure:?}"),
            format!("{outcome:?}"),
            format!("{:?}", expecting(port, None, &json)),
        ] {
            assert!(!text.contains(CANARY), "{text}");
            assert!(!text.contains(EXPECTED), "{text}");
        }
    }
}

#[test]
fn an_http_body_read_notices_a_cancel_within_its_read_slice() {
    let (listener, port) = listener();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = [0_u8; 256];
        let _ = stream.read(&mut head).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"ok\"")
            .unwrap();
        pause(Duration::from_secs(2));
    });
    let cancel = Cancel::new();
    let canceller = cancel.clone();
    let cancelled_at = Arc::new(std::sync::Mutex::new(None));
    let stamp = Arc::clone(&cancelled_at);
    let setter = thread::spawn(move || {
        pause(Duration::from_millis(200));
        *stamp.lock().unwrap() = Some(Instant::now());
        canceller.cancel();
    });
    let json = [present("/ok")];
    let mut check = expecting(port, None, &json);
    assert_eq!(
        check.attempt(Duration::from_secs(30), &cancel),
        Attempt::Failed(Failure::Cancelled)
    );
    let ended = Instant::now();
    setter.join().unwrap();
    let cancelled = cancelled_at.lock().unwrap().unwrap();
    // One read slice is 50 ms. A busy machine may stretch it.
    let took = ended.saturating_duration_since(cancelled);
    assert!(took < Duration::from_millis(500), "took {took:?}");
    server.join().unwrap();
}
