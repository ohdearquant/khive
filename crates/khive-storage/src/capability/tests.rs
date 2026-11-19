use super::StorageCapability;

#[test]
fn additive_capabilities_preserve_the_public_discriminant_order() {
    assert_eq!(StorageCapability::Blob as usize, 8);
    assert_eq!(StorageCapability::Attachments as usize, 9);
}
