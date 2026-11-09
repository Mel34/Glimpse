//! Track selection menu for audio streams and subtitles (default OFF).

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, CheckButton, Label, Orientation, Popover};
use glimpse_engine::{TrackId, TrackList};

pub fn create_track_popover<FAudio, FSub>(
    tracks: &TrackList,
    on_audio_selected: FAudio,
    on_subtitle_selected: FSub,
) -> Popover
where
    FAudio: Fn(Option<TrackId>) + 'static + Clone,
    FSub: Fn(Option<TrackId>) + 'static + Clone,
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

    let mut first_audio_btn: Option<CheckButton> = None;
    for track in &tracks.audio_tracks {
        let btn = CheckButton::with_label(&track.display_label());
        if let Some(ref group) = first_audio_btn {
            btn.set_group(Some(group));
        } else {
            first_audio_btn = Some(btn.clone());
        }

        if tracks.active_audio == Some(track.id) {
            btn.set_active(true);
        }

        let cb = on_audio_selected.clone();
        let tid = track.id;
        btn.connect_toggled(move |b| {
            if b.is_active() {
                cb(Some(tid));
            }
        });
        container.append(&btn);
    }

    let sub_label = Label::new(Some("Subtitles"));
    sub_label.set_halign(gtk4::Align::Start);
    sub_label.add_css_class("heading");
    container.append(&sub_label);

    let off_btn = CheckButton::with_label("Off");
    let is_off = tracks.active_subtitle.is_none();
    off_btn.set_active(is_off);

    let cb_off = on_subtitle_selected.clone();
    off_btn.connect_toggled(move |b| {
        if b.is_active() {
            cb_off(None);
        }
    });
    container.append(&off_btn);

    for track in &tracks.subtitle_tracks {
        let btn = CheckButton::with_label(&track.display_label());
        btn.set_group(Some(&off_btn));

        if tracks.active_subtitle == Some(track.id) {
            btn.set_active(true);
        }

        let cb = on_subtitle_selected.clone();
        let tid = track.id;
        btn.connect_toggled(move |b| {
            if b.is_active() {
                cb(Some(tid));
            }
        });
        container.append(&btn);
    }

    popover.set_child(Some(&container));
    popover
}