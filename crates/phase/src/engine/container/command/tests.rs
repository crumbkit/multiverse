use std::ops::ControlFlow;
use std::time::Duration;

use super::stream_program;
use crate::clock::now;

/// Runs `script` with `/bin/sh -c` through [`stream_program`], and keeps
/// each line with whether it came from standard error, until `keep` says
/// to break.
fn streamed(
    script: &str,
    mut keep: impl FnMut(&[(bool, String)]) -> bool,
) -> (Option<std::process::ExitStatus>, Vec<(bool, String)>) {
    let mut lines = Vec::new();
    let status = stream_program(
        "/bin/sh".as_ref(),
        ["-c", script],
        "test",
        None,
        &mut |is_stderr, text| {
            lines.push((is_stderr, text.to_owned()));
            if keep(&lines) {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        },
    )
    .unwrap();
    (status, lines)
}

#[test]
fn a_stream_that_its_reader_breaks_kills_and_reaps_the_program() {
    // The shell prints its pid, then becomes `yes`, which prints until it
    // is killed.
    let (status, lines) = streamed("echo $$; exec yes", |lines| lines.len() < 3);
    assert!(status.is_none());
    let pid: i32 = lines[0].1.parse().unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    // A zombie still answers a test signal, so a pid that no longer
    // answers was reaped.
    assert!(rustix::process::test_kill_process(pid).is_err());
}

#[test]
fn a_stream_passes_each_line_while_the_other_stream_waits_for_output() {
    // Standard output stays open and silent after its first line, so a
    // reader that waited on it would hold back standard error's line for
    // the whole sleep.
    let started = now();
    let (status, lines) = streamed(
        "echo out; sleep 0.2; echo err >&2; exec sleep 30",
        |lines| lines.len() < 2,
    );
    assert!(status.is_none());
    assert_eq!(lines, [(false, "out".to_owned()), (true, "err".to_owned())]);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_stream_ends_with_the_programs_exit_once_both_streams_close() {
    let (status, lines) = streamed("printf 'a\\r\\nb'; exit 3", |_| true);
    assert_eq!(status.and_then(|status| status.code()), Some(3));
    assert_eq!(lines, [(false, "a".to_owned()), (false, "b".to_owned())]);
}

#[test]
fn a_reader_that_panics_still_leaves_the_program_killed_and_reaped() {
    let mut pid = None;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        stream_program(
            "/bin/sh".as_ref(),
            ["-c", "echo $$; exec sleep 30"],
            "test",
            None,
            &mut |_, text| {
                pid = text.parse::<i32>().ok();
                panic!("the reader failed");
            },
        )
    }));
    assert!(outcome.is_err());
    let pid = rustix::process::Pid::from_raw(pid.unwrap()).unwrap();
    // A zombie still answers a test signal, so a pid that no longer
    // answers was reaped.
    assert!(rustix::process::test_kill_process(pid).is_err());
}

#[test]
fn a_line_longer_than_the_bound_comes_out_in_parts() {
    let most = super::MOST_LINE_BYTES;
    let script = format!("head -c {} /dev/zero | tr '\\000' a", most + 10);
    let (status, lines) = streamed(&script, |_| true);
    assert_eq!(status.and_then(|status| status.code()), Some(0));
    let lengths: Vec<usize> = lines.iter().map(|(_, text)| text.len()).collect();
    assert_eq!(lengths, [most, 10]);
}

#[test]
fn a_stream_with_an_idle_deadline_kills_a_program_that_prints_nothing_for_it() {
    let started = now();
    let mut lines = Vec::new();
    let result = stream_program(
        "/bin/sh".as_ref(),
        ["-c", "echo first; exec sleep 30"],
        "test",
        Some(Duration::from_millis(300)),
        &mut |_, text| {
            lines.push(text.to_owned());
            ControlFlow::Continue(())
        },
    );
    assert!(
        matches!(
            &result,
            Err(crate::engine::container::ContainerError::Failed {
                cause: crate::record::DockerCause::TimedOut,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(lines, ["first"]);
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// The variable that tells
/// [`a_stream_runs_here_until_a_signal_ends_this_process`] to run its
/// stream, which only
/// [`a_sigint_to_the_callers_group_ends_the_streamed_program_with_the_caller`]
/// sets.
const STREAM_UNTIL_SIGNAL: &str = "PHASE_TEST_STREAM_UNTIL_SIGNAL";

/// What the helper prints before the streamed program's process id.
const STREAMED_PID: &str = "streamed-pid=";

#[test]
fn a_stream_runs_here_until_a_signal_ends_this_process() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the helper runs only when the test that starts it sets this variable"
    )]
    if std::env::var_os(STREAM_UNTIL_SIGNAL).is_none() {
        return;
    }
    let _ = stream_program(
        "/bin/sh".as_ref(),
        ["-c", "echo $$; exec sleep 30"],
        "test",
        None,
        &mut |_, text| {
            println!("{STREAMED_PID}{text}");
            ControlFlow::Continue(())
        },
    );
}

#[test]
fn a_sigint_to_the_callers_group_ends_the_streamed_program_with_the_caller() {
    use std::io::BufRead as _;
    use std::os::unix::process::CommandExt as _;
    // The helper leads a process group of its own, so the SIGINT reaches
    // only the helper and what it started, as Ctrl-C reaches the terminal's
    // foreground group.
    #[expect(
        clippy::disallowed_methods,
        reason = "the test starts its own binary as the caller of the stream"
    )]
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "engine::container::command::tests::a_stream_runs_here_until_a_signal_ends_this_process",
            "--exact",
            "--nocapture",
        ])
        .env(STREAM_UNTIL_SIGNAL, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .process_group(0);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test starts its own binary as the caller of the stream"
    )]
    let mut helper = command.spawn().unwrap();
    let stdout = helper.stdout.take().unwrap();
    let streamed = std::io::BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix(STREAMED_PID).map(str::to_owned))
        .expect("the helper printed the streamed program's process id");
    let streamed = rustix::process::Pid::from_raw(streamed.parse().unwrap()).unwrap();
    let group = rustix::process::Pid::from_raw(i32::try_from(helper.id()).unwrap()).unwrap();
    rustix::process::kill_process_group(group, rustix::process::Signal::INT).unwrap();
    let status = helper.wait().unwrap();
    assert!(!status.success(), "{status:?}");
    // Once the helper is gone, init reaps the program, so a process id
    // that no longer answers a test signal belongs to a program that ended.
    let gone = (0..100).any(|_| {
        let gone = rustix::process::test_kill_process(streamed).is_err();
        if !gone {
            crate::clock::pause(Duration::from_millis(50));
        }
        gone
    });
    assert!(gone, "the streamed program outlived its caller");
}

#[test]
fn a_part_of_a_long_line_never_ends_inside_a_utf8_character() {
    let most = super::MOST_LINE_BYTES;
    // One ASCII byte short of the bound, then a two-byte character that
    // the bound would cut in half.
    let script = format!(
        "head -c {} /dev/zero | tr '\\000' a; printf '\\303\\251b'",
        most - 1
    );
    let (status, lines) = streamed(&script, |_| true);
    assert_eq!(status.and_then(|status| status.code()), Some(0));
    let lengths: Vec<usize> = lines.iter().map(|(_, text)| text.len()).collect();
    assert_eq!(lengths, [most - 1, 3]);
    assert_eq!(lines[1].1, "\u{e9}b");
}

#[test]
fn a_broken_stream_returns_soon_when_a_child_of_the_program_holds_its_pipes() {
    // The shell does not exec, and its background `sleep` holds standard
    // output open after the shell is killed.
    let started = now();
    let (status, lines) = streamed("sleep 5 & echo $!; wait", |_| false);
    assert!(status.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    let pid: i32 = lines[0].1.parse().unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
}

/// What `docker compose up` printed on standard error for a port that
/// another container publishes, as Docker 29.8 and Compose 5.5 print it.
const PORT_CONFLICT: &str = " Network phase_default Creating \n Container phase-web-1 Creating \n Container phase-web-1 Created \n Container phase-web-1 Starting \nError response from daemon: failed to set up container networking: driver failed programming external connectivity on endpoint phase-web-1 (0123): Bind for 127.0.0.1:47391 failed: port is already allocated\n";

/// What `docker compose up` printed on standard error for an image that
/// the registry does not have.
const PULL_FAILURE: &str = " Image redis:no-such-tag Pulling \n Image redis:no-such-tag Error failed to resolve reference \"docker.io/library/redis:no-such-tag\": not found\nError response from daemon: failed to resolve reference \"docker.io/library/redis:no-such-tag\": not found\n";

/// What `docker compose up --pull never` printed on standard error for an
/// image that Docker does not have.
const MISSING_IMAGE: &str = " Container phase-web-1 Creating \n service:web:1 Error response from daemon: No such image: example:1.0 \nError response from daemon: No such image: example:1.0\n";

/// A value that stands in for a secret.
const SECRET: &str = "hunter2-secret-value";

#[test]
fn a_port_conflict_a_pull_failure_and_a_missing_image_yield_dockers_own_line() {
    assert_eq!(
        super::compose_docker_line(PORT_CONFLICT).as_deref(),
        Some(
            "Error response from daemon: failed to set up container networking: driver failed programming external connectivity on endpoint phase-web-1 (0123): Bind for 127.0.0.1:47391 failed: port is already allocated"
        )
    );
    assert_eq!(
        super::compose_docker_line(PULL_FAILURE).as_deref(),
        Some(
            "Error response from daemon: failed to resolve reference \"docker.io/library/redis:no-such-tag\": not found"
        )
    );
    assert_eq!(
        super::compose_docker_line(MISSING_IMAGE).as_deref(),
        Some("Error response from daemon: No such image: example:1.0")
    );
}

#[test]
fn each_of_dockers_own_error_forms_is_dockers_line() {
    for line in [
        "Error response from daemon: No such container: abc",
        "Error: No such container: abc",
        "error from daemon in stream: Error grabbing logs: EOF",
        "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
        "failed to connect to the docker API at unix:///var/run/docker.sock; check if the path is correct and if the daemon is running: dial unix /var/run/docker.sock: connect: no such file or directory",
    ] {
        assert_eq!(
            super::docker_line(&format!("{line}\n")).as_deref(),
            Some(line)
        );
        assert_eq!(
            super::compose_docker_line(&format!("{line}\n")).as_deref(),
            Some(line)
        );
    }
}

#[test]
fn a_line_that_a_container_wrote_never_becomes_dockers_line() {
    for stderr in [
        format!("Error: could not log in with password {SECRET}\n"),
        format!("ERROR token={SECRET}\n"),
        format!("docker: {SECRET}\n"),
        format!("error response from daemon: {SECRET}\n"),
        format!("  Error response from daemon: {SECRET}\n"),
        // A script inside the container wrote a line, then a line that
        // imitates Docker's. Docker prints nothing else when the process it
        // ran fails, so the whole of standard error is not Docker's.
        format!(
            "phase: the secret TOKEN arrived with 3 of 9 bytes\nError response from daemon: {SECRET}\n"
        ),
        format!("cat: write error\nCannot connect to the Docker daemon {SECRET}\n"),
        String::new(),
    ] {
        assert_eq!(super::docker_line(&stderr), None, "{stderr}");
    }
    // Compose prints its own progress lines, so only its last line counts,
    // and only when it is one of Docker's forms.
    for stderr in [
        format!("Error response from daemon: {SECRET}\nweb-1  | Error: {SECRET}\n"),
        format!(" Container phase-web-1 Starting \nError: {SECRET}\n"),
    ] {
        assert_eq!(super::compose_docker_line(&stderr), None, "{stderr}");
    }
}

#[test]
fn a_failed_command_carries_dockers_line_only_when_standard_error_is_dockers_alone() {
    use std::os::unix::process::ExitStatusExt;

    let finished = |stderr: &str| super::Finished {
        status: std::process::ExitStatus::from_raw(1 << 8),
        stdout: String::new(),
        stderr: stderr.to_owned(),
    };
    let docker_line = |stderr: &str| match finished(stderr).ok("secret write", Some("web")) {
        Err(super::Error::Failed { docker_line, .. }) => docker_line,
        other => panic!("{:?}", other.map(drop)),
    };
    assert_eq!(
        docker_line("Error response from daemon: container abc is not running\n").as_deref(),
        Some("Error response from daemon: container abc is not running")
    );
    let leaked = docker_line(&format!("{SECRET}\nError response from daemon: {SECRET}\n"));
    assert_eq!(leaked, None);
}
