use crate::error::Error;

#[test]
fn a_docker_that_is_not_on_path_reads_as_docker_missing() {
    let error = Error::of_docker(true, "Docker did not answer: docker is not installed");
    assert!(matches!(error, Error::DockerMissing { .. }), "{error:?}");
}

#[test]
fn a_missing_compose_plugin_keeps_dockers_own_line() {
    let error = Error::of_docker(
        false,
        "Docker refused a request of the run: docker's compose plugin is not installed: docker: unknown command: docker compose",
    );
    assert!(
        matches!(&error, Error::DockerComposeMissing { message } if message == "docker: unknown command: docker compose"),
        "{error:?}"
    );
}

#[test]
fn a_daemon_that_does_not_answer_reads_as_docker_unavailable() {
    let error = Error::of_docker(
        true,
        "Docker did not answer, so phase cannot tell what is running: the docker daemon does not answer: dial unix /nowhere.sock",
    );
    assert!(
        matches!(&error, Error::DockerUnavailable { message } if message == "dial unix /nowhere.sock"),
        "{error:?}"
    );
}

#[test]
fn a_failed_docker_command_names_its_operation_its_container_and_dockers_message() {
    let error = Error::of_docker(
        false,
        "Docker refused a request of the run: docker compose up failed for the container web: port is already allocated",
    );
    assert!(
        matches!(
            &error,
            Error::DockerFailed { operation: "compose up", container: Some(container), message }
                if container == "web" && message == "port is already allocated"
        ),
        "{error:?}"
    );
    let error = Error::of_docker(
        false,
        "Docker refused a request of the run: docker pull failed: not found",
    );
    assert!(
        matches!(&error, Error::DockerFailed { operation: "pull", container: None, message } if message == "not found"),
        "{error:?}"
    );
}

#[test]
fn a_refusal_that_names_no_docker_command_is_a_failed_request() {
    let error = Error::of_docker(false, "the run record names a container docker lacks");
    assert!(
        matches!(
            &error,
            Error::DockerFailed {
                operation: "request",
                container: None,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn an_object_whose_labels_name_another_scope_reads_as_foreign() {
    let error = Error::of_docker(
        false,
        "Docker refused a request of the run: the labels of the docker volume phase-ae72233197184b2a.srv do not name its scope",
    );
    assert!(
        matches!(&error, Error::DockerForeign { kind: "volume", name } if name == "phase-ae72233197184b2a.srv"),
        "{error:?}"
    );
}

#[test]
fn a_docker_error_whose_cause_is_an_unavailable_daemon_reads_as_docker_unavailable() {
    let source = phase::DockerError::unavailable(
        "the docker daemon does not answer: dial unix /nowhere.sock".to_owned(),
    );
    let error = Error::of_docker_error(&source, "phase::docker::unavailable");
    assert!(
        matches!(&error, Error::DockerUnavailable { message } if message == "dial unix /nowhere.sock"),
        "{error:?}"
    );
}

#[test]
fn a_refused_docker_error_names_its_operation_and_container_from_its_message() {
    let source = phase::DockerError::refused(
        "docker pull failed for the container web: manifest unknown".to_owned(),
    );
    let error = Error::of_docker_error(&source, "phase::docker::refused");
    assert!(
        matches!(
            &error,
            Error::DockerFailed { operation: "pull", container: Some(container), message }
                if container == "web" && message == "manifest unknown"
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
            Error::DockerFailed {
                operation: "request",
                container: None,
                ..
            }
        ),
        "{error:?}"
    );
}
