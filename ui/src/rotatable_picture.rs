//! Zero-copy, aspect-correct video presentation with user rotation.

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RotatablePicture {
        pub paintable: RefCell<Option<gdk4::Paintable>>,
        pub rotation: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RotatablePicture {
        const NAME: &'static str = "GlimpseRotatablePicture";
        type Type = super::RotatablePicture;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for RotatablePicture {}

    impl WidgetImpl for RotatablePicture {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let Some(paintable) = self.paintable.borrow().as_ref().cloned() else {
                return;
            };

            let widget = self.obj();
            let width = widget.width().max(0) as f64;
            let height = widget.height().max(0) as f64;
            if width <= 0.0 || height <= 0.0 {
                return;
            }

            let intrinsic_width = paintable.intrinsic_width().max(1) as f64;
            let intrinsic_height = paintable.intrinsic_height().max(1) as f64;
            let rotation = self.rotation.get();
            let quarter_turn = rotation.rem_euclid(180) != 0;

            // Fit the rotated bounds into the available viewport while keeping
            // the original texture dimensions and GPU-backed paintable intact.
            let scale = if quarter_turn {
                (width / intrinsic_height).min(height / intrinsic_width)
            } else {
                (width / intrinsic_width).min(height / intrinsic_height)
            };
            let draw_width = intrinsic_width * scale;
            let draw_height = intrinsic_height * scale;

            snapshot.save();
            snapshot.translate(&gtk4::graphene::Point::new(
                (width / 2.0) as f32,
                (height / 2.0) as f32,
            ));
            snapshot.rotate(rotation as f32);
            snapshot.translate(&gtk4::graphene::Point::new(
                (-draw_width / 2.0) as f32,
                (-draw_height / 2.0) as f32,
            ));
            paintable.snapshot(snapshot, draw_width, draw_height);
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct RotatablePicture(ObjectSubclass<imp::RotatablePicture>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl RotatablePicture {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_paintable(&self, paintable: Option<&gdk4::Paintable>) {
        self.imp().paintable.replace(paintable.cloned());
        self.queue_draw();
    }

    pub fn set_rotation(&self, degrees: i32) {
        self.imp().rotation.set(normalize_rotation(degrees));
        self.queue_draw();
    }

    pub fn rotate_by(&self, degrees: i32) {
        self.set_rotation(self.imp().rotation.get().saturating_add(degrees));
    }

    pub fn rotation(&self) -> i32 {
        self.imp().rotation.get()
    }
}

fn normalize_rotation(degrees: i32) -> i32 {
    degrees.rem_euclid(360)
}

#[cfg(test)]
mod tests {
    use super::normalize_rotation;

    #[test]
    fn rotation_wraps_in_both_directions() {
        assert_eq!(normalize_rotation(90), 90);
        assert_eq!(normalize_rotation(360), 0);
        assert_eq!(normalize_rotation(-90), 270);
        assert_eq!(normalize_rotation(450), 90);
    }
}
