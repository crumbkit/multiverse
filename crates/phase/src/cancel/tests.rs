use super::Cancel;

#[test]
fn a_new_cancel_is_not_cancelled() {
    assert!(!Cancel::new().is_cancelled());
}

#[test]
fn cancelling_a_clone_cancels_the_original() {
    let cancel = Cancel::new();
    let handler = cancel.clone();

    handler.cancel();

    assert!(cancel.is_cancelled());
}

/// Returns a `/bin/sh -c` command that runs `script`.
fn shell(script: &str) -> std::process::Command {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test prepares a command for the probe's bounded runner"
    )]
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", script]);
    command
}

#[test]
fn a_cancel_stops_a_run_that_started_before_it() {
    use std::time::{Duration, Instant};

    use crate::engine::probe::{CommandEnd, run_bounded};

    let cancel = Cancel::new();
    let canceller = cancel.clone();
    let setter = std::thread::spawn(move || {
        crate::clock::pause(Duration::from_millis(200));
        canceller.cancel();
    });
    let begun = Instant::now();
    let end = run_bounded(shell("exec sleep 20"), Duration::from_secs(30), &cancel).unwrap();
    let took = begun.elapsed();
    setter.join().unwrap();
    assert_eq!(end, CommandEnd::Cancelled);
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[test]
fn a_run_that_starts_after_the_cancel_starts_no_child() {
    use std::time::Duration;

    use crate::engine::probe::{CommandEnd, run_bounded};

    let dir = tempfile::tempdir().unwrap();
    let mark = dir.path().join("started");
    let cancel = Cancel::new();
    cancel.cancel();
    let end = run_bounded(
        shell(&format!("touch '{}'", mark.display())),
        Duration::from_secs(30),
        &cancel,
    )
    .unwrap();
    assert_eq!(end, CommandEnd::Cancelled);
    assert!(!mark.exists());
}
