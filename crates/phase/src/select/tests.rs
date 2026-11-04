use super::{BadTarget, Target};
use crate::name::BadName;

#[test]
fn a_target_reads_an_app_alone() {
    let target: Target = "api".parse().unwrap();
    assert_eq!(target.app.as_str(), "api");
    assert_eq!(target.process, None);
}

#[test]
fn a_target_reads_an_app_and_a_process() {
    let target: Target = "api/server".parse().unwrap();
    assert_eq!(target.to_string(), "api/server");
}

#[test]
fn a_target_refuses_an_empty_process_name() {
    assert_eq!(
        "api/".parse::<Target>(),
        Err(BadTarget::Process(BadName::Empty))
    );
}
