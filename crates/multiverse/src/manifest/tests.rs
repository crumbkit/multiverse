use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::manifest::{DEFAULT_READY_TIMEOUT, Manifest, ManifestError, ManifestProblem};

fn parse(text: &str) -> Result<Manifest, ManifestError> {
    Manifest::parse(text, Path::new("/repo/mxv.toml"))
}

fn problem(text: &str) -> (Option<usize>, ManifestProblem) {
    let error = parse(text).unwrap_err();
    (error.line, error.problem)
}

const APP: &str = r#"
ports = ["api", "web"]

[processes.api]
argv = ["cargo", "run", "--bin", "api"]
env = { RUST_LOG = "info" }
ports = { PORT = "api" }
listen = "api"
ready = { timeout_seconds = 120 }

[processes.worker]
argv = ["cargo", "run", "--bin", "worker"]
depends_on = ["api"]

[processes.web]
argv = ["npm", "run", "dev", "--", "--port", "$PORT"]
cwd = "web"
ports = { PORT = "web", API_PORT = "api" }
listen = "web"
depends_on = ["api"]
"#;

#[test]
fn a_manifest_of_three_processes_parses_with_its_defaults() {
    let manifest = parse(APP).unwrap();
    assert_eq!(manifest.ports, ["api", "web"]);
    let server = &manifest.processes["api"];
    assert_eq!(server.argv[0], "cargo");
    assert_eq!(server.cwd, PathBuf::from("."));
    assert_eq!(server.env["RUST_LOG"], "info");
    assert_eq!(server.ports["PORT"], "api");
    assert_eq!(server.listen.as_deref(), Some("api"));
    let ready = server.ready.as_ref().unwrap();
    assert_eq!(ready.port, "api");
    assert_eq!(ready.timeout, Duration::from_secs(120));
    let web = &manifest.processes["web"];
    assert_eq!(web.cwd, PathBuf::from("web"));
    assert_eq!(web.ready.as_ref().unwrap().timeout, DEFAULT_READY_TIMEOUT);
    assert_eq!(web.depends_on, ["api"]);
}

#[test]
fn an_empty_manifest_declares_no_ports_and_no_processes() {
    let manifest = parse("").unwrap();
    assert!(manifest.ports.is_empty());
    assert!(manifest.processes.is_empty());
}

#[test]
fn an_unknown_top_level_key_fails_and_names_the_key_and_its_line() {
    let (line, problem) = problem("ports = []\nprocess = {}\n");
    assert_eq!(line, Some(2));
    assert_eq!(
        problem,
        ManifestProblem::UnknownKey {
            key: "process".to_owned()
        }
    );
}

#[test]
fn an_unknown_key_of_a_process_fails_and_names_the_key_and_its_line() {
    let (line, problem) = problem("[processes.a]\nargv = [\"true\"]\ncommand = \"true\"\n");
    assert_eq!(line, Some(3));
    assert_eq!(
        problem,
        ManifestProblem::UnknownKey {
            key: "command".to_owned()
        }
    );
}

#[test]
fn an_unknown_key_of_a_readiness_check_fails_and_names_the_key_and_its_line() {
    let (line, problem) = problem(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"true\"]\nready = { port = \"api\", http = \"/\" }\n",
    );
    assert_eq!(line, Some(4));
    assert_eq!(
        problem,
        ManifestProblem::UnknownKey {
            key: "http".to_owned()
        }
    );
}

#[test]
fn each_reserved_section_fails_with_its_name_and_line() {
    for section in ["containers", "secrets", "actions"] {
        let (line, problem) = problem(&format!("ports = []\n\n[{section}.db]\nimage = \"x\"\n"));
        assert_eq!(line, Some(3), "{section}");
        assert_eq!(
            problem,
            ManifestProblem::Reserved {
                section: section.to_owned()
            }
        );
    }
}

#[test]
fn text_that_is_not_toml_fails_as_invalid_with_its_line() {
    let (line, problem) = problem("ports = []\n[processes.a\n");
    assert_eq!(line, Some(2));
    assert!(matches!(problem, ManifestProblem::Invalid { .. }));
}

#[test]
fn a_process_without_argv_fails_as_invalid() {
    let (_, problem) = problem("[processes.a]\ncwd = \".\"\n");
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains("argv")));
}

#[test]
fn an_empty_argv_fails_as_invalid_with_its_line() {
    let (line, problem) = problem("[processes.a]\nargv = []\n");
    assert_eq!(line, Some(2));
    assert!(matches!(problem, ManifestProblem::Invalid { .. }));
}

#[test]
fn a_port_variable_that_names_an_undeclared_port_fails_with_its_line() {
    let (line, problem) = problem(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"true\"]\nports = { PORT = \"web\" }\n",
    );
    assert_eq!(line, Some(4));
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains("web")));
}

#[test]
fn a_readiness_check_on_an_undeclared_port_fails() {
    let (_, problem) = problem("[processes.a]\nargv = [\"true\"]\nready = { port = \"api\" }\n");
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains("api")));
}

#[test]
fn a_dependency_on_an_undeclared_process_fails_with_its_line() {
    let (line, problem) = problem("[processes.a]\nargv = [\"true\"]\ndepends_on = [\"b\"]\n");
    assert_eq!(line, Some(3));
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains('b')));
}

#[test]
fn a_dependency_cycle_fails_as_invalid() {
    let (_, problem) = problem(
        "[processes.a]\nargv = [\"true\"]\ndepends_on = [\"b\"]\n[processes.b]\nargv = [\"true\"]\ndepends_on = [\"a\"]\n",
    );
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains("cycle")));
}

#[test]
fn a_process_name_with_a_slash_fails_as_invalid() {
    let (_, problem) = problem("[processes.\"a/b\"]\nargv = [\"true\"]\n");
    assert!(matches!(problem, ManifestProblem::Invalid { .. }));
}

#[test]
fn a_cwd_that_leaves_the_repository_fails_as_invalid() {
    for cwd in ["/tmp", "../other", "web/../../x"] {
        let (line, problem) = problem(&format!(
            "[processes.a]\nargv = [\"true\"]\ncwd = \"{cwd}\"\n"
        ));
        assert_eq!(line, Some(3), "{cwd}");
        assert!(matches!(problem, ManifestProblem::Invalid { .. }), "{cwd}");
    }
}

#[test]
fn a_variable_set_both_in_env_and_in_ports_fails_as_invalid() {
    let (_, problem) = problem(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"true\"]\nenv = { PORT = \"1\" }\nports = { PORT = \"api\" }\n",
    );
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains("PORT")));
}

#[test]
fn a_timeout_of_zero_seconds_fails_as_invalid() {
    let (_, problem) = problem(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"true\"]\nready = { port = \"api\", timeout_seconds = 0 }\n",
    );
    assert!(matches!(problem, ManifestProblem::Invalid { .. }));
}

#[test]
fn a_port_declared_twice_fails_as_invalid() {
    let (line, problem) = problem("ports = [\"api\",\n  \"api\"]\n");
    assert_eq!(line, Some(2));
    assert!(matches!(problem, ManifestProblem::Invalid { .. }));
}

#[test]
fn the_start_order_puts_each_dependency_before_its_dependents() {
    let manifest = parse(APP).unwrap();
    assert_eq!(manifest.start_order(&[]).unwrap(), ["api", "web", "worker"]);
}

#[test]
fn the_start_order_of_one_process_includes_its_dependencies() {
    let manifest = parse(APP).unwrap();
    assert_eq!(
        manifest.start_order(&["web".to_owned()]).unwrap(),
        ["api", "web"]
    );
}

#[test]
fn the_start_order_of_an_unknown_process_returns_its_name() {
    let manifest = parse(APP).unwrap();
    assert_eq!(
        manifest.start_order(&["search".to_owned()]).unwrap_err(),
        "search"
    );
}

#[test]
fn reading_a_checkout_without_a_manifest_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Manifest::read(dir.path()).unwrap().is_none());
}

#[test]
fn a_listener_without_ready_waits_for_its_listen_port_with_the_default_timeout() {
    let manifest = parse(APP).unwrap();
    let ready = manifest.processes["web"].ready.as_ref().unwrap();
    assert_eq!(ready.port, "web");
    assert_eq!(ready.timeout, DEFAULT_READY_TIMEOUT);
    assert_eq!(manifest.processes["worker"].ready, None);
}

#[test]
fn a_listen_port_that_the_process_does_not_receive_fails_with_its_line() {
    let (line, problem) =
        problem("ports = [\"api\"]\n[processes.a]\nargv = [\"a\"]\nlisten = \"api\"\n");
    assert_eq!(line, Some(4));
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains("receive")));
}

#[test]
fn a_port_with_two_listeners_fails_as_invalid() {
    let (_, problem) = problem(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"a\"]\nports = { PORT = \"api\" }\nlisten = \"api\"\n[processes.b]\nargv = [\"b\"]\nports = { PORT = \"api\" }\nlisten = \"api\"\n",
    );
    assert!(
        matches!(&problem, ManifestProblem::Invalid { message } if message.contains("listen")),
        "{problem:?}"
    );
}

#[test]
fn a_ready_port_that_the_process_neither_listens_on_nor_receives_fails() {
    let (line, problem) = problem(
        "ports = [\"api\", \"web\"]\n[processes.a]\nargv = [\"a\"]\nports = { PORT = \"api\" }\nready = { port = \"web\" }\n",
    );
    assert_eq!(line, Some(5));
    assert!(matches!(problem, ManifestProblem::Invalid { .. }));
}

#[test]
fn a_ready_table_without_a_port_or_a_listen_port_fails() {
    let (_, problem) = problem("[processes.a]\nargv = [\"a\"]\nready = { timeout_seconds = 5 }\n");
    assert!(matches!(problem, ManifestProblem::Invalid { .. }));
}

#[test]
fn a_manifest_of_version_1_parses_and_a_higher_version_fails_as_newer() {
    assert!(parse("version = 1\n").is_ok());
    let (line, problem) = problem("\nversion = 2\n");
    assert_eq!(line, Some(2));
    assert_eq!(problem, ManifestProblem::Newer { version: 2 });
    assert!(matches!(
        self::problem("version = 0\n").1,
        ManifestProblem::Invalid { .. }
    ));
}

#[test]
fn a_cwd_that_leaves_the_repository_through_a_symbolic_link_fails_on_read() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("web")).unwrap();
    crate::testing::write(
        dir.path().join("mxv.toml"),
        "[processes.web]\nargv = [\"true\"]\ncwd = \"web\"\n",
    );
    let error = Manifest::read(dir.path()).unwrap_err();
    assert!(
        matches!(&error.problem, ManifestProblem::Invalid { message } if message.contains("symbolic link")),
        "{error:?}"
    );
}
