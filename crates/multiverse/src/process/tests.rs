use std::time::{Duration, Instant};

use super::{Ran, run_command};

/// Returns the words of a `/bin/sh -c` that runs `script`.
fn shell(script: &str) -> Vec<String> {
    vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()]
}

#[test]
fn a_command_that_exits_gives_its_success_and_the_standard_output_it_kept() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        run_command(
            &shell("echo kept"),
            dir.path(),
            Duration::from_secs(10),
            true
        ),
        Ran::Exited {
            success: true,
            stdout: b"kept\n".to_vec(),
        }
    );
    assert_eq!(
        run_command(
            &shell("echo dropped; exit 3"),
            dir.path(),
            Duration::from_secs(10),
            false
        ),
        Ran::Exited {
            success: false,
            stdout: Vec::new(),
        }
    );
}

#[test]
fn a_check_whose_background_helper_holds_its_output_returns_once_the_check_exits() {
    let dir = tempfile::tempdir().unwrap();
    let begun = Instant::now();
    let ran = run_command(
        &shell("sleep 20 & echo ready"),
        dir.path(),
        Duration::from_secs(30),
        true,
    );
    let took = begun.elapsed();
    assert_eq!(
        ran,
        Ran::Exited {
            success: true,
            stdout: b"ready\n".to_vec(),
        }
    );
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[test]
fn a_check_past_its_deadline_is_timed_out() {
    let dir = tempfile::tempdir().unwrap();
    let begun = Instant::now();
    let ran = run_command(
        &shell("exec sleep 20"),
        dir.path(),
        Duration::from_millis(300),
        false,
    );
    let took = begun.elapsed();
    assert_eq!(ran, Ran::TimedOut);
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[test]
fn a_program_that_does_not_exist_is_unstartable() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        run_command(
            &["mxv-no-such-program".to_owned()],
            dir.path(),
            Duration::from_secs(10),
            false
        ),
        Ran::Unstartable
    );
    assert_eq!(
        run_command(&[], dir.path(), Duration::from_secs(10), false),
        Ran::Unstartable
    );
}
