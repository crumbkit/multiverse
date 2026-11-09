use super::{MISE_JOBS, MOST_COMMAND_BYTES, cut, job_variables};

#[test]
fn a_command_longer_than_the_limit_is_cut_on_a_character_boundary() {
    let command = "é".repeat(MOST_COMMAND_BYTES);
    let kept = cut(&command);
    assert_eq!(kept.len(), MOST_COMMAND_BYTES);
    assert!(kept.chars().all(|character| character == 'é'));
}

#[test]
fn a_command_within_the_limit_is_kept_whole() {
    assert_eq!(cut("cargo build"), "cargo build");
}

#[test]
fn the_job_variables_are_deadbands_and_mise_runs_one_task_at_a_time() {
    let variables = job_variables(&[("CARGO_BUILD_JOBS", "3".to_owned())]);
    let shown: Vec<(&str, &str)> = variables
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(shown, [("CARGO_BUILD_JOBS", "3"), (MISE_JOBS, "1")]);
}
