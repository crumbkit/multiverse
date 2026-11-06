use std::fs;
use std::path::Path;

use crate::lookup::ManifestSource;
use crate::manifest::ManifestProblem;
use crate::record::{Checkout, Workspace};
use crate::testing::write;

fn workspace(dir: &Path) -> Workspace {
    let path = dir.join("reading-list");
    let checkout = Checkout::new(
        "app".to_owned(),
        &dir.join("source"),
        path.join("app"),
        "reading-list".to_owned(),
        "refs/remotes/origin/main".to_owned(),
    );
    fs::create_dir_all(path.join("app")).unwrap();
    Workspace::new("reading-list".parse().unwrap(), path, vec![checkout])
}

#[test]
fn a_workspace_with_neither_file_is_bare() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.source(), ManifestSource::None);
    assert_eq!(inputs.chosen(), None);
    assert_eq!(workspace.manifest().unwrap(), None);
}

#[test]
fn the_checkouts_manifest_is_read_when_the_workspace_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.source(), ManifestSource::Repository);
    assert_eq!(inputs.repository, Some(workspace.path.join("app/mxv.toml")));
    assert_eq!(workspace.manifest().unwrap().unwrap().ports, ["api"]);
}

#[test]
fn the_workspace_manifest_wins_and_is_never_merged_with_the_checkouts() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    write(workspace.path.join("mxv.toml"), "ports = [\"web\"]\n");
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.source(), ManifestSource::Workspace);
    assert_eq!(inputs.workspace, Some(workspace.path.join("mxv.toml")));
    assert_eq!(inputs.repository, Some(workspace.path.join("app/mxv.toml")));
    assert_eq!(
        inputs.chosen(),
        Some(workspace.path.join("mxv.toml").as_path())
    );
    assert_eq!(workspace.manifest().unwrap().unwrap().ports, ["web"]);
}

#[test]
fn an_invalid_workspace_manifest_fails_and_never_falls_back_to_the_checkouts() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    write(workspace.path.join("mxv.toml"), "ports = \n");
    let error = workspace.manifest().unwrap_err();
    assert_eq!(error.path, workspace.path.join("mxv.toml"));
}

#[test]
fn a_cwd_in_the_workspace_manifest_is_relative_to_the_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    fs::create_dir(workspace.path.join("app/web")).unwrap();
    write(
        workspace.path.join("mxv.toml"),
        "[processes.web]\nargv = [\"true\"]\ncwd = \"web\"\n",
    );
    let manifest = workspace.manifest().unwrap().unwrap();
    assert_eq!(manifest.processes["web"].cwd, Path::new("web"));
}

#[test]
fn a_dangling_link_at_the_workspace_manifest_fails_as_unreadable_and_names_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    std::os::unix::fs::symlink(dir.path().join("gone.toml"), workspace.manifest_file()).unwrap();
    let error = workspace.manifest().unwrap_err();
    assert_eq!(error.path, workspace.manifest_file());
    let ManifestProblem::Unreadable { message } = &error.problem else {
        panic!("{error:?}");
    };
    assert!(message.contains("gone.toml"), "{message}");
}

#[test]
fn a_regular_workspace_manifest_is_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.manifest_file(), "disposable = [\"data/\"]\n");
    assert!(workspace.manifest_trusted());
}

#[test]
fn a_workspace_manifest_that_links_to_the_checkouts_is_read_and_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(
        workspace.path.join("app/mxv.toml"),
        "disposable = [\"data/\"]\n",
    );
    std::os::unix::fs::symlink("app/mxv.toml", workspace.manifest_file()).unwrap();
    assert_eq!(
        workspace.manifest_inputs().source(),
        ManifestSource::Workspace
    );
    assert_eq!(workspace.manifest().unwrap().unwrap().disposable.len(), 1);
    assert!(!workspace.manifest_trusted());
}

#[test]
fn a_workspace_manifest_with_a_second_hard_link_is_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(
        workspace.path.join("app/mxv.toml"),
        "disposable = [\"data/\"]\n",
    );
    fs::hard_link(
        workspace.path.join("app/mxv.toml"),
        workspace.manifest_file(),
    )
    .unwrap();
    assert!(!workspace.manifest_trusted());
}

#[test]
fn a_record_whose_checkout_holds_the_workspace_manifests_path_has_no_workspace_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reading-list");
    fs::create_dir_all(&path).unwrap();
    // An old record whose checkout is the workspace's directory itself.
    let checkout = Checkout::new(
        "app".to_owned(),
        &dir.path().join("source"),
        path.clone(),
        "reading-list".to_owned(),
        "refs/remotes/origin/main".to_owned(),
    );
    let workspace = Workspace::new("reading-list".parse().unwrap(), path, vec![checkout]);
    write(workspace.manifest_file(), "disposable = [\"data/\"]\n");
    assert!(workspace.checkout_holds_manifest_file());
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.workspace, None);
    assert_eq!(inputs.source(), ManifestSource::Repository);
    assert!(!workspace.manifest_trusted());
}

/// Returns a workspace with the checkouts `folders`, each a directory.
fn several(dir: &Path, folders: &[&str]) -> Workspace {
    let path = dir.join("triage");
    let checkouts = folders
        .iter()
        .map(|folder| {
            fs::create_dir_all(path.join(folder)).unwrap();
            Checkout::new(
                (*folder).to_owned(),
                &dir.join("source").join(folder),
                path.join(folder),
                "triage".to_owned(),
                "refs/remotes/origin/main".to_owned(),
            )
        })
        .collect();
    Workspace::new("triage".parse().unwrap(), path, checkouts)
}

const API: &str = r#"ports = ["http"]

[processes.server]
argv = ["cargo", "run"]
cwd = "server"
env = { SELF = "http://127.0.0.1:{port.http}/{{x}}" }
ports = { PORT = "http" }
listen = "http"
depends_on = ["db"]

[containers.db]
image = "postgres:16.4"
ports = { 5432 = "http" }
"#;

#[test]
fn a_workspace_with_several_checkouts_merges_their_manifests_with_prefixed_names() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    fs::create_dir_all(workspace.path.join("api/server")).unwrap();
    write(
        workspace.path.join("api/mxv.toml"),
        API.replace("ports = { 5432 = \"http\" }\n", ""),
    );
    write(
        workspace.path.join("web/mxv.toml"),
        "ports = [\"http\"]\n\n[processes.server]\nargv = [\"npm\", \"run\", \"dev\"]\nports = { PORT = \"http\", API = \"http\" }\n",
    );
    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.source(), ManifestSource::Merged);
    assert_eq!(inputs.files().len(), 2);
    let manifest = workspace.manifest().unwrap().unwrap();
    assert_eq!(manifest.ports, ["api-http", "web-http"]);
    let server = &manifest.processes["api-server"];
    assert_eq!(server.cwd, workspace.path.join("api/server"));
    assert_eq!(server.ports["PORT"], "api-http");
    assert_eq!(server.listen.as_deref(), Some("api-http"));
    assert_eq!(server.depends_on, ["api-db"]);
    assert!(server.env["SELF"] == "http://127.0.0.1:{port.api-http}/{{x}}");
    assert!(manifest.containers.contains_key("api-db"));
    assert_eq!(
        manifest.processes["web-server"].cwd,
        workspace.path.join("web")
    );
}

#[test]
fn the_workspace_manifest_joins_the_checkouts_with_its_names_as_written_and_paths_from_the_workspace()
 {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    write(workspace.path.join("api/mxv.toml"), "ports = [\"http\"]\n");
    write(
        workspace.manifest_file(),
        "ports = [\"proxy\"]\n\n[processes.proxy]\nargv = [\"caddy\"]\ncwd = \"web\"\nenv = { API = \"{port.api-http}\" }\n",
    );
    let manifest = workspace.manifest().unwrap().unwrap();
    assert_eq!(manifest.ports, ["proxy", "api-http"]);
    assert_eq!(manifest.processes["proxy"].cwd, workspace.path.join("web"));
}

#[test]
fn two_equal_merged_names_fail_with_a_name_clash_that_names_both_files() {
    let dir = tempfile::tempdir().unwrap();
    // The checkout `a-b` with the port `c`, and the checkout `a` with the
    // port `b-c`, both give `a-b-c`.
    let workspace = several(dir.path(), &["a-b", "a"]);
    write(workspace.path.join("a-b/mxv.toml"), "ports = [\"c\"]\n");
    write(workspace.path.join("a/mxv.toml"), "ports = [\"b-c\"]\n");
    let error = workspace.manifest().unwrap_err();
    assert_eq!(error.path, workspace.path.join("a/mxv.toml"));
    assert_eq!(
        error.problem,
        ManifestProblem::NameClash {
            name: "a-b-c".to_owned(),
            other: workspace.path.join("a-b/mxv.toml"),
        }
    );
}

/// A manifest whose container `db` mounts the volume `data`, which phase
/// names `<workspace>.data` for every app of the workspace.
const DB: &str = "[containers.db]\nimage = \"postgres:16.4\"\nvolumes = { data = \"/var/lib/postgresql/data\" }\n\n[containers.replica]\nimage = \"postgres:16.4\"\nvolumes = { data = \"/var/lib/postgresql/data\" }\n";

#[test]
fn each_checkouts_volumes_carry_its_prefix_so_two_checkouts_never_share_one() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    write(workspace.path.join("api/mxv.toml"), DB);
    write(workspace.path.join("web/mxv.toml"), DB);
    let manifest = workspace.manifest().unwrap().unwrap();
    let volumes = |name: &str| {
        manifest.containers[name]
            .volumes
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(volumes("api-db"), ["api-data"]);
    assert_eq!(volumes("api-replica"), ["api-data"]);
    assert_eq!(volumes("web-db"), ["web-data"]);
}

#[test]
fn a_volume_that_two_files_declare_once_prefixed_fails_with_a_name_clash() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    write(workspace.path.join("api/mxv.toml"), DB);
    write(
        workspace.path.join("mxv.toml"),
        "[containers.cache]\nimage = \"redis:7.4\"\nvolumes = { api-data = \"/data\" }\n",
    );
    let error = workspace.manifest().unwrap_err();
    assert_eq!(
        error.problem,
        ManifestProblem::NameClash {
            name: "api-data".to_owned(),
            other: workspace.path.join("mxv.toml"),
        }
    );
}

#[test]
fn a_unit_and_a_port_may_share_a_merged_name() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    write(
        workspace.path.join("api/mxv.toml"),
        "ports = [\"api\"]\n\n[processes.api]\nargv = [\"true\"]\nports = { PORT = \"api\" }\n",
    );
    assert!(
        workspace
            .manifest()
            .unwrap()
            .unwrap()
            .processes
            .contains_key("api-api")
    );
}

#[test]
fn a_merged_name_longer_than_a_manifest_name_may_be_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let long = "a".repeat(60);
    let workspace = several(dir.path(), &[long.as_str(), "web"]);
    write(
        workspace.path.join(&long).join("mxv.toml"),
        "ports = [\"http-port\"]\n",
    );
    let error = workspace.manifest().unwrap_err();
    assert!(
        matches!(error.problem, ManifestProblem::Invalid { ref message } if message.contains("longer than 64")),
        "{error}"
    );
}

#[test]
fn a_checkout_prefix_is_its_folder_in_lower_case_with_other_characters_written_as_dashes() {
    assert_eq!(crate::lookup::checkout_prefix("Web.App"), "web-app");
    assert_eq!(crate::lookup::checkout_prefix("api_v2"), "api_v2");
}

#[test]
fn a_workspace_with_one_checkout_keeps_its_names_without_a_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    write(workspace.path.join("app/mxv.toml"), "ports = [\"api\"]\n");
    assert_eq!(workspace.manifest().unwrap().unwrap().ports, ["api"]);
    assert_eq!(workspace.manifest_inputs().files().len(), 1);
}

/// A manifest whose process `worker` needs the dynamodb service `store`.
const STORE: &str = "[service.store]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"pk:S\" }\nenv = { INBOX_TABLE = \"{table.inbox}\" }\n\n[processes.worker]\nargv = [\"worker\"]\nneeds = [\"store\"]\n";

#[test]
fn two_checkouts_that_declare_one_service_alike_share_it_and_its_backend() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    write(workspace.path.join("api/mxv.toml"), STORE);
    write(workspace.path.join("web/mxv.toml"), STORE);
    let manifest = workspace.manifest().unwrap().unwrap();
    assert_eq!(manifest.services.keys().collect::<Vec<_>>(), ["store"]);
    assert_eq!(manifest.processes["api-worker"].needs, ["store"]);
    assert_eq!(manifest.processes["web-worker"].needs, ["store"]);
    assert_eq!(manifest.ledger_ports(), ["mxv-dynamodb"]);
}

#[test]
fn two_checkouts_that_declare_one_service_differently_fail_with_a_name_clash() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    write(workspace.path.join("api/mxv.toml"), STORE);
    write(
        workspace.path.join("web/mxv.toml"),
        "[service.store]\nkind = \"s3\"\n",
    );
    let error = workspace.manifest().unwrap_err();
    assert_eq!(
        error.problem,
        ManifestProblem::NameClash {
            name: "store".to_owned(),
            other: workspace.path.join("api/mxv.toml"),
        }
    );
}

#[test]
fn two_checkouts_whose_services_declare_one_table_differently_fail_with_a_service_clash() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    write(workspace.path.join("api/mxv.toml"), STORE);
    write(
        workspace.path.join("web/mxv.toml"),
        "[service.db]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"id:N\" }\n",
    );
    let error = workspace.manifest().unwrap_err();
    assert_eq!(error.path, workspace.path.join("web/mxv.toml"));
    assert!(
        matches!(&error.problem, ManifestProblem::ServiceClash { name, .. } if name == "inbox"),
        "{error}"
    );
}

#[test]
fn the_workspace_manifest_may_need_a_service_that_a_checkout_declares() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["api", "web"]);
    write(workspace.path.join("api/mxv.toml"), STORE);
    write(
        workspace.path.join("mxv.toml"),
        "[processes.tests]\nargv = [\"tests\"]\nneeds = [\"store\"]\n",
    );
    let manifest = workspace.manifest().unwrap().unwrap();
    assert_eq!(manifest.processes["tests"].needs, ["store"]);
}

#[test]
fn a_checkout_folder_that_gives_its_names_the_mxv_prefix_fails_as_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = several(dir.path(), &["mxv", "web"]);
    write(
        workspace.path.join("mxv/mxv.toml"),
        "[processes.api]\nargv = [\"api\"]\n",
    );
    let error = workspace.manifest().unwrap_err();
    assert!(
        matches!(&error.problem, ManifestProblem::Invalid { message } if message.contains("`mxv-api`")),
        "{error}"
    );
}
