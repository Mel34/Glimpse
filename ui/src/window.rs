use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use adw::prelude::*;
use adw::{Application, ApplicationWindow};
use gdk4::DragAction;
use gio::SimpleAction;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, DropTarget, EventControllerKey, EventControllerMotion,
    FileDialog, FileFilter, GestureClick, Image, Label, MenuButton,
    Orientation, Overlay, Separator, Settings, Stack, StackTransitionType, WindowHandle,
};

use glimpse_engine::{Event, Player, PlayerState};
use crate::controls::{create_floating_bottom_bar, ControlBarWidgets};
use crate::interaction::{adaptive_scrub_speed, attach_popover_scroll_guard, attach_seekbar_wheel, attach_window_volume_wheel, VideoClickStateMachine};
use crate::track_menu::create_track_popover;
use crate::video_texture::{bind_subtitle_scaling, VideoPlaybackHandle};
use crate::rotatable_picture::RotatablePicture;

const SCRUB_TARGET_TOLERANCE: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, Debug)]
enum AdaptiveScrubState {
    Idle,
    Dragging { original_speed: f32, was_playing: bool },
    CatchingUp { target: Duration, original_speed: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AbLoopState {
    Unset,
    PointA(Duration),
    Range { start: Duration, end: Duration },
}

fn next_ab_loop_state(state: AbLoopState, position: Duration) -> AbLoopState {
    match state {
        AbLoopState::Unset => AbLoopState::PointA(position),
        AbLoopState::PointA(start) if position > start => {
            AbLoopState::Range { start, end: position }
        }
        AbLoopState::PointA(_) => state,
        AbLoopState::Range { .. } => AbLoopState::Unset,
    }
}

fn playback_loop_target(
    position: Duration,
    duration: Option<Duration>,
    playback_state: PlayerState,
    has_audio: bool,
    loop_enabled: bool,
    ab_loop: AbLoopState,
) -> Option<Duration> {
    if let AbLoopState::Range { start, end } = ab_loop {
        if position >= end && matches!(playback_state, PlayerState::Playing | PlayerState::Ended) {
            return Some(start);
        }
    }

    let reached_end = if has_audio {
        playback_state == PlayerState::Ended
    } else {
        duration.is_some_and(|duration| position >= duration)
    };

    (loop_enabled && reached_end).then_some(Duration::ZERO)
}

pub struct GlimpseWindow {
    window: ApplicationWindow,
    player: Rc<RefCell<Option<Player>>>,
    video_playback: Rc<RefCell<Option<VideoPlaybackHandle>>>,
    bottom_controls: ControlBarWidgets,
    picture: RotatablePicture,
    subtitle_label: Label,
    #[allow(dead_code)]
    top_controls: GtkBox,
    #[allow(dead_code)]
    overlay: Overlay,
    stack: Stack,
    ab_loop: Rc<Cell<AbLoopState>>,
    adaptive_scrub_state: Rc<Cell<AdaptiveScrubState>>,
    #[allow(dead_code)]
    drop_overlay: GtkBox,
}

impl GlimpseWindow {
    pub fn new(app: &Application) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Glimpse")
            .default_width(840)
            .default_height(600)
            .icon_name("com.github.Mel34.Glimpse")
            .build();

        window.add_css_class("glimpse-window");

        let player: Rc<RefCell<Option<Player>>> = Rc::new(RefCell::new(None));
        let video_playback: Rc<RefCell<Option<VideoPlaybackHandle>>> = Rc::new(RefCell::new(None));
        let ab_loop = Rc::new(Cell::new(AbLoopState::Unset));
        let adaptive_scrub_state = Rc::new(Cell::new(AdaptiveScrubState::Idle));
        let seekbar_drag_moved = Rc::new(Cell::new(false));
        let video_playback_on_focus = Rc::clone(&video_playback);
        window.connect_is_active_notify(move |window| {
            if let Some(video) = video_playback_on_focus.borrow_mut().as_mut() {
                video.set_window_active(window.is_active());
            }
        });
        let video_playback_on_close = Rc::clone(&video_playback);
        let player_on_close = Rc::clone(&player);
        window.connect_close_request(move |_| {
            if let Some(ref p) = *player_on_close.borrow() { p.stop(); }
            video_playback_on_close.borrow_mut().take();
            glib::Propagation::Proceed
        });

        // Root Overlay: Contains Stack as base child and the Drop Hover Overlay on top
        let root_overlay = Overlay::new();
        root_overlay.set_vexpand(true);
        root_overlay.set_hexpand(true);

        let stack = Stack::new();
        stack.set_transition_type(StackTransitionType::Crossfade);
        stack.set_transition_duration(200);

        // =====================================================================
        // PAGE 1: Showtime Loader / Empty State (Watch Videos, Drag & Drop, Open...)
        // =====================================================================
        let loader_page = GtkBox::new(Orientation::Vertical, 0);
        loader_page.add_css_class("loader-view");
        loader_page.set_vexpand(true);
        loader_page.set_hexpand(true);

        // Loader Top Bar: Title "Glimpse" in center, hamburger & close on right (6px corner margins)
        let loader_header = gtk4::CenterBox::new();
        loader_header.add_css_class("loader-header");
        loader_header.set_valign(Align::Start);
        loader_header.set_margin_start(6);
        loader_header.set_margin_end(6);
        loader_header.set_margin_top(6);

        let loader_title = Label::new(Some("Glimpse"));
        loader_title.add_css_class("loader-title");
        loader_header.set_center_widget(Some(&loader_title));

        let loader_right_box = GtkBox::new(Orientation::Horizontal, 6);
        loader_right_box.set_halign(Align::End);

        let loader_menu_btn = MenuButton::new();
        loader_menu_btn.set_icon_name("open-menu-symbolic");
        loader_menu_btn.add_css_class("flat");
        loader_menu_btn.add_css_class("header-button");
        loader_menu_btn.set_size_request(28, 28);
        loader_menu_btn.set_can_focus(false);
        loader_menu_btn.set_tooltip_text(Some("Main Menu"));
        loader_right_box.append(&loader_menu_btn);

        let loader_close_btn = Button::new();
        let l_close_icon = Image::from_icon_name("window-close-symbolic");
        l_close_icon.set_pixel_size(12);
        loader_close_btn.set_child(Some(&l_close_icon));
        loader_close_btn.add_css_class("flat");
        loader_close_btn.add_css_class("header-close-button");
        loader_close_btn.set_size_request(24, 24);
        loader_close_btn.set_valign(Align::Center);
        loader_close_btn.set_halign(Align::Center);
        loader_close_btn.set_can_focus(false);
        loader_close_btn.set_tooltip_text(Some("Close"));
        let win_close = window.clone();
        loader_close_btn.connect_clicked(move |_| win_close.close());
        loader_right_box.append(&loader_close_btn);

        loader_header.set_end_widget(Some(&loader_right_box));

        // Make top header draggable to move window
        let loader_handle = WindowHandle::new();
        loader_handle.set_child(Some(&loader_header));
        loader_page.append(&loader_handle);

        // Center Box: Large video camera icon, "Watch Videos", "Drag and drop videos here", "Open..." button
        let center_box = GtkBox::new(Orientation::Vertical, 0);
        center_box.set_vexpand(true);
        center_box.set_hexpand(true);
        center_box.set_valign(Align::Center);
        center_box.set_halign(Align::Center);
        center_box.add_css_class("loader-center");

        let camera_icon = Image::from_icon_name("video-x-generic-symbolic");
        camera_icon.set_pixel_size(110);
        camera_icon.add_css_class("loader-icon");
        center_box.append(&camera_icon);

        let heading_label = Label::new(Some("Watch Videos"));
        heading_label.add_css_class("loader-heading");
        center_box.append(&heading_label);

        let subheading_label = Label::new(Some("Drag and drop videos here"));
        subheading_label.add_css_class("loader-subheading");
        center_box.append(&subheading_label);

        let open_btn = Button::with_label("Open...");
        open_btn.add_css_class("loader-open-btn");
        open_btn.add_css_class("suggested-action");
        open_btn.add_css_class("pill");
        center_box.append(&open_btn);

        loader_page.append(&center_box);
        stack.add_named(&loader_page, Some("loader"));

        // =====================================================================
        // PAGE 2: Showtime Full-Bleed Video Playback & Floating OSD
        // =====================================================================
        let player_overlay = Overlay::new();
        player_overlay.set_vexpand(true);
        player_overlay.set_hexpand(true);

        // Video picture surface (letterboxed/contain aspect ratio fit)
        let picture = RotatablePicture::new();
        picture.set_vexpand(true);
        picture.set_hexpand(true);
        picture.set_can_target(true);
        player_overlay.set_child(Some(&picture));

        // Subtitle overlay label (hidden by default to avoid dark square on video)
        let subtitle_label = Label::new(None);
        subtitle_label.add_css_class("subtitles-overlay");
        bind_subtitle_scaling(&picture, &subtitle_label);
        subtitle_label.set_valign(Align::End);
        subtitle_label.set_halign(Align::Center);
        subtitle_label.set_margin_bottom(90);
        subtitle_label.set_visible(false);
        player_overlay.add_overlay(&subtitle_label);

        // Build the bottom controls first so the filename label can live in
        // the top-left OSD while remaining part of the shared control state.
        let bottom_controls = create_floating_bottom_bar();
        let loop_enabled = Rc::clone(&bottom_controls.loop_enabled);
        let loop_menu_buttons = Rc::clone(&bottom_controls.loop_menu_buttons);

        let screenshot_picture = picture.clone();
        let screenshot_window = window.clone();
        bottom_controls.screenshot_button.connect_clicked(move |button| {
            match screenshot_picture.save_screenshot() {
                Ok(path) => {
                    tracing::debug!(path = %path.display(), "Screenshot saved");
                    button.set_tooltip_text(Some(&format!("Saved: {}", path.display())));
                }
                Err(error) => {
                    tracing::error!(%error, "Could not save screenshot");
                    let dialog = gtk4::AlertDialog::builder()
                        .modal(true)
                        .message("Could not save screenshot")
                        .detail(&error)
                        .buttons(["Close"])
                        .build();
                    dialog.show(Some(&screenshot_window));
                }
            }
        });

        // Top Floating OSD Bar (margins 6px from corner matching standard Libadwaita)
        let top_controls = GtkBox::new(Orientation::Horizontal, 0);
        top_controls.add_css_class("floating-osd-top");
        top_controls.set_valign(Align::Start);
        top_controls.set_margin_start(6);
        top_controls.set_margin_end(6);
        top_controls.set_margin_top(6);

        // Top-left: Fullscreen toggle button
        let fullscreen_btn = Button::new();
        let fs_icon = Image::from_icon_name("view-fullscreen-symbolic");
        fs_icon.set_pixel_size(14);
        fullscreen_btn.set_child(Some(&fs_icon));
        fullscreen_btn.add_css_class("osd-icon-button");
        fullscreen_btn.set_size_request(32, 32);
        fullscreen_btn.set_can_focus(false);
        fullscreen_btn.set_halign(Align::Start);
        fullscreen_btn.set_tooltip_text(Some("Toggle Fullscreen (F)"));
        let win_fs = window.clone();
        fullscreen_btn.connect_clicked(move |_| {
            if win_fs.is_fullscreen() {
                win_fs.unfullscreen();
            } else {
                win_fs.fullscreen();
            }
        });
        let fs_icon_notif = fs_icon.clone();
        let fs_btn_notif = fullscreen_btn.clone();
        window.connect_fullscreened_notify(move |w| {
            if w.is_fullscreen() {
                fs_icon_notif.set_icon_name(Some("view-restore-symbolic"));
                fs_btn_notif.set_tooltip_text(Some("Leave Fullscreen (F / Esc)"));
            } else {
                fs_icon_notif.set_icon_name(Some("view-fullscreen-symbolic"));
                fs_btn_notif.set_tooltip_text(Some("Toggle Fullscreen (F)"));
            }
        });        top_controls.append(&fullscreen_btn);

        // Keep the filename in the top-left OSD, away from subtitle lines.
        let title_label = bottom_controls.title_label.clone();
        title_label.set_margin_start(12);
        top_controls.append(&title_label);

        let top_spacer = GtkBox::new(Orientation::Horizontal, 0);
        top_spacer.set_hexpand(true);
        top_controls.append(&top_spacer);

        // Top-right: Hamburger menu & Close window buttons
        let right_top_box = GtkBox::new(Orientation::Horizontal, 6);
        right_top_box.set_halign(Align::End);

        let menu_btn = MenuButton::new();
        menu_btn.set_icon_name("open-menu-symbolic");
        menu_btn.add_css_class("osd-icon-button");
        menu_btn.set_size_request(32, 32);
        menu_btn.set_can_focus(false);
        menu_btn.set_tooltip_text(Some("Main Menu"));
        right_top_box.append(&menu_btn);

        let close_btn = Button::new();
        let close_icon = Image::from_icon_name("window-close-symbolic");
        close_icon.set_pixel_size(14);
        close_btn.set_child(Some(&close_icon));
        close_btn.add_css_class("osd-icon-button");
        close_btn.set_size_request(32, 32);
        close_btn.set_valign(Align::Center);
        close_btn.set_halign(Align::Center);
        close_btn.set_can_focus(false);
        close_btn.set_tooltip_text(Some("Close"));
        let win_close2 = window.clone();
        close_btn.connect_clicked(move |_| win_close2.close());
        right_top_box.append(&close_btn);

        top_controls.append(&right_top_box);

        let top_handle = WindowHandle::new();
        top_handle.set_valign(Align::Start);
        top_handle.set_child(Some(&top_controls));
        player_overlay.add_overlay(&top_handle);

        // Bottom Floating OSD Bar (Volume, Settings, Seekbar, Timestamps)
        // Keep the timeline independent of video-frame presentation so audio-only
        // playback and audio that outlives the video stream still update the controls.
        let timeline_player = Rc::clone(&player);
        let timeline_video = Rc::clone(&video_playback);
        let timeline_loop_enabled = Rc::clone(&loop_enabled);
        let timeline_ab_loop = Rc::clone(&ab_loop);
        let timeline_scrub_state = Rc::clone(&adaptive_scrub_state);
        let loop_resume: Rc<RefCell<Option<crossbeam_channel::Receiver<()>>>> = Rc::new(RefCell::new(None));
        let timeline_loop_resume = Rc::clone(&loop_resume);
        let timeline_seekbar = bottom_controls.seekbar.clone();
        let timeline_elapsed = bottom_controls.elapsed_label.clone();
        let timeline_dragging = Rc::clone(&bottom_controls.seekbar_dragging);
        let timeline_drag_moved = Rc::clone(&seekbar_drag_moved);
        let timeline_window = window.downgrade();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(_window) = timeline_window.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if let Some(current_player) = timeline_player.borrow().as_ref() {
                if timeline_dragging.get() {
                    if let AdaptiveScrubState::Dragging { original_speed, was_playing } = timeline_scrub_state.get() {
                        if !timeline_drag_moved.get() {
                            current_player.set_playback_speed(original_speed);
                            return glib::ControlFlow::Continue;
                        }
                        let position = current_player.position();
                        let target = Duration::from_secs_f64(timeline_seekbar.value().max(0.0));
                        if was_playing
                            && current_player.state() == PlayerState::Playing
                            && target.saturating_sub(position) > SCRUB_TARGET_TOLERANCE
                        {
                            current_player.set_scrub_speed(adaptive_scrub_speed(
                                target.saturating_sub(position).as_secs_f64(),
                                original_speed,
                            ));
                        } else {
                            current_player.set_playback_speed(original_speed);
                        }
                    }
                } else {
                    if let AdaptiveScrubState::CatchingUp { target, original_speed } = timeline_scrub_state.get() {
                        let position = current_player.position();
                        if target.saturating_sub(position) <= SCRUB_TARGET_TOLERANCE {
                            current_player.seek(target);
                            current_player.set_playback_speed(original_speed);
                            if let Some(video) = timeline_video.borrow_mut().as_mut() {
                                video.seek(target);
                            }
                            timeline_scrub_state.set(AdaptiveScrubState::Idle);
                        } else if current_player.state() == PlayerState::Playing {
                            current_player.set_scrub_speed(adaptive_scrub_speed(
                                target.saturating_sub(position).as_secs_f64(),
                                original_speed,
                            ));
                        } else {
                            current_player.set_playback_speed(original_speed);
                            timeline_scrub_state.set(AdaptiveScrubState::Idle);
                        }
                    }

                    let media_position = current_player.position();
                    let readiness = timeline_loop_resume.borrow().as_ref().map(|ready| ready.try_recv());
                    if let Some(readiness) = readiness {
                        match readiness {
                            Ok(()) | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                timeline_loop_resume.borrow_mut().take();
                                current_player.play();
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => {}
                        }
                    } else {
                        let target = playback_loop_target(
                            media_position,
                            current_player.duration(),
                            current_player.state(),
                            !current_player.tracks().audio_tracks.is_empty(),
                            timeline_loop_enabled.get(),
                            timeline_ab_loop.get(),
                        );
                        if let Some(target) = target {
                            // Keep the shared media clock frozen while both paths
                            // prepare the new loop iteration. Audio starts only
                            // after GTK presents the first frame at A.
                            current_player.pause();
                            current_player.seek(target);
                            let ready = timeline_video.borrow_mut().as_mut()
                                .and_then(|video| video.seek_for_sync(target));
                            if let Some(ready) = ready {
                                *timeline_loop_resume.borrow_mut() = Some(ready);
                            } else {
                                current_player.play();
                            }
                        }
                    }

                    let position = current_player.position().as_secs_f64().max(0.0);
                    let position = current_player.duration()
                        .map(|duration| position.min(duration.as_secs_f64()))
                        .unwrap_or(position);
                    timeline_seekbar.set_value(position);
                    timeline_elapsed.set_text(
                        &crate::controls::format_duration(Duration::from_secs_f64(position)),
                    );
                }
            }
            glib::ControlFlow::Continue
        });

        bottom_controls.container.set_valign(Align::End);
        player_overlay.add_overlay(&bottom_controls.container);

        let player_handle = WindowHandle::new();
        player_handle.set_vexpand(true);
        player_handle.set_hexpand(true);
        player_handle.set_child(Some(&player_overlay));
        stack.add_named(&player_handle, Some("player"));
        stack.set_visible_child_name("loader");

        root_overlay.set_child(Some(&stack));

        // =====================================================================
        // ACTIVE DRAG-AND-DROP HOVER OVERLAY (Matching Showtime Screenshot 2)
        // =====================================================================
        let drop_overlay = GtkBox::new(Orientation::Vertical, 0);
        drop_overlay.add_css_class("drop-overlay-container");
        drop_overlay.set_vexpand(true);
        drop_overlay.set_hexpand(true);
        drop_overlay.set_valign(Align::Fill);
        drop_overlay.set_halign(Align::Fill);
        drop_overlay.set_visible(false);

        let drop_center = GtkBox::new(Orientation::Vertical, 0);        drop_center.set_vexpand(true);
        drop_center.set_hexpand(true);
        drop_center.set_valign(Align::Center);
        drop_center.set_halign(Align::Center);

        let drop_camera_icon = Image::from_icon_name("video-x-generic-symbolic");
        drop_camera_icon.set_pixel_size(120);
        drop_camera_icon.add_css_class("drop-overlay-icon");
        drop_center.append(&drop_camera_icon);

        let drop_label = Label::new(Some("Drop Video to Play"));
        drop_label.add_css_class("drop-overlay-label");
        drop_center.append(&drop_label);

        drop_overlay.append(&drop_center);
        root_overlay.add_overlay(&drop_overlay);

        window.set_content(Some(&root_overlay));

        // =====================================================================        // GESTURES & INTERACTION RULES (Rules 1 - 5)
        // =====================================================================

        // Rule 1 & Rule 2: Single-click = play/pause, double-click = fullscreen
        // Playback must NEVER pause when dragging the window
        let double_click_time = Settings::default()
            .map(|s| s.gtk_double_click_time() as u32)
            .unwrap_or(250)
            .min(300);

        let click_state = Rc::new(RefCell::new(VideoClickStateMachine::new(double_click_time)));

        let click_gesture = GestureClick::new();
        click_gesture.set_button(1); // Left mouse button only

        let p_toggle = Rc::clone(&player);
        let v_toggle = Rc::clone(&video_playback);
        let win_click = window.clone();
        let click_state_press = Rc::clone(&click_state);

        // On press: if double-click (n_press >= 2), toggle fullscreen immediately and cancel any timer
        click_gesture.connect_pressed(move |_gesture, n_press, x, y| {
            tracing::debug!(n_press, x, y, "Video surface gesture pressed");
            if n_press >= 2 {
                tracing::debug!("Double-click detected; toggling fullscreen");
                click_state_press.borrow_mut().cancel();
                if win_click.is_fullscreen() {
                    win_click.unfullscreen();
                } else {
                    win_click.fullscreen();
                }
            }
        });

        // On release: schedule single-click play/pause unless this is a double-click.
        let click_state_rel = Rc::clone(&click_state);
        let win_rel = window.clone();
        click_gesture.connect_released(move |_gesture, n_press, x, y| {
            tracing::debug!(n_press, x, y, "Video surface gesture released");
            if n_press == 1 {
                tracing::debug!("Single video-surface click accepted; scheduling play/pause toggle");
                let p_action = Rc::clone(&p_toggle);
                let v_action = Rc::clone(&v_toggle);
                let w_fs = win_rel.clone();
                VideoClickStateMachine::handle_click(
                    Rc::clone(&click_state_rel), 1,
                    move || {
                        if let Some(ref p) = *p_action.borrow() {
                            let was_playing = p.state() == PlayerState::Playing;
                            tracing::debug!(was_playing, "Video-surface play/pause timer fired");
                            if was_playing { p.pause(); }
                            else {
                                if p.state() == PlayerState::Ended {
                                    if let Some(ref mut video) = *v_action.borrow_mut() { video.seek(Duration::ZERO); }
                                }
                                p.play();
                            }
                        } else {
                            tracing::debug!("Video-surface play/pause timer fired without an active player");
                        }
                    },
                    move || {
                        if w_fs.is_fullscreen() {
                            w_fs.unfullscreen();
                        } else {
                            w_fs.fullscreen();
                        }
                    },
                );
            }
        });

        // GTK may emit "stopped" after a completed click (for example, when its
        // double-click interval expires). The release handler has already accepted
        // the click, so this signal must not cancel its pending play/pause action.
        click_gesture.connect_stopped(move |_| {
            tracing::debug!("Video surface click gesture stopped by GTK");
        });

        picture.add_controller(click_gesture);

        // The surrounding player WindowHandle provides native window dragging,
        // matching Showtime without manually starting compositor moves.

        // Rule 3: Mouse wheel over seekbar seeks in 5s steps (consumed)
        let p_seek_wheel = Rc::clone(&player);
        let p_duration_wheel = Rc::clone(&player);
        let p_position_wheel = Rc::clone(&player);
        let v_seek_wheel = Rc::clone(&video_playback);

        attach_seekbar_wheel(
            &bottom_controls.seekbar,
            move || p_duration_wheel.borrow().as_ref().and_then(Player::duration),
            move || p_position_wheel.borrow().as_ref().map(Player::position).unwrap_or(Duration::ZERO),
            move |target_pos| {
                if let Some(ref p) = *p_seek_wheel.borrow() { p.seek(target_pos); }
                if let Some(ref mut video) = *v_seek_wheel.borrow_mut() { video.seek(target_pos); }
            },
        );
        // Observe pointer events without claiming GTK's native GtkRange gesture.
        let seekbar_events = gtk4::EventControllerLegacy::new();
        seekbar_events.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let seekbar_drag_state = Rc::clone(&bottom_controls.seekbar_dragging);
        let seekbar_motion_dragging = Rc::clone(&bottom_controls.seekbar_dragging);
        let seekbar_motion_moved = Rc::clone(&seekbar_drag_moved);
        let seekbar_motion = EventControllerMotion::new();
        seekbar_motion.connect_motion(move |_, _, _| {
            if seekbar_motion_dragging.get() {
                seekbar_motion_moved.set(true);
            }
        });
        bottom_controls.seekbar.add_controller(seekbar_motion);
        let seekbar_for_release = bottom_controls.seekbar.clone();
        let p_seekbar_release = Rc::clone(&player);
        let v_seekbar_release = Rc::clone(&video_playback);
        let scrub_state_for_events = Rc::clone(&adaptive_scrub_state);
        seekbar_events.connect_event(move |_, event| {
            match event.event_type() {
                gdk4::EventType::ButtonPress => {
                    if event.downcast_ref::<gdk4::ButtonEvent>()
                        .is_some_and(|button| button.button() == 1)
                    {
                        if let Some(p) = p_seekbar_release.borrow().as_ref() {
                            if let AdaptiveScrubState::CatchingUp { original_speed, .. } = scrub_state_for_events.get() {
                                p.set_playback_speed(original_speed);
                            }
                            scrub_state_for_events.set(AdaptiveScrubState::Dragging {
                                original_speed: p.playback_speed(),
                                was_playing: p.state() == PlayerState::Playing,
                            });
                        } else {
                            scrub_state_for_events.set(AdaptiveScrubState::Idle);
                        }
                        seekbar_drag_moved.set(false);
                        seekbar_drag_state.set(true);
                    }
                }
                gdk4::EventType::ButtonRelease => {
                    if event.downcast_ref::<gdk4::ButtonEvent>()
                        .is_some_and(|button| button.button() == 1)
                        && seekbar_drag_state.replace(false)
                    {
                        let target = Duration::from_secs_f64(seekbar_for_release.value().max(0.0));
                        let did_drag = seekbar_drag_moved.replace(false);
                        if let Some(p) = p_seekbar_release.borrow().as_ref() {
                            match scrub_state_for_events.get() {
                                AdaptiveScrubState::Dragging { original_speed, was_playing }
                                    if did_drag
                                        && was_playing
                                        && p.state() == PlayerState::Playing
                                        && target.saturating_sub(p.position()) > SCRUB_TARGET_TOLERANCE =>
                                {
                                    let distance = target.saturating_sub(p.position()).as_secs_f64();
                                    scrub_state_for_events.set(AdaptiveScrubState::CatchingUp {
                                        target,
                                        original_speed,
                                    });
                                    p.set_scrub_speed(adaptive_scrub_speed(distance, original_speed));
                                }
                                AdaptiveScrubState::Dragging { original_speed, .. } => {
                                    scrub_state_for_events.set(AdaptiveScrubState::Idle);
                                    p.set_playback_speed(original_speed);
                                    p.seek(target);
                                    if let Some(video) = v_seekbar_release.borrow_mut().as_mut() {
                                        video.seek(target);
                                    }
                                }
                                _ => {
                                    scrub_state_for_events.set(AdaptiveScrubState::Idle);
                                    p.seek(target);
                                    if let Some(video) = v_seekbar_release.borrow_mut().as_mut() {
                                        video.seek(target);
                                    }
                                }
                            }
                        } else if let Some(video) = v_seekbar_release.borrow_mut().as_mut() {
                            video.seek(target);
                        }
                    }
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        bottom_controls.seekbar.add_controller(seekbar_events);

        // Non-drag changes (keyboard, accessibility) seek immediately. During a pointer
        // drag, update only the elapsed-time preview; the release handler commits the seek.
        let p_seekbar = Rc::clone(&player);
        let v_seekbar = Rc::clone(&video_playback);
        let seekbar_dragging = Rc::clone(&bottom_controls.seekbar_dragging);
        let elapsed_seekbar = bottom_controls.elapsed_label.clone();
        bottom_controls.seekbar.connect_change_value(move |_, _, val| {
            let target = Duration::from_secs_f64(val.max(0.0));
            if seekbar_dragging.get() {
                elapsed_seekbar.set_text(
                    &crate::controls::format_duration(target),
                );
            } else {
                if let Some(ref p) = *p_seekbar.borrow() { p.seek(target); }
                if let Some(ref mut video) = *v_seekbar.borrow_mut() { video.seek(target); }
            }
            glib::Propagation::Proceed
        });

        // Rule 4: Mouse wheel over window body adjusts volume in 5% steps
        let p_vol_wheel = Rc::clone(&player);
        let p_vol_wheel_value = Rc::clone(&player);
        let win_ref = window.clone().upcast::<gtk4::Window>();
        let v_icon_w = bottom_controls.volume_icon.clone();
        let v_btn_w = bottom_controls.volume_button.clone();
        let v_slider_w = bottom_controls.volume_slider.clone();
        let v_pct_w = bottom_controls.volume_percent_label.clone();
        let last_volume_w = Rc::clone(&bottom_controls.last_nonzero_volume);

        attach_window_volume_wheel(
            &win_ref,
            move || p_vol_wheel_value.borrow().as_ref().map(Player::volume).unwrap_or(1.0),
            move |new_vol| {
                if !v_btn_w.is_sensitive() {
                    return;
                }
                if let Some(ref p) = *p_vol_wheel.borrow() {
                    p.set_volume(new_vol);
                }
                crate::controls::remember_nonzero_volume(&last_volume_w, new_vol as f64);
                crate::controls::update_volume_indicator(
                    &v_icon_w,
                    &v_btn_w,
                    &v_slider_w,
                    &v_pct_w,
                    new_vol as f64,
                    new_vol <= 0.001,
                );
            },
        );

        // Volume Popover Slider: connects user slider dragging to engine & updates indicator
        let p_vol_slider = Rc::clone(&player);
        let v_icon_sl = bottom_controls.volume_icon.clone();
        let v_btn_sl = bottom_controls.volume_button.clone();
        let v_slider_sl = bottom_controls.volume_slider.clone();
        let v_pct_sl = bottom_controls.volume_percent_label.clone();
        let last_volume_sl = Rc::clone(&bottom_controls.last_nonzero_volume);
        bottom_controls.volume_slider.connect_change_value(move |_, _, new_val| {
            let val = new_val.clamp(0.0, 1.5);
            if let Some(ref p) = *p_vol_slider.borrow() {
                p.set_volume(val as f32);
            }
            crate::controls::remember_nonzero_volume(&last_volume_sl, val);
            crate::controls::update_volume_indicator(
                &v_icon_sl,
                &v_btn_sl,
                &v_slider_sl,
                &v_pct_sl,
                val,
                val <= 0.001,
            );
            glib::Propagation::Proceed
        });

        let hide_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let last_pointer_position: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
        let motion = EventControllerMotion::new();
        motion.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let top_motion = top_controls.clone();
        let bottom_motion = bottom_controls.container.clone();
        let timer_motion = Rc::clone(&hide_timer);
        let popovers_enter = Rc::clone(&bottom_controls.open_popovers);
        let pointer_enter = Rc::clone(&last_pointer_position);
        motion.connect_enter(move |_, x, y| {
            pointer_enter.set(Some((x, y)));
            reveal_and_schedule_controls(&top_motion, &bottom_motion, &timer_motion, &popovers_enter);
        });
        let top_motion = top_controls.clone();
        let bottom_motion = bottom_controls.container.clone();
        let timer_motion = Rc::clone(&hide_timer);
        let popovers_motion = Rc::clone(&bottom_controls.open_popovers);
        let pointer_motion = Rc::clone(&last_pointer_position);
        motion.connect_motion(move |_, x, y| {
            let moved = pointer_motion.get()
                .map(|(last_x, last_y)| (x - last_x).hypot(y - last_y) >= 1.0)
                .unwrap_or(true);
            if moved {
                pointer_motion.set(Some((x, y)));
                reveal_and_schedule_controls(&top_motion, &bottom_motion, &timer_motion, &popovers_motion);
            }
        });
        let pointer_leave = Rc::clone(&last_pointer_position);
        motion.connect_leave(move |_| {
            pointer_leave.set(None);
        });
        player_overlay.add_controller(motion);

        let top_fullscreen = top_controls.clone();
        let bottom_fullscreen = bottom_controls.container.clone();
        let timer_fullscreen = Rc::clone(&hide_timer);
        let popovers_fullscreen = Rc::clone(&bottom_controls.open_popovers);
        window.connect_fullscreened_notify(move |_| {
            reveal_and_schedule_controls(
                &top_fullscreen,
                &bottom_fullscreen,
                &timer_fullscreen,
                &popovers_fullscreen,
            );
        });

        // =====================================================================
        // DRAG AND DROP & FILE OPEN CONTROLLERS
        // =====================================================================
        let drop_target = DropTarget::new(gio::File::static_type(), DragAction::COPY);

        let drop_overlay_enter = drop_overlay.clone();
        drop_target.connect_enter(move |_, _, _| {
            drop_overlay_enter.set_visible(true);
            DragAction::COPY
        });

        let drop_overlay_leave = drop_overlay.clone();
        drop_target.connect_leave(move |_| {
            drop_overlay_leave.set_visible(false);
        });

        let drop_overlay_drop = drop_overlay.clone();
        let win_for_drop = window.clone();
        let stack_for_drop = stack.clone();
        let player_for_drop = Rc::clone(&player);
        let video_for_drop = Rc::clone(&video_playback);
        let picture_for_drop = picture.clone();
        let subtitle_for_drop = subtitle_label.clone();
        let bottom_for_drop = bottom_controls.clone();
        let ab_loop_for_drop = Rc::clone(&ab_loop);
        let adaptive_scrub_for_drop = Rc::clone(&adaptive_scrub_state);

        drop_target.connect_drop(move |_, val, _, _| {
            drop_overlay_drop.set_visible(false);
            if let Ok(file) = val.get::<gio::File>() {
                let uri = file.uri();
                load_and_play_file(
                    uri.as_str(),
                    &win_for_drop,
                    &stack_for_drop,
                    &player_for_drop,
                    &video_for_drop,
                    &picture_for_drop,
                    &subtitle_for_drop,
                    &bottom_for_drop,
                    &ab_loop_for_drop,
                    &adaptive_scrub_for_drop,
                );
                return true;
            }
            false
        });
        window.add_controller(drop_target);

        // "Open..." button on loader page opens native GTK FileDialog
        let win_for_dialog = window.clone();
        let stack_for_dialog = stack.clone();
        let player_for_dialog = Rc::clone(&player);
        let video_for_dialog = Rc::clone(&video_playback);
        let picture_for_dialog = picture.clone();
        let subtitle_for_dialog = subtitle_label.clone();
        let bottom_for_dialog = bottom_controls.clone();
        let ab_loop_for_dialog = Rc::clone(&ab_loop);
        let adaptive_scrub_for_dialog = Rc::clone(&adaptive_scrub_state);

        open_btn.connect_clicked(move |_| {
            trigger_open_file_dialog(
                &win_for_dialog,
                &stack_for_dialog,
                &player_for_dialog,
                &video_for_dialog,
                &picture_for_dialog,
                &subtitle_for_dialog,
                &bottom_for_dialog,
                &ab_loop_for_dialog,
                &adaptive_scrub_for_dialog,
            );
        });

        // Showtime Main Menu Popovers (populated with New Window, Open..., Shortcuts, About)
        let loader_popover = create_main_menu_popover(
            &window,
            &stack,
            &player,
            &video_playback,
            &picture,
            &subtitle_label,
            &bottom_controls,
            &ab_loop,
            &adaptive_scrub_state,
        );
        crate::controls::track_popover_visibility(&loader_popover, &bottom_controls.open_popovers);
        loader_menu_btn.set_popover(Some(&loader_popover));
        let player_popover = create_main_menu_popover(
            &window,
            &stack,
            &player,
            &video_playback,
            &picture,
            &subtitle_label,
            &bottom_controls,
            &ab_loop,
            &adaptive_scrub_state,
        );
        crate::controls::track_popover_visibility(&player_popover, &bottom_controls.open_popovers);
        menu_btn.set_popover(Some(&player_popover));

        // Actions: Open, New Window, Shortcuts, About
        let action_open = SimpleAction::new("open", None);
        let win_act = window.clone();
        let stack_act = stack.clone();
        let player_act = Rc::clone(&player);
        let video_act = Rc::clone(&video_playback);
        let pic_act = picture.clone();
        let sub_act = subtitle_label.clone();
        let btm_act = bottom_controls.clone();
        let ab_loop_act = Rc::clone(&ab_loop);
        let adaptive_scrub_act = Rc::clone(&adaptive_scrub_state);
        action_open.connect_activate(move |_, _| {
            trigger_open_file_dialog(
                &win_act,
                &stack_act,
                &player_act,
                &video_act,
                &pic_act,
                &sub_act,
                &btm_act,
                &ab_loop_act,
                &adaptive_scrub_act,
            );
        });
        window.add_action(&action_open);

        let action_new_win = SimpleAction::new("new-window", None);
        let app_clone = app.clone();
        action_new_win.connect_activate(move |_, _| {
            let new_win = GlimpseWindow::new(&app_clone);
            new_win.present();
        });
        window.add_action(&action_new_win);

        let action_shortcuts = SimpleAction::new("shortcuts", None);
        let win_sc = window.clone();
        action_shortcuts.connect_activate(move |_, _| {
            show_shortcuts_dialog(&win_sc);
        });
        window.add_action(&action_shortcuts);

        let action_about = SimpleAction::new("about", None);
        let win_ab = window.clone();
        action_about.connect_activate(move |_, _| {
            let about = gtk4::AboutDialog::builder()
                .transient_for(&win_ab)
                .modal(true)
                .program_name("Glimpse")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Linux media player with an in-process FFmpeg playback engine and native PipeWire audio output.")
                .website("https://github.com/Mel34/Glimpse")
                .logo_icon_name("com.github.Mel34.Glimpse")
                .license_type(gtk4::License::Lgpl21)
                .build();
            about.present();
        });
        window.add_action(&action_about);

        // Space always controls playback, even when a button has keyboard focus.
        let space_controller = EventControllerKey::new();
        space_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let p_space = Rc::clone(&player);
        let video_space = Rc::clone(&video_playback);
        space_controller.connect_key_pressed(move |_, keyval, _, _| {
            if keyval != gdk4::Key::space {
                return glib::Propagation::Proceed;
            }
            if let Some(ref p) = *p_space.borrow() {
                if p.state() == PlayerState::Playing {
                    p.pause();
                } else {
                    if p.state() == PlayerState::Ended {
                        if let Some(ref mut video) = *video_space.borrow_mut() {
                            video.seek(Duration::ZERO);
                        }
                    }
                    p.play();
                }
            }
            glib::Propagation::Stop
        });
        window.add_controller(space_controller);

        // Other shortcuts run in Bubble phase so focused controls get first refusal.
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Bubble);
        let p_keys = Rc::clone(&player);
        let loop_enabled_keys = Rc::clone(&loop_enabled);
        let loop_menu_buttons_keys = Rc::clone(&loop_menu_buttons);
        let ab_loop_keys = Rc::clone(&ab_loop);
        let win_keys = window.clone();
        let win_open_k = window.clone();
        let stack_open_k = stack.clone();
        let p_open_k = Rc::clone(&player);
        let video_open_k = Rc::clone(&video_playback);
        let pic_open_k = picture.clone();
        let sub_open_k = subtitle_label.clone();
        let btm_open_k = bottom_controls.clone();
        let ab_loop_open_k = Rc::clone(&ab_loop);
        let adaptive_scrub_open_k = Rc::clone(&adaptive_scrub_state);
        let top_keys = top_controls.clone();
        let bottom_keys = bottom_controls.container.clone();
        let timer_keys = Rc::clone(&hide_timer);
        let popovers_keys = Rc::clone(&bottom_controls.open_popovers);
        let last_volume_keys = Rc::clone(&bottom_controls.last_nonzero_volume);

        key_controller.connect_key_pressed(move |_, keyval, _keycode, modifier| {
            match keyval {
                // mpv-style screenshot capture: save the clean video frame without subtitles or OSD.
                gdk4::Key::s => {
                    if btm_open_k.screenshot_button.is_sensitive() {
                        match pic_open_k.save_screenshot() {
                            Ok(path) => {
                                tracing::debug!(path = %path.display(), "Screenshot saved");
                                btm_open_k.screenshot_button.set_tooltip_text(
                                    Some(&format!("Saved: {}", path.display())),
                                );
                            }
                            Err(error) => {
                                tracing::error!(%error, "Could not save screenshot");
                                let dialog = gtk4::AlertDialog::builder()
                                    .modal(true)
                                    .message("Could not save screenshot")
                                    .detail(&error)
                                    .buttons(["Close"])
                                    .build();
                                dialog.show(Some(&win_open_k));
                            }
                        }
                    }
                    glib::Propagation::Stop
                }
                // mpv-style playback speed controls.
                gdk4::Key::bracketleft
                | gdk4::Key::bracketright
                | gdk4::Key::braceleft
                | gdk4::Key::braceright
                | gdk4::Key::BackSpace => {
                    if let Some(ref p) = *p_keys.borrow() {
                        let current = p.playback_speed();
                        let speed = match keyval {
                            gdk4::Key::bracketleft => current - 0.1,
                            gdk4::Key::bracketright => current + 0.1,
                            gdk4::Key::braceleft => current / 2.0,
                            gdk4::Key::braceright => current * 2.0,
                            gdk4::Key::BackSpace => 1.0,
                            _ => current,
                        }.clamp(0.5, 2.0);
                        p.set_playback_speed(speed);
                        reveal_and_schedule_controls(
                            &top_keys,
                            &bottom_keys,
                            &timer_keys,
                            &popovers_keys,
                        );
                    }
                    glib::Propagation::Stop
                }
                // Shift+L toggles whole-file looping; lowercase l cycles A-B loop points.
                gdk4::Key::l | gdk4::Key::L => {
                    reveal_and_schedule_controls(&top_keys, &bottom_keys, &timer_keys, &popovers_keys);
                    if modifier.contains(gdk4::ModifierType::SHIFT_MASK) {
                        let enabled = !loop_enabled_keys.get();
                        loop_enabled_keys.set(enabled);
                        for button in loop_menu_buttons_keys.borrow().iter() {
                            button.set_active(enabled);
                        }
                    } else if let Some(ref p) = *p_keys.borrow() {
                        let position = p.position();
                        let next = next_ab_loop_state(ab_loop_keys.get(), position);
                        ab_loop_keys.set(next);
                        match next {
                            AbLoopState::PointA(point) => tracing::info!(point = point.as_secs_f64(), "A-B loop point A set"),
                            AbLoopState::Range { start, end } => tracing::info!(start = start.as_secs_f64(), end = end.as_secs_f64(), "A-B loop enabled"),
                            AbLoopState::Unset => tracing::info!("A-B loop cleared"),
                        }
                    }
                    glib::Propagation::Stop
                }
                // 'f' or 'F': Fullscreen toggle
                gdk4::Key::f | gdk4::Key::F => {
                    if win_keys.is_fullscreen() {
                        win_keys.unfullscreen();
                    } else {
                        win_keys.fullscreen();
                    }
                    glib::Propagation::Stop
                }
                // Escape: Un-fullscreen
                gdk4::Key::Escape => {
                    if win_keys.is_fullscreen() {
                        win_keys.unfullscreen();
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
                // Ctrl+O: Open File
                gdk4::Key::o | gdk4::Key::O if modifier.contains(gdk4::ModifierType::CONTROL_MASK) => {
                    trigger_open_file_dialog(
                        &win_open_k,
                        &stack_open_k,
                        &p_open_k,
                        &video_open_k,
                        &pic_open_k,
                        &sub_open_k,
                        &btm_open_k,
                        &ab_loop_open_k,
                        &adaptive_scrub_open_k,
                    );
                    glib::Propagation::Stop
                }
                // Left Arrow: Seek -5s
                gdk4::Key::Left => {
                    reveal_and_schedule_controls(&top_keys, &bottom_keys, &timer_keys, &popovers_keys);
                    if let Some(ref p) = *p_keys.borrow() {
                        let target = p.position().saturating_sub(Duration::from_secs(5));
                        p.seek(target);
                        if let Some(ref mut video) = *video_open_k.borrow_mut() { video.seek(target); }
                    }
                    glib::Propagation::Stop
                }
                // Right Arrow: Seek +5s
                gdk4::Key::Right => {
                    reveal_and_schedule_controls(&top_keys, &bottom_keys, &timer_keys, &popovers_keys);
                    if let Some(ref p) = *p_keys.borrow() {
                        let next = p.position() + Duration::from_secs(5);
                        let target = p.duration().map(|d| next.min(d)).unwrap_or(next);
                        p.seek(target);
                        if let Some(ref mut video) = *video_open_k.borrow_mut() { video.seek(target); }
                    }
                    glib::Propagation::Stop
                }
                // Up Arrow: Volume +5%
                gdk4::Key::Up => {
                    if btm_open_k.volume_button.is_sensitive() {
                        reveal_and_schedule_controls(&top_keys, &bottom_keys, &timer_keys, &popovers_keys);
                        let v = p_keys
                            .borrow()
                            .as_ref()
                            .map(|p| (p.volume() + 0.05).min(1.5) as f64)
                            .unwrap_or(1.0);
                        if let Some(ref p) = *p_keys.borrow() {
                            p.set_volume(v as f32);
                        }
                        crate::controls::remember_nonzero_volume(&last_volume_keys, v);
                        crate::controls::update_volume_indicator(
                            &btm_open_k.volume_icon,
                            &btm_open_k.volume_button,
                            &btm_open_k.volume_slider,
                            &btm_open_k.volume_percent_label,
                            v,
                            v <= 0.001,
                        );
                    }
                    glib::Propagation::Stop
                }
                // Down Arrow: Volume -5%
                gdk4::Key::Down => {
                    if btm_open_k.volume_button.is_sensitive() {
                        reveal_and_schedule_controls(&top_keys, &bottom_keys, &timer_keys, &popovers_keys);
                        let v = p_keys
                            .borrow()
                            .as_ref()
                            .map(|p| (p.volume() - 0.05).max(0.0) as f64)
                            .unwrap_or(1.0);
                        if let Some(ref p) = *p_keys.borrow() {
                            p.set_volume(v as f32);
                        }
                        crate::controls::remember_nonzero_volume(&last_volume_keys, v);
                        crate::controls::update_volume_indicator(
                            &btm_open_k.volume_icon,
                            &btm_open_k.volume_button,
                            &btm_open_k.volume_slider,
                            &btm_open_k.volume_percent_label,
                            v,
                            v <= 0.001,
                        );
                    }
                    glib::Propagation::Stop
                }
                // 'm' or 'M': Toggle mute
                gdk4::Key::m | gdk4::Key::M => {
                    if btm_open_k.volume_button.is_sensitive() {
                        if let Some(ref p) = *p_keys.borrow() {