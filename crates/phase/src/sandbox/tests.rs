//! The sandbox's tests. The unit tests build specs and capability sets from
//! plans. The tests of a real sandbox start this test binary as the
//! launcher: its arguments run only the test
//! [`the_test_binary_runs_the_launcher_when_it_is_started_as_one`], which
//! finds `--sandboxed` among them and runs the launcher as the program that
//! links phase would.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::num::NonZeroU16;
use std::path::PathBuf;

use tempfile::TempDir;

use super::{
    AppDirs, Context, Launcher, Platform, SandboxError, SandboxSpec, Sandboxing, UNDECLARED,
    launch, launcher_command, sandboxing, spec,
};
use crate::plan::{AppPlan, Permissions, ProcessPlan, RunnerKind};
use crate::record::testing::{app, workspace};
use crate::source::Source;

/// The full name of the test that runs the launcher.
const ENTRY: &str = "sandbox::tests::the_test_binary_runs_the_launcher_when_it_is_started_as_one";

#[test]
fn the_test_binary_runs_the_launcher_when_it_is_started_as_one() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let Some(at) = args.iter().position(|arg| arg == "--sandboxed") else {
        return;
    };
    // `launch` returns only when it fails, and then the process must end
    // with its status, which only an exit can give a test.
    if let Some(code) = launch(args.get(at..).unwrap()) {
        let code = if code == std::process::ExitCode::from(super::SANDBOX_REFUSED) {
            i32::from(super::SANDBOX_REFUSED)
        } else {
            1
        };
        #[expect(
            clippy::disallowed_methods,
            reason = "the test binary runs as the launcher, which must end with the launcher's status"
        )]
        std::process::exit(code);
    }
}

/// A workspace on disk for one test: a checkout, phase's state directory and
/// a home directory with `~/.ssh` and Cargo's directories. It lies beside
/// this test binary, because macOS's per-user temporary directory, where
/// `tempfile` puts directories by default, is protected.
struct Fixture {
    /// The directory that holds everything, removed when the test ends.
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let beside = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let root = tempfile::Builder::new()
            .prefix("sandbox-")
            .tempdir_in(beside)
            .unwrap();
        let fixture = Self { root };
        for dir in [
            fixture.checkout(),
            fixture.state(),
            fixture.home().join(".ssh"),
            fixture.home().join(".cargo/bin"),
            fixture.home().join(".cargo/registry"),
            fixture.home().join(".rustup"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes the files that the sandbox reads or refuses"
        )]
        {
            std::fs::write(fixture.checkout().join("README"), "readable").unwrap();
            std::fs::write(fixture.home().join(".cargo/credentials.toml"), "test").unwrap();
            std::fs::write(fixture.workspace_dir().join("mxv.toml"), "").unwrap();
        }
        fixture
    }

    /// The workspace directory, which holds the checkout and the manifest.
    fn workspace_dir(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap().join("ws")
    }

    fn checkout(&self) -> PathBuf {
        self.workspace_dir().join("api")
    }

    fn state(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap().join("state")
    }

    fn home(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap().join("home")
    }

    fn outside(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap().join("outside")
    }

    /// Returns the host app `api` of the checkout, with one process that
    /// runs `run` and the port `http` on `port`, which it may listen on.
    fn app(&self, run: Vec<String>, port: u16) -> AppPlan {
        let mut api = AppPlan::new(
            app("api"),
            RunnerKind::Host,
            Source::checkout(self.checkout(), None),
        );
        api.ports
            .insert("http".parse().unwrap(), NonZeroU16::new(port).unwrap());
        let mut permissions = Permissions::default();
        permissions.listen.insert("http".parse().unwrap());
        api.permissions = Some(permissions);
        api.processes
            .push(ProcessPlan::new("server".parse().unwrap(), run));
        api
    }

    /// Returns the spec of the first process of `api`, with `PATH` set to
    /// `path` and `connect` as the ports of its dependencies.
    fn spec_of(
        &self,
        api: &AppPlan,
        path: &str,
        connect: BTreeSet<u16>,
    ) -> Result<SandboxSpec, SandboxError> {
        let home = self.home();
        let id = workspace();
        let state = self.state();
        let context = Context {
            state_dir: &state,
            workspace: &id,
            path: Some(path),
            home: Some(&home),
            protected: &[],
            connect,
            launcher: test_launcher(),
        };
        spec(
            api,
            &api.processes[0],
            api.permissions.as_ref().unwrap(),
            &context,
        )
    }
}

/// Returns the launcher that runs this test binary as the launcher.
fn test_launcher() -> Launcher {
    Launcher {
        program: std::env::current_exe().unwrap(),
        words: vec![
            "--exact".into(),
            "--nocapture".into(),
            "--".into(),
            ENTRY.into(),
        ],
    }
}

fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

#[test]
fn an_app_without_permissions_runs_unsandboxed_and_says_why() {
    let fixture = Fixture::new();
    let mut api = fixture.app(sh("true"), 41_000);
    api.permissions = None;

    assert_eq!(
        sandboxing(&api, &Platform::Available(Sandboxing::Seatbelt)),
        Some(Sandboxing::Unsandboxed {
            reason: UNDECLARED.to_owned()
        })
    );
}

#[test]
fn an_app_with_permissions_runs_unsandboxed_on_a_platform_without_a_sandbox_and_says_why() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let platform = Platform::Unavailable("the kernel offers no Landlock".to_owned());

    assert_eq!(
        sandboxing(&api, &platform),
        Some(Sandboxing::Unsandboxed {
            reason: "the kernel offers no Landlock".to_owned()
        })
    );
    assert_eq!(
        sandboxing(&api, &Platform::Available(Sandboxing::Landlock)),
        Some(Sandboxing::Landlock)
    );
}

#[test]
fn a_container_app_has_no_sandbox_of_phase() {
    let db = AppPlan::new(
        app("db"),
        RunnerKind::Container,
        Source::Image {
            image: "postgres:17".parse().unwrap(),
        },
    );

    assert_eq!(
        sandboxing(&db, &Platform::Available(Sandboxing::Seatbelt)),
        None
    );
}

#[test]
fn unsandboxed_status_says_why_in_json() {
    let row = Sandboxing::Unsandboxed {
        reason: UNDECLARED.to_owned(),
    };

    assert_eq!(
        serde_json::to_value(&row).unwrap(),
        serde_json::json!({ "kind": "unsandboxed", "reason": UNDECLARED })
    );
}

#[test]
fn a_spec_reads_the_checkout_and_writes_only_the_apps_own_directories() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);

    let spec = fixture
        .spec_of(&api, "/usr/bin:/bin", BTreeSet::new())
        .unwrap();

    let dirs = AppDirs::of(&fixture.state(), &workspace(), &app("api"));
    assert!(spec.read.contains(&fixture.checkout()));
    assert_eq!(
        spec.write,
        BTreeSet::from([dirs.state.clone(), dirs.tmp.clone()])
    );
    assert_eq!(spec.tmp, dirs.tmp);
    assert!(dirs.state.is_dir() && dirs.tmp.is_dir());
    assert!(!spec.read.contains(&fixture.workspace_dir()));
    assert!(spec.deny.contains(&fixture.home().join(".ssh")));
}

#[test]
fn a_spec_writes_the_state_directory_and_paths_inside_the_checkout_that_the_permissions_name() {
    let fixture = Fixture::new();
    let mut api = fixture.app(sh("true"), 41_000);
    api.permissions.as_mut().unwrap().write = ["$STATE".to_owned(), "target".to_owned()].into();

    let spec = fixture
        .spec_of(&api, "/usr/bin:/bin", BTreeSet::new())
        .unwrap();

    assert!(spec.write.contains(&fixture.checkout().join("target")));
    assert_eq!(spec.write.len(), 3);
}

#[test]
fn a_spec_listens_on_the_apps_ports_and_connects_only_to_its_dependencies_ports() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);

    let spec = fixture
        .spec_of(&api, "/usr/bin:/bin", BTreeSet::from([41_005]))
        .unwrap();

    assert_eq!(spec.listen, BTreeSet::from([41_000]));
    assert_eq!(spec.connect, BTreeSet::from([41_005]));
}

#[test]
fn a_spec_reads_cargos_programs_and_crates_but_never_its_credentials() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let path = format!("{}:/usr/bin", fixture.home().join(".cargo/bin").display());

    let spec = fixture.spec_of(&api, &path, BTreeSet::new()).unwrap();

    let cargo = fixture.home().join(".cargo");
    assert!(spec.read.contains(&cargo.join("bin")));
    assert!(spec.read.contains(&cargo.join("registry")));
    assert!(spec.read.contains(&fixture.home().join(".rustup")));
    assert!(!spec.read.iter().any(|path| cargo.starts_with(path)));
}

#[test]
fn a_spec_ignores_a_path_entry_that_would_reach_a_protected_path() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let path = format!("{}:/usr/bin", fixture.home().display());

    let spec = fixture.spec_of(&api, &path, BTreeSet::new()).unwrap();

    assert!(!spec.read.contains(&fixture.home()));
}

#[test]
fn a_spec_refuses_a_write_that_reaches_the_ssh_directory() {
    let fixture = Fixture::new();
    let mut api = fixture.app(sh("true"), 41_000);
    api.permissions.as_mut().unwrap().write = [fixture.home().display().to_string()].into();

    assert_eq!(
        fixture.spec_of(&api, "/usr/bin", BTreeSet::new()),
        Err(SandboxError::Protected {
            allowed: fixture.home(),
            protected: fixture.home().join(".ssh"),
        })
    );
}

#[test]
fn a_spec_refuses_a_write_of_the_workspace_outside_the_checkout() {
    let fixture = Fixture::new();
    let mut api = fixture.app(sh("true"), 41_000);
    api.permissions.as_mut().unwrap().write =
        [fixture.workspace_dir().display().to_string()].into();

    assert!(matches!(
        fixture.spec_of(&api, "/usr/bin", BTreeSet::new()),
        Err(SandboxError::Protected { protected, .. }) if protected == fixture.workspace_dir()
    ));
}

#[test]
fn a_spec_refuses_a_write_of_phases_state_directory() {
    let fixture = Fixture::new();
    let mut api = fixture.app(sh("true"), 41_000);
    api.permissions.as_mut().unwrap().write =
        [fixture.state().join("runs").display().to_string()].into();

    assert!(matches!(
        fixture.spec_of(&api, "/usr/bin", BTreeSet::new()),
        Err(SandboxError::Protected { protected, .. }) if protected == fixture.state()
    ));
}

#[test]
fn a_spec_refuses_an_app_that_runs_from_a_release() {
    let fixture = Fixture::new();
    let mut api = fixture.app(sh("true"), 41_000);
    api.source = Source::release(
        "v1.0.0".parse().unwrap(),
        crate::source::ReleaseSpec::new(
            "crumbkit/example-api".parse().unwrap(),
            "api-{target}.tar.gz".to_owned(),
            PathBuf::from("api"),
            crate::source::Verification::Sha256,
        ),
    );

    assert_eq!(
        fixture.spec_of(&api, "/usr/bin", BTreeSet::new()),
        Err(SandboxError::Release)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_spec_refuses_app_directories_inside_the_per_user_temporary_directory() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let state = tempfile::tempdir().unwrap();
    let state = state.path().canonicalize().unwrap();
    let home = fixture.home();
    let id = workspace();
    let context = Context {
        state_dir: &state,
        workspace: &id,
        path: Some("/usr/bin"),
        home: Some(&home),
        protected: &[],
        connect: BTreeSet::new(),
        launcher: test_launcher(),
    };

    assert!(matches!(
        spec(&api, &api.processes[0], api.permissions.as_ref().unwrap(), &context),
        Err(SandboxError::Protected { protected, .. }) if protected == std::path::Path::new("/private/var/folders")
    ));
}

#[test]
fn the_launcher_runs_only_when_its_flag_comes_first() {
    assert_eq!(launch(&["--workspace".into(), "x".into()]), None);
    assert_eq!(launch(&[]), None);
}

#[test]
fn a_launcher_that_cannot_read_its_sandbox_refuses_to_start_the_app() {
    let code = launch(&[
        "--sandboxed".into(),
        "{}".into(),
        "--".into(),
        "/bin/true".into(),
    ]);

    assert_eq!(
        code,
        Some(std::process::ExitCode::from(super::SANDBOX_REFUSED))
    );
}

#[test]
fn the_launcher_command_passes_the_sandbox_and_then_the_apps_command() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

    let command = launcher_command(&spec, &sh("true")).unwrap();

    let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
    assert_eq!(command.get_program(), spec.launcher.program.as_os_str());
    assert_eq!(args[4], "--sandboxed");
    let read: SandboxSpec = serde_json::from_str(args[5].to_str().unwrap()).unwrap();
    assert_eq!(read, spec);
    assert_eq!(&args[6..], ["--", "/bin/sh", "-c", "true"]);
}

#[cfg(target_os = "macos")]
mod seatbelt {
    use std::collections::BTreeSet;
    use std::io::Read as _;
    use std::net::TcpListener;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use nono::NetworkMode;

    use super::{Fixture, sh};
    use crate::sandbox::capabilities::seatbelt_rules;
    use crate::sandbox::{capabilities, launcher_command};

    #[test]
    fn the_capability_set_blocks_the_network_and_dns_and_reads_the_checkout() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let spec = fixture
            .spec_of(&api, "/usr/bin", BTreeSet::from([41_005]))
            .unwrap();

        let caps = capabilities(&spec).unwrap();

        assert_eq!(caps.network_mode(), &NetworkMode::Blocked);
        assert!(!caps.dns_enabled());
        assert!(caps.localhost_ports().is_empty());
        assert!(
            caps.fs_capabilities()
                .iter()
                .any(|cap| cap.resolved == fixture.checkout()
                    && cap.access == nono::AccessMode::Read)
        );
        assert_eq!(caps.platform_rules(), seatbelt_rules(&spec).as_slice());
    }

    #[test]
    fn every_network_rule_of_phase_starts_so_that_nono_writes_it_after_its_network_deny() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let spec = fixture
            .spec_of(&api, "/usr/bin", BTreeSet::from([41_005]))
            .unwrap();

        let rules = seatbelt_rules(&spec);

        let network: Vec<&String> = rules
            .iter()
            .filter(|rule| rule.contains("network"))
            .collect();
        assert_eq!(
            network,
            [
                "(allow network-bind (local tcp \"localhost:41000\"))",
                "(allow network-inbound (local tcp \"localhost:41000\"))",
                "(allow network-outbound (remote tcp \"localhost:41005\"))",
            ]
        );
        assert!(
            network
                .iter()
                .all(|rule| rule.starts_with("(allow network"))
        );
    }

    #[test]
    fn the_profile_denies_the_workspace_and_then_allows_the_checkout_inside_it_again() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

        let rules = seatbelt_rules(&spec);

        let deny = rules
            .iter()
            .position(|rule| {
                *rule
                    == format!(
                        "(deny file-read-data file-write* (subpath \"{}\"))",
                        fixture.workspace_dir().display()
                    )
            })
            .unwrap();
        let allow = rules
            .iter()
            .position(|rule| {
                *rule
                    == format!(
                        "(allow file-read-data (subpath \"{}\"))",
                        fixture.checkout().display()
                    )
            })
            .unwrap();
        assert!(deny < allow);
    }

    /// Returns the per-user temporary directory, which holds the socket of
    /// starmap's agent.
    fn user_temp_dir() -> PathBuf {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test asks getconf for the per-user temporary directory"
        )]
        let output = Command::new("/usr/bin/getconf")
            .arg("DARWIN_USER_TEMP_DIR")
            .output()
            .unwrap();
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    }

    /// Runs `script` with `/bin/sh` under the sandbox of the fixture's app,
    /// which may listen on `port` and connect to `connect`, and returns its
    /// standard output and exit status.
    fn sandboxed(fixture: &Fixture, script: &str, port: u16, connect: u16) -> (String, i32) {
        let api = fixture.app(sh(script), port);
        let spec = fixture
            .spec_of(&api, "/usr/bin:/bin", BTreeSet::from([connect]))
            .unwrap();
        let mut command = launcher_command(&spec, &sh(script)).unwrap();
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", &spec.tmp)
            .current_dir(fixture.checkout())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts this test binary as the launcher"
        )]
        let mut child = command.spawn().unwrap();
        let mut output = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        let status = child.wait().unwrap();
        (output, status.code().unwrap_or(-1))
    }

    /// Returns the value of `case` in `output`, a line `case=value`.
    fn outcome<'a>(output: &'a str, case: &str) -> &'a str {
        output
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{case}=")))
            .unwrap_or_else(|| panic!("no {case} in {output}"))
    }

    #[test]
    fn a_sandboxed_app_reads_its_checkout_writes_its_state_and_is_denied_everything_else() {
        let fixture = Fixture::new();
        let dependency = TcpListener::bind("127.0.0.1:0").unwrap();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let own = free.local_addr().unwrap().port();
        drop(free);
        let agent_dir = tempfile::Builder::new()
            .prefix("phase-agent-")
            .tempdir_in(user_temp_dir())
            .unwrap();
        let socket = agent_dir.path().join("agent.sock");
        let _agent = UnixListener::bind(&socket).unwrap();
        std::fs::create_dir_all(fixture.outside()).unwrap();
        let script = format!(
            r#"
            check() {{ if "$@" >/dev/null 2>&1; then echo "$case=allowed"; else echo "$case=denied"; fi; }}
            case=read-checkout check cat README
            case=write-state check sh -c 'echo x > "$TMPDIR/../state/file"'
            case=write-tmp check sh -c 'echo x > "$TMPDIR/file"'
            case=write-checkout check sh -c 'echo x > README.new'
            case=write-outside check sh -c 'echo x > "{outside}/file"'
            case=read-manifest check cat ../mxv.toml
            case=read-ssh check ls "{home}/.ssh"
            case=read-credentials check cat "{home}/.cargo/credentials.toml"
            case=agent-socket check /usr/bin/perl -MIO::Socket::UNIX -e 'IO::Socket::UNIX->new(Peer => shift) or exit 1' "{socket}"
            case=perl-runs check /usr/bin/perl -e 1
            case=connect-dependency check nc -z -w 2 127.0.0.1 {dependency}
            case=connect-dependency-by-name check nc -z -w 2 localhost {dependency}
            case=connect-other check nc -z -w 2 127.0.0.1 {other}
            case=listen-other check nc -l 127.0.0.1 {other_port}
            nc -l 127.0.0.1 {own} 2>/dev/null & listener=$!
            sleep 1
            if kill -0 $listener 2>/dev/null; then echo listen-own=allowed; kill $listener; else echo listen-own=denied; fi
            "#,
            outside = fixture.outside().display(),
            home = fixture.home().display(),
            socket = socket.display(),
            dependency = dependency.local_addr().unwrap().port(),
            other = other.local_addr().unwrap().port(),
            other_port = own + 1,
        );

        let (output, _) = sandboxed(
            &fixture,
            &script,
            own,
            dependency.local_addr().unwrap().port(),
        );

        for (case, expected) in [
            ("read-checkout", "allowed"),
            ("write-state", "allowed"),
            ("write-tmp", "allowed"),
            ("write-checkout", "denied"),
            ("write-outside", "denied"),
            ("read-manifest", "denied"),
            ("read-ssh", "denied"),
            ("read-credentials", "denied"),
            ("perl-runs", "allowed"),
            ("agent-socket", "denied"),
            ("connect-dependency", "allowed"),
            ("connect-other", "denied"),
            ("listen-other", "denied"),
            ("listen-own", "allowed"),
        ] {
            assert_eq!(outcome(&output, case), expected, "{case} in {output}");
        }
        assert!(!fixture.outside().join("file").exists());
        // DNS is blocked, so whether `localhost` resolves depends on the
        // resolver reading /etc/hosts itself, which the eval records.
        println!(
            "connect-dependency-by-name={}",
            outcome(&output, "connect-dependency-by-name")
        );
    }

    #[test]
    fn the_agent_socket_answers_a_process_outside_the_sandbox() {
        let agent_dir = tempfile::Builder::new()
            .prefix("phase-agent-")
            .tempdir_in(user_temp_dir())
            .unwrap();
        let socket = agent_dir.path().join("agent.sock");
        let _agent = UnixListener::bind(&socket).unwrap();

        #[expect(
            clippy::disallowed_methods,
            reason = "the test connects to the socket without a sandbox, as the control of the sandboxed case"
        )]
        let status = Command::new("/usr/bin/perl")
            .args([
                "-MIO::Socket::UNIX",
                "-e",
                "IO::Socket::UNIX->new(Peer => shift) or exit 1",
            ])
            .arg(&socket)
            .status()
            .unwrap();

        assert!(status.success());
    }

    #[test]
    fn a_launcher_that_cannot_find_the_program_writes_why_and_exits_with_its_status() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();
        let mut command = launcher_command(&spec, &["/nonexistent/program".to_owned()]).unwrap();
        command.stdout(Stdio::null()).stderr(Stdio::piped());

        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts this test binary as the launcher"
        )]
        let output = command.output().unwrap();

        assert_eq!(
            output.status.code(),
            Some(i32::from(crate::sandbox::SANDBOX_REFUSED))
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot start under the sandbox"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(target_os = "linux")]
mod landlock {
    use std::collections::BTreeSet;
    use std::io::Read as _;
    use std::net::TcpListener;
    use std::process::Stdio;

    use super::{Fixture, sh};
    use crate::sandbox::{Platform, launcher_command, platform};

    #[test]
    fn a_sandboxed_app_reads_its_checkout_and_is_denied_a_write_outside_and_other_ports() {
        if !matches!(platform(), Platform::Available(_)) {
            return;
        }
        let fixture = Fixture::new();
        let dependency = TcpListener::bind("127.0.0.1:0").unwrap();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        std::fs::create_dir_all(fixture.outside()).unwrap();
        let script = format!(
            r#"
            check() {{ if "$@" >/dev/null 2>&1; then echo "$case=allowed"; else echo "$case=denied"; fi; }}
            case=read-checkout check cat README
            case=write-tmp check sh -c 'echo x > "$TMPDIR/file"'
            case=write-outside check sh -c 'echo x > "{outside}/file"'
            case=read-ssh check ls "{home}/.ssh"
            case=connect-dependency check bash -c 'exec 3<>/dev/tcp/127.0.0.1/{dependency}'
            case=connect-other check bash -c 'exec 3<>/dev/tcp/127.0.0.1/{other}'
            "#,
            outside = fixture.outside().display(),
            home = fixture.home().display(),
            dependency = dependency.local_addr().unwrap().port(),
            other = other.local_addr().unwrap().port(),
        );
        let api = fixture.app(sh(&script), 41_000);
        let spec = fixture
            .spec_of(
                &api,
                "/usr/bin:/bin",
                BTreeSet::from([dependency.local_addr().unwrap().port()]),
            )
            .unwrap();
        let mut command = launcher_command(&spec, &sh(&script)).unwrap();
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", &spec.tmp)
            .current_dir(fixture.checkout())
            .stdout(Stdio::piped());
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts this test binary as the launcher"
        )]
        let mut child = command.spawn().unwrap();
        let mut output = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        child.wait().unwrap();

        for (case, expected) in [
            ("read-checkout", "allowed"),
            ("write-tmp", "allowed"),
            ("write-outside", "denied"),
            ("read-ssh", "denied"),
            ("connect-dependency", "allowed"),
            ("connect-other", "denied"),
        ] {
            let line = format!("{case}={expected}");
            assert!(
                output.lines().any(|seen| seen == line),
                "{line} in {output}"
            );
        }
    }
}
