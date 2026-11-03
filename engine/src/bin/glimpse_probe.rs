//! Glimpse CLI container-probe diagnostic utility.

use std::env;
use std::process;

use glimpse_engine::{demuxer, Player, TrackInfo};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let play_audio = args.iter().any(|arg| arg == "--play");
    let file_path = args.iter().find(|arg| !arg.starts_with("--"));

    let Some(file_path) = file_path else {
        eprintln!("Usage: glimpse_probe [--play] <file>");
        process::exit(2);
    };

    let probe = match demuxer::probe_media(file_path) {
        Ok(probe) => probe,
        Err(error) => {
            eprintln!("Probe failed: {error}");
            process::exit(1);
        }
    };

    match probe.duration {
        Some(duration) => println!("Duration: {:.3}s", duration.as_secs_f64()),
        None => println!("Duration: unknown"),
    }
    println!(
        "Streams: {} video, {} audio, {} subtitle",
        probe.tracks.video_tracks.len(),
        probe.tracks.audio_tracks.len(),
        probe.tracks.subtitle_tracks.len()
    );

    print_tracks("Video", &probe.tracks.video_tracks);
    print_tracks("Audio", &probe.tracks.audio_tracks);
    print_tracks("Subtitle", &probe.tracks.subtitle_tracks);

    if play_audio {
        match Player::open(file_path) {
            Ok((player, _)) => {
                player.play();
                println!(
                    "Engine state demonstration only (no decoding/output yet): {:?}",
                    player.state()
                );
            }
            Err(error) => {
                eprintln!("Could not initialise player: {error}");
                process::exit(1);
            }
        }
    }
}

fn print_tracks(label: &str, tracks: &[TrackInfo]) {
    for track in tracks {
        println!(
            "  {label} #{}: {}{}{}",
            track.id,
            track.codec_name,
            track
                .width
                .zip(track.height)
                .map(|(width, height)| format!(" {width}x{height}"))
                .unwrap_or_default(),
            track
                .language
                .as_deref()
                .map(|language| format!(" [{language}]"))
                .unwrap_or_default()
        );
    }
}
