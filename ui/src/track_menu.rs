//! Audio and embedded subtitle stream selection.

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, CheckButton, Label, Orientation, Popover, PolicyType, ScrolledWindow};
use glimpse_engine::{TrackId, TrackInfo, TrackList};
use crate::interaction::attach_popover_scroll_guard;

pub fn create_track_popover<FAudio, FSubtitle>(
    tracks: &TrackList,
    on_audio_selected: FAudio,
    on_subtitle_selected: FSubtitle,
) -> Popover
where
    FAudio: Fn(Option<TrackId>) + 'static + Clone,
    FSubtitle: Fn(Option<TrackId>) + 'static + Clone,
{
    let popover = Popover::new();
    popover.add_css_class("menu-popover");
    let container = GtkBox::new(Orientation::Vertical, 8);
    container.set_margin_start(12);
    container.set_margin_end(12);
    container.set_margin_top(12);
    container.set_margin_bottom(12);

    let audio_label = Label::new(Some("Audio Streams"));
    audio_label.set_halign(gtk4::Align::Start);
    audio_label.add_css_class("heading");
    container.append(&audio_label);

    let audio_off_btn = CheckButton::with_label("Off");
    audio_off_btn.set_active(tracks.active_audio.is_none());
    let cb_audio_off = on_audio_selected.clone();
    audio_off_btn.connect_toggled(move |button| {
        if button.is_active() { cb_audio_off(None); }
    });
    container.append(&audio_off_btn);

    let mut first_audio_btn: Option<CheckButton> = Some(audio_off_btn);
    for track in &tracks.audio_tracks {
        let btn = CheckButton::with_label(&track.display_label());
        btn.set_group(first_audio_btn.as_ref());
        if tracks.active_audio == Some(track.id) { btn.set_active(true); }
        let cb = on_audio_selected.clone();
        let tid = track.id;
        btn.connect_toggled(move |button| {
            if button.is_active() { cb(Some(tid)); }
        });
        if first_audio_btn.is_none() { first_audio_btn = Some(btn.clone()); }
        container.append(&btn);
    }

    let subtitle_label = Label::new(Some("Subtitles"));
    subtitle_label.set_halign(gtk4::Align::Start);
    subtitle_label.add_css_class("heading");
    container.append(&subtitle_label);

    let subtitle_off_btn = CheckButton::with_label("Off");
    subtitle_off_btn.set_active(tracks.active_subtitle.is_none());
    let cb_subtitle_off = on_subtitle_selected.clone();
    subtitle_off_btn.connect_toggled(move |button| {
        if button.is_active() { cb_subtitle_off(None); }
    });
    container.append(&subtitle_off_btn);

    let mut first_subtitle_btn: Option<CheckButton> = Some(subtitle_off_btn);
    let subtitle_tracks_box = GtkBox::new(Orientation::Vertical, 4);
    for track in ordered_subtitle_tracks(&tracks.subtitle_tracks) {
        let btn = CheckButton::with_label(&track.display_label());
        btn.set_group(first_subtitle_btn.as_ref());
        if tracks.active_subtitle == Some(track.id) { btn.set_active(true); }
        let cb = on_subtitle_selected.clone();
        let tid = track.id;
        btn.connect_toggled(move |button| {
            if button.is_active() { cb(Some(tid)); }
        });
        if first_subtitle_btn.is_none() { first_subtitle_btn = Some(btn.clone()); }
        subtitle_tracks_box.append(&btn);
    }

    let subtitle_scroll = ScrolledWindow::new();
    subtitle_scroll.set_policy(PolicyType::Never, PolicyType::Automatic);
    subtitle_scroll.set_max_content_height(280);
    subtitle_scroll.set_propagate_natural_height(true);
    subtitle_scroll.set_child(Some(&subtitle_tracks_box));
    container.append(&subtitle_scroll);

    popover.set_child(Some(&container));
    attach_popover_scroll_guard(&popover);
    popover
}

fn ordered_subtitle_tracks(tracks: &[TrackInfo]) -> Vec<&TrackInfo> {
    let preferred = glib::language_names();
    let mut languages = Vec::new();
    for locale in preferred {
        let language = locale.split(['_', '-', '.']).next().unwrap_or("").to_ascii_lowercase();
        if !language.is_empty() && language != "c" && !languages.contains(&language) {
            languages.push(language);
        }
    }
    let english = languages.iter().position(|language| language == "en");
    let mut ranked: Vec<_> = tracks.iter().collect();
    ranked.sort_by_key(|track| {
        let language = track.language.as_deref().unwrap_or("").to_ascii_lowercase();
        let normalized = match language.as_str() {
            "eng" => "en",
            "hrv" | "scr" => "hr",
            "deu" | "ger" => "de",
            "fra" | "fre" => "fr",
            "spa" => "es",
            "ita" => "it",
            "por" => "pt",
            "jpn" => "ja",
            "zho" | "chi" => "zh",
            other => other,
        };
        languages.iter().position(|preferred| preferred == normalized)
            .map(|rank| (rank, 0usize))
            .or_else(|| (normalized == "en").then_some((english.unwrap_or(usize::MAX - 1), 0)))
            .unwrap_or((usize::MAX, track.id.0))
    });
    ranked
}
