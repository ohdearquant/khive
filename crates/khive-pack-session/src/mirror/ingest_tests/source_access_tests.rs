#[cfg(unix)]
#[test]
fn source_open_refuses_a_symlink() {
    let dir = TempDir::new().expect("tempdir");
    let outside = NamedTempFile::new().expect("outside file");
    let link = dir.path().join("linked.jsonl");
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    assert!(open_source_file(&link).is_err());
}

#[cfg(unix)]
#[test]
fn source_open_preserves_bytes_and_ancestor_links_but_refuses_nonfiles() {
    let dir = TempDir::new().expect("tempdir");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).expect("source directory");
    let source = real.join("source.jsonl");
    let bytes = b"source\0bytes\xff\n";
    std::fs::write(&source, bytes).expect("source bytes");
    let linked = dir.path().join("linked-parent");
    std::os::unix::fs::symlink(&real, &linked).expect("ancestor link");
    let mut opened = open_source_file(&linked.join("source.jsonl")).expect("open source");
    let mut read = Vec::new();
    opened.read_to_end(&mut read).expect("read source");
    assert_eq!(read, bytes);

    let error = open_source_file(&real).expect_err("directory is not a source file");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "mirror source is not a regular file");
    let missing = open_source_file(&real.join("missing")).expect_err("missing source");
    assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
}

#[cfg(unix)]
#[test]
fn scheduled_file_rejects_replaced_parent_symlink_at_probe_and_open() {
    let dir = TempDir::new().expect("tempdir");
    let fixture = std::fs::canonicalize(dir.path()).expect("fixture directory");
    let root = fixture.join("root");
    let parent = root.join("staged");
    let outside = fixture.join("outside");
    std::fs::create_dir_all(&parent).expect("inside parent");
    std::fs::create_dir_all(&outside).expect("outside parent");
    let source = parent.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("inside source");
    let outside_source = outside.join("source.jsonl");
    std::fs::write(&outside_source, b"outside\n").expect("outside source");

    let (original_probe, directory_identities) =
        open_source_file_beneath(&root, &source, None).expect("initial probe");
    assert!(original_probe
        .metadata()
        .expect("inside metadata")
        .is_file());
    std::fs::rename(&parent, root.join("staged-old")).expect("move inside parent");
    std::os::unix::fs::symlink(&outside, &parent).expect("replace parent with symlink");

    assert!(open_source_file_beneath(&root, &source, None).is_err());
    let outside_identity = file_identity(&std::fs::File::open(&outside_source).expect("outside"))
        .expect("outside identity");
    assert!(read_bounded_chunk(
        &source,
        0,
        LineTailSource::ClaudeCode,
        None,
        MirrorLimits::production(),
        Some(&outside_identity),
        Some(TrustedSource {
            root: &root,
            directory_identities: &directory_identities,
        }),
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn source_root_opens_beneath_a_root_owned_system_ancestor_symlink() {
    use std::os::unix::fs::MetadataExt;

    let mut fixture = None;
    for parent in [PathBuf::from("/tmp"), std::env::temp_dir()] {
        let mut has_root_owned_link = false;
        let mut all_links_root_owned = true;
        for ancestor in parent.ancestors() {
            let Ok(metadata) = std::fs::symlink_metadata(ancestor) else {
                all_links_root_owned = false;
                break;
            };
            if metadata.file_type().is_symlink() {
                has_root_owned_link = true;
                all_links_root_owned &= metadata.uid() == 0;
            }
        }
        if !has_root_owned_link || !all_links_root_owned {
            continue;
        }
        if let Ok(temp) = TempDir::new_in(&parent) {
            assert!(
                temp.path().starts_with(&parent),
                "fixture retains the system ancestor spelling"
            );
            fixture = Some(temp);
            break;
        }
    }
    let Some(temp) = fixture else {
        eprintln!("QUALIFIED SKIP: no writable temporary base beneath a root-owned system ancestor symlink; run on macOS /tmp or /var/folders");
        return;
    };
    let root = temp.path().join("root");
    std::fs::create_dir(&root).expect("configured root");
    let source = root.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("source fixture");
    let physical_source = std::fs::canonicalize(&source).expect("physical source fixture");
    let expected_identity =
        file_identity(&std::fs::File::open(&physical_source).expect("physical source"))
            .expect("physical source identity");
    let physical_root = std::fs::canonicalize(&root).expect("physical root fixture");
    let expected_root_identity =
        file_identity(&std::fs::File::open(&physical_root).expect("physical root"))
            .expect("physical root identity");

    let (opened, directories) =
        open_source_file_beneath(&root, &source, None).expect("root-owned ancestor admission");
    assert_eq!(
        file_identity(&opened).expect("opened identity"),
        expected_identity
    );
    assert_eq!(directories, vec![expected_root_identity]);
    let (reopened, _) = open_source_file_beneath(&root, &source, Some(&directories))
        .expect("checked source reopen through system ancestor");
    assert_eq!(
        file_identity(&reopened).expect("reopened identity"),
        expected_identity
    );
    assert_eq!(
        std::fs::read(&physical_source).expect("unchanged source"),
        b"inside\n"
    );
}

#[cfg(unix)]
#[test]
fn source_root_refuses_an_ancestor_symlink_in_a_writable_parent() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("fixture outside source tree");
    let fixture = std::fs::canonicalize(temp.path()).expect("physical fixture anchor");
    let protected = fixture.join("protected");
    let root = protected.join("exports");
    std::fs::create_dir_all(&root).expect("protected fixture root");
    let source = root.join("source.jsonl");
    std::fs::write(&source, b"protected\n").expect("protected source");
    let before = std::fs::metadata(&source).expect("protected metadata before");
    let (original, original_directories) =
        open_source_file_beneath(&root, &source, None).expect("ordinary root admission");
    let original_identity = file_identity(&original).expect("protected source identity");
    let ancestor = fixture.join("linked");
    std::os::unix::fs::symlink(&protected, &ancestor).expect("fixture ancestor link");
    // A parent writable by group and others without the sticky bit is refused whoever owns
    // the link.
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o777))
        .expect("unsafe link parent");
    let linked_root = ancestor.join("exports");
    let linked_source = linked_root.join("source.jsonl");
    let Err(error) = open_source_file_beneath(&linked_root, &linked_source, None) else {
        panic!("an ancestor link in an unsafe parent must refuse before admitting a root");
    };
    let refusal: &khive_fs::directory_walk::AncestorLinkRefusal = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref())
        .expect("shared ancestor policy refusal");
    assert_eq!(
        refusal.condition,
        khive_fs::directory_walk::AncestorLinkCondition::ParentPermissions
    );
    let (reopened, directories) =
        open_source_file_beneath(&root, &source, None).expect("ordinary root remains usable");
    assert_eq!(
        file_identity(&reopened).expect("reopened identity"),
        original_identity
    );
    assert_eq!(directories, original_directories);
    let after = std::fs::metadata(&source).expect("protected metadata after");
    assert_eq!(
        std::fs::read(&source).expect("protected bytes after"),
        b"protected\n"
    );
    assert_eq!(after.len(), before.len());
    assert_eq!(
        after.modified().expect("mtime after"),
        before.modified().expect("mtime before")
    );
}

#[cfg(unix)]
#[test]
fn source_root_refuses_its_leaf_symlink() {
    let temp = TempDir::new().expect("fixture outside source tree");
    let fixture = std::fs::canonicalize(temp.path()).expect("physical fixture anchor");
    let root = fixture.join("root");
    std::fs::create_dir(&root).expect("ordinary source root");
    let source = root.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("source fixture");
    let linked_root = fixture.join("linked-root");
    std::os::unix::fs::symlink(&root, &linked_root).expect("configured-root leaf link");
    assert!(
        open_source_file_beneath(&linked_root, &linked_root.join("source.jsonl"), None).is_err()
    );
    let (opened, _) = open_source_file_beneath(&root, &source, None).expect("ordinary root");
    assert_eq!(
        file_identity(&opened).expect("opened identity"),
        file_identity(&std::fs::File::open(&source).expect("source handle"))
            .expect("source identity")
    );
    assert_eq!(
        std::fs::read(&source).expect("unchanged source"),
        b"inside\n"
    );
}

#[cfg(unix)]
#[test]
fn source_root_keeps_absolute_relative_empty_and_parent_directory_semantics() {
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("current directory"))
        .expect("physical current directory");
    let dir = TempDir::new().expect("fixture outside source tree");
    let absolute_fixture = std::fs::canonicalize(dir.path()).expect("fixture directory");
    let absolute_root = absolute_fixture.join("root");
    std::fs::create_dir(&absolute_root).expect("source root");
    let source = absolute_root.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("source fixture");
    let identity = file_identity(&std::fs::File::open(&source).expect("source handle"))
        .expect("source identity");
    let mut cwd_components = cwd.components().peekable();
    let mut fixture_components = absolute_fixture.components().peekable();
    while cwd_components.peek().is_some() && cwd_components.peek() == fixture_components.peek() {
        cwd_components.next();
        fixture_components.next();
    }
    let mut relative_fixture = PathBuf::new();
    for _ in cwd_components {
        relative_fixture.push("..");
    }
    for component in fixture_components {
        relative_fixture.push(component.as_os_str());
    }
    let relative_root = relative_fixture.join("root");

    for root in [
        absolute_root.clone(),
        relative_root.clone(),
        relative_root.join("..").join("root"),
    ] {
        let path = root.join("source.jsonl");
        let (file, directories) =
            open_source_file_beneath(&root, &path, None).expect("configured root probe");
        assert_eq!(file_identity(&file).expect("opened identity"), identity);
        assert_eq!(directories.len(), 1, "root witness shape remains unchanged");
        let (file, _) = open_source_file_beneath(&root, &path, Some(&directories))
            .expect("checked source reopen");
        assert_eq!(file_identity(&file).expect("reopened identity"), identity);
    }

    let cwd_identity = file_identity(&std::fs::File::open(&cwd).expect("current directory"))
        .expect("current-directory identity");
    for root in [Path::new(""), Path::new(".")] {
        let directories = open_source_root(root).expect("current-directory root probe");
        assert_eq!(directories.len(), 1);
        assert_eq!(
            file_identity(&directories[0]).expect("root identity"),
            cwd_identity
        );
    }

    let filesystem_root = open_source_root(Path::new("/")).expect("filesystem root");
    assert_eq!(filesystem_root.len(), 1);
    assert!(filesystem_root[0]
        .metadata()
        .expect("root metadata")
        .is_dir());
}

#[cfg(windows)]
#[test]
fn mirror_windows_file_identity_changes_when_renamed_replacement_takes_the_path() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("transcript.jsonl");
    std::fs::write(&path, b"original\n").expect("original file");
    let original =
        file_identity(&open_source_file(&path).expect("open original")).expect("original identity");
    assert!(
        original.starts_with("windows:"),
        "unexpected identity spelling: {original}"
    );

    let mut appender = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for append");
    appender.write_all(b"appended\n").expect("append");
    drop(appender);
    assert_eq!(
        file_identity(&open_source_file(&path).expect("open appended")).expect("appended identity"),
        original,
        "an append keeps the file identity"
    );

    // Save-by-rename: write a sibling temp file, then rename it over the
    // original name. The replacement may inherit the original's creation
    // time, so only the handle's file id can tell the two files apart.
    let staged = dir.path().join("transcript.jsonl.tmp");
    std::fs::write(&staged, b"replacement\n").expect("staged file");
    std::fs::rename(&staged, &path).expect("rename over original");
    let replaced = file_identity(&open_source_file(&path).expect("open replacement"))
        .expect("replacement identity");
    assert_ne!(
        replaced, original,
        "a same-path replacement must not reuse the original identity"
    );
}

#[cfg(windows)]
#[test]
fn mirror_windows_scheduled_file_rejects_parent_and_file_reparse_points() {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("root");
    let parent = root.join("staged");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&parent).expect("inside parent");
    std::fs::create_dir_all(&outside).expect("outside parent");
    let source = parent.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("inside source");
    let outside_source = outside.join("source.jsonl");
    std::fs::write(&outside_source, b"outside\n").expect("outside source");

    let (_, directory_identities) =
        open_source_file_beneath(&root, &source, None).expect("initial probe");
    std::fs::rename(&parent, root.join("staged-old")).expect("move inside parent");
    symlink_dir(&outside, &parent).expect("create parent link");
    assert!(open_source_file_beneath(&root, &source, None).is_err());
    assert!(open_source_file_beneath(&root, &source, Some(&directory_identities)).is_err());

    std::fs::remove_dir(&parent).expect("remove parent link");
    std::fs::create_dir(&parent).expect("restore parent");
    symlink_file(&outside_source, &source).expect("link source");
    assert!(open_source_file_beneath(&root, &source, None).is_err());
    assert!(open_source_file(&source).is_err());

    let linked_root_parent = dir.path().join("linked-root-parent");
    symlink_dir(&root, &linked_root_parent).expect("link root parent");
    let nested_root = linked_root_parent.join("staged-old");
    let nested_source = nested_root.join("source.jsonl");
    assert!(open_source_file_beneath(&nested_root, &nested_source, None).is_err());
}
