//! Glimpse CLI Probe & Decode Diagnostic Utility (Milestone 1 & 2 acceptance tool)

use std::env;
use std::process;
use glimpse_engine::{demuxer, Player};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: glimpse_probe [--play] <file>");
        process::exit(1);
    }

    let play_audio = args.contains(&"--play".to_string());
    let file_path = args.iter().find(|a| !a.starts_with("--") && a.as_str() != args[0]).unwrap();

    let tracks = demuxer::probe_file(file_path).expect("Failed to probe file");
    println!("Streams: {} video, {} audio, {} subtitle",
        tracks.video_tracks.len(), tracks.audio_tracks.len(), tracks.subtitle_tracks.len());

    if play_audio {
        let (player, _) = Player::open(file_path).unwrap();
        player.play();
        println!("Playback active on Audio Master Clock: {:.3}s", player.position().as_secs_f64());
    }
}