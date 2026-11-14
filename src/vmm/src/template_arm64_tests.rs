use super::*;

#[test]
fn arm64_capture_requires_frozen_publication_and_seals_storage() {
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0x4000_0000), 16384)]).unwrap();
    let control = 0x4000_0000;
    assert!(Template::capture(&memory, control, 1, 1).is_err());
    let mut page = [0u8; 4096];
    for (index, value) in [
        (0, nitro_sandbox::cooperative::FROZEN_MAGIC),
        (5, 0x4000_1000),
        (6, 1),
        (7, 0x4000_2000),
    ] {
        page[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    memory.write_slice(&page, GuestAddress(control)).unwrap();
    let template = Template::capture_with_manifest(
        &memory,
        control,
        1,
        1,
        0,
        b"devices",
        b"arm64-kernel-identity",
    )
    .unwrap();
    let imported = Template::open(template.file.try_clone().unwrap()).unwrap();
    assert_eq!(imported.control, control);
    assert_eq!(imported.topology, b"devices");
    assert_eq!(imported.compatibility, b"arm64-kernel-identity");
    assert_eq!(imported.regions.len(), 1);
    assert_eq!(imported.regions[0].0, 0x4000_0000);
    assert!(imported.file.write_all_at(&[0], 0).is_err());
}
