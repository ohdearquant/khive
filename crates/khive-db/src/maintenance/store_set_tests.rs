use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use super::StoreSet;

#[test]
fn store_set_canonical_order_and_digest_are_stable() {
    let main = PathBuf::from("/stores/main.db");
    let events = PathBuf::from("/stores/main.db.events.db");
    let second = PathBuf::from("/stores/second.db");
    let expected_paths = vec![main.clone(), events.clone(), second.clone()];
    let set = StoreSet::from_canonical(
        main.clone(),
        vec![second, main.clone(), events, main.clone()],
    )
    .expect("resolved topology");
    assert_eq!(set.paths(), expected_paths);
    // Independently frozen Python hashlib/struct vector, not this implementation.
    assert_eq!(
        set.digest(),
        &[
            9, 26, 85, 219, 44, 85, 100, 54, 70, 110, 22, 181, 5, 93, 205, 0, 202, 5, 29, 58, 151,
            96, 78, 152, 192, 242, 13, 225, 9, 39, 73, 121,
        ]
    );
    assert_eq!(
        set,
        StoreSet::from_canonical(main.clone(), expected_paths).unwrap()
    );
    assert_ne!(
        set.digest(),
        StoreSet::from_canonical(main.clone(), vec![main])
            .unwrap()
            .digest()
    );
}

#[test]
fn store_set_preserves_main_and_non_utf8_paths() {
    let earlier = PathBuf::from("/stores/a.db");
    let main = PathBuf::from("/stores/z.db");
    let opaque = PathBuf::from(OsString::from_vec(b"/stores/\xff.db".to_vec()));
    let paths = vec![opaque.clone(), main.clone(), earlier.clone()];
    let set = StoreSet::from_canonical(main.clone(), paths.clone()).unwrap();
    assert_eq!(set.main(), main);
    assert_eq!(set.paths()[0], earlier);
    assert_eq!(set.paths()[2].as_os_str().as_bytes(), b"/stores/\xff.db");
    let other_main = StoreSet::from_canonical(opaque, paths).unwrap();
    assert_eq!(set.paths(), other_main.paths());
    assert_ne!(
        set.digest(),
        other_main.digest(),
        "main role must participate in identity"
    );
    assert_eq!(set.clone(), set);
}

#[test]
fn store_set_refuses_invalid_path_contract() {
    let main = PathBuf::from("/stores/main.db");
    let invalid = [
        "",
        "relative.db",
        "/",
        "/stores/../main.db",
        "/stores/./main.db",
        "/stores//main.db",
        "//stores/main.db",
        "/stores/main.db/",
    ];
    for spelling in invalid {
        let path = PathBuf::from(spelling);
        assert_eq!(
            StoreSet::from_canonical(path.clone(), vec![path.clone()])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput,
            "invalid main {spelling:?}"
        );
        assert_eq!(
            StoreSet::from_canonical(main.clone(), vec![main.clone(), path])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput,
            "invalid member {spelling:?}"
        );
    }
    let nul = PathBuf::from(OsString::from_vec(b"/stores/a\0.db".to_vec()));
    assert_eq!(
        StoreSet::from_canonical(main.clone(), vec![main.clone(), nul])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert!(StoreSet::from_canonical(main.clone(), Vec::new()).is_err());
    assert!(StoreSet::from_canonical(main, vec![PathBuf::from("/stores/other.db")]).is_err());
}
