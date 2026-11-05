use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::manifest::{
    DEFAULT_READY_TIMEOUT, Joined, Manifest, ManifestError, ManifestProblem, Permissions,
    Published, ReadyCheck, Unit,
};

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
    assert_eq!(ready.check, ReadyCheck::Port("api".to_owned()));
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
    let (line, problem) = problem("ports = []\nprocess = 1\n");
    assert_eq!(line, Some(2));
    assert_eq!(
        problem,
        ManifestProblem::UnknownKey {
            key: "process".to_owned()
        }
    );
}

#[test]
fn a_top_level_table_of_another_tool_is_accepted_and_left_alone() {
    let manifest = parse(
        "ports = [\"api\"]\n\n[app]\nname = \"reading-list\"\n\n[needs.db]\nkind = \"postgres\"\n\n[release]\nchannel = \"beta\"\n\n[[steps]]\nrun = \"make\"\n",
    )
    .unwrap();
    assert_eq!(manifest.ports, ["api"]);
    assert!(manifest.processes.is_empty());
}

#[test]
fn an_unknown_top_level_key_that_is_not_a_table_fails_and_names_the_key_and_its_line() {
    let (line, problem) = problem("ports = []\nport = [\"api\"]\n");
    assert_eq!(line, Some(2));
    assert_eq!(
        problem,
        ManifestProblem::UnknownKey {
            key: "port".to_owned()
        }
    );
}

#[test]
fn a_table_named_like_one_of_mxvs_own_belongs_to_another_tool() {
    let manifest = parse("[process.api]\nargv = [\"true\"]\n").unwrap();
    assert!(manifest.processes.is_empty());
}

#[test]
fn phases_process_table_parses_with_a_warning_that_mxv_runs_another_table() {
    let manifest = parse("ports = []\n\n[process.api]\nargv = [\"true\"]\n").unwrap();
    assert_eq!(
        manifest.warnings,
        [
            "/repo/mxv.toml:3: mxv ignores the table `[process.*]`, which is phase's, and runs only the processes of `[processes.*]`"
        ]
    );
}

#[test]
fn a_misspelt_name_of_one_of_mxvs_tables_parses_with_a_warning_that_names_it() {
    for (written, meant) in [
        ("proceses", "processes"),
        ("Processes", "processes"),
        ("container", "containers"),
        ("disposible", "disposable"),
        ("PORTS", "ports"),
    ] {
        let manifest = parse(&format!("[{written}.web]\nx = 1\n")).unwrap();
        assert_eq!(
            manifest.warnings,
            [format!(
                "/repo/mxv.toml:1: mxv ignores the table `[{written}]`, whose name is close to mxv's `{meant}`, and reads nothing from it"
            )],
            "{written}"
        );
    }
}

#[test]
fn the_tables_of_phase_and_of_other_tools_parse_without_a_warning() {
    let manifest = parse(
        "[app]\nname = \"x\"\n[release]\na = 1\n[build]\na = 1\n[needs.db]\na = 1\n[deploy]\na = 1\n",
    )
    .unwrap();
    assert_eq!(manifest.warnings, Vec::<String>::new());
}

#[test]
fn a_file_that_is_not_toml_fails_even_inside_another_tools_table() {
    let (line, problem) = problem("ports = []\n[app]\nname = \n");
    assert_eq!(line, Some(3));
    assert!(
        matches!(problem, ManifestProblem::Invalid { .. }),
        "{problem:?}"
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
    for section in ["secrets", "actions"] {
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
    assert_eq!(ready.check, ReadyCheck::Port("web".to_owned()));
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

const WITH_CONTAINERS: &str = r#"
ports = ["api", "dynamodb", "localstack"]

[containers.dynamodb]
image = "amazon/dynamodb-local:2.5.4"
command = ["-jar", "DynamoDBLocal.jar", "-sharedDb", "-dbPath", "/data"]
ports = { 8000 = "dynamodb" }
volumes = { data = "/data" }

[containers.localstack]
image = "localstack/localstack:4.0.3"
env = { SERVICES = "s3,sqs" }
ports = { 4566 = "localstack" }
ready = { timeout_seconds = 90 }

[processes.api]
argv = ["cargo", "run", "--bin", "api"]
ports = { PORT = "api", DYNAMODB_PORT = "dynamodb", LOCALSTACK_PORT = "localstack" }
listen = "api"
depends_on = ["dynamodb", "localstack"]
"#;

fn container_problem(body: &str) -> (Option<usize>, ManifestProblem) {
    problem(&format!(
        "ports = [\"db\", \"other\"]\n\n[containers.db]\n{body}"
    ))
}

fn invalid_message(body: &str) -> String {
    match container_problem(body).1 {
        ManifestProblem::Invalid { message } => message,
        other => panic!("expected an invalid manifest, got {other:?}"),
    }
}

#[test]
fn a_manifest_of_containers_and_a_process_parses_with_the_containers_defaults() {
    let manifest = parse(WITH_CONTAINERS).unwrap();
    let dynamodb = &manifest.containers["dynamodb"];
    assert_eq!(dynamodb.image, "amazon/dynamodb-local:2.5.4");
    assert_eq!(dynamodb.command.as_ref().unwrap()[0], "-jar");
    assert_eq!(
        dynamodb.ports,
        [Published {
            container: 8000,
            port: "dynamodb".to_owned()
        }]
    );
    assert_eq!(dynamodb.volumes["data"], "/data");
    let ready = dynamodb.ready.as_ref().unwrap();
    assert_eq!(ready.check, ReadyCheck::Port("dynamodb".to_owned()));
    assert_eq!(ready.timeout, DEFAULT_READY_TIMEOUT);
    let localstack = &manifest.containers["localstack"];
    assert!(localstack.command.is_none());
    assert_eq!(localstack.env["SERVICES"], "s3,sqs");
    assert_eq!(
        localstack.ready.as_ref().unwrap().timeout,
        Duration::from_secs(90)
    );
}

#[test]
fn the_start_order_puts_the_containers_that_a_process_depends_on_first() {
    let manifest = parse(WITH_CONTAINERS).unwrap();
    assert_eq!(
        manifest.start_order(&["api".to_owned()]).unwrap(),
        ["dynamodb", "localstack", "api"]
    );
    assert_eq!(
        manifest.unit("dynamodb"),
        Some(Unit::Container(&manifest.containers["dynamodb"]))
    );
    assert_eq!(
        manifest.unit("api"),
        Some(Unit::Process(&manifest.processes["api"]))
    );
    assert_eq!(manifest.unit("search"), None);
}

#[test]
fn a_container_may_depend_on_a_process() {
    let manifest = parse(
        "ports = [\"db\"]\n\n[processes.seed]\nargv = [\"true\"]\n\n[containers.db]\nimage = \"busybox:1.37.0\"\nports = { 80 = \"db\" }\ndepends_on = [\"seed\"]\n",
    )
    .unwrap();
    assert_eq!(manifest.start_order(&[]).unwrap(), ["seed", "db"]);
}

#[test]
fn an_image_without_a_tag_or_with_the_latest_tag_fails_as_unpinned() {
    for image in ["busybox", "busybox:latest", "localhost:5000/busybox"] {
        let message = invalid_message(&format!("image = \"{image}\"\n"));
        assert!(message.contains("pin"), "{image}: {message}");
    }
}

#[test]
fn an_image_pinned_by_digest_or_by_a_registry_and_tag_parses() {
    for image in [
        "busybox@sha256:2919d0172f7524b2d8df9e50066a682669e6d170ac0f6a49676d54358fe970b5",
        "localhost:5000/team/busybox:1.37.0",
    ] {
        let text = format!("[containers.db]\nimage = \"{image}\"\n");
        assert_eq!(parse(&text).unwrap().containers["db"].image, image);
    }
}

#[test]
fn an_image_that_starts_with_a_dash_fails_as_invalid() {
    let message = invalid_message("image = \"--privileged:1\"\n");
    assert!(message.contains("image"), "{message}");
}

#[test]
fn a_container_without_an_image_fails_as_invalid() {
    let (line, problem) = container_problem("ports = { 80 = \"db\" }\n");
    assert_eq!(line, Some(3));
    assert!(matches!(problem, ManifestProblem::Invalid { .. }));
}

#[test]
fn a_container_port_that_is_not_a_number_from_1_to_65535_fails_with_its_line() {
    for key in ["0", "65536", "http"] {
        let (line, problem) = container_problem(&format!(
            "image = \"busybox:1.37.0\"\nports = {{ {key} = \"db\" }}\n"
        ));
        assert_eq!(line, Some(5), "{key}");
        assert!(matches!(problem, ManifestProblem::Invalid { .. }), "{key}");
    }
}

#[test]
fn a_container_that_publishes_an_undeclared_port_fails_with_its_line() {
    let (line, _) = container_problem("image = \"busybox:1.37.0\"\nports = { 80 = \"nope\" }\n");
    assert_eq!(line, Some(5));
}

#[test]
fn a_port_that_a_container_publishes_and_a_process_listens_on_fails_as_two_listeners() {
    let message = match problem(
        "ports = [\"db\"]\n\n[containers.db]\nimage = \"busybox:1.37.0\"\nports = { 80 = \"db\" }\n\n[processes.api]\nargv = [\"true\"]\nports = { PORT = \"db\" }\nlisten = \"db\"\n",
    )
    .1
    {
        ManifestProblem::Invalid { message } => message,
        other => panic!("{other:?}"),
    };
    assert!(message.contains("at most one listener"), "{message}");
}

#[test]
fn a_container_and_a_process_with_one_name_fail_as_invalid() {
    let message = match problem(
        "[containers.db]\nimage = \"busybox:1.37.0\"\n\n[processes.db]\nargv = [\"true\"]\n",
    )
    .1
    {
        ManifestProblem::Invalid { message } => message,
        other => panic!("{other:?}"),
    };
    assert!(
        message.contains("both a process and a container"),
        "{message}"
    );
}

#[test]
fn a_volume_path_that_is_not_absolute_or_holds_a_colon_fails_as_invalid() {
    for path in ["data", "/data:ro", "/data,x"] {
        let message = invalid_message(&format!(
            "image = \"busybox:1.37.0\"\nvolumes = {{ data = \"{path}\" }}\n"
        ));
        assert!(message.contains("volume"), "{path}: {message}");
    }
}

#[test]
fn a_volume_name_with_a_slash_fails_as_invalid() {
    let message =
        invalid_message("image = \"busybox:1.37.0\"\nvolumes = { \"a/b\" = \"/data\" }\n");
    assert!(message.contains("volume"), "{message}");
}

#[test]
fn a_ready_table_of_a_container_that_publishes_two_ports_must_name_one() {
    let message = invalid_message(
        "image = \"busybox:1.37.0\"\nports = { 80 = \"db\", 81 = \"other\" }\nready = { timeout_seconds = 5 }\n",
    );
    assert!(message.contains("ready"), "{message}");
    let manifest = parse(
        "ports = [\"db\", \"other\"]\n\n[containers.db]\nimage = \"busybox:1.37.0\"\nports = { 80 = \"db\", 81 = \"other\" }\nready = { port = \"other\" }\n",
    )
    .unwrap();
    assert_eq!(
        manifest.containers["db"].ready.as_ref().unwrap().check,
        ReadyCheck::Port("other".to_owned())
    );
}

#[test]
fn a_container_ready_port_that_it_does_not_publish_fails() {
    let message = invalid_message(
        "image = \"busybox:1.37.0\"\nports = { 80 = \"db\" }\nready = { port = \"other\" }\n",
    );
    assert!(message.contains("publish"), "{message}");
}

#[test]
fn a_container_that_publishes_no_port_has_no_readiness_check() {
    let manifest = parse("[containers.db]\nimage = \"busybox:1.37.0\"\n").unwrap();
    assert!(manifest.containers["db"].ready.is_none());
}

#[test]
fn an_unknown_key_of_a_container_fails_and_names_the_key() {
    let (_, problem) = container_problem("image = \"busybox:1.37.0\"\nbinds = []\n");
    assert_eq!(
        problem,
        ManifestProblem::UnknownKey {
            key: "binds".to_owned()
        }
    );
}

#[test]
fn a_container_name_or_a_volume_name_with_a_dot_fails_as_invalid() {
    let (_, problem) = problem("[containers.\"db.x\"]\nimage = \"busybox:1.37.0\"\n");
    assert!(
        matches!(problem, ManifestProblem::Invalid { .. }),
        "{problem:?}"
    );
    let message =
        invalid_message("image = \"busybox:1.37.0\"\nvolumes = { \"data.x\" = \"/data\" }\n");
    assert!(message.contains("volume"), "{message}");
}

#[test]
fn a_container_ready_table_with_a_log_pattern_fails_as_invalid() {
    let message = invalid_message(
        "image = \"busybox:1.37.0\"\nports = { 80 = \"db\" }\nready = { log = \"ready\" }\n",
    );
    assert!(message.contains("`log`"), "{message}");
}

#[test]
fn an_env_value_names_host_ports_with_port_placeholders_and_doubles_braces_for_literal_ones() {
    let manifest = parse(
        "ports = [\"api\", \"db\"]\n[processes.a]\nargv = [\"true\"]\nenv = { API_URL = \"http://127.0.0.1:{port.api}/v1\", DB = \"{port.db}\", JSON = \"{{\\\"a\\\": 1}}\", PLAIN = \"info\" }\n",
    )
    .unwrap();
    let env = &manifest.processes["a"].env;
    let ports = BTreeMap::from([("api".to_owned(), 24001), ("db".to_owned(), 24002)]);
    assert_eq!(env["API_URL"].render(&ports), "http://127.0.0.1:24001/v1");
    assert_eq!(env["DB"].render(&ports), "24002");
    assert_eq!(env["JSON"].render(&ports), "{\"a\": 1}");
    assert_eq!(env["PLAIN"].render(&ports), "info");
    assert_eq!(env["PLAIN"], "info");
    assert_eq!(env["API_URL"].ports().collect::<Vec<_>>(), ["api"]);
}

#[test]
fn a_port_placeholder_that_names_an_undeclared_port_fails_with_its_line() {
    let (line, problem) = problem(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"true\"]\nenv = { URL = \"127.0.0.1:{port.web}\" }\n",
    );
    assert_eq!(line, Some(4));
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains("`web`")));
}

#[test]
fn a_brace_that_opens_no_port_placeholder_fails_with_its_line() {
    for value in ["{api}", "{port.api", "a}b", "{port.}", "{ port.api }"] {
        let (line, problem) = problem(&format!(
            "ports = [\"api\"]\n[processes.a]\nargv = [\"true\"]\nenv = {{ URL = \"{value}\" }}\n"
        ));
        assert_eq!(line, Some(4), "{value}");
        assert!(
            matches!(&problem, ManifestProblem::Invalid { message } if message.contains("{{") && message.contains("`URL`")),
            "{value}: {problem:?}"
        );
    }
}

#[test]
fn a_readiness_check_on_a_log_pattern_needs_no_port() {
    let manifest = parse(
        "[processes.worker]\nargv = [\"cargo\", \"run\", \"--bin\", \"worker\"]\nready = { log = \"^worker ready\", timeout_seconds = 300 }\n",
    )
    .unwrap();
    let ready = manifest.processes["worker"].ready.as_ref().unwrap();
    assert_eq!(ready.timeout, Duration::from_secs(300));
    match &ready.check {
        ReadyCheck::Log(pattern) => {
            assert_eq!(pattern.as_str(), "^worker ready");
            assert!(pattern.is_match("worker ready on queue jobs"));
            assert!(!pattern.is_match("compiling worker ready"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_readiness_check_with_both_a_port_and_a_log_pattern_fails_with_its_line() {
    let (line, problem) = problem(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"true\"]\nports = { PORT = \"api\" }\nready = { port = \"api\", log = \"ready\" }\n",
    );
    assert_eq!(line, Some(5));
    assert!(matches!(problem, ManifestProblem::Invalid { message } if message.contains("log")));
}

#[test]
fn a_log_pattern_that_is_not_a_regular_expression_fails_with_its_line() {
    let (line, problem) = problem("[processes.a]\nargv = [\"true\"]\nready = { log = \"(\" }\n");
    assert_eq!(line, Some(3));
    assert!(
        matches!(&problem, ManifestProblem::Invalid { message } if message.contains("regular expression") && message.contains("unclosed group") && !message.contains('\n')),
        "{problem:?}"
    );
}

#[test]
fn the_env_values_of_processes_and_containers_print_as_redacted_and_render_for_the_environment() {
    let manifest = parse(
        "ports = [\"api\"]\n[processes.a]\nargv = [\"true\"]\nenv = { TOKEN = \"s3cret-{port.api}\" }\n[containers.db]\nimage = \"busybox:1.37.0\"\nenv = { PASSWORD = \"hunter2-secret\" }\n",
    )
    .unwrap();
    let printed = format!("{manifest:?}");
    assert!(!printed.contains("s3cret"), "{printed}");
    assert!(!printed.contains("hunter2"), "{printed}");
    assert!(printed.contains("<redacted>"), "{printed}");
    let token = &manifest.processes["a"].env["TOKEN"];
    assert_eq!(format!("{token:?}"), "<redacted>");
    let ports = BTreeMap::from([("api".to_owned(), 24001)]);
    let rendered = token.render(&ports);
    assert_eq!(format!("{rendered:?}"), "<redacted>");
    assert_eq!(rendered.reveal(), "s3cret-24001");
    assert_eq!(
        manifest.containers["db"].env["PASSWORD"].reveal(),
        "hunter2-secret"
    );
}

#[test]
fn a_manifest_declares_disposable_paths_and_directories_to_clone_as_globs() {
    let manifest = parse(
        r#"
disposable = ["web/test-results/", "coverage/"]
clone_on_new = ["node_modules/", "web/.venv/"]
"#,
    )
    .unwrap();
    let disposable: Vec<&str> = manifest
        .disposable
        .iter()
        .map(|glob| glob.as_str())
        .collect();
    assert_eq!(disposable, ["web/test-results/", "coverage/"]);
    let cloned: Vec<&str> = manifest
        .clone_on_new
        .iter()
        .map(|glob| glob.as_str())
        .collect();
    assert_eq!(cloned, ["node_modules/", "web/.venv/"]);
}

#[test]
fn a_manifest_without_globs_declares_no_disposable_paths_and_no_directories_to_clone() {
    let manifest = parse("").unwrap();
    assert!(manifest.disposable.is_empty());
    assert!(manifest.clone_on_new.is_empty());
}

#[test]
fn an_absolute_disposable_glob_fails_as_invalid_with_its_line() {
    let (line, problem) =
        problem("ports = []\ndisposable = [\n  \"web/\",\n  \"/tmp/cache/\",\n]\n");
    assert_eq!(line, Some(4));
    let ManifestProblem::Invalid { message } = problem else {
        panic!("{problem:?}");
    };
    assert!(message.contains("/tmp/cache/"), "{message}");
    assert!(message.contains("absolute"), "{message}");
}

#[test]
fn a_disposable_glob_that_leaves_the_repository_fails_as_invalid_with_its_line() {
    let (line, problem) = problem("disposable = [\"../other/target/\"]\n");
    assert_eq!(line, Some(1));
    assert!(
        matches!(problem, ManifestProblem::Invalid { .. }),
        "{problem:?}"
    );
}

#[test]
fn a_clone_glob_that_names_target_fails_as_invalid_with_its_line() {
    let (line, problem) = problem("\nclone_on_new = [\"target/\"]\n");
    assert_eq!(line, Some(2));
    let ManifestProblem::Invalid { message } = problem else {
        panic!("{problem:?}");
    };
    assert!(message.contains("target"), "{message}");
}

#[test]
fn a_clone_glob_that_leaves_the_repository_fails_as_invalid() {
    let (_, problem) = problem("clone_on_new = [\"../node_modules/\"]\n");
    assert!(
        matches!(problem, ManifestProblem::Invalid { .. }),
        "{problem:?}"
    );
}

#[test]
fn a_disposable_glob_that_could_match_any_ignored_path_fails_as_invalid_with_its_line() {
    for glob in ["*", "**", "**/*", "*/", "?*", ".env"] {
        let (line, problem) = problem(&format!("ports = []\ndisposable = [{glob:?}]\n"));
        assert_eq!(line, Some(2), "{glob}");
        let ManifestProblem::Invalid { message } = problem else {
            panic!("{glob}: {problem:?}");
        };
        assert!(message.contains("directory"), "{message}");
        assert!(
            message.contains("only the config file's `disposable`"),
            "{message}"
        );
    }
}

#[test]
fn a_clone_glob_that_could_match_any_ignored_directory_fails_as_invalid() {
    let (line, problem) = problem("clone_on_new = [\"*/\"]\n");
    assert_eq!(line, Some(1));
    let ManifestProblem::Invalid { message } = problem else {
        panic!("{problem:?}");
    };
    assert!(!message.contains("config file"), "{message}");
}

/// The manifest of an application whose four processes each run under the
/// sandbox, with the containers that two of them use.
const SANDBOXED: &str = r#"
ports = ["auth", "api", "web", "dynamodb", "localstack"]

[containers.dynamodb]
image = "amazon/dynamodb-local:2.5.4"
ports = { 8000 = "dynamodb" }

[containers.localstack]
image = "localstack/localstack:4.0.3"
ports = { 4566 = "localstack" }

[processes.auth]
argv = ["cargo", "run", "--bin", "auth"]
ports = { PORT = "auth" }
listen = "auth"
permissions = { write = ["target"], listen = ["auth"] }

[processes.server]
argv = ["cargo", "run", "--bin", "server"]
ports = { PORT = "api", AUTH_PORT = "auth", DYNAMODB_PORT = "dynamodb", LOCALSTACK_PORT = "localstack" }
listen = "api"
depends_on = ["auth", "dynamodb", "localstack"]

[processes.server.permissions]
write = ["target"]
listen = ["api"]
connect = ["auth", "dynamodb", "localstack"]

[processes.worker]
argv = ["cargo", "run", "--bin", "worker"]
ports = { DYNAMODB_PORT = "dynamodb", LOCALSTACK_PORT = "localstack" }
ready = { log = "^worker ready" }
depends_on = ["dynamodb", "localstack"]
permissions = { write = ["target"], connect = ["dynamodb", "localstack"] }

[processes.web]
argv = ["pnpm", "run", "dev"]
cwd = "web"
ports = { PORT = "web", API_PORT = "api", AUTH_PORT = "auth" }
listen = "web"
depends_on = ["server", "auth"]
permissions = { write = ["web/node_modules/.vite", "web/dist"], listen = ["web"], connect = ["server", "auth"] }
"#;

/// Returns a manifest whose process `app` listens on `api`, receives
/// `other` and depends on the container `db`, with the permissions table
/// `permissions`, which starts on line 12.
fn with_permissions(permissions: &str) -> String {
    format!(
        "ports = [\"api\", \"other\", \"db\", \"spare\"]\n\n[containers.db]\nimage = \"postgres:17.2\"\nports = {{ 5432 = \"db\" }}\n\n[processes.app]\nargv = [\"true\"]\nports = {{ PORT = \"api\", OTHER_PORT = \"other\" }}\nlisten = \"api\"\ndepends_on = [\"db\"]\n{permissions}"
    )
}

/// Returns the problem of the manifest of [`with_permissions`].
fn permissions_problem(permissions: &str) -> (Option<usize>, ManifestProblem) {
    problem(&with_permissions(permissions))
}

/// Returns the message of the invalid manifest of [`permissions_problem`].
fn permissions_message(permissions: &str) -> (Option<usize>, String) {
    match permissions_problem(permissions) {
        (line, ManifestProblem::Invalid { message }) => (line, message),
        other => panic!("expected an invalid manifest, got {other:?}"),
    }
}

#[test]
fn a_process_with_permissions_parses_its_writes_listen_ports_and_connections() {
    let manifest = parse(SANDBOXED).unwrap();
    let server = manifest.processes["server"].permissions.as_ref().unwrap();
    assert_eq!(server.write, BTreeSet::from(["target".to_owned()]));
    assert_eq!(server.listen, BTreeSet::from(["api".to_owned()]));
    assert_eq!(
        server.connect,
        BTreeSet::from(["auth", "dynamodb", "localstack"].map(str::to_owned))
    );
    let web = manifest.processes["web"].permissions.as_ref().unwrap();
    assert_eq!(
        web.write,
        BTreeSet::from(["web/node_modules/.vite", "web/dist"].map(str::to_owned))
    );
    assert_eq!(
        web.connect,
        BTreeSet::from(["server", "auth"].map(str::to_owned))
    );
    let worker = manifest.processes["worker"].permissions.as_ref().unwrap();
    assert!(worker.listen.is_empty());
}

#[test]
fn an_empty_permissions_table_grants_nothing_and_an_absent_one_leaves_the_process_unsandboxed() {
    let manifest = parse(
        "[processes.a]\nargv = [\"true\"]\n\n[processes.a.permissions]\n\n[processes.b]\nargv = [\"true\"]\n",
    )
    .unwrap();
    assert_eq!(
        manifest.processes["a"].permissions,
        Some(Permissions::default())
    );
    assert_eq!(manifest.processes["b"].permissions, None);
}

#[test]
fn a_write_path_may_be_state() {
    let manifest = parse(
        "[processes.a]\nargv = [\"true\"]\npermissions = { write = [\"$STATE\", \"./out\"] }\n",
    )
    .unwrap();
    assert_eq!(
        manifest.processes["a"].permissions.as_ref().unwrap().write,
        BTreeSet::from(["$STATE", "./out"].map(str::to_owned))
    );
}

#[test]
fn a_write_path_that_is_absolute_leaves_the_checkout_or_names_all_of_it_fails_with_its_line() {
    for (path, says) in [
        ("/tmp/out", "absolute"),
        ("../other", "`..`"),
        ("web/../target", "`..`"),
        ("", "empty"),
        (".", "whole checkout"),
        ("$HOME/out", "$STATE"),
        ("out\\\\dir", "backslash"),
    ] {
        let (line, message) = permissions_message(&format!(
            "\n[processes.app.permissions]\nwrite = [{path:?}]\n"
        ));
        assert_eq!(line, Some(14), "{path}");
        assert!(message.contains("`app`"), "{path}: {message}");
        assert!(message.contains(says), "{path}: {message}");
    }
}

#[test]
fn a_listen_port_that_the_process_neither_listens_on_nor_receives_fails_with_its_line() {
    let (line, message) = permissions_message("permissions = { listen = [\"api\", \"spare\"] }\n");
    assert_eq!(line, Some(12));
    assert!(message.contains("`spare`"), "{message}");
    assert!(message.contains("receive"), "{message}");
}

#[test]
fn a_listen_port_that_the_manifest_does_not_declare_fails_with_its_line() {
    let (line, message) = permissions_message("permissions = { listen = [\"api\", \"nope\"] }\n");
    assert_eq!(line, Some(12));
    assert!(message.contains("`nope`"), "{message}");
    assert!(message.contains("does not declare"), "{message}");
}

#[test]
fn a_port_that_the_process_receives_may_be_a_listen_port_of_its_permissions() {
    let manifest = parse(&with_permissions(
        "permissions = { listen = [\"api\", \"other\"] }\n",
    ))
    .unwrap();
    assert_eq!(
        manifest.processes["app"]
            .permissions
            .as_ref()
            .unwrap()
            .listen,
        BTreeSet::from(["api", "other"].map(str::to_owned))
    );
}

#[test]
fn a_sandboxed_process_whose_permissions_do_not_name_its_listen_port_fails_with_its_line() {
    let (line, message) = permissions_message("permissions = { listen = [\"other\"] }\n");
    assert_eq!(line, Some(12));
    assert!(message.contains("listens on `api`"), "{message}");
    assert!(message.contains("listen = [\"api\"]"), "{message}");
}

#[test]
fn a_connection_to_a_unit_that_the_manifest_does_not_declare_fails_with_its_line() {
    let (line, message) = permissions_message(
        "permissions = { listen = [\"api\"], connect = [\"db\", \"example.com\"] }\n",
    );
    assert_eq!(line, Some(12));
    assert!(message.contains("`example.com`"), "{message}");
    assert!(message.contains("external host"), "{message}");
}

#[test]
fn a_connection_to_a_unit_that_the_process_does_not_depend_on_fails_and_says_to_depend_on_it() {
    let text = "ports = [\"api\"]\n\n[processes.api]\nargv = [\"true\"]\nports = { PORT = \"api\" }\nlisten = \"api\"\n\n[processes.web]\nargv = [\"true\"]\npermissions = { connect = [\"api\"] }\n";
    let (line, problem) = problem(text);
    assert_eq!(line, Some(10));
    let ManifestProblem::Invalid { message } = problem else {
        panic!("{problem:?}");
    };
    assert!(message.contains("depends_on"), "{message}");
    assert!(message.contains("`api`"), "{message}");
}

#[test]
fn secrets_in_a_processs_permissions_fails_as_an_unknown_key() {
    let (line, problem) =
        permissions_problem("\n[processes.app.permissions]\nsecrets = [\"TOKEN\"]\n");
    assert_eq!(line, Some(14));
    assert_eq!(
        problem,
        ManifestProblem::UnknownKey {
            key: "secrets".to_owned()
        }
    );
}

#[test]
fn permissions_of_a_container_fail_with_its_line_and_say_that_only_processes_take_them() {
    let (line, message) = match container_problem(
        "image = \"postgres:17.2\"\npermissions = { write = [\"data\"] }\n",
    ) {
        (line, ManifestProblem::Invalid { message }) => (line, message),
        other => panic!("{other:?}"),
    };
    assert_eq!(line, Some(5));
    assert!(message.contains("`db`"), "{message}");
    assert!(message.contains("host process"), "{message}");
}

#[test]
fn a_top_level_permissions_table_fails_with_its_line_and_names_the_processs_table() {
    let (line, problem) = problem("ports = []\n\n[permissions]\nwrite = [\"target\"]\n");
    assert_eq!(line, Some(3));
    let ManifestProblem::Invalid { message } = problem else {
        panic!("{problem:?}");
    };
    assert!(
        message.contains("[processes.<name>.permissions]"),
        "{message}"
    );
}

#[test]
fn renaming_a_manifest_renames_the_listen_ports_and_connections_of_its_permissions() {
    let renamed = parse(SANDBOXED)
        .unwrap()
        .renamed(&|name| format!("inbox-{name}"));
    let server = renamed.processes["inbox-server"]
        .permissions
        .as_ref()
        .unwrap();
    assert_eq!(server.write, BTreeSet::from(["target".to_owned()]));
    assert_eq!(server.listen, BTreeSet::from(["inbox-api".to_owned()]));
    assert_eq!(
        server.connect,
        BTreeSet::from(["inbox-auth", "inbox-dynamodb", "inbox-localstack"].map(str::to_owned))
    );
}

#[test]
fn a_listen_permission_on_another_processs_listen_port_fails_with_its_line() {
    let text = "ports = [\"api\", \"web\"]\n\n[processes.api]\nargv = [\"true\"]\nports = { PORT = \"api\" }\nlisten = \"api\"\n\n[processes.web]\nargv = [\"true\"]\nports = { PORT = \"web\", API_PORT = \"api\" }\nlisten = \"web\"\ndepends_on = [\"api\"]\npermissions = { listen = [\"web\", \"api\"] }\n";
    let (line, problem) = problem(text);
    assert_eq!(line, Some(13));
    let ManifestProblem::Invalid { message } = problem else {
        panic!("{problem:?}");
    };
    assert!(
        message.contains("`api` listens on or publishes"),
        "{message}"
    );
}

#[test]
fn a_listen_permission_on_a_containers_published_port_fails_with_its_line() {
    let text = "ports = [\"db\"]\n\n[containers.db]\nimage = \"postgres:17.2\"\nports = { 5432 = \"db\" }\n\n[processes.app]\nargv = [\"true\"]\nports = { DB_PORT = \"db\" }\ndepends_on = [\"db\"]\npermissions = { listen = [\"db\"] }\n";
    let (line, problem) = problem(text);
    assert_eq!(line, Some(11));
    let ManifestProblem::Invalid { message } = problem else {
        panic!("{problem:?}");
    };
    assert!(
        message.contains("`db` listens on or publishes"),
        "{message}"
    );
}

#[test]
fn a_listen_permission_on_a_port_that_a_joined_unit_listens_on_fails_with_its_line() {
    let joined = Joined {
        ports: vec!["api-http".to_owned()],
        units: vec!["api-server".to_owned()],
        listened: vec![("api-http".to_owned(), "api-server".to_owned())],
        checkouts: vec!["api".to_owned()],
        services: BTreeMap::new(),
    };
    let error = Manifest::parse_joining(
        "[processes.proxy]\nargv = [\"true\"]\nports = { UPSTREAM = \"api-http\" }\npermissions = { listen = [\"api-http\"] }\n",
        Path::new("/ws/mxv.toml"),
        &joined,
    )
    .unwrap_err();
    assert_eq!(error.line, Some(4));
    let ManifestProblem::Invalid { message } = error.problem else {
        panic!("{:?}", error.problem);
    };
    assert!(
        message.contains("`api-server` listens on or publishes"),
        "{message}"
    );
}

#[test]
fn a_write_path_inside_the_checkouts_git_fails_with_its_line_in_any_case() {
    for path in [".git", ".git/hooks", "./.GIT/x", ".Git"] {
        let (line, message) = permissions_message(&format!(
            "\n[processes.app.permissions]\nwrite = [{path:?}]\nlisten = [\"api\"]\n"
        ));
        assert_eq!(line, Some(14), "{path}");
        assert!(message.contains("`.git`"), "{path}: {message}");
    }
}

/// Returns the problem of a workspace manifest of the checkouts `api` and
/// `web` whose process `proxy` runs in `api` and may write `write`.
fn workspace_write_message(write: &str) -> String {
    let joined = Joined {
        checkouts: vec!["api".to_owned(), "web".to_owned()],
        ..Joined::default()
    };
    let error = Manifest::parse_joining(
        &format!("[processes.proxy]\nargv = [\"true\"]\ncwd = \"api\"\npermissions = {{ write = [{write:?}] }}\n"),
        Path::new("/ws/mxv.toml"),
        &joined,
    )
    .unwrap_err();
    assert_eq!(error.line, Some(4), "{write}");
    match error.problem {
        ManifestProblem::Invalid { message } => message,
        other => panic!("{write}: {other:?}"),
    }
}

#[test]
fn a_workspace_manifests_write_path_that_names_a_whole_checkout_fails_with_its_line() {
    let message = workspace_write_message("api");
    assert!(message.contains("whole checkout `api`"), "{message}");
    assert!(
        message.contains("relative to the workspace's directory"),
        "{message}"
    );
    assert!(!message.contains("relative to the checkout"), "{message}");
}

#[test]
fn a_workspace_manifests_write_path_outside_every_checkout_fails_with_its_line() {
    let message = workspace_write_message("notes/out");
    assert!(
        message.contains("none of the workspace's checkouts"),
        "{message}"
    );
    assert!(message.contains("`api`, `web`"), "{message}");
}

#[test]
fn a_workspace_manifests_write_path_inside_a_checkout_parses() {
    let joined = Joined {
        checkouts: vec!["api".to_owned(), "web".to_owned()],
        ..Joined::default()
    };
    let manifest = Manifest::parse_joining(
        "[processes.proxy]\nargv = [\"true\"]\ncwd = \"api\"\npermissions = { write = [\"api/target\"] }\n",
        Path::new("/ws/mxv.toml"),
        &joined,
    )
    .unwrap();
    assert_eq!(
        manifest.processes["proxy"]
            .permissions
            .as_ref()
            .unwrap()
            .write,
        BTreeSet::from(["api/target".to_owned()])
    );
}
