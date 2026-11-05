//! Tests of the wrapper and the writer, which run in the host's own
//! `/bin/sh` with a temporary directory in place of `/run/secrets`.

use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use crate::clock::pause;

use super::{ImageCommand, entrypoint, waiting_wrapper, write_args, writer};

/// Runs `script` in the host's `/bin/sh` with `args`, and returns what it
/// printed and how it exited.
fn sh(script: &str, args: &[&str], input: Option<&str>) -> Output {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs the wrapper's script in the host's shell"
    )]
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script).arg("phase-test").args(args);
    match input {
        None => {
            #[expect(
                clippy::disallowed_methods,
                reason = "the test runs the wrapper's script in the host's shell"
            )]
            let output = command.output().unwrap();
            output
        }
        Some(input) => {
            use std::io::Write as _;
            use std::process::Stdio;
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[expect(
                clippy::disallowed_methods,
                reason = "the test runs the writer's script in the host's shell"
            )]
            let mut child = command.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        }
    }
}

/// Writes `text` to the file at `path`, as phase's writer would.
fn write(path: &std::path::Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test stands in for the writer of the secrets' files"
    )]
    std::fs::write(path, text).unwrap();
}

#[test]
fn the_wrapper_waits_for_every_secret_file_and_then_runs_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let writer = thread::spawn(move || {
        pause(Duration::from_millis(400));
        write(&path.join("TOKEN"), "test-token");
        pause(Duration::from_millis(400));
        write(&path.join("KEY"), "test-key");
    });
    let began = Instant::now();

    let output = sh(
        &waiting_wrapper(&dir.path().to_string_lossy(), 10),
        &[
            "TOKEN",
            "KEY",
            "--",
            "sh",
            "-c",
            "echo ran with $0",
            "the-app",
        ],
        None,
    );

    writer.join().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "ran with the-app\n"
    );
    assert!(began.elapsed() >= Duration::from_millis(800));
}

#[test]
fn the_wrapper_gives_up_on_a_file_that_does_not_appear_and_names_it() {
    let dir = tempfile::tempdir().unwrap();

    let output = sh(
        &waiting_wrapper(&dir.path().to_string_lossy(), 1),
        &["TOKEN", "--", "echo", "ran"],
        None,
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("/TOKEN did not appear within 1 seconds"),
        "{said}"
    );
}

#[test]
fn the_wrapper_fails_when_there_is_no_command_to_run() {
    let dir = tempfile::tempdir().unwrap();

    let output = sh(
        &waiting_wrapper(&dir.path().to_string_lossy(), 1),
        &["--"],
        None,
    );

    assert_eq!(output.status.code(), Some(127));
}

#[test]
fn the_entrypoint_runs_the_wrapper_in_sh_with_the_names_and_escapes_dollar_signs_for_compose() {
    let names = ["KEY".to_owned(), "TOKEN".to_owned()];

    let entrypoint = entrypoint(names.iter());

    assert_eq!(entrypoint[..2], ["/bin/sh", "-c"]);
    assert!(entrypoint[2].contains("exec \"$$@\""), "{}", entrypoint[2]);
    assert!(!entrypoint[2].replace("$$", "").contains('$'));
    assert_eq!(entrypoint[3..], ["phase-secrets", "KEY", "TOKEN", "--"]);
}

#[test]
fn the_writer_replaces_the_secret_file_with_its_input_and_leaves_no_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    // The host has no `/proc/1/status`, so the test's script reads the
    // owner from an empty file and gives the file to user and group 0,
    // which only root may do. The test therefore leaves `chown` out.
    let script = writer(&dir.path().to_string_lossy())
        .replace("/proc/1/status", "/dev/null")
        .replace("chown \"$u:$g\" \"$d\" \"$t\"\n", "");

    let first = sh(&script, &["TOKEN"], Some("first-test-value"));
    let second = sh(&script, &["TOKEN"], Some("second-test-value\n"));

    assert!(first.status.success(), "{first:?}");
    assert!(second.status.success(), "{second:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("TOKEN")).unwrap(),
        "second-test-value\n"
    );
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["TOKEN"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.path().join("TOKEN"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o400);
    }
}

#[test]
fn the_write_runs_as_root_in_the_container_and_names_the_secret_but_holds_no_value() {
    let args = write_args("abc123", "TOKEN");

    assert_eq!(
        args[..6],
        ["exec", "--interactive", "--user", "0", "--", "abc123"]
    );
    assert_eq!(args.last().map(String::as_str), Some("TOKEN"));
}

#[test]
fn an_image_command_reads_dockers_nulls_as_empty_and_the_plans_command_replaces_the_images() {
    let printed = r#"{"entrypoint":["docker-entrypoint.sh"],"cmd":["postgres"]}"#;
    let image = ImageCommand::parse(printed).unwrap();

    assert_eq!(image.argv(&[]), ["docker-entrypoint.sh", "postgres"]);
    assert_eq!(
        image.argv(&[
            "postgres".to_owned(),
            "-c".to_owned(),
            "fsync=off".to_owned()
        ]),
        ["docker-entrypoint.sh", "postgres", "-c", "fsync=off"]
    );
    let bare = ImageCommand::parse("{\"entrypoint\":null,\"cmd\":[\"sh\"]}\n").unwrap();
    assert_eq!(bare.argv(&[]), ["sh"]);
    assert_eq!(ImageCommand::parse("no json"), None);
}
