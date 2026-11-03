use std::time::Duration;
use glimpse_engine::{MasterClock, Player, Serial};
use glimpse_engine::queue::BoundedQueue;

#[test]
fn test_audio_master_clock_progression() {
    let clock = MasterClock::new();
    clock.reset(0.0);
    assert!((clock.get_pts() - 0.0).abs() < 1e-4);

    clock.resume();
    clock.set_audio_pts(1.250, 0.030);

    let current = clock.get_pts();
    assert!((current - 1.220).abs() < 0.020);
}

#[test]
fn test_serial_invalidation_on_seek() {
    let queue = BoundedQueue::<String>::new(50, 1024 * 1024);
    let s0 = Serial(0);

    queue.push("packet_1_before_seek".into(), s0, 20).unwrap();
    queue.push("packet_2_before_seek".into(), s0, 20).unwrap();
    assert_eq!(queue.stats().0, 2);

    let s1 = s0.next();
    queue.flush(s1);
    assert_eq!(queue.stats().0, 0);

    // Stale s0 packet must be discarded
    queue.push("stale_packet".into(), s0, 20).unwrap();
    assert_eq!(queue.stats().0, 0);

    // Fresh s1 packet must be accepted
    queue.push("fresh_packet_after_seek".into(), s1, 20).unwrap();
    assert_eq!(queue.stats().0, 1);

    let item = queue.pop_timeout(Duration::from_millis(50)).expect("Should pop s1 packet");
    assert_eq!(item.item, "fresh_packet_after_seek");
    assert_eq!(item.serial, s1);
}

#[test]
fn test_player_seek_clamping() {
    let (player, _) = Player::open("test.mp4").expect("Failed to open player");
    player.seek(Duration::from_secs(45));
    assert_eq!(player.position(), Duration::from_secs(45));

    player.seek(Duration::from_secs(500));
    assert_eq!(player.position(), Duration::from_secs(124));
}