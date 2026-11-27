//! End-to-end tests of the provided variables of a process's starmap
//! file. Each test builds two repositories, `catalogue` and `shelf`, each
//! with a bare origin of its own, in a temporary directory, and runs the
//! built `mxv` binary with its own state directory and workspace root
//! there. The catalogue's process `api` listens on its port `http`, and the
//! shelf's `starmap.toml` binds variables to that port.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the real clock and the files and directories that they and the code under test write"
)]

#[path = "support/grants.rs"]
mod grants;
#[path = "support/git.rs"]
#[expect(
    dead_code,
    reason = "these tests make their repositories with their files in one commit, and the helper also makes an empty one"
)]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git};
use crate::test_ports::PortBlock;

/// The catalogue's manifest: `api` listens on `http` and is ready once it
/// prints `ready`.
const CATALOGUE: &str = "ports = [\"http\"]\n\n[processes.api]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nports = { PORT = \"http\" }\nlisten = \"http\"\nready = { log = \"^ready$\" }\n";

/// The shelf's manifest: `web` writes its environment to `seen.txt` and is
/// ready once it prints `ready`, and the task `check` exits 0.
const SHELF: &str = "[processes.web]\nargv = [\"sh\", \"-c\", \"env > seen.txt; echo ready; exec sleep 60\"]\nready = { log = \"^ready$\" }\n\n[tasks.check]\nargv = [\"true\"]\n";

/// A temporary directory with the repositories `catalogue` and `shelf`, a
/// workspace root, a state directory and a block of host ports.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The host ports that `mxv up` allocates from.
    ports: PortBlock,
    /// The workspaces other than `triage` that the test created, which
    /// the setup takes down when it drops.
    others: std::cell::RefCell<Vec<String>>,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let setup = Self {
            dir,
            ports: PortBlock::claim(),
            others: std::cell::RefCell::default(),
        };
        let config = setup.root().join("config/mxv/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        write_text(&config, &setup.ports.config_line());
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Makes the clone `<name>` of a bare origin `<name>.git` of its own,
    /// whose one commit on `main` holds `files`, each a path and its text.
    fn repository(&self, name: &str, files: &[(&str, &str)]) {
        let root = self.root();
        let seed = root.join(format!("{name}-seed"));
        fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        for (path, text) in files {
            write_text(&seed.join(path), text);
            git(&seed, &["add", path]);
        }
        git(
            &seed,
            &["commit", "--quiet", "--allow-empty", "-m", "Start"],
        );
        let origin = format!("{name}.git");
        let seed_name = format!("{name}-seed");
        git(&root, &["clone", "--quiet", "--bare", &seed_name, &origin]);
        fs::remove_dir_all(&seed).unwrap();
        git(&root, &["clone", "--quiet", &origin, name]);
        git(
            &root.join(name),
            &["remote", "set-url", "origin", &format!("../{origin}")],
        );
    }

    /// Returns the command `mxv` with `args`, a home directory, base
    /// directories and a workspace root of its own.
    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        grants::before(&root.join("state/mxv"), args);
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
    }

    /// Runs `mxv` with `args`, and returns the exit code and standard
    /// error.
    fn text(&self, args: &[&str]) -> (i32, String) {
        let output = self.command(args).output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    /// Runs `mxv` with `args` and `--json`, and returns the exit code and
    /// the JSON document.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command(args).arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    /// Creates the workspace `triage` with a checkout of `catalogue` and
    /// one of `shelf`, whose `starmap.toml` holds `spec`, and returns the
    /// workspace's directory.
    fn workspace(&self, spec: &str) -> PathBuf {
        self.repository("catalogue", &[("mxv.toml", CATALOGUE)]);
        self.repository("shelf", &[("mxv.toml", SHELF), ("starmap.toml", spec)]);
        let (exit, document) =
            self.json(&["new", "triage", "--repo", "catalogue", "--repo", "shelf"]);
        assert_eq!(exit, 0, "{document}");
        self.root().join("workspaces/triage")
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.json(&["down", "-w", "triage"]);
        for name in self.others.borrow().iter() {
            let _ = self.json(&["down", "-w", name]);
        }
    }
}

/// Writes `text` to the file at `path`, as a person editing a file by hand
/// would.
fn write_text(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write by hand"
    )]
    fs::write(path, text).unwrap();
}

/// Returns a starmap file entry that declares the required variable `name`
/// as provided with `bind`.
fn provided(name: &str, bind: &str) -> String {
    format!(
        "[variables.{name}]\npurpose = \"A value of the catalogue.\"\ntype = \"string\"\nunset = \"required\"\nshow = \"shown\"\nsource = \"provided\"\nbind = {bind}\n\n"
    )
}

/// Returns the variables of the environment that a process wrote to
/// `file`. The process writes the file before it prints the line that
/// makes it ready, so the file is complete once `mxv up` returns.
fn seen(file: &Path) -> Vec<(String, String)> {
    fs::read_to_string(file)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

#[test]
fn up_gives_a_process_the_url_and_port_of_another_checkouts_unit_that_its_starmap_file_binds() {
    let setup = Setup::new();
    let spec = provided(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
    ) + &provided(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue/api" }"#,
    );
    let workspace = setup.workspace(&spec);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, shown) = setup.json(&["show", "triage"]);
    assert_eq!(exit, 0, "{shown}");
    let port = shown["data"][0]["ports"]["catalogue-http"]
        .as_u64()
        .unwrap_or_else(|| panic!("no port catalogue-http: {shown}"));
    let vars = seen(&workspace.join("shelf/seen.txt"));
    let value = |name: &str| {
        vars.iter()
            .find(|(variable, _)| variable == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("no {name}: {vars:?}"))
    };
    assert_eq!(value("CATALOGUE_PORT"), port.to_string());
    assert_eq!(value("CATALOGUE_URL"), format!("http://127.0.0.1:{port}"));
}

#[test]
fn up_refuses_a_bind_to_a_unit_that_does_not_exist_before_any_process_starts() {
    let setup = Setup::new();
    let workspace = setup.workspace(&provided(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/search" }"#,
    ));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_ne!(exit, 0, "{document}");
    assert_eq!(
        document["code"], "mxv::manifest::provided_unknown",
        "{document}"
    );
    assert_eq!(document["fields"]["unit"], "shelf-web", "{document}");
    assert_eq!(
        document["fields"]["variable"], "CATALOGUE_URL",
        "{document}"
    );
    assert_eq!(document["fields"]["app"], "catalogue/search", "{document}");
    assert_eq!(document["fields"]["port"], "listen", "{document}");
    let message = document["message"].as_str().unwrap();
    assert!(message.contains("`catalogue/search`"), "{document}");
    assert!(
        message.contains("the listen port of `catalogue/search`"),
        "{document}"
    );
    assert!(!workspace.join("shelf/seen.txt").exists());
}

#[test]
fn up_refuses_a_bind_to_a_port_that_the_unit_does_not_have_and_names_both() {
    let setup = Setup::new();
    setup.workspace(&provided(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/api", port = "grpc" }"#,
    ));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_ne!(exit, 0, "{document}");
    assert_eq!(
        document["code"], "mxv::manifest::provided_unknown",
        "{document}"
    );
    assert_eq!(document["fields"]["app"], "catalogue/api", "{document}");
    assert_eq!(document["fields"]["port"], "grpc", "{document}");
    let message = document["message"].as_str().unwrap();
    assert!(message.contains("`catalogue/api`"), "{document}");
    assert!(message.contains("`grpc`"), "{document}");
}

#[test]
fn up_refuses_a_starmap_file_that_does_not_parse() {
    let setup = Setup::new();
    setup.workspace("[variables.BROKEN]\npurpose = 3\n");
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_ne!(exit, 0, "{document}");
    assert_eq!(
        document["code"], "mxv::manifest::starmap_invalid",
        "{document}"
    );
    assert!(
        document["fields"]["file"]
            .as_str()
            .unwrap()
            .ends_with("shelf/starmap.toml"),
        "{document}"
    );
}

/// Returns the starmap file that binds `CATALOGUE_URL` and
/// `CATALOGUE_PORT` to the catalogue's api.
fn both_binds() -> String {
    provided(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
    ) + &provided(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue/api" }"#,
    )
}

#[test]
fn restart_gives_the_process_its_provided_variables_again() {
    let setup = Setup::new();
    let workspace = setup.workspace(&both_binds());
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (_, shown) = setup.json(&["show", "triage"]);
    let port = shown["data"][0]["ports"]["catalogue-http"]
        .as_u64()
        .unwrap();
    let file = workspace.join("shelf/seen.txt");
    fs::remove_file(&file).unwrap();
    let (exit, document) = setup.json(&["restart", "-w", "triage", "shelf-web"]);
    assert_eq!(exit, 0, "{document}");
    let vars = seen(&file);
    assert!(
        vars.contains(&("CATALOGUE_PORT".to_owned(), port.to_string())),
        "{vars:?}"
    );
}

#[test]
fn run_of_a_task_succeeds_when_every_bind_names_a_port_of_the_workspace() {
    let setup = Setup::new();
    setup.workspace(&both_binds());
    let (exit, stderr) = setup.text(&["run", "-w", "triage", "check"]);
    assert_eq!(exit, 0, "{stderr}");
}

#[test]
fn run_of_a_task_refuses_a_bind_to_a_unit_that_does_not_exist() {
    let setup = Setup::new();
    setup.workspace(&provided(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/search" }"#,
    ));
    let (exit, stderr) = setup.text(&["run", "-w", "triage", "check"]);
    assert_ne!(exit, 0, "{stderr}");
    assert!(
        stderr.contains("code:  mxv::manifest::provided_unknown"),
        "{stderr}"
    );
}

/// Returns the manifest of a neighbour whose process `unit` listens on
/// `http`, writes its environment to `seen.txt` and is ready once it
/// prints `ready`. Its task `check` prints `checked` on standard error.
fn neighbour(unit: &str) -> String {
    format!(
        "ports = [\"http\"]\n\n[processes.{unit}]\nargv = [\"sh\", \"-c\", \"env > seen.txt; echo ready; exec sleep 60\"]\nports = {{ PORT = \"http\" }}\nlisten = \"http\"\nready = {{ log = \"^ready$\" }}\n\n[tasks.check]\nargv = [\"sh\", \"-c\", \"echo checked >&2\"]\n"
    )
}

/// The three neighbours, each with its checkout, its process and the
/// variable that the other two bind to its URL.
const NEIGHBOURS: [(&str, &str, &str); 3] = [
    ("catalogue", "api", "CATALOGUE_URL"),
    ("shelf", "web", "SHELF_URL"),
    ("ledger", "book", "LEDGER_URL"),
];

/// Returns a starmap file entry that declares the optional variable `name`
/// as provided with `bind`.
fn optional(name: &str, bind: &str) -> String {
    format!(
        "[variables.{name}]\npurpose = \"A value of a neighbour.\"\ntype = \"string\"\nunset = \"optional\"\nshow = \"shown\"\nsource = \"provided\"\nbind = {bind}\n\n"
    )
}

/// Returns the starmap file of the neighbour `own`, which binds an
/// optional variable to the URL of each other neighbour, and names
/// `shelf_unit` in place of the catalogue's `api` when `own` is `shelf`.
fn neighbour_spec(own: &str, shelf_unit: &str) -> String {
    NEIGHBOURS
        .iter()
        .filter(|(checkout, _, _)| *checkout != own)
        .map(|(checkout, unit, variable)| {
            let unit = if own == "shelf" && *checkout == "catalogue" {
                shelf_unit
            } else {
                unit
            };
            optional(
                variable,
                &format!(r#"{{ provided = "multiverse/url", app = "{checkout}/{unit}" }}"#),
            )
        })
        .collect()
}

impl Setup {
    /// Makes the three neighbours' repositories, where the shelf binds
    /// `CATALOGUE_URL` to `catalogue/<shelf_unit>`.
    fn neighbours(&self, shelf_unit: &str) {
        for (checkout, unit, _) in NEIGHBOURS {
            let spec = neighbour_spec(checkout, shelf_unit);
            self.repository(
                checkout,
                &[("mxv.toml", &neighbour(unit)), ("starmap.toml", &spec)],
            );
        }
    }

    /// Creates the workspace `name` with a checkout of each of `checkouts`,
    /// and returns its directory.
    fn workspace_of(&self, name: &str, checkouts: &[&str]) -> PathBuf {
        let mut args = vec!["new", name];
        for checkout in checkouts {
            args.extend(["--repo", checkout]);
        }
        let (exit, document) = self.json(&args);
        assert_eq!(exit, 0, "{document}");
        if name != "triage" {
            self.others.borrow_mut().push(name.to_owned());
        }
        self.root().join("workspaces").join(name)
    }
}

/// Returns the names of the neighbours' variables that the environment in
/// `file` holds.
fn neighbour_variables(file: &Path) -> Vec<String> {
    let vars = seen(file);
    NEIGHBOURS
        .iter()
        .map(|(_, _, variable)| *variable)
        .filter(|variable| vars.iter().any(|(name, _)| name == variable))
        .map(str::to_owned)
        .collect()
}

#[test]
fn up_leaves_an_optional_bind_to_a_missing_checkout_unset_and_starts_every_process() {
    let setup = Setup::new();
    setup.neighbours("api");
    let cases: [(&str, &[&str]); 3] = [
        ("solo", &["shelf"]),
        ("pair", &["shelf", "catalogue"]),
        ("trio", &["shelf", "catalogue", "ledger"]),
    ];
    for (name, checkouts) in cases {
        let workspace = setup.workspace_of(name, checkouts);
        let (exit, document) = setup.json(&["up", "-w", name]);
        assert_eq!(exit, 0, "{name}: {document}");
        let rows = document["data"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: no rows: {document}"));
        assert_eq!(rows.len(), checkouts.len(), "{name}: {document}");
        for (checkout, _, own) in NEIGHBOURS {
            if !checkouts.contains(&checkout) {
                continue;
            }
            let expected: Vec<String> = NEIGHBOURS
                .iter()
                .filter(|(other, _, variable)| checkouts.contains(other) && *variable != own)
                .map(|(_, _, variable)| (*variable).to_owned())
                .collect();
            assert_eq!(
                neighbour_variables(&workspace.join(checkout).join("seen.txt")),
                expected,
                "{name}: {checkout}"
            );
            let missing: Vec<&str> = NEIGHBOURS
                .iter()
                .filter(|(other, _, _)| *other != checkout && !checkouts.contains(other))
                .map(|(other, _, _)| *other)
                .collect();
            let row = rows
                .iter()
                .find(|row| {
                    row["name"].as_str().is_some_and(|unit| {
                        unit.ends_with(NEIGHBOURS.iter().find(|n| n.0 == checkout).unwrap().1)
                    })
                })
                .unwrap_or_else(|| panic!("{name}: no row of {checkout}: {document}"));
            let notes = row["notes"].as_array().cloned().unwrap_or_default();
            assert_eq!(notes.len(), missing.len(), "{name}: {checkout}: {row}");
            for other in missing {
                assert!(
                    notes.iter().any(|note| {
                        note["level"] == "info"
                            && note["message"].as_str().is_some_and(|message| {
                                message.ends_with(&format!(
                                    "because the workspace has no checkout `{other}`"
                                ))
                            })
                    }),
                    "{name}: {checkout}: {row}"
                );
            }
        }
        let (exit, document) = setup.json(&["down", "-w", name]);
        assert_eq!(exit, 0, "{name}: {document}");
    }
}

#[test]
fn up_refuses_an_optional_bind_to_a_unit_that_a_present_checkout_lacks() {
    let setup = Setup::new();
    setup.neighbours("apii");
    let workspace = setup.workspace_of("triage", &["shelf", "catalogue", "ledger"]);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_ne!(exit, 0, "{document}");
    assert_eq!(
        document["code"], "mxv::manifest::provided_unknown",
        "{document}"
    );
    assert_eq!(document["fields"]["app"], "catalogue/apii", "{document}");
    for (checkout, _, _) in NEIGHBOURS {
        assert!(
            !workspace.join(checkout).join("seen.txt").exists(),
            "{checkout}"
        );
    }
}

#[test]
fn run_of_a_task_prints_one_line_per_skipped_bind_before_the_task_starts() {
    let setup = Setup::new();
    setup.neighbours("api");
    setup.workspace_of("triage", &["shelf"]);
    let (exit, stderr) = setup.text(&["run", "-w", "triage", "check"]);
    assert_eq!(exit, 0, "{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    let checked = lines
        .iter()
        .position(|line| *line == "checked")
        .unwrap_or_else(|| panic!("the task printed nothing: {stderr}"));
    let skipped: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.starts_with("info: ")
                && line.contains(" unset, because the workspace has no checkout ")
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(skipped.len(), 2, "{stderr}");
    assert!(skipped.iter().all(|index| *index < checked), "{stderr}");
    for (checkout, variable) in [("catalogue", "CATALOGUE_URL"), ("ledger", "LEDGER_URL")] {
        assert!(
            lines.iter().any(|line| line.contains(variable)
                && line.contains(&format!("no checkout `{checkout}`"))),
            "{stderr}"
        );
    }
}
