//! Bottom playback control bar matching Showtime's floating OSD layout.
//!
//! The media filename lives in the top-left OSD row; this bar contains
//! volume/track controls, the seekbar, and timestamps.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, CheckButton, DrawingArea, Image, Label, MenuButton, Orientation, Overlay, Scale};
use crate::interaction::attach_popover_scroll_guard;

#[derive(Clone)]
pub struct ControlBarWidgets {
    pub container: GtkBox,
    pub title_label: Label,
    pub volume_button: MenuButton,
    pub speed_button: Button,
    pub playback_speed: Rc<Cell<f32>>,
    pub speed_button_held: Rc<Cell<bool>>,
    pub volume_icon: Image,
    pub volume_slider: Scale,
    pub volume_percent_label: Label,
    pub last_nonzero_volume: Rc<Cell<f64>>,
    pub settings_button: MenuButton,
    pub screenshot_button: Button,
    pub seekbar: Scale,
    pub seekbar_dragging: Rc<Cell<bool>>,
    pub seekbar_marker: DrawingArea,
    pub seekbar_marker_position: Rc<Cell<Option<f64>>>,
    pub elapsed_label: Label,
    pub duration_label: Label,
    pub open_popovers: Rc<Cell<usize>>,
    pub loop_enabled: Rc<Cell<bool>>,
    pub loop_menu_buttons: Rc<RefCell<Vec<CheckButton>>>,
}

pub fn create_floating_bottom_bar() -> ControlBarWidgets {
    let container = GtkBox::new(Orientation::Vertical, 6);
    container.add_css_class("floating-osd-bottom");
    container.set_margin_start(24);
    container.set_margin_end(24);
    container.set_margin_bottom(20);
    container.set_valign(gtk4::Align::End);

    // Keep volume and track controls grouped at the right edge.
    let top_row = GtkBox::new(Orientation::Horizontal, 12);
    top_row.set_halign(gtk4::Align::End);

    let title_label = Label::new(Some("No media loaded"));
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    title_label.set_max_width_chars(64);
    title_label.add_css_class("video-title");

    let last_nonzero_volume = Rc::new(Cell::new(1.0));

    let open_popovers = Rc::new(Cell::new(0));
    let loop_enabled = Rc::new(Cell::new(false));
    let loop_menu_buttons = Rc::new(RefCell::new(Vec::new()));

    let volume_button = MenuButton::new();
    let volume_icon = Image::from_icon_name("audio-volume-high-symbolic");
    volume_icon.set_pixel_size(16);
    volume_button.set_child(Some(&volume_icon));
    volume_button.add_css_class("osd-icon-button");
    volume_button.set_size_request(32, 32);
    volume_button.set_tooltip_text(Some("Volume: 100%"));

    let speed_button = Button::with_label("1.0×");
    speed_button.add_css_class("osd-speed-button");
    speed_button.set_size_request(56, 32);
    speed_button.set_can_focus(false);
    speed_button.set_sensitive(false);
    speed_button.set_tooltip_text(Some("Playback speed: hold for 2.0×, scroll to adjust"));
    let playback_speed = Rc::new(Cell::new(1.0f32));
    let speed_button_held = Rc::new(Cell::new(false));

    // Volume Popover containing vertical slider and percentage
    let volume_popover = gtk4::Popover::new();
    volume_popover.add_css_class("menu-popover");

    let vol_vbox = GtkBox::new(Orientation::Vertical, 8);
    vol_vbox.set_margin_start(6);
    vol_vbox.set_margin_end(6);
    vol_vbox.set_margin_top(12);
    vol_vbox.set_margin_bottom(12);
    vol_vbox.set_halign(gtk4::Align::Center);

    let volume_percent_label = Label::new(Some("100%"));
    volume_percent_label.add_css_class("caption");
    vol_vbox.append(&volume_percent_label);

    let volume_slider = Scale::with_range(Orientation::Vertical, 0.0, 1.5, 0.02);
    volume_slider.set_inverted(true); // 1.0 at the top, 0.0 at the bottom
    volume_slider.set_size_request(16, 140);
    volume_slider.set_margin_start(8);
    volume_slider.set_value(1.0);
    volume_slider.set_draw_value(false);
    volume_slider.add_mark(1.0, gtk4::PositionType::Right, None);
    volume_slider.add_css_class("volume-scale");
    vol_vbox.append(&volume_slider);

    volume_popover.set_child(Some(&vol_vbox));
    track_popover_visibility(&volume_popover, &open_popovers);
    attach_popover_scroll_guard(&volume_popover);
    volume_button.set_popover(Some(&volume_popover));
    top_row.append(&volume_button);
    top_row.append(&speed_button);

    let screenshot_button = Button::new();
    let screenshot_icon = Image::from_icon_name("camera-photo-symbolic");
    screenshot_icon.set_pixel_size(16);
    screenshot_button.set_child(Some(&screenshot_icon));
    screenshot_button.add_css_class("osd-icon-button");
    screenshot_button.set_size_request(32, 32);
    screenshot_button.set_can_focus(false);
    screenshot_button.set_sensitive(false);
    screenshot_button.set_tooltip_text(Some("Take Screenshot"));
    top_row.append(&screenshot_button);

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

    // Draw the adaptive-scrub destination above the normal progress trough.
    let seekbar_overlay = Overlay::new();
    seekbar_overlay.set_child(Some(&seekbar));
    let seekbar_marker_position: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
    let seekbar_marker = DrawingArea::new();
    seekbar_marker.set_hexpand(true);
    seekbar_marker.set_vexpand(true);
    seekbar_marker.set_can_target(false);
    let marker_position_for_draw = Rc::clone(&seekbar_marker_position);
    seekbar_marker.set_draw_func(move |_area, cr, width, height| {
        let Some(fraction) = marker_position_for_draw.get() else {
            return;
        };
        let inset = 8.0;
        let x = inset + fraction.clamp(0.0, 1.0) * (width as f64 - inset * 2.0).max(0.0);
        let center_y = height as f64 / 2.0;
        cr.set_line_cap(gtk4::cairo::LineCap::Round);
        cr.set_line_width(4.0);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.75);
        cr.move_to(x, center_y - 6.0);
        cr.line_to(x, center_y + 6.0);
        let _ = cr.stroke();
        cr.set_line_width(2.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.move_to(x, center_y - 6.0);
        cr.line_to(x, center_y + 6.0);
        let _ = cr.stroke();
    });
    seekbar_overlay.add_overlay(&seekbar_marker);
    container.append(&seekbar_overlay);

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
        speed_button,
        playback_speed,
        speed_button_held,
        volume_icon,
        volume_slider,
        volume_percent_label,
        last_nonzero_volume,
        settings_button,
        screenshot_button,
        seekbar,
        seekbar_dragging,
        seekbar_marker,
        seekbar_marker_position,
        elapsed_label,
        duration_label,
        open_popovers,
        loop_enabled,
        loop_menu_buttons,
    }
}



/// Move the persistent playback speed by one tenth for a vertical wheel step.
pub fn adjust_playback_speed(current: f32, dy: f64) -> f32 {
    let current = if current.is_finite() {
        current.clamp(0.5, 2.0)
    } else {
        1.0
    };
    let step = if dy < 0.0 { 1.0 } else { -1.0 };
    ((current * 10.0).round() + step).clamp(5.0, 20.0) / 10.0
}

pub fn format_speed_label(speed: f32) -> String {
    format!("{speed:.1}×")
}

/// Track visible playback popovers so the OSD does not auto-hide behind an open menu.
pub fn track_popover_visibility(popover: &gtk4::Popover, open_popovers: &Rc<Cell<usize>>) {
    let was_visible = Rc::new(Cell::new(popover.is_visible()));
    let count = Rc::clone(open_popovers);
    popover.connect_notify_local(Some("visible"), move |popover, _| {
        let visible = popover.is_visible();
        let previous = was_visible.replace(visible);
        if visible && !previous {
            count.set(count.get() + 1);
        } else if !visible && previous {
            count.set(count.get().saturating_sub(1));
        }
    });
}

/// Remember the most recent audible level without replacing it with zero while muted.
pub fn remember_nonzero_volume(last_nonzero_volume: &Cell<f64>, volume: f64) {
    let clamped = volume.clamp(0.0, 1.5);
    if clamped > 0.001 {
        last_nonzero_volume.set(clamped);
    }
}

/// Toggle mute while preserving the previous non-zero volume level.
pub fn toggle_mute_volume(current_volume: f64, last_nonzero_volume: &Cell<f64>) -> f64 {
    let current = current_volume.clamp(0.0, 1.5);
    if current > 0.001 {
        last_nonzero_volume.set(current);
        0.0
    } else {
        last_nonzero_volume.get().clamp(0.001, 1.5)
    }
}

#[cfg(test)]
mod volume_tests {
    use super::{adjust_playback_speed, remember_nonzero_volume, toggle_mute_volume};
    use std::cell::Cell;

    #[test]
    fn playback_speed_wheel_adjusts_in_tenths_and_clamps_to_supported_range() {
        assert_eq!(adjust_playback_speed(1.0, -1.0), 1.1);
        assert_eq!(adjust_playback_speed(1.0, 1.0), 0.9);
        assert_eq!(adjust_playback_speed(2.0, -1.0), 2.0);
        assert_eq!(adjust_playback_speed(0.5, 1.0), 0.5);
    }

    #[test]
    fn playback_speed_wheel_recovers_from_non_finite_values() {
        assert_eq!(adjust_playback_speed(f32::NAN, -1.0), 1.1);
    }

    #[test]
    fn mute_remembers_current_volume_and_returns_zero() {
        let last_volume = Cell::new(1.0);
        assert_eq!(toggle_mute_volume(0.35, &last_volume), 0.0);
        assert_eq!(last_volume.get(), 0.35);
    }

    #[test]
    fn unmute_restores_last_nonzero_volume() {
        let last_volume = Cell::new(0.42);
        assert_eq!(toggle_mute_volume(0.0, &last_volume), 0.42);
    }

    #[test]
    fn zero_volume_changes_do_not_overwrite_remembered_level() {
        let last_volume = Cell::new(0.6);
        remember_nonzero_volume(&last_volume, 0.0);
        assert_eq!(last_volume.get(), 0.6);
    }

    #[test]
    fn positive_volume_changes_update_remembered_level() {
        let last_volume = Cell::new(1.0);
        remember_nonzero_volume(&last_volume, 0.27);
        assert_eq!(last_volume.get(), 0.27);
    }

    #[test]
    fn amplified_volume_is_preserved_when_remembered_and_unmuted() {
        let last_volume = Cell::new(1.0);
        remember_nonzero_volume(&last_volume, 1.35);
        assert_eq!(last_volume.get(), 1.35);
        assert_eq!(toggle_mute_volume(0.0, &last_volume), 1.35);
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

    let clamped = vol.clamp(0.0, 1.5);
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
        remember_nonzero_volume(&controls.last_nonzero_volume, volume);
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
