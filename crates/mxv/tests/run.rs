//! End-to-end tests of `mxv run`. Each test builds a repository in a
//! temporary directory and runs the built `mxv` binary with its own state
//! directory and workspace root there.
//!
//! Most tests put a small script named `mise` on `PATH`, so they run on a
//! machine without mise. The tests whose names say `real_mise` use the
//! installed program, and skip with a line that says so when it is missing.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, repository};

/// The longest that a test lets one `mxv` command run.
const MOST_WAIT: Duration = Duration::from_secs(60);

/// A `PATH` that holds no mise on the machines that the tests run on.
const PATH_WITHOUT_MISE: &str = "/usr/bin:/bin";

/// A deadband socket that no daemon listens on, so a test never waits for
/// a place on the machine.
const MISSING_SOCKET: &str = "/nonexistent/deadband.sock";

/// Writes a fixture file.
#[expect(
    clippy::disallowed_methods,
    reason = "the tests write a checkout's files, which the library's store does not hold"
)]
fn write_file(path: impl AsRef<Path>, text: impl AsRef<[u8]>) -> std::io::Result<()> {
    fs::write(path, text)
}

/// A temporary directory with a repository `app`, a workspace root and a
/// state directory, and the `PATH` that each run receives.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The `PATH` of each run.
    path: String,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        repository(&dir.path().canonicalize().unwrap());
        Self {
            dir,
            path: PATH.to_owned(),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Puts a script named `mise` that runs `body` first on `PATH`.
    fn fake_mise(&mut self, body: &str) {
        let bin = self.root().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let script = bin.join("mise");
        write_file(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        self.path = format!("{}:{PATH}", bin.display());
    }

    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", &self.path)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
            .env("DEADBAND_SOCKET", MISSING_SOCKET)
            .timeout(MOST_WAIT)
    }

    /// Runs `mxv` and returns its exit code, standard output and error.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let output = self.command(args).output().unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Runs `mxv` with `--json` and returns its exit code and document.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        // `--json` goes first, because each word after a task is the task's.
        let args: Vec<&str> = std::iter::once("--json")
            .chain(args.iter().copied())
            .collect();
        let (exit, stdout, stderr) = self.run(&args);
        let document = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("{error}: {stdout} {stderr}"));
        (exit, document)
    }

    /// Creates the workspace `name` of the repository `app`, writes
    /// `manifest` as its checkout's `mxv.toml` unless it is `None`, and
    /// returns the checkout's path.
    fn workspace(&self, name: &str, manifest: Option<&str>) -> PathBuf {
        let (exit, document) = self.json(&["new", name, "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        let checkout = self.root().join("workspaces").join(name).join("app");
        if let Some(manifest) = manifest {
            write_file(checkout.join("mxv.toml"), manifest).unwrap();
        }
        checkout
    }

    /// Creates the workspace `name` with two checkouts of `app`, `app` and
    /// `second`, and returns their paths.
    fn two_checkouts(&self, name: &str) -> (PathBuf, PathBuf) {
        let (exit, document) = self.json(&[
            "new", name, "--repo", "app", "--repo", "app", "--as", "second",
        ]);
        assert_eq!(exit, 0, "{document}");
        let workspace = self.root().join("workspaces").join(name);
        (workspace.join("app"), workspace.join("second"))
    }
}

/// How long a test waits to see that a signal had no further effect. A
/// wrong outcome shows at once, so a short wait is enough.
const SIGNAL_SETTLES: Duration = Duration::from_millis(1000);

/// Returns the text of `path` without its trailing line break.
fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .trim_end()
        .to_owned()
}

/// Returns a task section named `name` whose `sh -c` runs `script`.
fn task(name: &str, script: &str) -> String {
    format!("[tasks.{name}]\nargv = [\"sh\", \"-c\", {script:?}, \"task\"]\n")
}

/// A fake mise that lists the tasks `test` and `lint:fix`, with fields that
/// mxv does not read, and records each run in the file `ran`.
const LISTS_TWO: &str = r#"
if [ "$1" = tasks ]; then
  printf '[{"name":"lint:fix","run":[1],"aliases":{}},{"name":"test","depends":null}]'
  exit 0
fi
printf '%s\n' "$* JOBS=$MISE_JOBS PORT=$MXV_PORT_WEB" > ran
"#;

#[test]
fn run_passes_a_declared_tasks_exit_code_through() {
    let setup = Setup::new();
    setup.workspace("triage", Some(&task("check", "exit 7")));
    let (exit, _, _) = setup.run(&["run", "-w", "triage", "check"]);
    assert_eq!(exit, 7);
}

#[test]
fn a_task_that_a_signal_ends_exits_with_128_plus_the_signal() {
    let setup = Setup::new();
    setup.workspace("triage", Some(&task("check", "kill -9 $$")));
    let (exit, _, _) = setup.run(&["run", "-w", "triage", "check"]);
    assert_eq!(exit, 137);
}

#[test]
fn run_adds_the_words_after_the_name_to_the_tasks_argv_and_keeps_their_flags() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", Some(&task("check", "echo \"$@\" > seen")));
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "check", "--one", "two"]);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(read(&checkout.join("seen")), "--one two");
}

#[test]
fn a_declared_task_runs_in_its_dir_with_the_workspaces_port_variables() {
    let setup = Setup::new();
    let manifest = format!(
        "ports = [\"web-ui\"]\n\n{}dir = \"sub\"\n",
        task("check", "echo $MXV_PORT_WEB_UI > seen")
    );
    let checkout = setup.workspace("triage", Some(&manifest));
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "check"]);
    assert_eq!(exit, 0, "{stderr}");
    let port: u16 = read(&checkout.join("sub/seen")).parse().unwrap();
    assert!(port > 0);
}

#[test]
fn a_declared_task_receives_the_variables_of_the_services_it_needs() {
    let setup = Setup::new();
    let manifest = format!(
        "[service.queues]\nkind = \"sqs\"\nqueues.jobs = {{}}\nenv = {{ JOBS_QUEUE = \"set\" }}\n\n{}needs = [\"queues\"]\n\n{}",
        task("needs-it", "echo $JOBS_QUEUE > seen"),
        task("needs-nothing", "echo \"[$JOBS_QUEUE]\" > none"),
    );
    let checkout = setup.workspace("triage", Some(&manifest));
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "needs-it"]);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(read(&checkout.join("seen")), "set");
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "needs-nothing"]);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(read(&checkout.join("none")), "[]");
}

#[test]
fn run_of_a_program_that_does_not_exist_fails_as_unstartable() {
    let setup = Setup::new();
    let manifest = "[tasks.check]\nargv = [\"no-such-program-here\"]\n";
    setup.workspace("triage", Some(manifest));
    let (exit, document) = setup.json(&["run", "-w", "triage", "check"]);
    assert_ne!(exit, 0);
    assert_eq!(
        document["code"], "mxv::cli::usage",
        "--json is refused first"
    );
    let (exit, stdout, stderr) = setup.run(&["run", "-w", "triage", "check"]);
    assert_ne!(exit, 0);
    assert!(
        format!("{stdout}{stderr}").contains("mxv::exec::unstartable"),
        "{stdout}{stderr}"
    );
}

#[test]
fn up_never_starts_a_task() {
    let setup = Setup::new();
    let manifest = format!(
        "[processes.api]\nargv = [\"sleep\", \"60\"]\n\n{}",
        task("only-a-task", "touch ran")
    );
    let checkout = setup.workspace("triage", Some(&manifest));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    assert!(!status.to_string().contains("mxv-task"), "{status}");
    assert!(!status.to_string().contains("only-a-task"), "{status}");
    assert!(!checkout.join("ran").exists());
    let (exit, _) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0);
}

#[test]
fn run_with_no_name_lists_declared_and_mise_tasks() {
    let mut setup = Setup::new();
    setup.fake_mise(LISTS_TWO);
    setup.workspace("triage", Some(&task("build", "true")));
    let (exit, document) = setup.json(&["run", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let text = document.to_string();
    for name in [
        "\"build\"",
        "\"lint:fix\"",
        "\"test\"",
        "\"mise\"",
        "mxv.toml",
    ] {
        assert!(text.contains(name), "{name} is missing from {text}");
    }
}

#[test]
fn an_unknown_task_fails_with_task_unknown_and_lists_the_names() {
    let mut setup = Setup::new();
    setup.fake_mise(LISTS_TWO);
    setup.workspace("triage", Some(&task("build", "true")));
    let (exit, document) = setup.json(&["run", "-w", "triage", "nothing"]);
    assert_ne!(exit, 0);
    assert_eq!(document["code"], "mxv::task::unknown");
    let text = document.to_string();
    assert!(
        text.contains("build") && text.contains("lint:fix"),
        "{text}"
    );
}

#[test]
fn a_mise_task_runs_with_no_manifest_and_gets_one_job_and_its_arguments() {
    let mut setup = Setup::new();
    setup.fake_mise(LISTS_TWO);
    let checkout = setup.workspace("triage", None);
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "lint:fix", "--all"]);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(
        read(&checkout.join("ran")),
        "run lint:fix --all JOBS=1 PORT="
    );
}

#[test]
fn a_declared_task_wins_a_clash_with_a_mise_task_and_says_so() {
    let mut setup = Setup::new();
    setup.fake_mise(LISTS_TWO);
    let checkout = setup.workspace("triage", Some(&task("test", "touch declared")));
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "test"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(checkout.join("declared").exists());
    assert!(!checkout.join("ran").exists());
    assert!(
        stderr.contains("mxv.toml") && stderr.contains("mise"),
        "{stderr}"
    );
}

#[test]
fn when_mise_refuses_a_declared_task_still_runs_and_a_mise_name_is_unknown() {
    let mut setup = Setup::new();
    setup.fake_mise("echo 'config is not trusted, says mise' >&2\nexit 3");
    let checkout = setup.workspace("triage", Some(&task("build", "touch built")));
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "build"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(checkout.join("built").exists());
    assert!(
        stderr.contains("config is not trusted, says mise"),
        "{stderr}"
    );

    let (exit, stdout, stderr) = setup.run(&["run", "-w", "triage", "lint"]);
    assert_ne!(exit, 0);
    assert!(
        format!("{stdout}{stderr}").contains("mxv::task::unknown"),
        "{stdout}{stderr}"
    );
    assert!(
        stderr.contains("config is not trusted, says mise"),
        "{stderr}"
    );
}

#[test]
fn a_machine_without_mise_still_runs_declared_tasks_and_says_nothing_about_mise() {
    let mut setup = Setup::new();
    setup.path = PATH_WITHOUT_MISE.to_owned();
    let (exit, document) = setup.json(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let checkout = setup.root().join("workspaces/triage/app");
    write_file(checkout.join("mxv.toml"), task("build", "touch built")).unwrap();
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "build"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(checkout.join("built").exists());
    assert!(!stderr.contains("mise"), "{stderr}");
}

#[test]
fn a_name_in_several_checkouts_is_refused_and_repo_chooses() {
    let setup = Setup::new();
    let (first, second) = setup.two_checkouts("multi");
    write_file(first.join("mxv.toml"), task("check", "touch first")).unwrap();
    write_file(second.join("mxv.toml"), task("check", "touch second")).unwrap();

    let (exit, document) = setup.json(&["run", "-w", "multi", "check"]);
    assert_ne!(exit, 0);
    assert_eq!(document["code"], "mxv::task::ambiguous");
    let text = document.to_string();
    assert!(text.contains("app") && text.contains("second"), "{text}");
    assert!(!first.join("first").exists());

    let (exit, _, stderr) = setup.run(&["run", "-w", "multi", "--repo", "second", "check"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(second.join("second").exists());
    assert!(!first.join("first").exists());
}

#[test]
fn repo_that_names_no_checkout_fails_with_checkout_not_found() {
    let setup = Setup::new();
    setup.two_checkouts("multi");
    let (exit, document) = setup.json(&["run", "-w", "multi", "--repo", "other", "check"]);
    assert_ne!(exit, 0);
    assert_eq!(document["code"], "mxv::checkout::not_found");
}

#[test]
fn json_is_refused_for_a_task_because_the_task_owns_the_output() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", Some(&task("check", "touch ran")));
    let (exit, document) = setup.json(&["run", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, stdout, _) = setup.run(&["--json", "run", "-w", "triage", "check"]);
    assert_ne!(exit, 0, "{stdout}");
    assert!(stdout.contains("--json is not available"), "{stdout}");
    assert!(!checkout.join("ran").exists());
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test starts mxv as a child so that it can signal it"
)]
fn sigint_to_mxv_alone_leaves_the_task_running_and_mxv_ends_with_its_exit_code() {
    let setup = Setup::new();
    let checkout = setup.workspace(
        "triage",
        Some(&task(
            "check",
            "touch started; while [ ! -e finish ]; do sleep 0.05; done; exit 3",
        )),
    );
    let root = setup.root();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"))
        .args(["run", "-w", "triage", "check"])
        .current_dir(&root)
        .env_clear()
        .env("PATH", PATH)
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("MXV_ROOT", root.join("workspaces"))
        .env("DEADBAND_SOCKET", MISSING_SOCKET)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !checkout.join("started").exists() {
        assert!(started.elapsed() < MOST_WAIT, "the task never started");
        std::thread::yield_now();
    }
    let killed = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    std::thread::sleep(SIGNAL_SETTLES);
    assert!(
        child.try_wait().unwrap().is_none(),
        "mxv ended on SIGINT before the task did"
    );
    write_file(checkout.join("finish"), "").unwrap();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < MOST_WAIT, "mxv never ended");
        std::thread::yield_now();
    };
    assert_eq!(status.code(), Some(3));
}

/// One Ctrl-C typed at a terminal. `script` gives the shell a new pty and
/// copies this test's bytes to it, so the terminal itself turns the byte
/// 0x03 into `SIGINT` for the foreground process group, which holds mxv and
/// the task.
///
/// This misses Linux, whose `script` takes other arguments. A pty opened
/// by the test would replace `script`, and would run on both systems.
#[cfg(target_os = "macos")]
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test starts `script` as a child so that it can type Ctrl-C into its pty"
)]
fn one_ctrl_c_at_a_terminal_reaches_the_task_once_and_nothing_follows_it() {
    use std::io::Write as _;

    let setup = Setup::new();
    let checkout = setup.workspace(
        "triage",
        Some(&task(
            "check",
            "trap 'echo INT >> signals' INT; trap 'echo TERM >> signals' TERM; \
             touch started; while [ ! -e finish ]; do sleep 0.05; done; \
             echo done >> signals; exit 7",
        )),
    );
    let root = setup.root();
    let exit_file = root.join("exit");
    // The shell survives the Ctrl-C through its own trap, so it can record
    // mxv's exit code. A trapped signal resets to its default in mxv.
    let shell = format!(
        "trap : INT; {} run -w triage check; echo $? > {}",
        env!("CARGO_BIN_EXE_mxv"),
        exit_file.display()
    );
    let mut script = std::process::Command::new("/usr/bin/script")
        .args(["-q", "/dev/null", "/bin/sh", "-c", &shell])
        .current_dir(&root)
        .env_clear()
        .env("PATH", PATH)
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("MXV_ROOT", root.join("workspaces"))
        .env("DEADBAND_SOCKET", MISSING_SOCKET)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let begun = Instant::now();
    let wait_for = |what: &str, ready: &dyn Fn() -> bool| {
        while !ready() {
            assert!(begun.elapsed() < MOST_WAIT, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let signals = checkout.join("signals");
    wait_for("the task never started", &|| {
        checkout.join("started").exists()
    });
    let mut typed = script.stdin.take().unwrap();
    typed.write_all(&[0x03]).unwrap();
    typed.flush().unwrap();
    wait_for("the task never saw the Ctrl-C", &|| signals.exists());

    // A cancel makes phase send TERM at once, and KILL after its stop wait,
    // so TERM is the first thing that this wait would show.
    std::thread::sleep(SIGNAL_SETTLES);
    assert_eq!(read(&signals), "INT");
    write_file(checkout.join("finish"), "").unwrap();
    wait_for("mxv never ended", &|| {
        fs::read_to_string(&exit_file).is_ok_and(|text| text.ends_with('\n'))
    });
    drop(typed);
    let _ = script.wait();

    assert_eq!(read(&signals), "INT\ndone");
    assert_eq!(read(&exit_file), "7");
}

/// A task of the checkout `app` and a task of the workspace manifest, in a
/// workspace whose checkouts `app` and `second` declare the ports `web` and
/// `api`.
fn ports_of_two_checkouts(setup: &Setup) -> (PathBuf, PathBuf) {
    let (first, second) = setup.two_checkouts("multi");
    write_file(
        first.join("mxv.toml"),
        format!(
            "ports = [\"web\"]\n\n{}",
            task("own", "echo \"$MXV_PORT_WEB $MXV_PORT_SECOND_API\" > seen")
        ),
    )
    .unwrap();
    write_file(second.join("mxv.toml"), "ports = [\"api\"]\n").unwrap();
    (first, second)
}

#[test]
fn a_task_receives_its_own_checkouts_ports_unprefixed_and_the_others_with_their_prefix() {
    let setup = Setup::new();
    let (first, _) = ports_of_two_checkouts(&setup);

    let (exit, _, stderr) = setup.run(&["run", "-w", "multi", "own"]);
    assert_eq!(exit, 0, "{stderr}");

    let seen = read(&first.join("seen"));
    let numbers: Vec<u16> = seen
        .split_whitespace()
        .map(|word| word.parse().unwrap())
        .collect();
    assert_eq!(numbers.len(), 2, "{seen}");
}

#[test]
fn a_task_of_the_workspace_manifest_receives_each_checkouts_ports_with_its_prefix() {
    let setup = Setup::new();
    ports_of_two_checkouts(&setup);
    let workspace = setup.root().join("workspaces/multi");
    write_file(
        workspace.join("mxv.toml"),
        task(
            "all",
            "echo \"${MXV_PORT_WEB:-none} $MXV_PORT_APP_WEB $MXV_PORT_SECOND_API\" > seen",
        ),
    )
    .unwrap();

    let (exit, _, stderr) = setup.run(&["run", "-w", "multi", "all"]);
    assert_eq!(exit, 0, "{stderr}");

    let seen = read(&workspace.join("seen"));
    let words: Vec<&str> = seen.split_whitespace().collect();
    assert_eq!(words.len(), 3, "{seen}");
    assert_eq!(words[0], "none", "{seen}");
    assert!(words[1].parse::<u16>().is_ok(), "{seen}");
    assert!(words[2].parse::<u16>().is_ok(), "{seen}");
}

#[test]
fn a_task_of_the_workspace_manifest_that_hides_a_checkouts_task_prints_a_note() {
    let setup = Setup::new();
    let (first, _) = setup.two_checkouts("multi");
    write_file(first.join("mxv.toml"), task("check", "touch checkout")).unwrap();
    let workspace = setup.root().join("workspaces/multi");
    write_file(workspace.join("mxv.toml"), task("check", "touch shared")).unwrap();

    let (exit, _, stderr) = setup.run(&["run", "-w", "multi", "check"]);

    assert_eq!(exit, 0, "{stderr}");
    assert!(workspace.join("shared").exists());
    assert!(!first.join("checkout").exists());
    assert!(stderr.contains("workspace manifest"), "{stderr}");
    assert!(stderr.contains("--repo"), "{stderr}");
}

#[test]
fn an_unknown_task_says_so_when_mise_could_not_be_asked() {
    let mut setup = Setup::new();
    setup.path = PATH_WITHOUT_MISE.to_owned();
    let (exit, document) = setup.json(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");

    let (exit, document) = setup.json(&["run", "-w", "triage", "lint"]);

    assert_ne!(exit, 0);
    assert_eq!(document["code"], "mxv::task::unknown");
    assert!(document.to_string().contains("mise"), "{document}");
}

#[test]
fn repo_asks_mise_about_the_chosen_checkout_alone() {
    let mut setup = Setup::new();
    let asked = setup.root().join("asked");
    setup.fake_mise(&format!(
        "pwd >> {}\necho '[{{\"name\":\"test\"}}]'",
        asked.display()
    ));
    let (_, second) = setup.two_checkouts("multi");

    let (exit, document) = setup.json(&["run", "-w", "multi", "--repo", "second"]);
    assert_eq!(exit, 0, "{document}");

    assert_eq!(read(&asked), second.display().to_string());
}

/// Reports whether mise is installed, and says that the test skips when it
/// is not.
fn real_mise_installed(test: &str) -> bool {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test asks whether the mise program can start"
    )]
    let installed = std::process::Command::new("mise")
        .arg("--version")
        .env("PATH", PATH)
        .output()
        .is_ok_and(|output| output.status.success());
    if !installed {
        println!("skipping {test}: mise is not installed");
    }
    installed
}

#[test]
fn a_real_mise_task_runs_with_no_manifest_and_gets_the_workspaces_variables() {
    if !real_mise_installed("a_real_mise_task_runs_with_no_manifest") {
        return;
    }
    let setup = Setup::new();
    let checkout = setup.workspace("triage", None);
    write_file(
        checkout.join("mise.toml"),
        "[tasks.\"lint:all\"]\nrun = 'printf \"jobs=%s arg=%s\\n\" \"$MISE_JOBS\"'\n",
    )
    .unwrap();
    let (exit, stdout, stderr) = setup.run(&["run", "-w", "triage", "lint:all", "word"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(stdout.contains("jobs=1 arg=word"), "{stdout}{stderr}");
}

#[test]
fn a_real_mise_task_gets_the_port_variables_of_a_manifest_that_lists_ports() {
    if !real_mise_installed("a_real_mise_task_gets_the_port_variables") {
        return;
    }
    let setup = Setup::new();
    let checkout = setup.workspace("triage", Some("ports = [\"web\"]\n"));
    write_file(
        checkout.join("mise.toml"),
        "[tasks.show]\nrun = 'echo $MXV_PORT_WEB > seen'\n",
    )
    .unwrap();
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "show"]);
    assert_eq!(exit, 0, "{stderr}");
    let port: u16 = read(&checkout.join("seen")).parse().unwrap();
    assert!(port > 0);
}

#[test]
fn a_declared_task_wins_a_clash_with_a_real_mise_task_and_says_so() {
    if !real_mise_installed("a_declared_task_wins_a_clash_with_a_real_mise_task") {
        return;
    }
    let setup = Setup::new();
    let checkout = setup.workspace("triage", Some(&task("test", "touch declared")));
    write_file(
        checkout.join("mise.toml"),
        "[tasks.test]\nrun = 'touch mise'\n",
    )
    .unwrap();
    let (exit, _, stderr) = setup.run(&["run", "-w", "triage", "test"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(checkout.join("declared").exists());
    assert!(!checkout.join("mise").exists());
    assert!(stderr.contains("mxv.toml"), "{stderr}");
}

#[test]
fn a_real_mise_refusal_of_an_untrusted_config_is_shown_and_the_task_stays_unknown() {
    if !real_mise_installed("a_real_mise_refusal_of_an_untrusted_config") {
        return;
    }
    let setup = Setup::new();
    let checkout = setup.workspace("triage", None);
    write_file(
        checkout.join("mise.toml"),
        "[env]\nA = \"1\"\n\n[tasks.show]\nrun = 'touch ran'\n",
    )
    .unwrap();
    let (exit, stdout, stderr) = setup.run(&["run", "-w", "triage", "show"]);
    assert_ne!(exit, 0, "{stdout}");
    assert!(stderr.contains("trust"), "{stderr}");
    assert!(!checkout.join("ran").exists());
}
