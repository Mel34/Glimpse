//! Exercises the Player-owned PipeWire session lifecycle against a real media file.

use std::env;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use glimpse_engine::{Event, Player, PlayerState};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("Usage: glimpse_player_lifecycle_probe <media-file>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("Usage: glimpse_player_lifecycle_probe <media-file>");
        return ExitCode::from(2);
    }

    match run(&path) {
        Ok(()) => {
            println!("Player lifecycle: pause, resume, EOF, replay, and stop all passed");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Player lifecycle probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &str) -> Result<(), String> {
    let (player, events) = Player::open(path)?;
    while events.try_recv().is_ok() {}

    player.set_volume(0.8);
    player.play();
    thread::sleep(Duration::from_millis(700));
    let before_pause = player.position();
    if before_pause < Duration::from_millis(50) {
        return Err(format!("Playback clock did not advance before pause: {before_pause:?}"));
    }

    player.pause();
    let paused_at = player.position();
    thread::sleep(Duration::from_millis(300));
    let paused_after = player.position();
    if paused_after.abs_diff(paused_at) > Duration::from_millis(30) {
        return Err(format!(
            "Clock moved while paused: {paused_at:?} -> {paused_after:?}"
        ));
    }
    println!("Pause: clock held at {paused_after:?}");

    player.set_volume(0.6);
    player.play();
    thread::sleep(Duration::from_millis(500));
    let resumed_at = player.position();
    if resumed_at <= paused_after + Duration::from_millis(50) {
        return Err(format!(
            "Playback clock did not resume: paused={paused_after:?}, resumed={resumed_at:?}"
        ));
    }
    println!("Resume: clock advanced to {resumed_at:?}");

    let timeout = player
        .duration()
        .unwrap_or(Duration::from_secs(600))
        .saturating_add(Duration::from_secs(15));
    wait_for_end_of_stream(&events, timeout)?;
    if player.state() != PlayerState::Ended {
        return Err(format!("Expected PlayerState::Ended, got {:?}", player.state()));
    }
    println!("Natural EOF: PlayerState::Ended");

    player.play();
    thread::sleep(Duration::from_millis(700));
    let replay_position = player.position();
    if replay_position < Duration::from_millis(50)
        || replay_position > Duration::from_secs(3)
    {
        return Err(format!(
            "Replay did not restart near the beginning: {replay_position:?}"
        ));
    }
    println!("Replay: restarted at {replay_position:?}");

    let stop_started = Instant::now();
    player.stop();
    let stop_elapsed = stop_started.elapsed();
    if stop_elapsed > Duration::from_secs(3) {
        return Err(format!("Stopping Player took too long: {stop_elapsed:?}"));
    }
    if player.state() != PlayerState::Paused || player.position() > Duration::from_millis(50) {
        return Err("Stop did not reset the Player to the paused start position".to_string());
    }
    println!("Stop: session joined and position reset in {stop_elapsed:?}");
    drop(player);
    Ok(())
}

fn wait_for_end_of_stream(events: &Receiver<Event>, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("Timed out waiting for end-of-stream after {timeout:?}"));
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(Event::EndOfStream) => return Ok(()),
            Ok(Event::Error(error)) => return Err(error),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Player event channel disconnected before end-of-stream".to_string());
            }
        }
    }
}
