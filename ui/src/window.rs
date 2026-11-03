use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use adw::prelude::*;
use adw::{Application, ApplicationWindow};
use gdk4::prelude::PaintableExt;
use gdk4::DragAction;
use gio::SimpleAction;
use gtk4::{
    Align, Box as GtkBox, Button, DropTarget, EventControllerKey, EventControllerMotion,
    FileDialog, FileFilter, GestureClick, Image, Label, MediaFile, MenuButton,
    Orientation, Overlay, Picture, Separator, Settings, Stack, StackTransitionType, WindowHandle,
};

use glimpse_engine::{Player, PlayerState};
use crate::controls::{create_floating_bottom_bar, ControlBarWidgets};
use crate::interaction::{attach_seekbar_wheel, attach_window_volume_wheel, VideoClickStateMachine};
use crate::track_menu::create_track_popover;

pub struct GlimpseWindow {
    window: ApplicationWindow,
    player: Rc<RefCell<Option<Player>>>,
    media_file: Rc<RefCell<Option<MediaFile>>>,
    bottom_controls: ControlBarWidgets,
    picture: Picture,
    #[allow(dead_code)]
    top_controls: GtkBox,
    #[allow(dead_code)]
    overlay: Overlay,
    stack: Stack,
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
        let media_file: Rc<RefCell<Option<MediaFile>>> = Rc::new(RefCell::new(None));

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
        let loader_header = GtkBox::new(Orientation::Horizontal, 0);
        loader_header.add_css_class("loader-header");
        loader_header.set_valign(Align::Start);
        loader_header.set_margin_start(6);
        loader_header.set_margin_end(6);
        loader_header.set_margin_top(6);

        let left_spacer = GtkBox::new(Orientation::Horizontal, 0);
        left_spacer.set_hexpand(true);
        loader_header.append(&left_spacer);

        let loader_title = Label::new(Some("Glimpse"));
        loader_title.add_css_class("loader-title");
        loader_header.append(&loader_title);

        let right_spacer = GtkBox::new(Orientation::Horizontal, 0);
        right_spacer.set_hexpand(true);
        loader_header.append(&right_spacer);

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

        loader_header.append(&loader_right_box);

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
        let picture = Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_vexpand(true);
        picture.set_hexpand(true);
        picture.set_can_target(true);
        player_overlay.set_child(Some(&picture));

        // Subtitle overlay label (hidden by default to avoid dark square on video)
        let subtitle_label = Label::new(None);
        subtitle_label.add_css_class("subtitles-overlay");
        subtitle_label.set_valign(Align::End);
        subtitle_label.set_halign(Align::Center);
        subtitle_label.set_margin_bottom(90);
        subtitle_label.set_visible(false);
        player_overlay.add_overlay(&subtitle_label);

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
        fullscreen_btn.add_css_class("flat");
        fullscreen_btn.add_css_class("header-button");
        fullscreen_btn.set_size_request(28, 28);
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
        });
        top_controls.append(&fullscreen_btn);

        let top_spacer = GtkBox::new(Orientation::Horizontal, 0);
        top_spacer.set_hexpand(true);
        top_controls.append(&top_spacer);

        // Top-right: Hamburger menu & Close window buttons
        let right_top_box = GtkBox::new(Orientation::Horizontal, 6);
        right_top_box.set_halign(Align::End);

        let menu_btn = MenuButton::new();
        menu_btn.set_icon_name("open-menu-symbolic");
        menu_btn.add_css_class("flat");
        menu_btn.add_css_class("header-button");
        menu_btn.set_size_request(28, 28);
        menu_btn.set_can_focus(false);
        menu_btn.set_tooltip_text(Some("Main Menu"));
        right_top_box.append(&menu_btn);

        let close_btn = Button::new();
        let close_icon = Image::from_icon_name("window-close-symbolic");
        close_icon.set_pixel_size(12);
        close_btn.set_child(Some(&close_icon));
        close_btn.add_css_class("flat");
        close_btn.add_css_class("header-close-button");
        close_btn.set_size_request(24, 24);
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

        // Bottom Floating OSD Bar (Title, Volume, Settings, Seekbar, Timestamps)
        let bottom_controls = create_floating_bottom_bar();
        bottom_controls.container.set_valign(Align::End);
        player_overlay.add_overlay(&bottom_controls.container);

        stack.add_named(&player_overlay, Some("player"));
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

        let drop_center = GtkBox::new(Orientation::Vertical, 0);
        drop_center.set_vexpand(true);
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

        // =====================================================================
        // GESTURES & INTERACTION RULES (Rules 1 - 5)
        // =====================================================================

        // Rule 1 & Rule 2: Single-click = play/pause, double-click = fullscreen
        // Playback must NEVER pause when dragging the window
        let double_click_time = Settings::default()
            .map(|s| s.gtk_double_click_time() as u32)
            .unwrap_or(250)
            .min(300);

        let click_state = Rc::new(RefCell::new(VideoClickStateMachine::new(double_click_time)));
        let is_dragging = Rc::new(RefCell::new(false));

        let click_gesture = GestureClick::new();
        click_gesture.set_button(1); // Left mouse button only

        let m_toggle = Rc::clone(&media_file);
        let p_toggle = Rc::clone(&player);
        let win_click = window.clone();
        let click_state_press = Rc::clone(&click_state);
        let is_dragging_press = Rc::clone(&is_dragging);

        // On press: if double-click (n_press >= 2), toggle fullscreen immediately and cancel any timer
        click_gesture.connect_pressed(move |_gesture, n_press, _x, _y| {
            if n_press >= 2 {
                click_state_press.borrow_mut().cancel();
                *is_dragging_press.borrow_mut() = false;
                if win_click.is_fullscreen() {
                    win_click.unfullscreen();
                } else {
                    win_click.fullscreen();
                }
            } else {
                *is_dragging_press.borrow_mut() = false;
            }
        });

        // On release: only trigger single-click play/pause if no dragging took place!
        let click_state_rel = Rc::clone(&click_state);
        let is_dragging_rel = Rc::clone(&is_dragging);
        let win_rel = window.clone();
        click_gesture.connect_released(move |_gesture, n_press, _x, _y| {
            if *is_dragging_rel.borrow() {
                // Dragging occurred: do not play/pause
                *is_dragging_rel.borrow_mut() = false;
                return;
            }

            if n_press == 1 {
                let m_action = Rc::clone(&m_toggle);
                let p_action = Rc::clone(&p_toggle);
                let w_fs = win_rel.clone();

                VideoClickStateMachine::handle_click(
                    Rc::clone(&click_state_rel),
                    1,
                    move || {
                        if let Some(ref m) = *m_action.borrow() {
                            if m.is_playing() {
                                m.pause();
                            } else {
                                m.play();
                            }
                        }
                        if let Some(ref p) = *p_action.borrow() {
                            if p.state() == PlayerState::Playing {
                                p.pause();
                            } else {
                                p.play();
                            }
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

        // If gesture was stopped or cancelled by GTK/compositor, cancel pending timer
        let click_state_stop = Rc::clone(&click_state);
        click_gesture.connect_stopped(move |_| {
            click_state_stop.borrow_mut().cancel();
        });

        picture.add_controller(click_gesture);

        // Window drag on video surface: starts moving window when drag exceeds threshold.
        // Cancels click timer and sets is_dragging so playback continues uninterrupted!
        let drag_gesture = gtk4::GestureDrag::new();
        let win_drag_p = window.clone();
        let click_state_drag_begin = Rc::clone(&click_state);

        drag_gesture.connect_drag_begin(move |_gesture, _start_x, _start_y| {
            click_state_drag_begin.borrow_mut().cancel();
        });

        let click_state_upd = Rc::clone(&click_state);
        let is_dragging_upd = Rc::clone(&is_dragging);
        drag_gesture.connect_drag_update(move |gesture, offset_x, offset_y| {
            if win_drag_p.is_fullscreen() {
                return;
            }
            if offset_x.hypot(offset_y) > 4.0 {
                *is_dragging_upd.borrow_mut() = true;
                click_state_upd.borrow_mut().cancel();

                if let Some(surface) = win_drag_p.surface() {
                    if let Ok(toplevel) = surface.downcast::<gdk4::Toplevel>() {
                        if let Some(device) = gesture.device() {
                            toplevel.begin_move(&device, 1, 0.0, 0.0, 0);
                        }
                    }
                }
            }
        });

        let is_dragging_end = Rc::clone(&is_dragging);
        drag_gesture.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
            let is_d = Rc::clone(&is_dragging_end);
            glib::timeout_add_local_once(Duration::from_millis(50), move || {
                *is_d.borrow_mut() = false;
            });
        });

        picture.add_controller(drag_gesture);

        // Rule 3: Mouse wheel over seekbar seeks in 5s steps (consumed)
        let m_seek_wheel = Rc::clone(&media_file);
        let p_seek_wheel = Rc::clone(&player);

        attach_seekbar_wheel(
            &bottom_controls.seekbar,
            {
                let m = Rc::clone(&media_file);
                move || {
                    m.borrow().as_ref().map(|m| Duration::from_micros(m.duration().max(0) as u64))
                }
            },
            {
                let m = Rc::clone(&media_file);
                move || {
                    m.borrow().as_ref().map(|m| Duration::from_micros(m.timestamp().max(0) as u64)).unwrap_or(Duration::ZERO)
                }
            },
            move |target_pos| {
                if let Some(ref m) = *m_seek_wheel.borrow() {
                    m.seek((target_pos.as_secs_f64() * 1_000_000.0) as i64);
                }
                if let Some(ref p) = *p_seek_wheel.borrow() {
                    p.seek(target_pos);
                }
            },
        );

        // Rule 4: Mouse wheel over window body adjusts volume in 5% steps
        let m_vol_wheel = Rc::clone(&media_file);
        let p_vol_wheel = Rc::clone(&player);
        let win_ref = window.clone().upcast::<gtk4::Window>();

        let v_icon_w = bottom_controls.volume_icon.clone();
        let v_btn_w = bottom_controls.volume_button.clone();
        let v_slider_w = bottom_controls.volume_slider.clone();
        let v_pct_w = bottom_controls.volume_percent_label.clone();

        attach_window_volume_wheel(
            &win_ref,
            {
                let m = Rc::clone(&media_file);
                move || {
                    m.borrow().as_ref().map(|m| m.volume() as f32).unwrap_or(1.0)
                }
            },
            move |new_vol| {
                if let Some(ref m) = *m_vol_wheel.borrow() {
                    m.set_volume(new_vol as f64);
                    m.set_muted(new_vol <= 0.001);
                }
                if let Some(ref p) = *p_vol_wheel.borrow() {
                    p.set_volume(new_vol);
                }
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
        let m_vol_slider = Rc::clone(&media_file);
        let p_vol_slider = Rc::clone(&player);
        let v_icon_sl = bottom_controls.volume_icon.clone();
        let v_btn_sl = bottom_controls.volume_button.clone();
        let v_slider_sl = bottom_controls.volume_slider.clone();
        let v_pct_sl = bottom_controls.volume_percent_label.clone();
        bottom_controls.volume_slider.connect_change_value(move |_, _, new_val| {
            let val = new_val.clamp(0.0, 1.0);
            if let Some(ref m) = *m_vol_slider.borrow() {
                m.set_volume(val);
                m.set_muted(val <= 0.001);
            }
            if let Some(ref p) = *p_vol_slider.borrow() {
                p.set_volume(val as f32);
            }
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

        // Autohide controls on inactivity
        let motion = EventControllerMotion::new();
        let top_ref = top_controls.clone();
        let btm_ref = bottom_controls.container.clone();
        motion.connect_motion(move |_, _, _| {
            top_ref.set_opacity(1.0);
            btm_ref.set_opacity(1.0);
        });
        player_overlay.add_controller(motion);

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
        let media_for_drop = Rc::clone(&media_file);
        let picture_for_drop = picture.clone();
        let bottom_for_drop = bottom_controls.clone();

        drop_target.connect_drop(move |_, val, _, _| {
            drop_overlay_drop.set_visible(false);
            if let Ok(file) = val.get::<gio::File>() {
                if let Some(path) = file.path() {
                    if let Some(p_str) = path.to_str() {
                        load_and_play_file(
                            p_str,
                            &win_for_drop,
                            &stack_for_drop,
                            &player_for_drop,
                            &media_for_drop,
                            &picture_for_drop,
                            &bottom_for_drop,
                        );
                        return true;
                    }
                }
            }
            false
        });
        window.add_controller(drop_target);

        // "Open..." button on loader page opens native GTK FileDialog
        let win_for_dialog = window.clone();
        let stack_for_dialog = stack.clone();
        let player_for_dialog = Rc::clone(&player);
        let media_for_dialog = Rc::clone(&media_file);
        let picture_for_dialog = picture.clone();
        let bottom_for_dialog = bottom_controls.clone();

        open_btn.connect_clicked(move |_| {
            trigger_open_file_dialog(
                &win_for_dialog,
                &stack_for_dialog,
                &player_for_dialog,
                &media_for_dialog,
                &picture_for_dialog,
                &bottom_for_dialog,
            );
        });

        // Showtime Main Menu Popovers (populated with New Window, Open..., Shortcuts, About)
        let loader_popover = create_main_menu_popover(
            &window,
            &stack,
            &player,
            &media_file,
            &picture,
            &bottom_controls,
        );
        loader_menu_btn.set_popover(Some(&loader_popover));

        let player_popover = create_main_menu_popover(
            &window,
            &stack,
            &player,
            &media_file,
            &picture,
            &bottom_controls,
        );
        menu_btn.set_popover(Some(&player_popover));

        // Actions: Open, New Window, Shortcuts, About
        let action_open = SimpleAction::new("open", None);
        let win_act = window.clone();
        let stack_act = stack.clone();
        let player_act = Rc::clone(&player);
        let media_act = Rc::clone(&media_file);
        let pic_act = picture.clone();
        let btm_act = bottom_controls.clone();
        action_open.connect_activate(move |_, _| {
            trigger_open_file_dialog(
                &win_act,
                &stack_act,
                &player_act,
                &media_act,
                &pic_act,
                &btm_act,
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
                .version("0.1.0")
                .comments("Clean, predictable Linux video player built with GTK4 and libadwaita, with an evolving in-process playback engine.")
                .website("https://github.com/Mel34/Glimpse")
                .logo_icon_name("com.github.Mel34.Glimpse")
                .license_type(gtk4::License::Lgpl21)
                .build();
            about.present();
        });
        window.add_action(&action_about);

        // Keyboard Shortcuts: Space (play/pause), F (fullscreen), Escape (unfullscreen), etc.
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let m_keys = Rc::clone(&media_file);
        let p_keys = Rc::clone(&player);
        let win_keys = window.clone();
        let win_open_k = window.clone();
        let stack_open_k = stack.clone();
        let p_open_k = Rc::clone(&player);
        let m_open_k = Rc::clone(&media_file);
        let pic_open_k = picture.clone();
        let btm_open_k = bottom_controls.clone();

        key_controller.connect_key_pressed(move |_, keyval, _keycode, modifier| {
            match keyval {
                // Space: Play / Pause toggle
                gdk4::Key::space => {
                    if let Some(ref m) = *m_keys.borrow() {
                        if m.is_playing() {
                            m.pause();
                        } else {
                            m.play();
                        }
                    }
                    if let Some(ref p) = *p_keys.borrow() {
                        if p.state() == PlayerState::Playing {
                            p.pause();
                        } else {
                            p.play();
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
                        &m_open_k,
                        &pic_open_k,
                        &btm_open_k,
                    );
                    glib::Propagation::Stop
                }
                // Left Arrow: Seek -5s
                gdk4::Key::Left => {
                    if let Some(ref m) = *m_keys.borrow() {
                        let cur = m.timestamp();
                        m.seek((cur - 5_000_000).max(0));
                    }
                    glib::Propagation::Stop
                }
                // Right Arrow: Seek +5s
                gdk4::Key::Right => {
                    if let Some(ref m) = *m_keys.borrow() {
                        let cur = m.timestamp();
                        let dur = m.duration();
                        let max_ts = if dur > 0 { dur } else { i64::MAX };
                        m.seek((cur + 5_000_000).min(max_ts));
                    }
                    glib::Propagation::Stop
                }
                // Up Arrow: Volume +5%
                gdk4::Key::Up => {
                    if let Some(ref m) = *m_keys.borrow() {
                        let v = (m.volume() + 0.05).min(1.0);
                        m.set_volume(v);
                        m.set_muted(v <= 0.001);
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
                    if let Some(ref m) = *m_keys.borrow() {
                        let v = (m.volume() - 0.05).max(0.0);
                        m.set_volume(v);
                        m.set_muted(v <= 0.001);
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
                    if let Some(ref m) = *m_keys.borrow() {
                        let is_muted = !m.is_muted();
                        m.set_muted(is_muted);
                        crate::controls::update_volume_indicator(
                            &btm_open_k.volume_icon,
                            &btm_open_k.volume_button,
                            &btm_open_k.volume_slider,
                            &btm_open_k.volume_percent_label,
                            m.volume(),
                            is_muted,
                        );
                    }
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        window.add_controller(key_controller);

        Self {
            window,
            player,
            media_file,
            bottom_controls,
            picture,
            top_controls,
            overlay: player_overlay,
            stack,
            drop_overlay,
        }
    }

    /// Open a video file, start hardware playback, populate tracks and timeline.
    pub fn open_file(&self, path: &str) {
        load_and_play_file(
            path,
            &self.window,
            &self.stack,
            &self.player,
            &self.media_file,
            &self.picture,
            &self.bottom_controls,
        );
    }

    /// Stop playback and return to the empty state loader screen.
    #[allow(dead_code)]
    pub fn close_file(&self) {
        if let Some(ref m) = *self.media_file.borrow() {
            m.pause();
        }
        *self.media_file.borrow_mut() = None;
        self.picture.set_paintable(gdk4::Paintable::NONE);

        if let Some(ref p) = *self.player.borrow() {
            p.pause();
        }
        *self.player.borrow_mut() = None;

        self.bottom_controls.title_label.set_text("No media loaded");
        self.bottom_controls.elapsed_label.set_text("0:00");
        self.bottom_controls.duration_label.set_text("0:00");
        self.bottom_controls.seekbar.set_value(0.0);
        self.window.set_title(Some("Video Player"));
        self.stack.set_visible_child_name("loader");
    }

    pub fn present(&self) {
        self.window.present();
    }
}

/// Helper function to open native GTK4 FileDialog filtered by video MIME types
fn trigger_open_file_dialog(
    window: &ApplicationWindow,
    stack: &Stack,
    player: &Rc<RefCell<Option<Player>>>,
    media_holder: &Rc<RefCell<Option<MediaFile>>>,
    picture: &Picture,
    bottom_controls: &ControlBarWidgets,
) {
    let file_dialog = FileDialog::builder()
        .title("Open Video")
        .accept_label("Open")
        .modal(true)
        .build();

    let filter = FileFilter::new();
    filter.set_name(Some("Video Files"));
    filter.add_mime_type("video/*");
    file_dialog.set_default_filter(Some(&filter));

    let win_cb = window.clone();
    let stack_cb = stack.clone();
    let player_cb = Rc::clone(player);
    let media_cb = Rc::clone(media_holder);
    let pic_cb = picture.clone();
    let bottom_cb = bottom_controls.clone();

    file_dialog.open(Some(window), gio::Cancellable::NONE, move |result| {
        if let Ok(file) = result {
            if let Some(path) = file.path() {
                if let Some(p_str) = path.to_str() {
                    load_and_play_file(
                        p_str,
                        &win_cb,
                        &stack_cb,
                        &player_cb,
                        &media_cb,
                        &pic_cb,
                        &bottom_cb,
                    );
                }
            }
        }
    });
}

/// Routine to load video into GTK4 MediaFile and Glimpse engine for playback
fn load_and_play_file(
    path: &str,
    window: &ApplicationWindow,
    stack: &Stack,
    player: &Rc<RefCell<Option<Player>>>,
    media_holder: &Rc<RefCell<Option<MediaFile>>>,
    picture: &Picture,
    bottom_controls: &ControlBarWidgets,
) {
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);

    window.set_title(Some(&format!("Glimpse - {}", file_name)));
    bottom_controls.title_label.set_text(file_name);

    // Stop previous media if any
    if let Some(ref old_m) = *media_holder.borrow() {
        old_m.pause();
    }
    *media_holder.borrow_mut() = None;

    // Create native GTK4 MediaFile for real video decoding & audio output
    let file = if path.starts_with("file://") || path.contains("://") {
        gio::File::for_uri(path)
    } else {
        gio::File::for_path(path)
    };

    let media = MediaFile::for_file(&file);
    media.set_loop(true);
    picture.set_paintable(Some(&media));
    media.play();

    // Duration update when container metadata arrives
    let seekbar_dur = bottom_controls.seekbar.clone();
    let dur_label = bottom_controls.duration_label.clone();
    media.connect_duration_notify(move |m| {
        let dur_micros = m.duration();
        if dur_micros > 0 {
            let dur_secs = dur_micros as f64 / 1_000_000.0;
            seekbar_dur.set_range(0.0, dur_secs);
            dur_label.set_text(&crate::controls::format_duration(Duration::from_micros(dur_micros as u64)));
        }
    });

    // Check if duration is already known immediately
    let initial_dur = media.duration();
    if initial_dur > 0 {
        let dur_secs = initial_dur as f64 / 1_000_000.0;
        bottom_controls.seekbar.set_range(0.0, dur_secs);
        bottom_controls.duration_label.set_text(&crate::controls::format_duration(Duration::from_micros(initial_dur as u64)));
    }

    // Timeline synchronization during playback
    let seekbar_pos = bottom_controls.seekbar.clone();
    let el_label = bottom_controls.elapsed_label.clone();
    media.connect_timestamp_notify(move |m| {
        let ts_micros = m.timestamp();
        let ts_secs = ts_micros as f64 / 1_000_000.0;
        seekbar_pos.set_value(ts_secs);
        el_label.set_text(&crate::controls::format_duration(Duration::from_micros(ts_micros.max(0) as u64)));
    });

    // Seeking via clicking / dragging seekbar
    let m_seek = media.clone();
    bottom_controls.seekbar.connect_change_value(move |_, _, val| {
        let target_micros = (val * 1_000_000.0) as i64;
        m_seek.seek(target_micros);
        glib::Propagation::Proceed
    });

    // Volume & Mute UI reflection
    let v_icon_m = bottom_controls.volume_icon.clone();
    let v_btn_m = bottom_controls.volume_button.clone();
    let v_slider_m = bottom_controls.volume_slider.clone();
    let v_pct_m = bottom_controls.volume_percent_label.clone();
    media.connect_volume_notify(move |m| {
        crate::controls::update_volume_indicator(
            &v_icon_m,
            &v_btn_m,
            &v_slider_m,
            &v_pct_m,
            m.volume(),
            m.is_muted(),
        );
    });

    let v_icon_mut = bottom_controls.volume_icon.clone();
    let v_btn_mut = bottom_controls.volume_button.clone();
    let v_slider_mut = bottom_controls.volume_slider.clone();
    let v_pct_mut = bottom_controls.volume_percent_label.clone();
    media.connect_muted_notify(move |m| {
        crate::controls::update_volume_indicator(
            &v_icon_mut,
            &v_btn_mut,
            &v_slider_mut,
            &v_pct_mut,
            m.volume(),
            m.is_muted(),
        );
    });

    // Window size clamps strictly to video aspect ratio (no letterbox margins)
    let win_aspect = window.clone();
    let has_resized = Rc::new(RefCell::new(false));
    let has_resized_c = Rc::clone(&has_resized);
    media.connect_prepared_notify(move |m| {
        if *has_resized_c.borrow() {
            return;
        }
        let iw = m.intrinsic_width();
        let ih = m.intrinsic_height();
        if iw > 0 && ih > 0 {
            *has_resized_c.borrow_mut() = true;
            let aspect = iw as f64 / ih as f64;
            let (tw, th) = if aspect >= 1.0 {
                let w = (iw as i32).clamp(640, 1080);
                let h = (w as f64 / aspect).round() as i32;
                (w, h)
            } else {
                let h = (ih as i32).clamp(540, 800);
                let w = (h as f64 * aspect).round() as i32;
                (w, h)
            };
            win_aspect.set_default_size(tw, th);
        }
    });

    *media_holder.borrow_mut() = Some(media);

    // Initialize Glimpse engine for stream probing and audio master clock tracking
    match Player::open(path) {
        Ok((p, _event_rx)) => {
            p.play();
            *player.borrow_mut() = Some(p);

            // Setup track popover with Rule 5 (Subtitles OFF by default)
            if let Some(ref current_p) = *player.borrow() {
                let tracks = current_p.tracks();

                // Check container stream video dimensions for immediate aspect ratio clamping
                if let Some(vt) = tracks.video_tracks.first() {
                    if let (Some(w), Some(h)) = (vt.width, vt.height) {
                        let aspect = w as f64 / h as f64;
                        let (tw, th) = if aspect >= 1.0 {
                            let tw = (w as i32).clamp(640, 1080);
                            let th = (tw as f64 / aspect).round() as i32;
                            (tw, th)
                        } else {
                            let th = (h as i32).clamp(540, 800);
                            let tw = (th as f64 * aspect).round() as i32;
                            (tw, th)
                        };
                        window.set_default_size(tw, th);
                    }
                }

                let p_audio = Rc::clone(player);
                let p_sub = Rc::clone(player);

                let popover = create_track_popover(
                    &tracks,
                    move |aid| {
                        if let Some(ref p) = *p_audio.borrow() {
                            p.select_audio(aid);
                        }
                    },
                    move |sid| {
                        if let Some(ref p) = *p_sub.borrow() {
                            p.select_subtitle(sid);
                        }
                    },
                );
                bottom_controls.settings_button.set_popover(Some(&popover));
            }
        }
        Err(e) => {
            eprintln!("Engine probe note: {}", e);
        }
    }

    // Switch view from loader to player
    stack.set_visible_child_name("player");
}

/// Modal dialog showing the keyboard shortcuts list
fn show_shortcuts_dialog(parent: &ApplicationWindow) {
    let dialog = gtk4::Window::builder()
        .title("Keyboard Shortcuts")
        .transient_for(parent)
        .modal(true)
        .default_width(380)
        .default_height(340)
        .build();

    dialog.add_css_class("glimpse-window");

    let vbox = GtkBox::new(Orientation::Vertical, 10);
    vbox.set_margin_start(24);
    vbox.set_margin_end(24);
    vbox.set_margin_top(20);
    vbox.set_margin_bottom(20);

    let title = Label::new(Some("Keyboard Shortcuts"));
    title.add_css_class("title-2");
    vbox.append(&title);

    let shortcuts = [
        ("Space", "Play / Pause video"),
        ("F", "Toggle Fullscreen"),
        ("Escape", "Leave Fullscreen"),
        ("Ctrl + O", "Open file"),
        ("Ctrl + N", "New window"),
        ("Left / Right", "Seek ±5 seconds (Rule 3)"),
        ("Up / Down", "Volume ±5% (Rule 4)"),
        ("M", "Toggle Mute"),
    ];

    for (key, desc) in shortcuts {
        let row = GtkBox::new(Orientation::Horizontal, 12);
        let l_desc = Label::new(Some(desc));
        l_desc.set_hexpand(true);
        l_desc.set_halign(Align::Start);

        let l_key = Label::new(Some(key));
        l_key.add_css_class("dim-label");
        l_key.set_halign(Align::End);

        row.append(&l_desc);
        row.append(&l_key);
        vbox.append(&row);
    }

    let close_btn = Button::with_label("Close");
    close_btn.add_css_class("pill");
    close_btn.set_halign(Align::Center);
    close_btn.set_margin_top(12);
    let dlg = dialog.clone();
    close_btn.connect_clicked(move |_| dlg.close());
    vbox.append(&close_btn);

    dialog.set_child(Some(&vbox));
    dialog.present();
}

/// Helper to create Showtime-style main menu popover with New Window, Open, Shortcuts, About
fn create_main_menu_popover(
    window: &ApplicationWindow,
    stack: &Stack,
    player: &Rc<RefCell<Option<Player>>>,
    media_file: &Rc<RefCell<Option<MediaFile>>>,
    picture: &Picture,
    bottom_controls: &ControlBarWidgets,
) -> gtk4::Popover {
    let popover = gtk4::Popover::new();
    popover.add_css_class("menu-popover");

    let vbox = GtkBox::new(Orientation::Vertical, 2);
    vbox.set_margin_start(4);
    vbox.set_margin_end(4);
    vbox.set_margin_top(4);
    vbox.set_margin_bottom(4);

    // 1. New Window (Ctrl+N)
    let btn_new_win = Button::new();
    btn_new_win.add_css_class("flat");
    btn_new_win.add_css_class("menu-item-button");
    let box_new_win = GtkBox::new(Orientation::Horizontal, 20);
    let l1 = Label::new(Some("New Window"));
    l1.set_halign(Align::Start);
    l1.set_hexpand(true);
    let s1 = Label::new(Some("Ctrl+N"));
    s1.add_css_class("dim-label");
    s1.set_halign(Align::End);
    box_new_win.append(&l1);
    box_new_win.append(&s1);
    btn_new_win.set_child(Some(&box_new_win));

    let pop_close1 = popover.clone();
    let win_app = window.clone();
    btn_new_win.connect_clicked(move |_| {
        pop_close1.popdown();
        if let Some(app) = win_app.application() {
            if let Ok(adw_app) = app.downcast::<adw::Application>() {
                let new_win = GlimpseWindow::new(&adw_app);
                new_win.present();
            }
        }
    });
    vbox.append(&btn_new_win);

    // 2. Open... (Ctrl+O)
    let btn_open = Button::new();
    btn_open.add_css_class("flat");
    btn_open.add_css_class("menu-item-button");
    let box_open = GtkBox::new(Orientation::Horizontal, 20);
    let l2 = Label::new(Some("Open…"));
    l2.set_halign(Align::Start);
    l2.set_hexpand(true);
    let s2 = Label::new(Some("Ctrl+O"));
    s2.add_css_class("dim-label");
    s2.set_halign(Align::End);
    box_open.append(&l2);
    box_open.append(&s2);
    btn_open.set_child(Some(&box_open));

    let pop_close2 = popover.clone();
    let win_op = window.clone();
    let stack_op = stack.clone();
    let p_op = Rc::clone(player);
    let m_op = Rc::clone(media_file);
    let pic_op = picture.clone();
    let btm_op = bottom_controls.clone();
    btn_open.connect_clicked(move |_| {
        pop_close2.popdown();
        trigger_open_file_dialog(&win_op, &stack_op, &p_op, &m_op, &pic_op, &btm_op);
    });
    vbox.append(&btn_open);

    // Separator
    let sep = Separator::new(Orientation::Horizontal);
    sep.set_margin_top(4);
    sep.set_margin_bottom(4);
    vbox.append(&sep);

    // 3. Keyboard Shortcuts (Ctrl+?)
    let btn_shortcuts = Button::new();
    btn_shortcuts.add_css_class("flat");
    btn_shortcuts.add_css_class("menu-item-button");
    let box_sc = GtkBox::new(Orientation::Horizontal, 20);
    let l3 = Label::new(Some("Keyboard Shortcuts"));
    l3.set_halign(Align::Start);
    l3.set_hexpand(true);
    let s3 = Label::new(Some("Ctrl+?"));
    s3.add_css_class("dim-label");
    s3.set_halign(Align::End);
    box_sc.append(&l3);
    box_sc.append(&s3);
    btn_shortcuts.set_child(Some(&box_sc));

    let pop_close3 = popover.clone();
    let win_sc = window.clone();
    btn_shortcuts.connect_clicked(move |_| {
        pop_close3.popdown();
        show_shortcuts_dialog(&win_sc);
    });
    vbox.append(&btn_shortcuts);

    // 4. About Glimpse
    let btn_about = Button::new();
    btn_about.add_css_class("flat");
    btn_about.add_css_class("menu-item-button");
    let box_ab = GtkBox::new(Orientation::Horizontal, 20);
    let l4 = Label::new(Some("About Glimpse"));
    l4.set_halign(Align::Start);
    l4.set_hexpand(true);
    box_ab.append(&l4);
    btn_about.set_child(Some(&box_ab));

    let pop_close4 = popover.clone();
    let win_ab = window.clone();
    btn_about.connect_clicked(move |_| {
        pop_close4.popdown();
        let about = gtk4::AboutDialog::builder()
            .transient_for(&win_ab)
            .modal(true)
            .program_name("Glimpse")
            .version("0.1.0")
            .comments("Clean, predictable Linux video player built with GTK4 and libadwaita, with an evolving in-process playback engine.")
            .website("https://github.com/Mel34/Glimpse")
            .logo_icon_name("com.github.Mel34.Glimpse")
            .license_type(gtk4::License::Lgpl21)
            .build();
        about.present();
    });
    vbox.append(&btn_about);

    popover.set_child(Some(&vbox));
    popover
}
