//! The sandbox's tests. The unit tests build specs and capability sets from
//! plans. The tests of a real sandbox start this test binary as the
//! launcher: its arguments run only the test
//! [`the_test_binary_runs_the_launcher_when_it_is_started_as_one`], which
//! finds `--sandboxed` among them and runs the launcher as the program that
//! links phase would.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::{
    AppDirs, Context, LANDLOCK_UNSANDBOXED, Launcher, Platform, SandboxError, SandboxSpec,
    Sandboxing, UNDECLARED, WorkspacePaths, add_toolchain, developer_dir, homebrew_deny,
    homebrew_read, launch, launcher_command, sandboxing, spec, toolchain, usr_without_local_data,
    verify,
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
/// a home directory with `~/.ssh` and Cargo's directories. It lies in the
/// build's target directory, because macOS's per-user temporary directory,
/// where `tempfile` puts directories by default, is protected, and the
/// directory of this test binary, which runs as the launcher, may only be
/// read.
struct Fixture {
    /// The directory that holds everything, removed when the test ends.
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let target = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("sandbox-fixtures");
        std::fs::create_dir_all(&target).unwrap();
        let root = tempfile::Builder::new()
            .prefix("sandbox-")
            .tempdir_in(target)
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

    /// Returns the workspace's own directory and its protected manifest,
    /// as mxv gives them.
    fn workspace_paths(&self) -> WorkspacePaths {
        WorkspacePaths {
            dir: Some(self.workspace_dir()),
            protected: vec![self.workspace_dir().join("mxv.toml")],
        }
    }

    /// Returns the spec of the first process of `api`, with `PATH` set to
    /// `path` and `connect` as the ports of its dependencies.
    fn spec_of(
        &self,
        api: &AppPlan,
        path: &str,
        connect: BTreeSet<u16>,
    ) -> Result<SandboxSpec, SandboxError> {
        self.spec_with(api, path, connect, test_launcher(), &[])
    }

    /// Returns the spec of the first process of `api` as [`Fixture::spec_of`]
    /// does, with `launcher` as the launcher and `protected` as the host
    /// program's protected paths.
    fn spec_with(
        &self,
        api: &AppPlan,
        path: &str,
        connect: BTreeSet<u16>,
        launcher: Launcher,
        protected: &[PathBuf],
    ) -> Result<SandboxSpec, SandboxError> {
        let home = self.home();
        let id = workspace();
        let state = self.state();
        let paths = self.workspace_paths();
        let context = Context {
            state_dir: &state,
            workspace: &id,
            workspace_paths: &paths,
            path: Some(path),
            home: Some(&home),
            protected,
            connect,
            launcher,
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
        sandboxing(&api, &Platform::Available(Sandboxing::seatbelt())),
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
        sandboxing(&api, &Platform::Available(Sandboxing::landlock())),
        Some(Sandboxing::landlock())
    );
}

#[test]
fn linux_reports_a_process_under_landlock_as_unsandboxed_and_says_why() {
    let landlock = Sandboxing::landlock();

    assert_eq!(
        landlock.to_string(),
        format!("unsandboxed ({LANDLOCK_UNSANDBOXED})")
    );
    assert!(LANDLOCK_UNSANDBOXED.contains("Unix sockets"));
    assert!(LANDLOCK_UNSANDBOXED.contains("Docker or D-Bus"));
    assert_eq!(
        serde_json::to_value(&landlock).unwrap(),
        serde_json::json!({ "kind": "unsandboxed", "reason": LANDLOCK_UNSANDBOXED })
    );
    assert_eq!(
        Sandboxing::seatbelt().to_string(),
        "seatbelt (partial: the sandbox cannot limit its listening ports to the loopback interface)"
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
        sandboxing(&db, &Platform::Available(Sandboxing::seatbelt())),
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
fn a_spec_reads_no_terminal_device_and_none_of_dev_but_the_nodes_toolchains_use() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);

    let spec = fixture
        .spec_of(&api, "/usr/bin:/bin", BTreeSet::new())
        .unwrap();

    let dev: Vec<&PathBuf> = spec
        .read
        .iter()
        .filter(|path| path.starts_with("/dev"))
        .collect();
    assert!(!dev.is_empty());
    for path in dev {
        assert!(
            [
                "/dev/null",
                "/dev/zero",
                "/dev/random",
                "/dev/urandom",
                "/dev/fd",
                "/dev/dtracehelper"
            ]
            .iter()
            .any(|node| path == Path::new(node)),
            "{}",
            path.display()
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn the_profile_denies_every_terminal_device_but_the_processs_own() {
    use crate::sandbox::seatbelt::{TERMINAL_DEVICES, rules};

    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let spec = fixture
        .spec_of(&api, "/usr/bin:/bin", BTreeSet::new())
        .unwrap();
    let pattern = regex::Regex::new(TERMINAL_DEVICES).unwrap();

    assert!(rules(&spec).contains(&format!(
        "(deny file-read* file-write* file-ioctl (regex #\"{TERMINAL_DEVICES}\"))"
    )));
    for device in ["/dev/ttys004", "/dev/ttys1000", "/dev/ptyp0", "/dev/ttyp0"] {
        assert!(pattern.is_match(device), "{device}");
    }
    for device in [
        "/dev/tty",
        "/dev/ptmx",
        "/dev/null",
        "/dev/ttys",
        "/dev/ttys004x",
    ] {
        assert!(!pattern.is_match(device), "{device}");
    }
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
        Err(SandboxError::Protected { protected, .. }) if protected.starts_with(fixture.workspace_dir())
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
    let paths = fixture.workspace_paths();
    let context = Context {
        state_dir: &state,
        workspace: &id,
        workspace_paths: &paths,
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

/// Returns the spec of `api` with `write` as its write entries.
fn writing(fixture: &Fixture, write: &[String], path: &str) -> Result<SandboxSpec, SandboxError> {
    let mut api = fixture.app(sh("true"), 41_000);
    api.permissions.as_mut().unwrap().write = write.iter().cloned().collect();
    fixture.spec_of(&api, path, BTreeSet::new())
}

#[test]
fn a_spec_refuses_a_write_that_covers_the_checkouts_git_directory() {
    let fixture = Fixture::new();

    assert_eq!(
        writing(
            &fixture,
            &[fixture.checkout().display().to_string()],
            "/usr/bin"
        ),
        Err(SandboxError::ReadOnly {
            allowed: fixture.checkout(),
            read_only: fixture.checkout().join(".git"),
        })
    );
}

#[test]
fn a_spec_refuses_a_write_of_the_launcher_programs_directory() {
    let fixture = Fixture::new();
    let launcher = std::env::current_exe().unwrap().canonicalize().unwrap();
    let dir = launcher.parent().unwrap().to_path_buf();

    assert!(matches!(
        writing(&fixture, &[dir.display().to_string()], "/usr/bin"),
        Err(SandboxError::ReadOnly { read_only, .. }) if read_only == dir || read_only == launcher
    ));
}

#[test]
fn a_spec_refuses_a_write_of_a_path_directory_or_a_toolchain_directory() {
    let fixture = Fixture::new();
    let bin = fixture.outside().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let registry = fixture.home().join(".cargo/registry");
    let path = format!(
        "{}:{}:/usr/bin",
        bin.display(),
        fixture.home().join(".cargo/bin").display()
    );

    for dir in [&bin, &registry] {
        assert_eq!(
            writing(&fixture, &[dir.display().to_string()], &path),
            Err(SandboxError::ReadOnly {
                allowed: dir.clone(),
                read_only: dir.clone(),
            }),
            "{}",
            dir.display()
        );
    }
}

#[test]
fn a_spec_refuses_a_write_of_a_credential_directory_in_the_home_directory() {
    let fixture = Fixture::new();

    for name in [
        ".aws",
        ".config/gh",
        ".docker",
        ".gnupg",
        ".claude",
        ".codex",
    ] {
        let dir = fixture.home().join(name);
        assert_eq!(
            writing(&fixture, &[dir.display().to_string()], "/usr/bin"),
            Err(SandboxError::Protected {
                allowed: dir.clone(),
                protected: dir.clone(),
            }),
            "{name}"
        );
    }
}

#[test]
fn a_spec_denies_the_home_directorys_credentials_and_startup_files() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);

    let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

    for name in [
        ".ssh",
        ".aws",
        ".netrc",
        ".git-credentials",
        ".cargo/credentials.toml",
        ".config/mise",
        ".zshrc",
        ".bashrc",
        ".profile",
        ".gitconfig",
        ".config/git",
        ".cargo/config.toml",
        ".npmrc",
        ".pypirc",
        ".kube",
        ".config/gcloud",
        ".azure",
    ] {
        assert!(spec.deny.contains(&fixture.home().join(name)), "{name}");
    }
}

#[test]
fn a_spec_refuses_an_app_whose_checkout_is_the_workspaces_own_directory() {
    let fixture = Fixture::new();
    let mut api = fixture.app(sh("true"), 41_000);
    api.source = Source::checkout(fixture.workspace_dir(), None);

    assert_eq!(
        fixture.spec_of(&api, "/usr/bin", BTreeSet::new()),
        Err(SandboxError::WorkspaceRoot(fixture.workspace_dir()))
    );
}

#[test]
fn a_spec_denies_the_workspace_manifest_that_the_host_program_protects() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);

    let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

    assert!(
        spec.deny
            .contains(&fixture.workspace_dir().join("mxv.toml"))
    );
    assert!(spec.deny.contains(&fixture.workspace_dir()));
}

#[test]
fn a_spec_sets_the_temporary_directory_readable_only_by_the_user_when_it_exists_already() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let dirs = AppDirs::of(&fixture.state(), &workspace(), &app("api"));
    std::fs::create_dir_all(&dirs.tmp).unwrap();
    std::fs::set_permissions(&dirs.tmp, std::fs::Permissions::from_mode(0o755)).unwrap();

    fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

    let mode = std::fs::metadata(&dirs.tmp).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn the_launchers_check_creates_a_write_directory_that_does_not_exist_yet() {
    let fixture = Fixture::new();
    let spec = writing(&fixture, &["target".to_owned()], "/usr/bin").unwrap();

    verify(&spec).unwrap();

    assert!(fixture.checkout().join("target").is_dir());
}

#[test]
fn the_launchers_check_refuses_a_write_directory_that_a_symbolic_link_moved_elsewhere() {
    let fixture = Fixture::new();
    let spec = writing(&fixture, &["target".to_owned()], "/usr/bin").unwrap();
    std::fs::create_dir_all(fixture.home().join(".ssh")).unwrap();
    std::os::unix::fs::symlink(
        fixture.home().join(".ssh"),
        fixture.checkout().join("target"),
    )
    .unwrap();

    assert!(
        matches!(
            verify(&spec),
            Err(SandboxError::Unusable { path, .. }) if path == fixture.checkout().join("target")
        ),
        "{:?}",
        verify(&spec)
    );
}

#[test]
fn the_launchers_check_refuses_a_spec_whose_write_reaches_a_protected_path() {
    let fixture = Fixture::new();
    let mut spec = writing(&fixture, &[], "/usr/bin").unwrap();
    spec.write.insert(fixture.home());

    assert_eq!(
        verify(&spec),
        Err(SandboxError::Protected {
            allowed: fixture.home(),
            protected: fixture.home().join(".ssh"),
        })
    );
}

#[test]
fn a_homebrew_path_entry_lets_the_app_read_its_programs_but_never_its_data_or_configuration() {
    let dirs = toolchain(std::path::Path::new("/opt/homebrew/bin"), None);

    assert!(dirs.contains(&PathBuf::from("/opt/homebrew/Cellar")));
    assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
    assert!(
        !dirs
            .iter()
            .any(|dir| dir.ends_with("var") || dir.ends_with("etc"))
    );
    assert!(!dirs.contains(&PathBuf::from("/opt/homebrew")));
}

/// Makes a Homebrew prefix in `root` whose `etc` holds OpenSSL's
/// directories, each with its configuration, certificates, a private key
/// and a `cert.pem` link to the CA bundle in `etc/ca-certificates`, as
/// Homebrew lays them out, its system git configuration, and another
/// configuration directory, and returns it.
fn homebrew_prefix_in(root: &std::path::Path) -> PathBuf {
    let prefix = root.join("homebrew");
    for dir in ["etc/openssl@3", "etc/openssl"] {
        let dir = prefix.join(dir);
        std::fs::create_dir_all(dir.join("certs")).unwrap();
        std::fs::create_dir_all(dir.join("private")).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes the OpenSSL files that the sandbox may or may not read"
        )]
        {
            std::fs::write(dir.join("openssl.cnf"), "# config\n").unwrap();
            std::fs::write(dir.join("private/key.pem"), "# key\n").unwrap();
        }
    }
    std::fs::create_dir_all(prefix.join("etc/ca-certificates")).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes the CA bundle that the OpenSSL directories link to"
    )]
    std::fs::write(prefix.join("etc/ca-certificates/cert.pem"), "# bundle\n").unwrap();
    for dir in ["etc/openssl@3", "etc/openssl"] {
        std::os::unix::fs::symlink(
            prefix.join("etc/ca-certificates/cert.pem"),
            prefix.join(dir).join("cert.pem"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(prefix.join("etc/other")).unwrap();
    std::fs::create_dir_all(prefix.join("etc/opensslish")).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes the system git configuration of the prefix"
    )]
    std::fs::write(prefix.join("etc/gitconfig"), "[core]\n").unwrap();
    prefix
}

#[test]
fn a_homebrew_prefix_lets_the_app_read_its_openssl_configuration_but_no_other_configuration() {
    let root = tempfile::tempdir().unwrap();
    let prefix = homebrew_prefix_in(root.path());

    let read = homebrew_read(&prefix);

    assert!(read.contains(&prefix.join("bin")));
    assert!(read.contains(&prefix.join("etc/gitconfig")));
    for dir in ["etc/openssl@3", "etc/openssl"] {
        for name in ["openssl.cnf", "cert.pem", "certs"] {
            assert!(read.contains(&prefix.join(dir).join(name)), "{dir}/{name}");
        }
        assert!(!read.contains(&prefix.join(dir)), "{dir}");
    }
    assert!(
        !read
            .iter()
            .any(|path| path.starts_with(prefix.join("etc/other"))
                || path.starts_with(prefix.join("etc/opensslish")))
    );
    assert!(!read.contains(&prefix.join("etc")));
}

#[test]
fn a_spec_records_each_toolchain_path_that_is_a_link_so_the_app_can_follow_it() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let prefix = homebrew_prefix_in(&fixture.outside());
    let mut spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

    add_toolchain(
        &mut spec.read,
        &mut spec.read_only,
        &mut spec.metadata,
        &homebrew_read(&prefix),
        &|_| false,
    );

    let link = prefix.join("etc/openssl@3/cert.pem");
    assert!(spec.metadata.contains(&link), "{:?}", spec.metadata);
    assert!(
        spec.read
            .contains(&prefix.join("etc/ca-certificates/cert.pem"))
    );
    assert!(!spec.read.contains(&link));
    assert!(
        spec.read
            .contains(&prefix.join("etc/openssl@3/openssl.cnf"))
    );
    assert!(
        !spec
            .metadata
            .contains(&prefix.join("etc/openssl@3/openssl.cnf"))
    );
}

#[test]
fn a_spec_lets_the_app_stat_only_the_git_configuration_in_the_home_directory() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);

    let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

    for name in [".gitconfig", ".config/git", ".config/git/config"] {
        assert!(spec.metadata.contains(&fixture.home().join(name)), "{name}");
    }
    assert!(
        !spec
            .metadata
            .contains(&fixture.home().join(".cargo/config.toml"))
    );
    assert!(!spec.read.contains(&fixture.home().join(".gitconfig")));
}

#[test]
fn no_grant_of_a_homebrew_prefix_lets_the_app_read_an_openssl_private_directory() {
    let root = tempfile::tempdir().unwrap();
    let prefix = homebrew_prefix_in(root.path());

    let read = homebrew_read(&prefix);
    let deny = homebrew_deny(&prefix);

    for dir in ["etc/openssl@3", "etc/openssl"] {
        let private = prefix.join(dir).join("private");
        assert!(
            !read
                .iter()
                .any(|path| path.starts_with(&private) || private.starts_with(path)),
            "{dir}"
        );
        assert_eq!(
            deny.contains(&private),
            cfg!(target_os = "macos"),
            "{dir}: {deny:?}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_spec_reads_only_the_toolchain_directories_of_library_and_denies_the_keychains() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);

    let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

    assert!(!spec.read.contains(&PathBuf::from("/Library")));
    assert!(spec.deny.contains(&PathBuf::from("/Library/Keychains")));
    assert!(spec.deny.contains(&PathBuf::from("/usr/local/var")));
    assert!(
        spec.deny
            .contains(&fixture.home().join("Library/Keychains"))
    );
    assert!(
        spec.deny
            .contains(&fixture.home().join("Library/LaunchAgents"))
    );
}

#[test]
fn a_spec_lets_an_app_use_its_own_directories_inside_a_directory_that_the_host_program_protects() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    // mxv protects its state directory, which holds phase's state. The
    // fixture's parent directory also holds the checkout, so the test
    // protects phase's state directory itself.
    let host_state = fixture.state();

    let spec = fixture
        .spec_with(
            &api,
            "/usr/bin",
            BTreeSet::new(),
            test_launcher(),
            std::slice::from_ref(&host_state),
        )
        .unwrap();

    let dirs = AppDirs::of(&fixture.state(), &workspace(), &app("api"));
    assert!(spec.write.contains(&dirs.state));
    assert!(spec.deny.contains(&host_state));
}

#[test]
fn a_spec_keeps_the_directory_that_holds_an_apps_own_directories_protected() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let host_state = fixture.state();

    let spec = fixture
        .spec_with(
            &api,
            "/usr/bin",
            BTreeSet::new(),
            test_launcher(),
            std::slice::from_ref(&host_state),
        )
        .unwrap();

    let dirs = AppDirs::of(&fixture.state(), &workspace(), &app("api"));
    let reached =
        |path: &std::path::Path| spec.protected.iter().any(|found| found.reached_by(path));
    assert!(reached(&dirs.base));
    assert!(reached(&dirs.base.join("other")));
    assert!(!reached(&dirs.state));
    assert!(!reached(&dirs.tmp));
}

#[test]
fn a_spec_refuses_a_write_that_a_symbolic_link_in_the_checkout_leads_outside_it() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.outside()).unwrap();
    std::os::unix::fs::symlink(fixture.outside(), fixture.checkout().join("target")).unwrap();

    assert_eq!(
        writing(&fixture, &["target".to_owned()], "/usr/bin"),
        Err(SandboxError::OutsideCheckout {
            path: fixture.outside(),
            checkout: fixture.checkout(),
        })
    );
}

#[test]
fn an_apps_own_directory_that_is_a_symbolic_link_is_refused_and_its_target_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let dirs = AppDirs::of(&fixture.state(), &workspace(), &app("api"));
    std::fs::create_dir_all(&dirs.base).unwrap();
    std::fs::create_dir_all(fixture.outside()).unwrap();
    std::fs::set_permissions(fixture.outside(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(fixture.outside(), &dirs.state).unwrap();

    let result = dirs.create();

    assert!(
        matches!(&result, Err(SandboxError::Unusable { path, .. }) if *path == dirs.state),
        "{result:?}"
    );
    assert_eq!(
        std::fs::metadata(fixture.outside())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn the_launchers_check_creates_nothing_through_a_symbolic_link_planted_above_a_write_directory() {
    let fixture = Fixture::new();
    let spec = writing(&fixture, &["cache/inner".to_owned()], "/usr/bin").unwrap();
    std::fs::create_dir_all(fixture.outside()).unwrap();
    std::os::unix::fs::symlink(fixture.outside(), fixture.checkout().join("cache")).unwrap();

    let result = verify(&spec);

    assert!(
        matches!(&result, Err(SandboxError::Unusable { path, .. }) if *path == fixture.checkout().join("cache/inner")),
        "{result:?}"
    );
    assert!(!fixture.outside().join("inner").exists());
}

#[test]
fn the_launchers_check_refuses_a_checkout_that_a_symbolic_link_moved_elsewhere() {
    let fixture = Fixture::new();
    let spec = writing(&fixture, &[], "/usr/bin").unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test moves the checkout as an app could"
    )]
    std::fs::rename(fixture.checkout(), fixture.outside()).unwrap();
    std::os::unix::fs::symlink(fixture.outside(), fixture.checkout()).unwrap();

    assert_eq!(
        verify(&spec),
        Err(SandboxError::Moved {
            path: fixture.checkout(),
            now: fixture.outside(),
        })
    );
}

#[test]
fn a_spec_refuses_a_write_inside_a_directory_that_others_may_write() {
    let fixture = Fixture::new();
    let tmp = PathBuf::from("/tmp").canonicalize().unwrap();

    let result = writing(
        &fixture,
        &["/tmp/phase-sandbox-shared".to_owned()],
        "/usr/bin",
    );

    assert!(
        matches!(&result, Err(SandboxError::SharedDirectory { dir, .. }) if *dir == tmp),
        "{result:?}"
    );
}

#[test]
fn the_launchers_check_creates_a_missing_write_directory_readable_only_by_the_user() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let spec = writing(&fixture, &["target".to_owned()], "/usr/bin").unwrap();

    verify(&spec).unwrap();

    let mode = std::fs::metadata(fixture.checkout().join("target"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn a_spec_keeps_every_link_of_the_launchers_chain_and_its_directory_read_only() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let real = fixture.outside().join("real");
    let hop = fixture.outside().join("hop");
    let bin = fixture.outside().join("bin");
    for dir in [&real, &hop, &bin] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), real.join("program")).unwrap();
    std::os::unix::fs::symlink(real.join("program"), hop.join("program")).unwrap();
    std::os::unix::fs::symlink(hop.join("program"), bin.join("program")).unwrap();
    let launcher = Launcher {
        program: bin.join("program"),
        words: Vec::new(),
    };

    let spec = fixture
        .spec_with(&api, "/usr/bin", BTreeSet::new(), launcher, &[])
        .unwrap();

    for dir in [&bin, &hop, &real] {
        assert!(spec.read_only.contains(dir), "{}", dir.display());
    }
}

#[test]
fn a_spec_looks_nothing_up_in_a_path_entry_that_reaches_a_protected_path() {
    let fixture = Fixture::new();
    let api = fixture.app(sh("true"), 41_000);
    let path = format!("{}:/usr/bin", fixture.home().display());

    let spec = fixture.spec_of(&api, &path, BTreeSet::new()).unwrap();

    assert_eq!(spec.path_dirs, [PathBuf::from("/usr/bin")]);
}

/// A temporary tree with an `Applications` directory for app bundles and a
/// `Library/Developer` directory for developer tools, in which
/// [`developer_dir`] may find a developer directory.
struct DeveloperRoots {
    root: TempDir,
}

impl DeveloperRoots {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("Applications")).unwrap();
        std::fs::create_dir_all(root.path().join("Library/Developer")).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().canonicalize().unwrap().join(name)
    }

    /// Returns a link named `name` to `target`.
    fn link(&self, name: &str, target: &std::path::Path) -> PathBuf {
        let link = self.path(name);
        std::os::unix::fs::symlink(target, &link).unwrap();
        link
    }

    /// Returns the developer directory that `link` names.
    fn developer_dir(&self, link: &std::path::Path) -> Option<PathBuf> {
        developer_dir(
            link,
            &[self.path("Applications")],
            &[self.path("Library/Developer")],
        )
    }
}

#[test]
fn the_developer_directory_of_the_command_line_tools_is_that_directory() {
    let roots = DeveloperRoots::new();
    let dir = roots.path("Library/Developer/CommandLineTools");
    std::fs::create_dir_all(&dir).unwrap();
    let link = roots.link("developer_dir", &dir);

    assert_eq!(roots.developer_dir(&link), Some(dir));
}

#[test]
fn the_developer_directory_of_an_xcode_is_its_whole_app_bundle() {
    let roots = DeveloperRoots::new();
    let bundle = roots.path("Applications/Xcode_26.6.app");
    std::fs::create_dir_all(bundle.join("Contents/Developer")).unwrap();
    let link = roots.link("developer_dir", &bundle.join("Contents/Developer"));

    assert_eq!(roots.developer_dir(&link), Some(bundle));
}

#[test]
fn a_link_to_anything_but_an_installed_xcode_or_the_developer_library_names_no_developer_directory()
{
    let roots = DeveloperRoots::new();
    for dir in [
        "Users/Shared/x",
        "Users/Shared/Xcode.app/Contents/Developer",
        "Applications/Tools/Developer",
        "Applications/Xcode.app/Contents/Other",
    ] {
        std::fs::create_dir_all(roots.path(dir)).unwrap();
    }

    for (name, target) in [
        ("root", PathBuf::from("/")),
        ("usr", PathBuf::from("/usr")),
        ("shared", PathBuf::from("/Users/Shared/x")),
        ("outside", roots.path("Users/Shared/x")),
        (
            "bundle-outside",
            roots.path("Users/Shared/Xcode.app/Contents/Developer"),
        ),
        ("not-a-bundle", roots.path("Applications/Tools/Developer")),
        (
            "inside-a-bundle",
            roots.path("Applications/Xcode.app/Contents/Other"),
        ),
        ("applications", roots.path("Applications")),
        ("library", roots.path("Library/Developer")),
        ("relative", PathBuf::from("Xcode.app/Contents/Developer")),
        (
            "dangling",
            PathBuf::from("/nonexistent/Xcode.app/Contents/Developer"),
        ),
    ] {
        let link = roots.link(&format!("link-{name}"), &target);
        assert_eq!(roots.developer_dir(&link), None, "{name}");
    }
    assert_eq!(roots.developer_dir(&roots.path("absent")), None);
}

#[test]
fn linux_reads_usr_by_its_entries_without_the_data_and_configuration_of_usr_local() {
    let usr = tempfile::tempdir().unwrap();
    for dir in [
        "bin",
        "lib",
        "local/bin",
        "local/etc",
        "local/var",
        "local/share",
    ] {
        std::fs::create_dir_all(usr.path().join(dir)).unwrap();
    }

    let mut read: Vec<String> = usr_without_local_data(usr.path())
        .iter()
        .map(|path| path.strip_prefix(usr.path()).unwrap().display().to_string())
        .collect();
    read.sort();

    assert_eq!(read, ["bin", "lib", "local/bin", "local/share"]);
}

/// Returns the variables that `command` sets or removes, in name order.
fn environment(command: &std::process::Command) -> Vec<(String, Option<String>)> {
    command
        .get_envs()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

#[test]
fn a_sandboxed_command_gets_the_sandbox_temporary_directory_and_reads_no_global_git_configuration()
{
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads the environment it sets and never starts the command"
    )]
    let mut command = std::process::Command::new("true");
    command.env("TMPDIR", "/elsewhere");

    super::sandboxed_environment(&mut command, std::path::Path::new("/sandbox/tmp"));

    let mut expected = vec![("GIT_CONFIG_GLOBAL".to_owned(), Some("/dev/null".to_owned()))];
    if cfg!(target_os = "macos") {
        expected.push((
            "SSL_CERT_FILE".to_owned(),
            Some("/etc/ssl/cert.pem".to_owned()),
        ));
    }
    expected.push(("TMPDIR".to_owned(), Some("/sandbox/tmp".to_owned())));
    assert_eq!(environment(&command), expected);
}

#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_command_on_macos_reads_the_systems_root_certificates_unless_its_environment_names_a_bundle()
 {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads the environment it sets and never starts the command"
    )]
    let mut given = std::process::Command::new("true");
    given.env("SSL_CERT_FILE", "/checkout/roots.pem");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads the environment it sets and never starts the command"
    )]
    let mut plain = std::process::Command::new("true");

    super::sandboxed_environment(&mut given, std::path::Path::new("/sandbox/tmp"));
    super::sandboxed_environment(&mut plain, std::path::Path::new("/sandbox/tmp"));

    let roots = |command: &std::process::Command| {
        environment(command)
            .into_iter()
            .find(|(name, _)| name == "SSL_CERT_FILE")
            .and_then(|(_, value)| value)
    };
    assert_eq!(roots(&given).as_deref(), Some("/checkout/roots.pem"));
    assert_eq!(roots(&plain).as_deref(), Some("/etc/ssl/cert.pem"));
}

#[test]
fn a_sandboxed_command_keeps_the_git_configuration_that_its_environment_names() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads the environment it sets and never starts the command"
    )]
    let mut command = std::process::Command::new("true");
    command.env("GIT_CONFIG_GLOBAL", "/plan/gitconfig");

    super::sandboxed_environment(&mut command, std::path::Path::new("/sandbox/tmp"));

    let git = environment(&command)
        .into_iter()
        .find(|(name, _)| name == "GIT_CONFIG_GLOBAL")
        .and_then(|(_, value)| value);
    assert_eq!(git.as_deref(), Some("/plan/gitconfig"));
}

/// The tests of phase's Seatbelt rules, which are plain text, so they run
/// on the Linux pull-request lane too.
mod rules {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use super::{Fixture, sh};
    use crate::sandbox::seatbelt::{NETWORK_RULE, rules};

    #[test]
    fn every_network_rule_of_phase_starts_so_that_nono_writes_it_after_its_network_deny() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let spec = fixture
            .spec_of(&api, "/usr/bin", BTreeSet::from([41_005]))
            .unwrap();

        let rules = rules(&spec);

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
        assert!(network.iter().all(|rule| rule.starts_with(NETWORK_RULE)));
    }

    #[test]
    fn the_profile_denies_the_workspace_and_then_allows_the_checkout_inside_it_again() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

        let rules = rules(&spec);

        let position = |wanted: String| {
            rules
                .iter()
                .position(|rule| *rule == wanted)
                .unwrap_or_else(|| panic!("no {wanted} in {rules:?}"))
        };
        let deny = position(format!(
            "(deny file-read-data file-write* (subpath \"{}\"))",
            fixture.workspace_dir().display()
        ));
        let allow = position(format!(
            "(allow file-read-data (subpath \"{}\"))",
            fixture.checkout().display()
        ));
        let git = position(format!(
            "(deny file-write* (subpath \"{}\"))",
            fixture.checkout().join(".git").display()
        ));
        assert!(deny < allow);
        assert!(git < allow);
    }

    #[test]
    fn a_deny_inside_a_grant_that_the_profile_allows_again_comes_after_every_allow_that_covers_it()
    {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let mut spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();
        let etc = PathBuf::from("/usr/local/etc");
        let openssl = etc.join("openssl@3");
        let private = openssl.join("private");
        spec.deny.extend([etc, private.clone()]);
        spec.read.insert(openssl);

        let rules = rules(&spec);

        let deny = rules
            .iter()
            .position(|rule| {
                rule.starts_with("(deny file-read-data")
                    && rule.contains(&format!("\"{}\"", private.display()))
            })
            .unwrap_or_else(|| panic!("no deny of {} in {rules:?}", private.display()));
        let covering = rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.starts_with("(allow file-read-data"))
            .filter(|(_, rule)| {
                rule.split('"')
                    .nth(1)
                    .is_some_and(|path| private.starts_with(path))
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert!(!covering.is_empty(), "{rules:?}");
        assert!(covering.iter().all(|&allow| allow < deny), "{rules:?}");
    }

    #[test]
    fn the_profile_lets_the_app_read_only_the_metadata_of_each_metadata_path_of_the_spec() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let mut spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();
        let link = fixture.outside().join("cert.pem");
        spec.metadata.insert(link.clone());

        let rules = rules(&spec);

        let wanted = format!(
            "(allow file-read-metadata (literal \"{}\"))",
            link.display()
        );
        assert!(rules.contains(&wanted), "{rules:?}");
    }

    #[test]
    fn the_profile_lets_a_path_through_the_var_or_etc_link_resolve() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

        let rules = rules(&spec);

        for link in ["/var", "/etc"] {
            let wanted = format!("(allow file-read-metadata (literal \"{link}\"))");
            assert!(rules.contains(&wanted), "{rules:?}");
        }
    }

    #[test]
    fn the_profile_denies_the_pasteboard_launch_services_and_apple_events() {
        let fixture = Fixture::new();
        let api = fixture.app(sh("true"), 41_000);
        let spec = fixture.spec_of(&api, "/usr/bin", BTreeSet::new()).unwrap();

        let rules = rules(&spec);

        for wanted in [
            "(deny mach-lookup (global-name \"com.apple.pasteboard.1\"))",
            "(deny mach-lookup (global-name \"com.apple.coreservices.launchservicesd\"))",
            "(deny appleevent-send)",
            "(deny lsopen)",
        ] {
            assert!(rules.iter().any(|rule| rule == wanted), "{wanted}");
        }
    }
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
    use crate::sandbox::seatbelt::rules;
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
        assert_eq!(caps.platform_rules(), rules(&spec).as_slice());
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

    #[test]
    fn a_sandboxed_app_can_stat_the_git_configuration_but_never_read_it_or_list_a_protected_directory()
     {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.home().join(".config/git")).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes the protected files that the sandbox may stat"
        )]
        {
            std::fs::write(fixture.home().join(".gitconfig"), "[user]\n").unwrap();
            std::fs::write(fixture.home().join(".config/git/config"), "[user]\n").unwrap();
            std::fs::write(fixture.home().join(".cargo/config.toml"), "[net]\n").unwrap();
            std::fs::write(fixture.home().join(".ssh/id_ed25519"), "key").unwrap();
        }
        let script = format!(
            r#"
            check() {{ if "$@" >/dev/null 2>&1; then echo "$case=allowed"; else echo "$case=denied"; fi; }}
            case=stat-gitconfig check stat "{home}/.gitconfig"
            case=read-gitconfig check cat "{home}/.gitconfig"
            case=stat-xdg check stat "{home}/.config/git/config"
            case=read-xdg check cat "{home}/.config/git/config"
            case=list-xdg check ls "{home}/.config/git"
            case=stat-cargo-config check stat "{home}/.cargo/config.toml"
            case=list-ssh check ls "{home}/.ssh"
            case=stat-ssh-key check stat "{home}/.ssh/id_ed25519"
            case=read-ssh-key check cat "{home}/.ssh/id_ed25519"
            "#,
            home = fixture.home().display(),
        );

        let (output, _) = sandboxed(&fixture, &script, 41_000, 41_005);

        // cargo stops when it can stat but not read `~/.cargo/config.toml`,
        // so only the git configuration keeps its metadata.
        for (case, expected) in [
            ("stat-gitconfig", "allowed"),
            ("read-gitconfig", "denied"),
            ("stat-xdg", "allowed"),
            ("read-xdg", "denied"),
            ("list-xdg", "denied"),
            ("stat-cargo-config", "denied"),
            ("list-ssh", "denied"),
            ("stat-ssh-key", "denied"),
            ("read-ssh-key", "denied"),
        ] {
            assert_eq!(outcome(&output, case), expected, "{case}: {output}");
        }
    }

    /// Returns the label of the launchd job that the sandboxed case tries
    /// to submit, which names this test process.
    fn launchd_label() -> String {
        format!("dev.crumbkit.phase-sandbox-test.{}", std::process::id())
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
        // The dependency also listens on ::1 at its port when this machine
        // has an IPv6 loopback address, as a dev server such as Vite does.
        let dependency_ipv6 =
            TcpListener::bind(("::1", dependency.local_addr().unwrap().port())).ok();
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
            case=connect-dependency-ipv6 check nc -z -w 2 ::1 {dependency}
            case=connect-other check nc -z -w 2 127.0.0.1 {other}
            case=listen-other check nc -l 127.0.0.1 {other_port}
            listens() {{ nc -l "$1" {own} 2>/dev/null & listener=$!; sleep 1; if kill -0 $listener 2>/dev/null; then echo "$case=allowed"; kill $listener; wait $listener 2>/dev/null; else echo "$case=denied"; fi; }}
            case=listen-own listens 127.0.0.1
            case=listen-own-ipv6 listens ::1
            case=listen-own-every-address listens 0.0.0.0
            if /usr/bin/perl -MIO::Socket::INET -e 'IO::Socket::INET->new(PeerAddr => shift, Timeout => 2) or print "$!"' 192.0.2.1:{dependency} | grep -q 'not permitted'; then echo connect-dependency-port-elsewhere=denied; else echo connect-dependency-port-elsewhere=allowed; fi
            limited() {{ /usr/bin/perl -e 'alarm 5; exec @ARGV' "$@"; }}
            case=open-app check limited /usr/bin/open -g -j -a Finder
            case=apple-event check limited /usr/bin/osascript -e 'tell application "Finder" to get name'
            case=launchctl-submit check limited /bin/launchctl submit -l {label} -- /usr/bin/true
            case=pasteboard check limited sh -c '/usr/bin/pbpaste 2>&1 >/dev/null | grep -q . && exit 1; /usr/bin/pbpaste >/dev/null'
            case=keeper-environment check limited sh -c '/bin/ps -E -o command= -p $PPID | grep -q PATH='
            "#,
            outside = fixture.outside().display(),
            home = fixture.home().display(),
            socket = socket.display(),
            dependency = dependency.local_addr().unwrap().port(),
            other = other.local_addr().unwrap().port(),
            other_port = own + 1,
            label = launchd_label(),
        );

        let (output, _) = sandboxed(
            &fixture,
            &script,
            own,
            dependency.local_addr().unwrap().port(),
        );
        // A job that a sandboxed `launchctl submit` managed to submit would
        // keep running, so the test removes it either way.
        #[expect(
            clippy::disallowed_methods,
            reason = "the test removes the launchd job that the sandboxed case tried to submit"
        )]
        let _removed = Command::new("/bin/launchctl")
            .args(["remove", &launchd_label()])
            .stderr(Stdio::null())
            .status();

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
            // Seatbelt restricts a bind by port and never by address, so
            // the app may bind its own port on every address. The inbound
            // rule admits only peers on this machine.
            ("listen-own-every-address", "allowed"),
            ("connect-dependency-port-elsewhere", "denied"),
            ("open-app", "denied"),
            ("apple-event", "denied"),
            ("launchctl-submit", "denied"),
            ("pasteboard", "denied"),
            ("keeper-environment", "denied"),
        ] {
            assert_eq!(outcome(&output, case), expected, "{case} in {output}");
        }
        // Seatbelt's `localhost` in a port rule covers ::1 as well as
        // 127.0.0.1, so an app may listen on its own port on ::1 and reach
        // a dependency that listens only there.
        if dependency_ipv6.is_some() {
            for case in ["connect-dependency-ipv6", "listen-own-ipv6"] {
                assert_eq!(outcome(&output, case), "allowed", "{case} in {output}");
            }
        }
        assert!(!fixture.outside().join("file").exists());
        // DNS is blocked, so whether `localhost` resolves depends on the
        // resolver reading /etc/hosts itself, which the eval records.
        println!(
            "connect-dependency-by-name={}",
            outcome(&output, "connect-dependency-by-name")
        );
    }

    /// Returns this machine's address on `en0`, or `None` when it has none.
    fn en0_address() -> Option<String> {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test asks ipconfig for the machine's address on en0"
        )]
        let output = Command::new("/usr/sbin/ipconfig")
            .args(["getifaddr", "en0"])
            .output()
            .ok()?;
        let address = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (!address.is_empty()).then_some(address)
    }

    /// Records what [`crate::sandbox::SEATBELT_PARTIAL`] says: Seatbelt's
    /// `localhost` matches every address of this machine, so a sandboxed
    /// process that listens on its port on every address accepts a peer
    /// that connects to the machine's address on `en0`. When Seatbelt
    /// refuses that peer, this test fails, and `status` can stop calling
    /// Seatbelt partial.
    #[test]
    fn a_sandboxed_listener_on_every_address_accepts_a_peer_on_this_machines_en0_address() {
        let Some(address) = en0_address() else {
            return;
        };
        let fixture = Fixture::new();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let own = free.local_addr().unwrap().port();
        drop(free);
        let script = format!(
            r#"/usr/bin/perl -MIO::Socket::INET -e '$s = IO::Socket::INET->new(LocalAddr => "0.0.0.0", LocalPort => {own}, Listen => 5, ReuseAddr => 1) or die "bind: $!"; $SIG{{ALRM}} = sub {{ exit 0 }}; alarm 6; while ($c = $s->accept) {{ print "accepted ", $c->peerhost, "\n"; }}'"#
        );
        let peer = std::thread::spawn(move || {
            crate::clock::pause(std::time::Duration::from_millis(1500));
            let reached = std::net::TcpStream::connect_timeout(
                &format!("{address}:{own}").parse().unwrap(),
                std::time::Duration::from_secs(2),
            )
            .is_ok();
            (address, reached)
        });

        let (output, _) = sandboxed(&fixture, &script, own, 1);

        let (address, reached) = peer.join().unwrap();
        assert!(reached, "{output}");
        assert!(output.contains(&format!("accepted {address}")), "{output}");
    }

    /// Runs `script` under a sandbox built from this machine's own `HOME`
    /// and `PATH`, with the checkout's `target` writable, and returns its
    /// output and exit status.
    fn with_real_toolchains(fixture: &Fixture, script: &str) -> (String, i32) {
        let mut command = real_toolchains_command(fixture, &sh(script));
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts this test binary as the launcher"
        )]
        let output = command.output().unwrap();
        (
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            output.status.code().unwrap_or(-1),
        )
    }

    /// Returns the launcher command that runs `run` under the sandbox of
    /// an app with this machine's real `HOME` and `PATH`, with the
    /// checkout's `target` writable, as [`with_real_toolchains`] does.
    fn real_toolchains_command(fixture: &Fixture, run: &[String]) -> Command {
        let mut api = fixture.app(run.to_vec(), 41_000);
        api.permissions.as_mut().unwrap().write = ["target".to_owned()].into();
        #[expect(
            clippy::disallowed_methods,
            reason = "the smoke test runs this machine's real toolchains"
        )]
        let (home, path) = (
            PathBuf::from(std::env::var("HOME").unwrap()),
            std::env::var("PATH").unwrap(),
        );
        let id = crate::record::testing::workspace();
        let state = fixture.state();
        let paths = fixture.workspace_paths();
        let context = crate::sandbox::Context {
            state_dir: &state,
            workspace: &id,
            workspace_paths: &paths,
            path: Some(&path),
            home: Some(&home),
            protected: &[],
            connect: BTreeSet::new(),
            launcher: super::test_launcher(),
        };
        let spec = crate::sandbox::spec(
            &api,
            &api.processes[0],
            api.permissions.as_ref().unwrap(),
            &context,
        )
        .unwrap();
        let mut command = launcher_command(&spec, run).unwrap();
        command
            .env_clear()
            .env("PATH", &path)
            .env("HOME", &home)
            .env("CARGO_TARGET_DIR", fixture.checkout().join("target"))
            .current_dir(fixture.checkout())
            .stdin(Stdio::null());
        crate::sandbox::sandboxed_environment(&mut command, &spec.tmp);
        command
    }

    /// The variable that makes [`native_roots_probe`] load the trusted
    /// roots, which it does only when a test starts it under the sandbox.
    const NATIVE_ROOTS_PROBE: &str = "PHASE_TEST_NATIVE_ROOTS_PROBE";

    /// Loads the system's trusted roots as rustls-native-certs does for
    /// the AWS SDK, and prints how many it found, when the test
    /// [`a_sandboxed_process_loads_the_systems_trusted_roots_with_rustls_native_certs`]
    /// starts this test binary under the sandbox. Otherwise it does
    /// nothing.
    #[test]
    fn native_roots_probe() {
        #[expect(
            clippy::disallowed_methods,
            reason = "the probe runs only when the test that starts it sets the variable"
        )]
        if std::env::var_os(NATIVE_ROOTS_PROBE).is_none() {
            return;
        }
        let roots = rustls_native_certs::load_native_certs();
        println!("roots={} errors={}", roots.certs.len(), roots.errors.len());
    }

    #[test]
    fn a_sandboxed_process_loads_the_systems_trusted_roots_with_rustls_native_certs() {
        let fixture = Fixture::new();
        let probe = vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
            "sandbox::tests::seatbelt::native_roots_probe".to_owned(),
        ];
        let mut command = real_toolchains_command(&fixture, &probe);
        command.env(NATIVE_ROOTS_PROBE, "1");

        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts this test binary as the launcher"
        )]
        let output = command.output().unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let found = stdout
            .lines()
            .find_map(|line| line.split_once("roots=").map(|(_, found)| found))
            .unwrap_or_else(|| {
                panic!(
                    "the probe printed no roots: {stdout}{}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        let (count, errors) = found.split_once(" errors=").unwrap();
        assert!(count.parse::<usize>().unwrap() > 0, "{stdout}");
        assert_eq!(errors, "0", "{stdout}");
    }

    #[test]
    fn node_reports_its_version_under_the_sandbox_when_its_output_is_a_log_the_app_may_not_read() {
        if !on_path("node") {
            println!("node=absent");
            return;
        }
        let fixture = Fixture::new();
        // The keeper writes an app's output to a log in phase's state
        // directory, which the sandbox denies, and node stops when it
        // cannot stat its own standard output.
        let logs = fixture.state().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let log = logs.join("api.server.log");
        #[expect(
            clippy::disallowed_methods,
            reason = "the test opens the log that the sandboxed node writes, as a keeper does"
        )]
        let file = std::fs::File::create(&log).unwrap();
        let mut command = real_toolchains_command(&fixture, &sh("node --version"));
        command.stdout(file.try_clone().unwrap()).stderr(file);

        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts this test binary as the launcher"
        )]
        let status = command.status().unwrap();

        let written = std::fs::read_to_string(&log).unwrap();
        assert!(status.success(), "{status}: {written}");
        // The launcher, which is this test binary, writes its own lines to
        // the log before it becomes node.
        assert!(
            written.lines().any(|line| line
                .strip_prefix('v')
                .is_some_and(|version| version.starts_with(|first: char| first.is_ascii_digit()))),
            "{written}"
        );
    }

    #[test]
    fn a_sandboxed_app_reads_a_homebrew_openssl_configuration_but_never_its_private_keys() {
        // The second prefix lies inside a denied path, as `/usr/local/etc`
        // is denied when Homebrew lives at `/usr/local`, so the profile
        // allows its OpenSSL grants again after the deny.
        for etc_denied in [false, true] {
            let fixture = Fixture::new();
            let prefix = super::homebrew_prefix_in(&fixture.outside());
            let dir = prefix.join("etc/openssl@3");
            let script = format!(
                r#"
                check() {{ if "$@" >/dev/null 2>&1; then echo "$case=allowed"; else echo "$case=denied"; fi; }}
                case=config check cat "{dir}/openssl.cnf"
                case=bundle check cat "{dir}/cert.pem"
                case=certs check ls "{dir}/certs"
                case=gitconfig check cat "{prefix}/etc/gitconfig"
                case=key check cat "{dir}/private/key.pem"
                case=list check ls "{dir}/private"
                case=other check ls "{prefix}/etc/other"
                "#,
                dir = dir.display(),
                prefix = prefix.display(),
            );
            let api = fixture.app(sh(&script), 41_000);
            let mut spec = fixture
                .spec_of(&api, "/usr/bin:/bin", BTreeSet::new())
                .unwrap();
            // The spec reads the prefix's grants and denies its OpenSSL
            // directories' `private`, as `spec` does for a prefix on `PATH`.
            crate::sandbox::add_toolchain(
                &mut spec.read,
                &mut spec.read_only,
                &mut spec.metadata,
                &crate::sandbox::homebrew_read(&prefix),
                &|_| false,
            );
            let deny = crate::sandbox::homebrew_deny(&prefix);
            spec.deny
                .extend(deny.iter().map(|path| crate::sandbox::canonical(path)));
            if etc_denied {
                spec.deny
                    .insert(crate::sandbox::canonical(&prefix.join("etc")));
            }
            let mut command = launcher_command(&spec, &sh(&script)).unwrap();
            command
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("TMPDIR", &spec.tmp)
                .current_dir(fixture.checkout())
                .stdin(Stdio::null());
            #[expect(
                clippy::disallowed_methods,
                reason = "the test starts this test binary as the launcher"
            )]
            let output = command.output().unwrap();
            let output = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            for (case, expected) in [
                ("config", "allowed"),
                ("bundle", "allowed"),
                ("certs", "allowed"),
                ("gitconfig", "allowed"),
                ("key", "denied"),
                ("list", "denied"),
                ("other", "denied"),
            ] {
                assert_eq!(
                    outcome(&output, case),
                    expected,
                    "{case} with etc denied {etc_denied}: {output}"
                );
            }
        }
    }

    /// Runs `program` with `args` outside the sandbox in `dir`, with only
    /// `HOME` and `PATH` set, and panics with its output when it fails.
    fn outside(
        program: &str,
        args: &[&str],
        dir: &std::path::Path,
        home: &std::path::Path,
        path: &str,
    ) {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test prepares, outside the sandbox, what the sandboxed process reads"
        )]
        let output = Command::new(program)
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", home)
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Returns the sysroot of the Rust toolchain that this machine's
    /// `rustc` names, whose `bin` holds its `cargo` and `rustc`.
    fn rust_sysroot() -> PathBuf {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test asks rustc where the real toolchain lies"
        )]
        let output = Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap();
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    }

    /// Records why a protected path keeps its metadata: libgit2 stats
    /// `~/.gitconfig` before it opens a repository, and when the stat
    /// fails it treats a git dependency that cargo fetched as unusable and
    /// tries to delete it, which the sandbox refuses, so cargo exits 101.
    #[test]
    fn cargo_reads_a_git_dependency_fetched_outside_the_sandbox_while_gitconfig_is_protected() {
        let fixture = Fixture::new();
        let home = fixture.home();
        let sysroot = rust_sysroot();
        let bin = sysroot.join("bin");
        let outside_path = format!("{}:/usr/bin:/bin", bin.display());
        let dependency = fixture.outside().join("dep");
        std::fs::create_dir_all(dependency.join("src")).unwrap();
        std::fs::create_dir_all(fixture.checkout().join("src")).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes the crate, its git dependency and the protected gitconfig"
        )]
        {
            std::fs::write(home.join(".gitconfig"), "[user]\n\tname = phase\n").unwrap();
            std::fs::write(
                dependency.join("Cargo.toml"),
                "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::write(dependency.join("src/lib.rs"), "pub fn two() -> u8 { 2 }\n").unwrap();
            std::fs::write(
                fixture.checkout().join("Cargo.toml"),
                format!(
                    "[package]\nname = \"smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\ndep = {{ git = \"file://{}\" }}\n\n[workspace]\n",
                    dependency.display()
                ),
            )
            .unwrap();
            std::fs::write(fixture.checkout().join("src/main.rs"), "fn main() {}\n").unwrap();
        }
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &[
                "-c",
                "user.email=phase@example.com",
                "commit",
                "-q",
                "-m",
                "dep",
            ],
        ] {
            outside("git", args, &dependency, &home, "/usr/bin:/bin");
        }
        // cargo fetches the dependency into the home's `.cargo/git` and
        // writes the lock file, outside the sandbox.
        outside(
            "cargo",
            &["generate-lockfile"],
            &fixture.checkout(),
            &home,
            &outside_path,
        );

        let script = "cargo metadata --locked --offline --format-version 1 >/dev/null";
        let path = format!("{}:{outside_path}", home.join(".cargo/bin").display());
        let api = fixture.app(sh(script), 41_000);
        let mut spec = fixture.spec_of(&api, &path, BTreeSet::new()).unwrap();
        // The toolchain lies outside the fixture's home, so the spec does
        // not know it as one and grants only its `bin`.
        spec.read.insert(crate::sandbox::canonical(&sysroot));
        let mut command = launcher_command(&spec, &sh(script)).unwrap();
        command
            .env_clear()
            .env("HOME", &home)
            .env("PATH", &path)
            .env("TMPDIR", &spec.tmp)
            .current_dir(fixture.checkout())
            .stdin(Stdio::null());
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts this test binary as the launcher"
        )]
        let output = command.output().unwrap();

        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn git_reports_its_version_and_the_status_of_a_checkout_under_the_sandbox() {
        if !on_path("git") {
            return;
        }
        let fixture = Fixture::new();
        #[expect(
            clippy::disallowed_methods,
            reason = "the test reads this machine's real PATH to find git"
        )]
        let path = std::env::var("PATH").unwrap();
        outside(
            "git",
            &["init", "-q"],
            &fixture.checkout(),
            &fixture.home(),
            &path,
        );

        let (output, code) = with_real_toolchains(&fixture, "git --version && git status");

        assert_eq!(code, 0, "{output}");
    }

    /// Reports whether `program` is on this machine's `PATH`.
    #[expect(
        clippy::disallowed_methods,
        reason = "the smoke test looks for this machine's real toolchains"
    )]
    fn on_path(program: &str) -> bool {
        std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .any(|dir| std::path::Path::new(dir).join(program).is_file())
    }

    #[test]
    fn xcrun_python_and_node_run_under_the_sandbox_where_they_exist() {
        let fixture = Fixture::new();
        for (program, script) in [
            ("xcrun", "xcrun --show-sdk-path"),
            ("python3", "python3 -c 'print(1 + 1)'"),
            ("node", "node -e 'console.log(1 + 1)'"),
        ] {
            if !on_path(program) {
                println!("{program}=absent");
                continue;
            }
            let (output, code) = with_real_toolchains(&fixture, script);
            println!("{program}={code}: {output}");
            assert_eq!(code, 0, "{program}: {output}");
        }
    }

    #[test]
    fn cargo_builds_a_crate_whose_build_script_compiles_c_under_the_sandbox() {
        if !on_path("cargo") {
            return;
        }
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.checkout().join("src")).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes the crate that the sandboxed cargo builds"
        )]
        {
            std::fs::write(
                fixture.checkout().join("Cargo.toml"),
                "[package]\nname = \"smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
            )
            .unwrap();
            std::fs::write(
                fixture.checkout().join("build.rs"),
                "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    let status = std::process::Command::new(\"cc\").args([\"-c\", \"src/hello.c\", \"-o\"]).arg(format!(\"{out}/hello.o\")).status().unwrap();\n    assert!(status.success());\n}\n",
            )
            .unwrap();
            std::fs::write(
                fixture.checkout().join("src/hello.c"),
                "int hello(void) { return 2; }\n",
            )
            .unwrap();
            std::fs::write(fixture.checkout().join("src/main.rs"), "fn main() {}\n").unwrap();
            // The checkout is read-only, so the lock file that cargo would
            // write already exists.
            std::fs::write(
                fixture.checkout().join("Cargo.lock"),
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"smoke\"\nversion = \"0.0.0\"\n",
            )
            .unwrap();
        }

        // cargo runs with the user's own `~/.cargo`, which the sandbox lets
        // it read and never write, so a sandboxed app cannot poison the
        // registry cache that unsandboxed builds use.
        let (output, code) = with_real_toolchains(
            &fixture,
            r#"cargo build && if touch "$HOME/.cargo/registry/phase-sandbox-probe" 2>/dev/null; then echo registry=writable; exit 1; fi"#,
        );

        assert_eq!(code, 0, "{output}");
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
    use crate::sandbox::{Platform, Sandboxing, launcher_command, platform};

    #[test]
    fn this_kernels_landlock_is_reported_as_unsandboxed_or_as_unavailable() {
        match platform() {
            Platform::Available(sandboxing) => assert_eq!(sandboxing, Sandboxing::landlock()),
            Platform::Unavailable(reason) => assert!(!reason.is_empty()),
        }
    }

    /// Runs `script` with `/bin/sh` under the fixture's sandbox and returns
    /// its standard output.
    fn sandboxed(fixture: &Fixture, script: &str, connect: BTreeSet<u16>) -> String {
        let api = fixture.app(sh(script), 41_000);
        let spec = fixture.spec_of(&api, "/usr/bin:/bin", connect).unwrap();
        let mut command = launcher_command(&spec, &sh(script)).unwrap();
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
        output
    }

    /// Records why Linux reports a process under Landlock as unsandboxed:
    /// it can connect to a Unix socket outside every path it may use,
    /// because Landlock before ABI 9 cannot restrict `connect` and nono 0.81
    /// does not. When nono restricts Unix sockets this test fails, and
    /// Linux can report Landlock as a sandbox.
    #[test]
    fn a_process_under_landlock_can_still_connect_to_a_unix_socket() {
        if !matches!(platform(), Platform::Available(_)) {
            return;
        }
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.outside()).unwrap();
        let socket = fixture.outside().join("agent.sock");
        let _agent = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let script = format!(
            "if perl -MIO::Socket::UNIX -e 'IO::Socket::UNIX->new(Peer => shift) or exit 1' '{}'; then echo unix-socket=allowed; else echo unix-socket=denied; fi",
            socket.display()
        );

        let output = sandboxed(&fixture, &script, BTreeSet::new());

        assert!(
            output.lines().any(|line| line == "unix-socket=allowed"),
            "{output}"
        );
    }

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

    #[test]
    fn a_sandboxed_app_reads_a_homebrew_openssl_configuration_but_never_its_private_keys() {
        if !matches!(platform(), Platform::Available(_)) {
            return;
        }
        let fixture = Fixture::new();
        let prefix = super::homebrew_prefix_in(&fixture.outside());
        let dir = prefix.join("etc/openssl@3");
        let script = format!(
            r#"
            check() {{ if "$@" >/dev/null 2>&1; then echo "$case=allowed"; else echo "$case=denied"; fi; }}
            case=config check cat "{dir}/openssl.cnf"
            case=bundle check cat "{dir}/cert.pem"
            case=certs check ls "{dir}/certs"
            case=key check cat "{dir}/private/key.pem"
            case=list check ls "{dir}/private"
            case=other check ls "{prefix}/etc/other"
            "#,
            dir = dir.display(),
            prefix = prefix.display(),
        );
        let api = fixture.app(sh(&script), 41_000);
        let mut spec = fixture
            .spec_of(&api, "/usr/bin:/bin", BTreeSet::new())
            .unwrap();
        // The spec reads the prefix's grants, as `spec` does for a prefix
        // on `PATH`.
        crate::sandbox::add_toolchain(
            &mut spec.read,
            &mut spec.read_only,
            &mut spec.metadata,
            &crate::sandbox::homebrew_read(&prefix),
            &|_| false,
        );
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
            ("config", "allowed"),
            ("bundle", "allowed"),
            ("certs", "allowed"),
            ("key", "denied"),
            ("list", "denied"),
            ("other", "denied"),
        ] {
            let line = format!("{case}={expected}");
            assert!(
                output.lines().any(|seen| seen == line),
                "{line} in {output}"
            );
        }
    }
}
