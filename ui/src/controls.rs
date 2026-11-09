//! Bottom playback control bar matching Showtime's floating OSD layout.
//!
//! Layout structure:
//! ┌──────────────────────────────────────────────────────────────┐
//! │  Title Label                           [Volume Icon] [Gear]  │
//! │  ━━━━━━━━━━━━━━━━━━━●━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  │  (Seekbar)
//! │  0:00                                                  1:10  │  (Timestamps)
//! └──────────────────────────────────────────────────────────────┘

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Image, Label, MenuButton, Orientation, Scale};

#[derive(Clone)]
pub struct ControlBarWidgets {
    pub container: GtkBox,
    pub title_label: Label,
    pub volume_button: MenuButton,
    pub volume_icon: Image,
    pub volume_slider: Scale,
    pub volume_percent_label: Label,
    pub settings_button: MenuButton,
    pub seekbar: Scale,
    pub seekbar_dragging: Rc<Cell<bool>>,
    pub elapsed_label: Label,
    pub duration_label: Label,
}

pub fn create_floating_bottom_bar() -> ControlBarWidgets {
    let container = GtkBox::new(Orientation::Vertical, 6);
    container.add_css_class("floating-osd-bottom");
    container.set_margin_start(24);
    container.set_margin_end(24);
    container.set_margin_bottom(20);
    container.set_valign(gtk4::Align::End);

    // Row 1: Title on left, Volume and Settings on right
    let top_row = GtkBox::new(Orientation::Horizontal, 12);
    top_row.set_hexpand(true);

    let title_label = Label::new(Some("No media loaded"));
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_hexpand(true);
    title_label.add_css_class("video-title");
    top_row.append(&title_label);

    let volume_button = MenuButton::new();
    let volume_icon = Image::from_icon_name("audio-volume-high-symbolic");
    volume_icon.set_pixel_size(16);
    volume_button.set_child(Some(&volume_icon));
    volume_button.add_css_class("osd-icon-button");
    volume_button.set_size_request(32, 32);
    volume_button.set_tooltip_text(Some("Volume: 100%"));

    // Volume Popover containing vertical slider and percentage
    let volume_popover = gtk4::Popover::new();
    volume_popover.add_css_class("menu-popover");

    let vol_vbox = GtkBox::new(Orientation::Vertical, 8);
    vol_vbox.set_margin_start(10);
    vol_vbox.set_margin_end(10);
    vol_vbox.set_margin_top(12);
    vol_vbox.set_margin_bottom(12);
    vol_vbox.set_halign(gtk4::Align::Center);

    let volume_percent_label = Label::new(Some("100%"));
    volume_percent_label.add_css_class("caption");
    vol_vbox.append(&volume_percent_label);

    let volume_slider = Scale::with_range(Orientation::Vertical, 0.0, 1.0, 0.02);
    volume_slider.set_inverted(true); // 1.0 at the top, 0.0 at the bottom
    volume_slider.set_size_request(24, 140);
    volume_slider.set_value(1.0);
    volume_slider.set_draw_value(false);
    volume_slider.add_css_class("volume-scale");
    vol_vbox.append(&volume_slider);

    volume_popover.set_child(Some(&vol_vbox));
    volume_button.set_popover(Some(&volume_popover));
    top_row.append(&volume_button);

    let settings_button = MenuButton::new();
    settings_button.set_icon_name("emblem-system-symbolic");
    settings_button.add_css_class("osd-icon-button");
    settings_button.set_size_request(32, 32);
    settings_button.set_tooltip_text(Some("Tracks & Preferences"));
    top_row.append(&settings_button);

    container.append(&top_row);

    // Row 2: Full-width seek scale with white knob
    let seekbar = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 0.1);
    seekbar.set_hexpand(true);
    seekbar.set_draw_value(false);
    seekbar.add_css_class("osd-scale");
    seekbar.set_tooltip_text(Some("Seek bar (Mouse wheel ±5s)"));
    let seekbar_dragging = Rc::new(Cell::new(false));
    container.append(&seekbar);

    // Row 3: Timestamps under the seek bar (left and right)
    let time_row = GtkBox::new(Orientation::Horizontal, 0);
    time_row.set_hexpand(true);

    let elapsed_label = Label::new(Some("0:00"));
    elapsed_label.set_halign(gtk4::Align::Start);
    elapsed_label.set_hexpand(true);
    elapsed_label.add_css_class("timestamp-label");
    time_row.append(&elapsed_label);

    let duration_label = Label::new(Some("0:00"));
    duration_label.set_halign(gtk4::Align::End);
    duration_label.add_css_class("timestamp-label");
    time_row.append(&duration_label);

    container.append(&time_row);

    ControlBarWidgets {
        container,
        title_label,
        volume_button,
        volume_icon,
        volume_slider,
        volume_percent_label,
        settings_button,
        seekbar,
        seekbar_dragging,
        elapsed_label,
        duration_label,
    }
}

pub fn update_volume_indicator(
    volume_icon: &Image,
    volume_button: &MenuButton,
    volume_slider: &Scale,
    percent_label: &Label,
    vol: f64,
    is_muted: bool,
) {
    // A video-only file keeps the mute indicator fixed; volume shortcuts and
    // wheel events must not turn it back into an apparently active control.
    if !volume_button.is_sensitive() {
        return;
    }

    let clamped = vol.clamp(0.0, 1.0);
    let pct = (clamped * 100.0).round() as i32;
    percent_label.set_text(&format!("{}%", pct));

    // Dynamic icon reflection across volume levels
    let icon_name = if is_muted || clamped <= 0.001 {
        "audio-volume-muted-symbolic"
    } else if clamped < 0.34 {
        "audio-volume-low-symbolic"
    } else if clamped < 0.67 {
        "audio-volume-medium-symbolic"
    } else {
        "audio-volume-high-symbolic"
    };

    volume_icon.set_icon_name(Some(icon_name));
    if is_muted {
        volume_button.set_tooltip_text(Some("Volume: Muted"));
    } else {
        volume_button.set_tooltip_text(Some(&format!("Volume: {}%", pct)));
    }

    if (volume_slider.value() - clamped).abs() > 0.01 {
        volume_slider.set_value(clamped);
    }
}

/// Reflect whether the loaded media contains an audio stream without changing
/// the Player's stored volume.
pub fn set_audio_available(controls: &ControlBarWidgets, has_audio: bool, volume: f64) {
    controls.volume_button.set_sensitive(has_audio);
    controls.volume_slider.set_sensitive(has_audio);

    if has_audio {
        update_volume_indicator(
            &controls.volume_icon,
            &controls.volume_button,
            &controls.volume_slider,
            &controls.volume_percent_label,
            volume,
            volume <= 0.001,
        );
    } else {
        controls.volume_icon.set_icon_name(Some("audio-volume-muted-symbolic"));
        controls.volume_button.set_tooltip_text(Some("No audio track"));
        controls.volume_percent_label.set_text("No audio");
    }
}

pub fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, mins, secs)
    } else {
        format!("{}:{:02}", mins, secs)
    }
}
