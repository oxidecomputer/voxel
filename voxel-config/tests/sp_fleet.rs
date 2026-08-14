use voxel_config::sp::SpFleet;

#[test]
fn five_sled_fleet_renders_all_serials() {
    let cfg = SpFleet::sim_for_gimlets(&[0, 1, 2, 3, 4]).sp_sim_config();
    for serial in ["2FAKE000", "2FAKE001", "2FAKE002", "2FAKE003", "2FAKE004"] {
        assert!(cfg.contains(serial), "{serial} missing:\n{cfg}");
    }
    assert_eq!(cfg.matches("simulated_sps.gimlet]]").count(), 5);
}
