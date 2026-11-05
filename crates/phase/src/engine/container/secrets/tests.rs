//! Tests of the wrapper and the staging and commit scripts, which run in
//! the host's own `/bin/sh` with a temporary directory in place of
//! `/run/secrets`, and of the owner of the secret files.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::clock::pause;
use crate::env::AppliedEnvironment;
use crate::plan::{AppPlan, ProcessPlan, RunnerKind};
use crate::source::Source;

use super::{
    ImageConfig, SecretBytes, WRAPPER_FAILED, commit_script, entrypoint, owner, stage,
    stage_script, waiting_wrapper,
};

/// Runs `script` in the host's `/bin/sh` with `args`, `input` on its
/// standard input and `path` as its `PATH`, and returns what it printed
/// and how it exited.
fn sh(script: &str, args: &[&str], input: &[u8], path: Option<&str>) -> Output {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs phase's scripts in the host's shell"
    )]
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .arg("phase-test")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs phase's scripts in the host's shell"
    )]
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
}

/// Writes `text` to the file at `path`, as phase's scripts would.
fn write(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test stands in for the writer of the secrets' files"
    )]
    std::fs::write(path, text).unwrap();
}

/// Returns the names in `dir`, in order.
fn listed(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
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
        b"",
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
fn the_wrapper_gives_up_on_a_file_that_does_not_appear_with_status_70_and_names_it() {
    let dir = tempfile::tempdir().unwrap();

    let output = sh(
        &waiting_wrapper(&dir.path().to_string_lossy(), 1),
        &["TOKEN", "--", "echo", "ran"],
        b"",
        None,
    );

    assert_eq!(output.status.code(), Some(WRAPPER_FAILED));
    assert!(output.stdout.is_empty());
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(said.lines().count(), 1, "{said}");
    assert!(
        said.contains("/TOKEN did not appear within 1 seconds"),
        "{said}"
    );
}

#[test]
fn the_wrapper_refuses_at_once_an_image_without_sleep_instead_of_spinning() {
    let dir = tempfile::tempdir().unwrap();
    let began = Instant::now();

    let output = sh(
        &waiting_wrapper(&dir.path().to_string_lossy(), 60),
        &["TOKEN", "--", "echo", "ran"],
        b"",
        Some("/nonexistent"),
    );

    assert_eq!(output.status.code(), Some(WRAPPER_FAILED));
    assert!(began.elapsed() < Duration::from_secs(5));
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("has no sleep program"), "{said}");
    assert!(said.contains("/TOKEN"), "{said}");
}

#[test]
fn the_wrapper_fails_with_status_70_when_there_is_no_command_to_run() {
    let dir = tempfile::tempdir().unwrap();

    let output = sh(
        &waiting_wrapper(&dir.path().to_string_lossy(), 1),
        &["--"],
        b"",
        None,
    );

    assert_eq!(output.status.code(), Some(WRAPPER_FAILED));
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

/// Returns the staging script for `dir` without its `chown`, which only
/// root may run with another owner.
fn host_stage(dir: &Path) -> String {
    stage_script(&dir.to_string_lossy()).replace("chown \"$2\" \"$d\" \"$t\"\n", "")
}

#[test]
fn staging_and_committing_replace_every_secret_file_and_leave_no_staged_file() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("TOKEN"), "old-test-value");
    let stage = host_stage(dir.path());

    let token = sh(&stage, &["TOKEN", "1:1"], b"18\nnew-test-value\nab\n", None);
    let key = sh(&stage, &["KEY", "1:1"], b"0\n", None);
    assert!(token.status.success(), "{token:?}");
    assert!(key.status.success(), "{key:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("TOKEN")).unwrap(),
        "old-test-value"
    );
    let commit = sh(
        &commit_script(&dir.path().to_string_lossy()),
        &["TOKEN", "KEY"],
        b"",
        None,
    );

    assert!(commit.status.success(), "{commit:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("TOKEN")).unwrap(),
        "new-test-value\nab\n"
    );
    assert_eq!(std::fs::read(dir.path().join("KEY")).unwrap(), b"");
    assert_eq!(listed(dir.path()), ["KEY", "TOKEN"]);
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
fn a_feed_cut_short_fails_its_staging_removes_the_staged_file_and_keeps_the_old_value() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("TOKEN"), "old-test-value");

    let cut = sh(
        &host_stage(dir.path()),
        &["TOKEN", "1:1"],
        b"14\nnew-test",
        None,
    );
    let garbled = sh(&host_stage(dir.path()), &["TOKEN", "1:1"], b"x1\nabc", None);

    assert_eq!(cut.status.code(), Some(66), "{cut:?}");
    let said = String::from_utf8_lossy(&cut.stderr);
    assert!(
        said.contains("the secret TOKEN arrived with 8 of 14 bytes"),
        "{said}"
    );
    assert!(!said.contains("new-test"));
    assert_eq!(garbled.status.code(), Some(65), "{garbled:?}");
    assert_eq!(listed(dir.path()), ["TOKEN"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("TOKEN")).unwrap(),
        "old-test-value"
    );
}

#[test]
fn staging_runs_as_root_in_the_container_and_names_the_secret_and_owner_but_holds_no_value() {
    let args = stage("abc123", "TOKEN", "999:999");

    assert_eq!(
        args[..7],
        [
            "exec",
            "--interactive",
            "--user",
            "0",
            "--",
            "abc123",
            "/bin/sh"
        ]
    );
    assert_eq!(
        args[args.len() - 3..],
        ["phase-secret-stage", "TOKEN", "999:999"]
    );
}

/// An environment that holds `TOKEN` and writes its value.
struct Token;

impl AppliedEnvironment for Token {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::from(["TOKEN".to_owned()])
    }

    fn expires_at(&self) -> Option<std::time::SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, _command: &mut Command, _reference: &dyn Fn(&str) -> Option<String>) {}

    fn write_value(&self, name: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        if name != "TOKEN" {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        out.write_all("test-värde".as_bytes())
    }
}

#[test]
fn a_secrets_bytes_are_framed_by_their_length_in_bytes() {
    let framed = SecretBytes::of(&Token, "TOKEN").unwrap();

    assert_eq!(framed.framed(), "11\ntest-värde".as_bytes());
    assert_eq!(
        SecretBytes::of(&Token, "OTHER")
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::NotFound)
    );
}

/// Returns a container app whose one process receives secrets as files.
fn app_with(user: Option<&str>, secret_owner: Option<&str>) -> (AppPlan, ProcessPlan) {
    let mut app = AppPlan::new(
        "db".parse().unwrap(),
        RunnerKind::Container,
        Source::image("busybox:1.37.0".parse().unwrap()),
    );
    app.container.user = user.map(str::to_owned);
    let mut process = ProcessPlan::new("server".parse().unwrap(), Vec::new());
    process.secret_owner = secret_owner.map(str::to_owned);
    (app, process)
}

#[test]
fn the_secret_files_belong_to_the_secret_owner_then_the_run_as_user_then_the_images_user() {
    let rooted = ImageConfig::new(&[], &[], "");
    let unprivileged = ImageConfig::new(&[], &[], "redis:redis");

    let (app, process) = app_with(Some("1000:1000"), Some("999:999"));
    assert_eq!(owner(&app, &process, &rooted).as_deref(), Some("999:999"));
    let (app, process) = app_with(Some("0:0"), Some("0:0"));
    assert_eq!(owner(&app, &process, &rooted).as_deref(), Some("0:0"));
    let (app, process) = app_with(Some("1000:1000"), None);
    assert_eq!(
        owner(&app, &process, &unprivileged).as_deref(),
        Some("1000:1000")
    );
    let (app, process) = app_with(None, None);
    assert_eq!(
        owner(&app, &process, &unprivileged).as_deref(),
        Some("redis:redis")
    );
}

#[test]
fn files_that_would_belong_to_root_without_a_secret_owner_have_no_owner() {
    let unprivileged = ImageConfig::new(&[], &[], "999");
    for user in ["0", "root", "0:0", "root:root", ""] {
        let (app, process) = app_with(Some(user), None);
        assert_eq!(owner(&app, &process, &unprivileged), None, "{user}");
        let (app, process) = app_with(None, None);
        assert_eq!(
            owner(&app, &process, &ImageConfig::new(&[], &[], user)),
            None,
            "{user}"
        );
    }
}

#[test]
fn an_image_config_reads_dockers_nulls_as_empty_and_the_plans_command_replaces_the_images() {
    let printed = r#"{"entrypoint":["docker-entrypoint.sh"],"cmd":["postgres"],"user":""}"#;
    let image = ImageConfig::parse(printed).unwrap();

    assert_eq!(image.argv(&[]), ["docker-entrypoint.sh", "postgres"]);
    assert_eq!(
        image.argv(&[
            "postgres".to_owned(),
            "-c".to_owned(),
            "fsync=off".to_owned()
        ]),
        ["docker-entrypoint.sh", "postgres", "-c", "fsync=off"]
    );
    let bare =
        ImageConfig::parse("{\"entrypoint\":null,\"cmd\":[\"sh\"],\"user\":\"1000\"}\n").unwrap();
    assert_eq!(bare.argv(&[]), ["sh"]);
    assert_eq!(bare, ImageConfig::new(&[], &["sh"], "1000"));
    assert_eq!(ImageConfig::parse("no json"), None);
}

/// Writes a long value one byte at a time, so the buffer that holds it
/// grows many times.
struct Trickle;

impl AppliedEnvironment for Trickle {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::from(["TOKEN".to_owned()])
    }

    fn expires_at(&self) -> Option<std::time::SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, _command: &mut Command, _reference: &dyn Fn(&str) -> Option<String>) {}

    fn write_value(&self, _name: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        for index in 0..5000_u32 {
            out.write_all(&[b'a' + u8::try_from(index % 26).unwrap()])?;
        }
        Ok(())
    }
}

#[test]
fn a_value_written_in_many_small_pieces_is_framed_whole() {
    let bytes = SecretBytes::of(&Trickle, "TOKEN").unwrap();

    let expected: Vec<u8> = (0..5000_u32)
        .map(|index| b'a' + u8::try_from(index % 26).unwrap())
        .collect();
    let mut framed = b"5000\n".to_vec();
    framed.extend_from_slice(&expected);
    assert_eq!(bytes.framed(), framed.as_slice());
}
