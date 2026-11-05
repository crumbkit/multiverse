//! End-to-end tests of a manifest's local AWS services: `up` starts
//! DynamoDB Local and LocalStack, provisions the declared resources, and
//! gives a process that needs the services the AWS SDKs' variables. The
//! tests skip, with a line that says so, when no docker daemon answers.
//! Each test's workspace has a stable id of its own, which labels and
//! names its containers, and the test removes every container, volume and
//! network that carries that id, so it never touches another workspace's.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, repository};
use crate::test_ports::PortBlock;

/// Writes `text` to the file at `path`, as a person working in a checkout
/// would.
fn write(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write in a checkout"
    )]
    fs::write(path, text).unwrap();
}

/// Runs `docker <args>` and returns its exit status and standard output.
fn docker(args: &[&str]) -> (bool, String) {
    match Command::new("docker").args(args).env("PATH", PATH).output() {
        Ok(output) => (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ),
        Err(_) => (false, String::new()),
    }
}

/// Reports whether a docker daemon answers, and says that the test skips
/// when none does.
fn docker_runs(test: &str) -> bool {
    let (answered, version) = docker(&["version", "--format", "{{.Server.Version}}"]);
    let runs = answered && !version.is_empty();
    if !runs {
        println!("skipping {test}: no docker daemon answers");
    }
    runs
}

/// Returns docker's configuration directory as the person who runs the
/// tests has it, which holds the CLI plugins of a per-user install.
#[expect(
    clippy::disallowed_methods,
    reason = "the test hands mxv the docker configuration of the person who runs it"
)]
fn docker_config() -> PathBuf {
    std::env::var_os("DOCKER_CONFIG").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".docker"),
        PathBuf::from,
    )
}

/// A temporary directory with a repository, a workspace root and state
/// and config directories, and one workspace whose docker objects it
/// removes when it drops. It finds them only by its own stable id.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The workspace's name.
    name: String,
    /// The block of host ports that the state directory allocates from.
    ports: PortBlock,
}

impl Setup {
    /// Creates the workspace `name` with `manifest` as its `mxv.toml`.
    fn new(name: &str, manifest: &str) -> Self {
        // The sandbox denies the per-user temporary directory, so the
        // test's files lie in the build's target directory.
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        repository(&dir.path().canonicalize().unwrap());
        let ports = PortBlock::claim();
        let setup = Self {
            dir,
            name: name.to_owned(),
            ports,
        };
        let config = setup.root().join("config/mxv/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        write(&config, &setup.ports.config_line());
        let (exit, document) = setup.json(&["new", name, "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        setup.write_manifest(manifest);
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn checkout(&self) -> PathBuf {
        self.root().join("workspaces").join(&self.name).join("app")
    }

    fn write_manifest(&self, manifest: &str) {
        write(&self.checkout().join("mxv.toml"), manifest);
    }

    /// Returns the command `mxv <args>` with temporary XDG directories and
    /// an AWS profile and access key in its environment, which no process
    /// of the workspace may receive.
    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("MXV_ROOT", root.join("workspaces"))
            .env("DOCKER_CONFIG", docker_config())
            .env("AWS_PROFILE", "production")
            .env("AWS_ACCESS_KEY_ID", "not-a-real-key")
            .env("AWS_REGION", "eu-west-1")
    }

    /// Runs `mxv` with `args` and `--json`, and returns the exit code and
    /// the JSON document.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command(args).arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?} printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    /// Returns the host port that the workspace holds for `port`.
    fn port(&self, port: &str) -> u16 {
        let (_, shown) = self.json(&["show", &self.name]);
        u16::try_from(shown["data"][0]["ports"][port].as_u64().unwrap()).unwrap()
    }

    /// Returns the workspace's stable id, or `None` before its first `up`.
    fn workspace_id(&self) -> Option<String> {
        let text = fs::read_to_string(
            self.root()
                .join(format!("state/mxv/workspaces/{}.json", self.name)),
        )
        .ok()?;
        let record: Value = serde_json::from_str(&text).unwrap();
        Some(record["id"].as_str()?.to_owned())
    }

    /// Returns the names of the docker objects of `kind` (`ps --all`,
    /// `volume ls` or `network ls`) that carry the workspace's stable id.
    fn objects(&self, kind: &[&str]) -> Vec<String> {
        let Some(id) = self.workspace_id() else {
            return Vec::new();
        };
        let filter = format!("label=mxv.workspace-id={id}");
        let mut args = kind.to_vec();
        args.extend(["--filter", &filter]);
        let (_, printed) = docker(&args);
        printed.lines().map(str::to_owned).collect()
    }

    fn containers(&self) -> Vec<String> {
        self.objects(&["ps", "--all", "--format", "{{.Names}}"])
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        for container in self.containers() {
            let _ = docker(&["rm", "--force", "--volumes", &container]);
        }
        for volume in self.objects(&["volume", "ls", "--quiet"]) {
            let _ = docker(&["volume", "rm", &volume]);
        }
        for network in self.objects(&["network", "ls", "--format", "{{.Name}}"]) {
            let _ = docker(&["network", "rm", &network]);
        }
    }
}

/// Returns the row of `name` in `document`.
fn row<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row {name} in {document}"))
}

/// The script of the process `check`, which saves its environment and
/// reaches each backend with plain HTTP through curl: DynamoDB Local with
/// an `Authorization` header that carries no real signature, and
/// LocalStack with no header at all.
const CHECK: &str = r#"set -e
env | sort > env.txt
curl -sSf -X POST "$AWS_ENDPOINT_URL_DYNAMODB" -H 'Authorization: AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/dynamodb/aws4_request, SignedHeaders=host, Signature=0' -H 'X-Amz-Target: DynamoDB_20120810.DescribeTimeToLive' -H 'Content-Type: application/x-amz-json-1.0' -d "{\"TableName\":\"$STORE_TABLE\"}" > dynamodb.json
curl -sSf -o /dev/null -w '%{http_code}' -I "$AWS_ENDPOINT_URL_S3/$FILES_BUCKET" > s3.txt
curl -sSf -X POST "$AWS_ENDPOINT_URL_SQS" -H 'X-Amz-Target: AmazonSQS.GetQueueAttributes' -H 'Content-Type: application/x-amz-json-1.0' -d "{\"QueueUrl\":\"$JOBS_URL\",\"AttributeNames\":[\"VisibilityTimeout\"]}" > sqs.json
curl -sSf -X POST "$AWS_ENDPOINT_URL_KMS" -H 'X-Amz-Target: TrentService.DescribeKey' -H 'Content-Type: application/x-amz-json-1.1' -d "{\"KeyId\":\"$KEY_ALIAS\"}" > kms.json
echo checked
trap 'exit 0' TERM
sleep 600 & wait"#;

/// Returns a manifest with one service of each kind and the process
/// `check`, which needs all four, with `buckets` as the s3 service's
/// buckets.
fn manifest(buckets: &str) -> String {
    format!(
        r#"[service.store]
kind = "dynamodb"
tables.mxv-test-items = {{ hash = "pk:S", range = "sk:S", ttl = "expires" }}
env = {{ STORE_TABLE = "{{table.mxv-test-items}}", STORE_PORT = "{{port}}" }}

[service.files]
kind = "s3"
buckets = [{buckets}]
env = {{ FILES_BUCKET = "{{bucket.mxv-test-files}}", FILES_HOST = "{{host_port}}" }}

[service.jobs]
kind = "sqs"
queues.mxv-test-jobs = {{ visibility_timeout_seconds = 45 }}
env = {{ JOBS_URL = "{{queue_url.mxv-test-jobs}}" }}

[service.keys]
kind = "kms"
keys = ["mxv-test-key"]
env = {{ KEY_ALIAS = "{{key.mxv-test-key}}" }}

[processes.check]
argv = ["sh", "-c", {script}]
needs = ["store", "files", "jobs", "keys"]
ready = {{ log = "^checked$", timeout_seconds = 120 }}
"#,
        script = toml_string(CHECK)
    )
}

/// Returns `text` as a TOML basic string.
fn toml_string(text: &str) -> String {
    let mut quoted = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Returns the variables that `check` saved, by name.
fn saved_environment(setup: &Setup) -> Vec<(String, String)> {
    fs::read_to_string(setup.checkout().join("env.txt"))
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// Returns the log of the app `name`, whose path its status row names.
fn log_of(setup: &Setup, name: &str) -> String {
    let (_, status) = setup.json(&["status", "-w", &setup.name, "--long"]);
    let path = row(&status, name)["log"].as_str().unwrap().to_owned();
    fs::read_to_string(path).unwrap()
}

#[test]
fn up_starts_the_backends_provisions_every_resource_and_gives_a_process_only_mxvs_aws_variables() {
    if !docker_runs("up_starts_the_backends") {
        return;
    }
    let name = "mxvtest-services";
    let setup = Setup::new(name, &manifest(r#""mxv-test-files""#));

    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    let names: Vec<&str> = document["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mxv-dynamodb", "mxv-aws", "mxv-provision", "check"]);
    assert_eq!(row(&document, "mxv-dynamodb")["kind"], "container");
    assert_eq!(row(&document, "mxv-provision")["state"], "exited");
    assert_eq!(row(&document, "mxv-provision")["code"], 0);
    assert_eq!(row(&document, "check")["state"], "running");

    let dynamodb = setup.port("mxv-dynamodb");
    let aws = setup.port("mxv-aws");
    let environment = saved_environment(&setup);
    let aws_variables: Vec<(&str, &str)> = environment
        .iter()
        .filter(|(variable, _)| variable.starts_with("AWS_"))
        .map(|(variable, value)| (variable.as_str(), value.as_str()))
        .collect();
    let dynamodb_endpoint = format!("http://127.0.0.1:{dynamodb}");
    let aws_endpoint = format!("http://127.0.0.1:{aws}");
    assert_eq!(
        aws_variables,
        [
            ("AWS_ACCESS_KEY_ID", "test"),
            ("AWS_CONFIG_FILE", "/dev/null"),
            ("AWS_EC2_METADATA_DISABLED", "true"),
            ("AWS_ENDPOINT_URL_DYNAMODB", dynamodb_endpoint.as_str()),
            ("AWS_ENDPOINT_URL_KMS", aws_endpoint.as_str()),
            ("AWS_ENDPOINT_URL_S3", aws_endpoint.as_str()),
            ("AWS_ENDPOINT_URL_SQS", aws_endpoint.as_str()),
            ("AWS_REGION", "us-east-1"),
            ("AWS_SECRET_ACCESS_KEY", "test"),
            ("AWS_SHARED_CREDENTIALS_FILE", "/dev/null"),
        ]
    );
    let value = |variable: &str| {
        environment
            .iter()
            .find(|(name, _)| name == variable)
            .map(|(_, value)| value.clone())
    };
    assert_eq!(value("STORE_TABLE").as_deref(), Some("mxv-test-items"));
    assert_eq!(value("STORE_PORT"), Some(dynamodb.to_string()));
    assert_eq!(value("FILES_BUCKET").as_deref(), Some("mxv-test-files"));
    assert_eq!(value("FILES_HOST"), Some(format!("127.0.0.1:{aws}")));
    assert_eq!(
        value("JOBS_URL"),
        Some(format!(
            "http://127.0.0.1:{aws}/queue/us-east-1/000000000000/mxv-test-jobs"
        ))
    );
    assert_eq!(value("KEY_ALIAS").as_deref(), Some("alias/mxv-test-key"));

    // The process reached each backend and found each resource that
    // mxv-provision created.
    let read = |file: &str| fs::read_to_string(setup.checkout().join(file)).unwrap();
    let ttl: Value = serde_json::from_str(&read("dynamodb.json")).unwrap();
    assert_eq!(ttl["TimeToLiveDescription"]["TimeToLiveStatus"], "ENABLED");
    assert_eq!(ttl["TimeToLiveDescription"]["AttributeName"], "expires");
    assert_eq!(read("s3.txt"), "200");
    let queue: Value = serde_json::from_str(&read("sqs.json")).unwrap();
    assert_eq!(queue["Attributes"]["VisibilityTimeout"], "45");
    let key: Value = serde_json::from_str(&read("kms.json")).unwrap();
    assert_eq!(key["KeyMetadata"]["KeySpec"], "SYMMETRIC_DEFAULT");

    let provisioned = log_of(&setup, "mxv-provision");
    assert!(
        provisioned.contains("created the table mxv-test-items, with expiry on `expires`"),
        "{provisioned}"
    );
    assert!(
        provisioned.contains("created the key alias/mxv-test-key"),
        "{provisioned}"
    );

    // `show` lists each service's variables with the ports `up` gave.
    let (_, shown) = setup.json(&["show", name, "--long"]);
    assert_eq!(
        shown["data"][0]["services"]["jobs"]["AWS_ENDPOINT_URL_SQS"],
        aws_endpoint
    );

    // A second `up` finds everything running and changes nothing.
    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "mxv-provision")["state"], "exited");
    assert_eq!(row(&document, "mxv-provision")["code"], 0);

    // A changed resource list runs the provisioning again, which finds the
    // resources it made before and creates only the new bucket.
    setup.write_manifest(&manifest(r#""mxv-test-files", "mxv-test-more""#));
    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    let provisioned = log_of(&setup, "mxv-provision");
    for line in [
        "the table mxv-test-items exists already",
        "the bucket mxv-test-files exists already",
        "created the bucket mxv-test-more",
        "the queue mxv-test-jobs exists already, and has the declared attributes",
        "the key alias/mxv-test-key exists already",
    ] {
        assert!(provisioned.contains(line), "{line}: {provisioned}");
    }

    let (exit, document) = setup.json(&["down", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    assert!(setup.containers().is_empty(), "{:?}", setup.containers());
}

#[test]
fn up_starts_a_manifest_of_services_alone_without_a_warning() {
    if !docker_runs("up_starts_a_manifest_of_services_alone") {
        return;
    }
    let name = "mxvtest-services-alone";
    let setup = Setup::new(
        name,
        "[service.files]\nkind = \"s3\"\nbuckets = [\"mxv-test-alone\"]\n",
    );
    let output = setup
        .command(&["up", "-w", name, "--json"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("started nothing"), "{stderr}");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = document["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mxv-aws", "mxv-provision"]);
    let (exit, document) = setup.json(&["down", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "mxv-aws")["kind"], "container");
    assert!(setup.containers().is_empty());
}

#[test]
fn a_kind_added_to_a_running_workspace_replaces_localstack_and_up_provisions_it() {
    if !docker_runs("a_kind_added_to_a_running_workspace") {
        return;
    }
    let name = "mxvtest-services-added";
    let files = "[service.files]\nkind = \"s3\"\nbuckets = [\"mxv-test-added\"]\n";
    let setup = Setup::new(name, files);
    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 0, "{document}");

    setup.write_manifest(&format!(
        "{files}\n[service.jobs]\nkind = \"sqs\"\nqueues.mxv-test-added = {{}}\n"
    ));
    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    let provisioned = log_of(&setup, "mxv-provision");
    assert!(
        provisioned.contains("created the queue mxv-test-added"),
        "{provisioned}"
    );

    let (exit, document) = setup.json(&["down", "-w", name]);
    assert_eq!(exit, 0, "{document}");
}

#[test]
fn a_table_whose_keys_change_fails_up_with_the_provisioning_log_until_down_resets_it() {
    if !docker_runs("a_table_whose_keys_change") {
        return;
    }
    let name = "mxvtest-services-changed";
    let table = |hash: &str| {
        format!(
            "[service.store]\nkind = \"dynamodb\"\ntables.mxv-test-changed = {{ hash = \"{hash}\" }}\n"
        )
    };
    let setup = Setup::new(name, &table("pk:S"));
    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 0, "{document}");

    setup.write_manifest(&table("pk:N"));
    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::exited", "{document}");
    assert_eq!(document["fields"]["process"], "mxv-provision", "{document}");
    let log = document["fields"]["log"]
        .as_str()
        .unwrap_or_else(|| panic!("{document}"));
    assert!(log.ends_with("mxv-provision.main.log"), "{log}");
    assert_eq!(document["fields"]["tail"], serde_json::json!([]));
    let hint = document["recovery"]["text"].as_str().unwrap_or_default();
    assert!(
        hint.contains(&format!("mxv logs -w {name} mxv-provision"))
            && hint.contains(&format!("mxv up -w {name}`")),
        "{document}"
    );
    let provisioned = fs::read_to_string(log).unwrap();
    assert!(
        provisioned.contains("the table mxv-test-changed exists with keys, attribute types or indexes other than the manifest declares")
            && provisioned.contains("run `mxv down`"),
        "{provisioned}"
    );

    let (exit, document) = setup.json(&["down", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["down", "-w", name]);
    assert_eq!(exit, 0, "{document}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_process_that_needs_a_service_reaches_its_backend() {
    if !docker_runs("a_sandboxed_process_that_needs_a_service_reaches_its_backend") {
        return;
    }
    let name = "mxvtest-services-sandboxed";
    let script = "set -e; curl -sSf -o /dev/null -w '%{http_code}' -I \
        \"$AWS_ENDPOINT_URL_S3/mxv-test-sandboxed\" > out/s3.txt; echo checked; exec sleep 600";
    let setup = Setup::new(
        name,
        &format!(
            r#"[service.files]
kind = "s3"
buckets = ["mxv-test-sandboxed"]

[processes.check]
argv = ["/bin/sh", "-c", {script}]
needs = ["files"]
ready = {{ log = "^checked$", timeout_seconds = 120 }}
permissions = {{ write = ["out"] }}
"#,
            script = toml_string(script)
        ),
    );
    fs::create_dir_all(setup.checkout().join("out")).unwrap();

    let (exit, document) = setup.json(&["up", "-w", name]);
    assert_eq!(exit, 0, "{document}");
    let (_, status) = setup.json(&["status", "-w", name, "--long"]);
    let sandbox = &row(&status, "check")["sandbox"];
    assert!(
        sandbox
            .as_str()
            .is_some_and(|sandbox| sandbox.starts_with("seatbelt")),
        "{status}"
    );
    let checked = fs::read_to_string(setup.checkout().join("out/s3.txt")).unwrap();
    assert_eq!(checked, "200");

    let (exit, document) = setup.json(&["down", "-w", name]);
    assert_eq!(exit, 0, "{document}");
}
