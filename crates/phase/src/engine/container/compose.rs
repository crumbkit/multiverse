//! `docker compose up` for the compose projects of phase's container apps,
//! and how a failed one is read.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::cancel::Cancel;
use crate::engine::container::ContainerError as Error;
use crate::engine::container::command::{
    ChildEnv, Finished, check, compose_docker_line, message, run_input,
};
use crate::engine::container::names::{Scope, ServiceName};
use crate::engine::container::objects::{image_config, inspect_named};
use crate::engine::container::secrets::ImageConfig;

/// The variables of a `docker` process that receives none on top of the
/// host program's own environment.
pub(crate) struct ComposeEnv;

/// Writes `$` as `$$`, which Compose reads back as `$`, so it interpolates
/// nothing in `text`.
pub(super) fn literal(text: &str) -> String {
    text.replace('$', "$$")
}

/// Returns the variable of the `docker compose` process that holds the
/// value of the `variable`th variable of the `service`th service, in the
/// order of the manifest's containers and of their variables.
pub(super) fn variable_of(service: usize, variable: usize) -> String {
    format!("MXV_ENV_{service}_{variable}")
}

/// Why a `docker compose up` failed, as its standard error tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpFailure {
    /// docker already publishes the host port, which it names when it
    /// can, for another container.
    PortAllocated(Option<u16>),
    /// docker has no free address pool for a network.
    NoAddressPool,
    /// Any other failure.
    Other,
}

/// What stands in Compose's standard error for text of an image's `USER`.
const HIDDEN_USER: &str = "<the image's USER>";

/// What stands for Compose's whole standard error when phase could not
/// read the image's `USER`, so it cannot tell which of Docker's lines
/// quote it.
const USER_UNREADABLE: &str =
    "phase could not read the image's USER, so it keeps none of Docker's text for this failure";

/// The words after which Docker quotes one field of a `USER` that it
/// cannot find. Docker splits a `USER` on every colon, so the field holds
/// no colon, and the first colon after these words ends it.
const QUOTED_AFTER: [&str; 2] = ["unable to find user ", "unable to find group "];

/// Returns `stderr` with each field that Docker quotes after
/// [`QUOTED_AFTER`] replaced, up to the end of the field. A field with no
/// colon after it hides the rest of `stderr`.
fn without_quoted_fields(stderr: &str) -> String {
    let mut cleaned = String::with_capacity(stderr.len());
    let mut rest = stderr;
    loop {
        let found = QUOTED_AFTER
            .iter()
            .filter_map(|words| rest.find(words).map(|at| at + words.len()))
            .min();
        let Some(start) = found else {
            cleaned.push_str(rest);
            return cleaned;
        };
        cleaned.push_str(&rest[..start]);
        cleaned.push_str(HIDDEN_USER);
        rest = &rest[start..];
        rest = &rest[rest.find(':').unwrap_or(rest.len())..];
    }
}

/// Returns `stderr` without text of the image's `USER`, which `user` holds
/// and whose author may have chosen it to look like one of Docker's
/// failures. When phase could not read the `USER`, `user` is `None` and
/// nothing of `stderr` remains.
///
/// It handles the field that Docker quotes when it cannot find a user or
/// a group, wherever that field comes from, and the whole `USER` wherever
/// it appears when the `USER` holds a character that a plain name lacks,
/// such as a space or an escape sequence. It leaves a `USER` of only
/// letters, digits, `-`, `_`, `.` and `:` alone outside the quoted field,
/// because a word such as `daemon` or `port`, or a number, is part of
/// Docker's own sentences and of the failure phrases. Such a `USER` holds
/// no escape sequence and no phrase, so a sentence of Docker's that quoted
/// it some other way would show only that plain text. A sentence that
/// quoted one field of a `USER` with other characters somewhere else would
/// show that field. phase knows of no such sentence, and a new one would
/// need its words in [`QUOTED_AFTER`].
pub(crate) fn without_image_user(stderr: &str, user: Option<&str>) -> String {
    let Some(user) = user else {
        return USER_UNREADABLE.to_owned();
    };
    let plain = user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if plain {
        without_quoted_fields(stderr)
    } else {
        without_quoted_fields(&stderr.replace(user, HIDDEN_USER))
    }
}

/// Returns why the `docker compose up` whose standard error is `stderr`
/// failed.
pub(crate) fn up_failure(stderr: &str) -> UpFailure {
    if stderr.contains("port is already allocated") {
        let port = stderr
            .split("Bind for ")
            .nth(1)
            .and_then(|rest| rest.split(" failed").next())
            .and_then(|address| address.rsplit_once(':'))
            .and_then(|(_, port)| port.parse().ok());
        UpFailure::PortAllocated(port)
    } else if stderr.contains("all predefined address pools have been fully subnetted") {
        UpFailure::NoAddressPool
    } else {
        UpFailure::Other
    }
}

/// What a `docker compose up` that did not fail as an error did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Up {
    /// The container started.
    Started,
    /// docker already publishes the host port, which it names when it
    /// can, for another container.
    PortAllocated {
        /// The host port, when docker names it.
        port: Option<u16>,
        /// Docker's own error line, as [`compose_docker_line`] takes it.
        docker_line: Option<String>,
    },
}

/// What `docker compose up` does with a service of phase's app project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppStep {
    /// It creates the container and leaves it created, so that Docker
    /// copies an image directory into a new volume before phase sets the
    /// volume's owner.
    Create,
    /// It starts the container, creating it if it does not exist, and
    /// keeps a container that exists.
    Start,
}

/// One `docker compose up` of a service of phase's app project.
pub(crate) struct AppUp<'a> {
    /// The scope of the app.
    pub(crate) scope: &'a Scope,
    /// The project directory, which holds no file that Compose reads.
    pub(crate) dir: &'a Path,
    /// The compose file, which Compose reads from standard input.
    pub(crate) file: &'a Value,
    /// The service.
    pub(crate) service: &'a ServiceName,
    /// What the command does.
    pub(crate) step: AppStep,
    /// The image whose `USER` the container runs as, or `None` when the
    /// plan names the user.
    pub(crate) runs_as_image_of: Option<&'a str>,
}

/// Runs `up`, with the variables `env`. Compose reads no `.env` file.
/// `until` holds the time the command may take and the flag that stops it.
///
/// The caller has checked the compose plugin with `docker compose version`,
/// and has pulled the image, so Compose pulls nothing. When the command
/// fails and the container runs as its image's `USER`, phase reads that
/// `USER` and removes its text from Compose's standard error before it
/// classifies or quotes any of it, as [`without_image_user`] says.
///
/// # Errors
///
/// Returns [`Error::Foreign`] when a container with the name of the
/// service lacks the scope's labels, [`Error::NoAddressPool`], the
/// errors of `docker version`, and [`Error::Failed`] when docker
/// cannot start the container.
pub(crate) fn app_up(
    up: &AppUp<'_>,
    env: impl ChildEnv,
    until: (Duration, &Cancel),
) -> Result<Up, Error> {
    let project = up.scope.project();
    let step_args: [&OsStr; 2] = match up.step {
        AppStep::Create => ["--no-start".as_ref(), "--no-recreate".as_ref()],
        AppStep::Start => ["--detach".as_ref(), "--no-recreate".as_ref()],
    };
    let args: [&OsStr; 17] = [
        "compose".as_ref(),
        "--project-name".as_ref(),
        project.as_str().as_ref(),
        "--project-directory".as_ref(),
        up.dir.as_os_str(),
        "--env-file".as_ref(),
        "/dev/null".as_ref(),
        "--file".as_ref(),
        "-".as_ref(),
        "up".as_ref(),
        step_args[0],
        step_args[1],
        "--no-deps".as_ref(),
        "--pull".as_ref(),
        "never".as_ref(),
        "--".as_ref(),
        up.service.as_ref(),
    ];
    let (deadline, cancel) = until;
    let mut finished = run_input(
        args,
        env,
        up.file.to_string().into_bytes(),
        "compose up",
        deadline,
        cancel,
    )?;
    if !finished.status.success()
        && let Some(image) = up.runs_as_image_of
    {
        let config = image_config(image).ok();
        finished.stderr =
            without_image_user(&finished.stderr, config.as_ref().map(ImageConfig::user));
    }
    up_result(up.scope, up.service, &finished)
}

/// Returns what the `docker compose up` of `service` in `scope` that
/// ended as `finished` did.
fn up_result(scope: &Scope, service: &ServiceName, finished: &Finished) -> Result<Up, Error> {
    if finished.status.success() {
        return Ok(Up::Started);
    }
    if finished.stderr.contains("Conflict") {
        // Another program created a container with the name after the
        // engine removed its own, so the name is foreign.
        inspect_named(scope, service)?;
    }
    match up_failure(&finished.stderr) {
        UpFailure::PortAllocated(port) => Ok(Up::PortAllocated {
            port,
            docker_line: compose_docker_line(&finished.stderr),
        }),
        UpFailure::NoAddressPool => Err(Error::NoAddressPool {
            message: message(&finished.stderr),
            docker_line: compose_docker_line(&finished.stderr),
        }),
        UpFailure::Other => {
            check()?;
            Err(Error::failed(
                "compose up",
                Some(service.as_str()),
                message(&finished.stderr),
            )
            .with_docker_line(compose_docker_line(&finished.stderr)))
        }
    }
}
