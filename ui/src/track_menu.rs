//! Audio stream selection menu with an explicit subtitle support status.

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, CheckButton, Label, Orientation, Popover};
use glimpse_engine::{TrackId, TrackList};
use crate::interaction::attach_popover_scroll_guard;

pub fn create_track_popover<FAudio>(
    tracks: &TrackList,
    on_audio_selected: FAudio,
) -> Popover
where
    FAudio: Fn(Option<TrackId>) + 'static + Clone,
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
        if button.is_active() {
            cb_audio_off(None);
        }
    });
    container.append(&audio_off_btn);

    let mut first_audio_btn: Option<CheckButton> = Some(audio_off_btn);
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

    let subtitle_status = Label::new(Some("Subtitle playback is not available yet"));
    subtitle_status.set_halign(gtk4::Align::Start);
    subtitle_status.add_css_class("dim-label");
    container.append(&subtitle_status);

    popover.set_child(Some(&container));
    attach_popover_scroll_guard(&popover);
    popover
}