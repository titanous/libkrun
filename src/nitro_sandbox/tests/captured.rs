use nitro_sandbox::device_snapshot;
#[test]
fn m8g_driver_ok_device_entries() {
    for (uid, kind, data) in [
        (1, 2, include_bytes!("fixtures/device-1.bin").as_slice()),
        (2, 19, include_bytes!("fixtures/device-2.bin").as_slice()),
        (3, 3, include_bytes!("fixtures/device-3.bin").as_slice()),
    ] {
        let snapshot = device_snapshot(data, uid).unwrap().unwrap();
        assert_eq!(snapshot.uid, uid);
        assert_eq!(snapshot.device_type, kind);
        assert_eq!(snapshot.status & 15, 15);
        assert!(!snapshot.queues.is_empty());
        for queue in snapshot.queues.iter().filter(|q| q.enabled) {
            assert!(queue.size.is_power_of_two());
            assert!(queue.descriptor >= 0x40000000);
            assert!(queue.available >= 0x40000000);
            assert!(queue.used >= 0x40000000);
        }
    }
}
