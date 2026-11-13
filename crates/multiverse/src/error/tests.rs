use phase::{DockerCause, DockerContext};

use crate::error::Error;

/// Returns the context that `json` describes, as phase writes it.
fn context(json: serde_json::Value) -> DockerContext {
    serde_json::from_value(json).unwrap()
}

#[test]
fn a_docker_that_is_not_on_path_reads_as_docker_missing() {
    let error = Error::of_docker(
        "phase::docker::unavailable",
        DockerCause::DockerMissing,
        &DockerContext::default(),
        "Docker did not answer: docker is not installed",
    );
    assert!(matches!(error, Error::DockerMissing { .. }), "{error:?}");
}

#[test]
fn a_failed_docker_command_names_its_operation_its_container_and_dockers_own_line() {
    let error = Error::of_docker(
        "phase::docker::refused",
        DockerCause::Refused,
        &context(serde_json::json!({
            "operation": "compose up",
            "service": "web",
            "docker_line": "Error response from daemon: port is already allocated",
        })),
        "Docker refused a request of the run: docker compose up failed for the container web: Error response from daemon: port is already allocated",
    );
    assert!(
        matches!(
            &error,
            Error::DockerFailed { operation, container: Some(container), message, .. }
                if operation == "compose up"
                    && container == "web"
                    && message == "Error response from daemon: port is already allocated"
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "docker compose up failed for the container web: Error response from daemon: port is already allocated"
    );
}

#[test]
fn a_failure_without_dockers_own_line_keeps_phases_message() {
    let error = Error::of_docker(
        "phase::docker::refused",
        DockerCause::Refused,
        &context(serde_json::json!({ "operation": "secret write", "service": "db" })),
        "Docker refused a request of the run: docker secret write failed for the container db: the script exited with 1",
    );
    assert!(
        matches!(
            &error,
            Error::DockerFailed { operation, container: Some(container), message, .. }
                if operation == "secret write"
                    && container == "db"
                    && message.starts_with("Docker refused a request of the run: ")
        ),
        "{error:?}"
    );
}

#[test]
fn a_failed_secret_write_prints_phases_message_once() {
    let message = "Docker refused a request of the run: docker secret write failed for the container db: the script exited with 1";
    let error = Error::of_docker(
        "phase::docker::refused",
        DockerCause::Refused,
        &context(serde_json::json!({ "operation": "secret write", "service": "db" })),
        message,
    );
    assert_eq!(error.to_string(), message);
}

#[test]
fn a_daemon_without_dockers_own_line_prints_phases_message_once() {
    let message = "Docker did not answer, so phase cannot tell what is running: the docker daemon does not answer: dial unix /nowhere.sock";
    let error = Error::of_docker(
        "phase::docker::unavailable",
        DockerCause::DaemonUnavailable,
        &DockerContext::default(),
        message,
    );
    assert_eq!(error.to_string(), message);
}

#[test]
fn a_failure_whose_command_phase_did_not_name_is_a_failed_request() {
    let error = Error::of_docker(
        "phase::docker::refused",
        DockerCause::Unexpected,
        &DockerContext::default(),
        "the run record names a container docker lacks",
    );
    assert!(
        matches!(
            &error,
            Error::DockerFailed { operation, container: None, .. } if operation == "request"
        ),
        "{error:?}"
    );
}

#[test]
fn a_daemon_that_does_not_answer_reads_as_docker_unavailable_with_dockers_own_line() {
    let error = Error::of_docker(
        "phase::docker::unavailable",
        DockerCause::DaemonUnavailable,
        &context(serde_json::json!({
            "operation": "container inspect",
            "docker_line": "Cannot connect to the Docker daemon at unix:///nowhere.sock",
        })),
        "Docker did not answer, so phase cannot tell what is running: the docker daemon does not answer: Cannot connect to the Docker daemon at unix:///nowhere.sock",
    );
    assert_eq!(
        error.to_string(),
        "the docker daemon does not answer: Cannot connect to the Docker daemon at unix:///nowhere.sock"
    );
}

#[test]
fn an_object_whose_labels_name_another_scope_reads_as_foreign_from_its_context() {
    let error = Error::of_docker(
        "phase::docker::refused",
        DockerCause::Foreign,
        &context(serde_json::json!({
            "foreign": { "kind": "volume", "name": "phase-ae72233197184b2a.srv" },
        })),
        "the message is not read",
    );
    assert!(
        matches!(&error, Error::DockerForeign { kind: "volume", name } if name == "phase-ae72233197184b2a.srv"),
        "{error:?}"
    );
}

#[test]
fn a_foreign_failure_that_names_no_object_is_phases_failure() {
    let error = Error::of_docker(
        "phase::docker::refused",
        DockerCause::Foreign,
        &DockerContext::default(),
        "the labels of the docker volume v do not name its scope",
    );
    assert!(
        matches!(
            &error,
            Error::Phase {
                code: "phase::docker::refused",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn the_cause_of_a_docker_error_chooses_the_failure_whatever_its_message_says() {
    let source = phase::DockerError::refused(
        "docker's compose plugin is not installed: docker: unknown command".to_owned(),
    );
    let error = Error::of_docker_error(&source, "phase::docker::refused");
    assert!(
        matches!(
            &error,
            Error::DockerFailed { operation, container: None, .. } if operation == "request"
        ),
        "{error:?}"
    );
}

#[test]
fn a_docker_error_without_dockers_own_line_keeps_its_whole_message() {
    let source = phase::DockerError::unavailable(
        "the docker daemon does not answer: dial unix /nowhere.sock".to_owned(),
    );
    let error = Error::of_docker_error(&source, "phase::docker::unavailable");
    assert!(
        matches!(
            &error,
            Error::DockerUnavailable { message, .. }
                if message == "the docker daemon does not answer: dial unix /nowhere.sock"
        ),
        "{error:?}"
    );
}

#[test]
fn a_container_engine_failure_without_its_own_kind_opens_with_what_failed() {
    let error = Error::engine_failed(&"docker pull was cancelled");
    assert_eq!(
        error.to_string(),
        "phase's container engine failed: docker pull was cancelled"
    );
}

#[test]
fn an_ssh_refusal_prints_the_host_and_the_line_ssh_gave() {
    let error = Error::SshRefused {
        remote: "ssh://git@example.invalid/acme/app.git".to_owned(),
        login: "git@example.invalid".to_owned(),
        port: None,
        refusal: crate::error::SshRefusal::HostKeyUnverified,
        message: "Host key verification failed.".to_owned(),
    };
    assert_eq!(
        error.to_string(),
        "ssh refused to fetch ssh://git@example.invalid/acme/app.git, because example.invalid has a key that the known hosts file does not hold: Host key verification failed."
    );
}
