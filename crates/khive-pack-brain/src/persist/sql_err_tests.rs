use super::sql_err;
use khive_runtime::RuntimeError;

#[test]
fn brain_persistence_sql_err_message_is_prefixed_context_and_error() {
    match sql_err("brain persistence insert", "boom") {
        RuntimeError::Internal(message) => {
            assert_eq!(message, "brain persistence insert: boom");
        }
        other => panic!("expected RuntimeError::Internal, got {other:?}"),
    }
}
