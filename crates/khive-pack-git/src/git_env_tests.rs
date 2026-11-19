use super::*;

#[test]
fn shared_env_drops_variables_set_before_it() {
    let mut command = Command::new("git");
    command.env("KHIVE_GIT_ENV_PRESET", "inherited");
    apply_shared_env(&mut command);
    let (_, envs) = describe(&command);
    assert!(
        !envs.contains_key("KHIVE_GIT_ENV_PRESET"),
        "the shared environment must start from an emptied one"
    );
    assert_eq!(envs.get("LC_ALL"), Some(&Some("C".to_owned())));
}
