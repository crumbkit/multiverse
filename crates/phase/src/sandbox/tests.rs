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
    AppDirs, Context, LANDLOCK_PARTIAL, Launcher, Platform, SandboxError, SandboxSpec, Sandboxing,
    UNDECLARED, WorkspacePaths, launch, launcher_command, sandboxing, spec, toolchain, verify,
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
        sandboxing(&api, &Platform::Available(Sandboxing::landlock())),
        Some(Sandboxing::landlock())
    );
}

#[test]
fn landlock_status_says_that_it_sandboxes_only_in_part_and_what_it_leaves_open() {
    let landlock = Sandboxing::landlock();

    assert_eq!(
        landlock.to_string(),
        "landlock (partial: Unix sockets and UDP unrestricted, TCP ports on every address)"
    );
    assert_eq!(
        serde_json::to_value(&landlock).unwrap(),
        serde_json::json!({ "kind": "landlock", "partial": LANDLOCK_PARTIAL })
    );
    assert_eq!(Sandboxing::Seatbelt.to_string(), "seatbelt");
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

    assert_eq!(
        verify(&spec),
        Err(SandboxError::Moved {
            path: fixture.checkout().join("target"),
            now: fixture.home().join(".ssh"),
        })
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

/// The tests of phase's Seatbelt rules, which are plain text, so they run
/// on the Linux pull-request lane too.
mod rules {
    use std::collections::BTreeSet;

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
                "(allow network-inbound (local tcp \"localhost:41000\") (remote tcp \"localhost:*\"))",
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
            listens() {{ nc -l "$1" {own} 2>/dev/null & listener=$!; sleep 1; if kill -0 $listener 2>/dev/null; then echo "$case=allowed"; kill $listener; wait $listener 2>/dev/null; else echo "$case=denied"; fi; }}
            case=listen-own listens 127.0.0.1
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
    use crate::sandbox::{Platform, Sandboxing, launcher_command, platform};

    #[test]
    fn this_kernels_landlock_is_reported_as_a_partial_sandbox_or_as_unavailable() {
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

    /// Records what the partial Linux sandbox leaves open: a sandboxed
    /// process can connect to a Unix socket outside every path it may use,
    /// because Landlock before ABI 9 cannot restrict `connect` and nono 0.81
    /// does not. When nono restricts Unix sockets this test fails, and
    /// `status` can stop calling Landlock partial.
    #[test]
    fn a_process_under_the_partial_linux_sandbox_can_still_connect_to_a_unix_socket() {
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
}
