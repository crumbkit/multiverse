use std::ops::ControlFlow;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::logs_with;
use crate::engine::container::ContainerError;

/// A line that a container could write, which holds a fake credential.
const SECRET_LINE: &str = "connecting with password=fake-hunter2";

/// A line that a container could write that imitates docker's own error
/// and holds a fake credential.
const SECRET_LOOKALIKE: &str = "Error response from daemon: token=fake-hunter2";

/// Writes a `docker` program into `dir` that runs `script` with `/bin/sh`,
/// whatever its arguments, and returns its path.
fn fake_docker(dir: &Path, script: &str) -> PathBuf {
    let path = dir.join("docker");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a fake docker program into its own temporary directory"
    )]
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn id() -> crate::record::ContainerId {
    crate::record::ContainerId::try_from("a".repeat(64)).unwrap()
}

/// One line that the logs passed: whether it came from standard error,
/// its text, and how long after the start it arrived.
type Passed = (bool, String, Duration);

/// Runs the logs of [`id`] through the fake docker `script`, with `follow`,
/// and returns the result with each line it passed.
fn logs_following(script: &str, follow: bool) -> (Result<(), ContainerError>, Vec<Passed>) {
    let dir = tempfile::tempdir().unwrap();
    let docker = fake_docker(dir.path(), script);
    let mut lines = Vec::new();
    let started = Instant::now();
    let result = logs_with(
        docker.as_os_str(),
        &id(),
        Some(10),
        follow,
        &mut |is_stderr, text| {
            lines.push((is_stderr, text.to_owned(), started.elapsed()));
            ControlFlow::Continue(())
        },
    );
    (result, lines)
}

/// Runs the logs of [`id`] through the fake docker `script` without
/// follow, and returns the result with each line it passed and whether it
/// came from standard error, sorted, because the two streams are read
/// apart and only the order within one holds.
fn logs_of(script: &str) -> (Result<(), ContainerError>, Vec<(bool, String)>) {
    let (result, lines) = logs_following(script, false);
    let mut lines: Vec<(bool, String)> = lines
        .into_iter()
        .map(|(is_stderr, text, _)| (is_stderr, text))
        .collect();
    lines.sort();
    (result, lines)
}

fn message(result: Result<(), ContainerError>) -> String {
    match result {
        Err(ContainerError::Failed { message, .. }) => message,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn stderr(text: &str) -> (bool, String) {
    (true, text.to_owned())
}

#[test]
fn a_failure_after_the_container_wrote_to_stderr_names_the_exit_status_and_not_its_line() {
    let (result, lines) = logs_of(&format!("echo '{SECRET_LINE}' >&2; exit 1"));

    let message = message(result);
    assert!(!message.contains("hunter2"), "{message}");
    assert!(message.contains("exit status: 1"), "{message}");
    assert_eq!(lines, [stderr(SECRET_LINE)]);
}

#[test]
fn a_failure_with_code_1_keeps_dockers_own_error_line_and_never_passes_it_as_a_line() {
    let (result, lines) = logs_of(&format!(
        "echo started; echo '{SECRET_LINE}' >&2; \
         echo 'Error response from daemon: No such container: abc' >&2; exit 1"
    ));

    assert_eq!(
        message(result),
        "Error response from daemon: No such container: abc"
    );
    assert_eq!(lines, [(false, "started".to_owned()), stderr(SECRET_LINE)]);
}

#[test]
fn each_recognised_docker_error_becomes_the_message_of_a_failure_with_code_1() {
    for docker_line in [
        "Error response from daemon: No such container: abc",
        "Error: No such container: abc",
        "error from daemon in stream: Error grabbing logs: EOF",
        "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.",
    ] {
        let (result, lines) = logs_of(&format!("echo '{docker_line}' >&2; exit 1"));

        assert_eq!(message(result), docker_line);
        assert!(lines.is_empty(), "{lines:?}");
    }
}

#[test]
fn a_line_of_standard_output_after_dockers_error_does_not_pass_the_error_as_a_line() {
    let (result, lines) =
        logs_of("echo 'Error response from daemon: No such container: abc' >&2; echo late; exit 1");

    assert_eq!(
        message(result),
        "Error response from daemon: No such container: abc"
    );
    assert_eq!(lines, [(false, "late".to_owned())]);
}

#[test]
fn a_docker_killed_by_term_names_its_signal_and_passes_a_lookalike_line_as_the_containers() {
    for last in [
        SECRET_LINE,
        "Error: login failed with password=fake-hunter2",
        SECRET_LOOKALIKE,
    ] {
        let (result, lines) = logs_of(&format!("echo '{last}' >&2; kill -TERM $$"));

        let message = message(result);
        assert!(!message.contains("hunter2"), "{message}");
        assert!(message.contains("signal"), "{message}");
        assert_eq!(lines, [stderr(last)]);
    }
}

#[test]
fn a_docker_killed_by_sigkill_as_the_oom_killer_or_pkill_does_names_its_signal_only() {
    let (result, lines) = logs_of(&format!("echo '{SECRET_LOOKALIKE}' >&2; kill -KILL $$"));

    let message = message(result);
    assert!(!message.contains("hunter2"), "{message}");
    assert!(message.contains("signal"), "{message}");
    assert_eq!(lines, [stderr(SECRET_LOOKALIKE)]);
}

#[test]
fn a_failure_with_another_code_names_the_exit_status_and_passes_a_lookalike_line() {
    let (result, lines) = logs_of(&format!("echo '{SECRET_LOOKALIKE}' >&2; exit 2"));

    let message = message(result);
    assert!(!message.contains("hunter2"), "{message}");
    assert!(message.contains("exit status: 2"), "{message}");
    assert_eq!(lines, [stderr(SECRET_LOOKALIKE)]);
}

#[test]
fn a_lookalike_line_reaches_the_sink_when_a_later_line_or_a_success_follows() {
    let (result, lines) = logs_following(
        "echo 'Error response from daemon: first' >&2; echo next >&2; \
         echo 'Error response from daemon: last' >&2; exit 0",
        false,
    );

    assert!(result.is_ok(), "{result:?}");
    let texts: Vec<&str> = lines.iter().map(|(_, text, _)| text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "Error response from daemon: first",
            "next",
            "Error response from daemon: last",
        ]
    );
}

#[test]
fn a_follow_passes_a_containers_error_at_once_and_a_lookalike_line_soon_after_it_goes_quiet() {
    let (result, lines) = logs_following(
        "echo 'Error: database connection refused' >&2; \
         echo 'Error response from daemon: lookalike' >&2; sleep 2; echo after >&2; exit 0",
        true,
    );

    assert!(result.is_ok(), "{result:?}");
    let texts: Vec<&str> = lines.iter().map(|(_, text, _)| text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "Error: database connection refused",
            "Error response from daemon: lookalike",
            "after",
        ]
    );
    // The lookalike line waits only its hold, and not for the line that
    // comes 2 seconds later.
    let (lookalike, after) = (lines[1].2, lines[2].2);
    assert!(
        after.saturating_sub(lookalike) >= Duration::from_secs(1),
        "{lookalike:?} {after:?}"
    );
}
