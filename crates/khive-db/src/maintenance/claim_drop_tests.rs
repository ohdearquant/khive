use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{process, ClaimError, DeclarationWriter, RecordLocation, RecordSlot};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("private claim fixture");
    let root = std::fs::canonicalize(temp.path()).expect("canonical private fixture");
    (temp, root)
}

fn location(root: &Path) -> RecordLocation {
    RecordLocation::open_discovered(
        root,
        RecordSlot::new(OsStr::new("claim-drop-test.json")).expect("fixture slot"),
    )
    .expect("private fixture location")
}

fn released_with_retained_description(root: &Path, writer: DeclarationWriter) -> bool {
    let retained = writer.claim.try_clone().expect("retain claim description");
    let active_probe = DeclarationWriter::claim(location(root));
    assert!(
        matches!(&active_probe, Err(ClaimError::Busy)),
        "fixture active writer must exclusively own its claim"
    );
    drop(active_probe);
    drop(writer);

    let after_drop = DeclarationWriter::claim(location(root));
    let released = match &after_drop {
        Ok(_) => true,
        Err(ClaimError::Busy) => false,
        Err(error) => panic!("post-drop fixture claim refused: {error}"),
    };
    // Save the oracle before cleanup. Passive aliases inherited by concurrent
    // process creation must not make fixture cleanup depend on the last close.
    // An acquired contender must close first so it cannot spoil recovery.
    drop(after_drop);
    fs4::FileExt::unlock(&retained).expect("fixture cleanup releases retained description");
    drop(retained);
    let recovered = DeclarationWriter::claim(location(root))
        .expect("fixture claim recovers after explicit retained-description cleanup");
    drop(recovered);
    released
}

#[test]
fn writer_drop_releases_claim_while_description_is_retained() {
    let (_temp, root) = fixture();
    let writer = DeclarationWriter::claim(location(&root)).expect("fixture writer");
    process::same_incarnation(&writer.incarnation, process::capture())
        .expect("fixture writer has the current full incarnation");

    assert!(
        released_with_retained_description(&root, writer),
        "claim_drop_releases_retained_description: writer drop must release before its final alias closes"
    );
}

#[test]
fn writer_drop_keeps_same_pid_foreign_incarnation_claim() {
    let (_temp, root) = fixture();
    let mut writer = DeclarationWriter::claim(location(&root)).expect("fixture writer");
    let observed = process::capture().expect("fixture current incarnation");
    assert_eq!(writer.incarnation.pid, observed.pid);
    // Synthetic identity-input coverage: the PID stays equal while the full
    // incarnation differs. This is not a real fork or PID-namespace fixture.
    writer.incarnation.start_ticks = writer.incarnation.start_ticks.wrapping_add(1);
    assert!(process::same_incarnation(&writer.incarnation, Ok(observed)).is_err());

    assert!(
        !released_with_retained_description(&root, writer),
        "claim_drop_preserves_other_incarnation_claim: matching PID alone must not authorize unlock"
    );
}
