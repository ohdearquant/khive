use super::load_fvecs;
use std::io::Write;

fn fixture(values: &[f32]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    for row in values.chunks_exact(2) {
        file.write_all(&2_i32.to_le_bytes()).unwrap();
        for value in row {
            file.write_all(&value.to_le_bytes()).unwrap();
        }
    }
    file.flush().unwrap();
    file
}

#[test]
fn fvecs_identity_hashes_the_exact_loaded_file_including_headers() {
    let file = fixture(&[1.0, 2.0, 3.0, 4.0]);
    let bytes = std::fs::read(file.path()).unwrap();
    let (values, dim, digest) = load_fvecs(file.path());
    assert_eq!(values, vec![1.0, 2.0, 3.0, 4.0]);
    assert_eq!(dim, 2);
    assert_eq!(digest, blake3::hash(&bytes).to_hex().to_string());
    let vector_bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    assert_ne!(digest, blake3::hash(&vector_bytes).to_hex().to_string());
}

#[test]
fn same_shape_changed_values_change_the_input_identity() {
    let first = fixture(&[1.0, 2.0, 3.0, 4.0]);
    let second = fixture(&[1.0, 2.0, 3.0, 5.0]);
    assert_ne!(load_fvecs(first.path()).2, load_fvecs(second.path()).2);
}
