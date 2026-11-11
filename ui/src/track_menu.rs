//! Audio, embedded subtitle, and external subtitle selection.

use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use gtk4::{Box as GtkBox, CheckButton, Label, Orientation, Popover, PolicyType, ScrolledWindow, Separator};
use glimpse_engine::{TrackId, TrackInfo, TrackList};
use crate::interaction::attach_popover_scroll_guard;

pub fn create_track_popover<FAudio, FSubtitle, FExternal, FSpeed>(
    tracks: &TrackList,
    on_audio_selected: FAudio,
    on_subtitle_selected: FSubtitle,
    on_external_selected: FExternal,
    initial_speed: f64,
    on_speed_changed: FSpeed,
    loop_enabled: &Rc<Cell<bool>>,
    loop_menu_buttons: &Rc<RefCell<Vec<CheckButton>>>,
) -> Popover
where
    FAudio: Fn(Option<TrackId>) + 'static + Clone,
    FSubtitle: Fn(Option<TrackId>) + 'static + Clone,
    FExternal: Fn(CheckButton, CheckButton) + 'static + Clone,
    FSpeed: Fn(f64) + 'static + Clone,
{
    let popover = Popover::new();
    popover.add_css_class("menu-popover");
    let container = GtkBox::new(Orientation::Vertical, 8);
    container.set_margin_start(12);
    container.set_margin_end(12);
    container.set_margin_top(12);
    container.set_margin_bottom(12);

    let playback_label = Label::new(Some("Playback"));
    playback_label.set_halign(gtk4::Align::Start);
    playback_label.add_css_class("heading");
    container.append(&playback_label);

    let speed_row = GtkBox::new(Orientation::Horizontal, 12);
    let speed_label = Label::new(Some("Playback speed"));
    speed_label.set_halign(gtk4::Align::Start);
    speed_label.set_hexpand(true);
    let speed_value = Label::new(Some(&format_playback_speed(initial_speed)));
    speed_value.add_css_class("dim-label");
    speed_row.append(&speed_label);
    speed_row.append(&speed_value);
    container.append(&speed_row);

    let speed_scale = gtk4::Scale::with_range(Orientation::Horizontal, 0.5, 2.0, 0.001);
    speed_scale.set_draw_value(false);
    speed_scale.set_hexpand(true);
    speed_scale.set_digits(3);
    speed_scale.set_value(initial_speed.clamp(0.5, 2.0));
    speed_scale.add_mark(0.5, gtk4::PositionType::Bottom, Some("0.5×"));
    speed_scale.add_mark(1.0, gtk4::PositionType::Bottom, Some("1.0×"));
    speed_scale.add_mark(2.0, gtk4::PositionType::Bottom, Some("2.0×"));
    let speed_value_changed = speed_value.clone();
    let speed_callback = on_speed_changed.clone();
    speed_scale.connect_value_changed(move |scale| {
        let speed = scale.value();
        speed_value_changed.set_text(&format_playback_speed(speed));
        speed_callback(speed);
    });
    container.append(&speed_scale);

    let loop_row = GtkBox::new(Orientation::Horizontal, 20);
    let loop_toggle = CheckButton::with_label("Loop");
    loop_toggle.set_hexpand(true);
    loop_toggle.set_halign(gtk4::Align::Start);
    loop_toggle.set_active(loop_enabled.get());
    loop_menu_buttons.borrow_mut().clear();
    loop_menu_buttons.borrow_mut().push(loop_toggle.clone());
    let loop_shortcut = Label::new(Some("Shift+L"));
    loop_shortcut.add_css_class("dim-label");
    loop_shortcut.set_halign(gtk4::Align::End);
    loop_row.append(&loop_toggle);
    loop_row.append(&loop_shortcut);
    let loop_enabled_toggle = Rc::clone(loop_enabled);
    let loop_buttons_toggle = Rc::clone(loop_menu_buttons);
    loop_toggle.connect_toggled(move |button| {
        let enabled = button.is_active();
        loop_enabled_toggle.set(enabled);
        for menu_button in loop_buttons_toggle.borrow().iter() {
            menu_button.set_active(enabled);
        }
    });
    container.append(&loop_row);

    let playback_separator = Separator::new(Orientation::Horizontal);
    container.append(&playback_separator);

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
    let external_button = CheckButton::with_label("Load External Subtitle…");
    external_button.set_group(Some(&subtitle_off_btn));

    let cb_subtitle_off = on_subtitle_selected.clone();
    let external_off_button = external_button.clone();
    subtitle_off_btn.connect_toggled(move |button| {
        if button.is_active() {
            external_off_button.set_label(Some("Load External Subtitle…"));
            cb_subtitle_off(None);
        }
    });
    container.append(&subtitle_off_btn);

    let cb_external = on_external_selected.clone();
    let off_button_for_external = subtitle_off_btn.clone();
    external_button.connect_toggled(move |button| {
        if button.is_active() {
            cb_external(button.clone(), off_button_for_external.clone());
        }
    });
    container.append(&external_button);

    let mut first_subtitle_btn: Option<CheckButton> = Some(subtitle_off_btn);
    let subtitle_tracks_box = GtkBox::new(Orientation::Vertical, 4);
    for track in ordered_subtitle_tracks(&tracks.subtitle_tracks) {
        let btn = CheckButton::with_label(&track.display_label());
        btn.set_group(first_subtitle_btn.as_ref());
        if tracks.active_subtitle == Some(track.id) { btn.set_active(true); }
        let cb = on_subtitle_selected.clone();
        let external_button_for_track = external_button.clone();
        let tid = track.id;
        btn.connect_toggled(move |button| {
            if button.is_active() {
                external_button_for_track.set_label(Some("Load External Subtitle…"));
                cb(Some(tid));
            }
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

fn format_playback_speed(speed: f64) -> String {
    let mut value = format!("{speed:.3}");
    while value.ends_with('0')
        && value.split('.').nth(1).is_some_and(|decimals| decimals.len() > 1)
    {
        value.pop();
    }
    format!("{value}×")
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
