//! Glimpse Interaction Specification Module
//! Ported behaviors from author's patches to GNOME Showtime (https://github.com/Mel34/showtime-patches).
//!
//! Rule 1: No center overlay buttons.
//! Rule 2: Single click = play/pause (after timer), double-click = fullscreen.
//! Rule 3: Mouse wheel over seek bar seeks in 5s steps (consumed, no volume change).
//! Rule 4: Mouse wheel elsewhere changes volume in 5% steps (bubble phase).
//! Rule 5: Subtitles are off by default.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use glib::SourceId;
use gtk4::prelude::*;
use gtk4::{glib, EventControllerScroll, EventControllerScrollFlags, Window};

pub struct VideoClickStateMachine {
    pending_timer: Option<SourceId>,
    double_click_time_ms: u32,
}

impl VideoClickStateMachine {
    pub fn new(double_click_time_ms: u32) -> Self {
        Self {
            pending_timer: None,
            double_click_time_ms: if double_click_time_ms > 0 { double_click_time_ms } else { 250 },
        }
    }

    pub fn cancel(&mut self) {
        if let Some(source) = self.pending_timer.take() {
            source.remove();
        }
    }

    pub fn handle_click<FPlay, FFullscreen>(
        state: Rc<RefCell<Self>>,
        n_press: i32,
        on_toggle_play: FPlay,
        on_toggle_fullscreen: FFullscreen,
    ) where
        FPlay: Fn() + 'static,
        FFullscreen: Fn() + 'static,
    {
        let mut st = state.borrow_mut();

        if let Some(source) = st.pending_timer.take() {
            source.remove();
        }

        if n_press >= 2 {
            on_toggle_fullscreen();
        } else if n_press == 1 {
            let state_clone = Rc::clone(&state);
            let delay_ms = st.double_click_time_ms;

            let source = glib::timeout_add_local_once(Duration::from_millis(delay_ms as u64), move || {
                state_clone.borrow_mut().pending_timer = None;
                on_toggle_play();
            });

            st.pending_timer = Some(source);
        }
    }
}

pub fn attach_seekbar_wheel<FSeek>(
    scale: &gtk4::Scale,
    get_duration: impl Fn() -> Option<Duration> + 'static,
    get_position: impl Fn() -> Duration + 'static,
    seek_cb: FSeek,
) where
    FSeek: Fn(Duration) + 'static,
{
    let controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);

    controller.connect_scroll(move |_, _dx, dy| {
        let Some(duration) = get_duration() else {
            return glib::Propagation::Stop;
        };

        let current = get_position();
        let step = Duration::from_secs(5);

        let new_pos = if dy < 0.0 {
            (current + step).min(duration)
        } else {
            current.saturating_sub(step)
        };

        seek_cb(new_pos);
        glib::Propagation::Stop
    });

    scale.add_controller(controller);
}

pub fn attach_window_volume_wheel<FVol>(
    window: &Window,
    get_volume: impl Fn() -> f32 + 'static,
    set_volume_cb: FVol,
) where
    FVol: Fn(f32) + 'static,
{
    let controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);

    controller.connect_scroll(move |_, _dx, dy| {
        let current = get_volume();
        let delta = if dy < 0.0 { 0.05 } else { -0.05 };
        let new_vol = (current + delta).clamp(0.0, 1.0);

        set_volume_cb(new_vol);
        glib::Propagation::Proceed
    });

    window.add_controller(controller);
}