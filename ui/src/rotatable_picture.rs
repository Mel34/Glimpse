//! Zero-copy, aspect-correct video presentation with user rotation.

use gdk4::prelude::*;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use std::cell::{Cell, RefCell};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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

    /// Save the currently presented video frame, including the user's rotation.
    pub fn save_screenshot(&self) -> Result<PathBuf, String> {
        let paintable = self.imp().paintable.borrow().clone()
            .ok_or_else(|| "There is no video frame to capture yet.".to_string())?;
        let texture = paintable.downcast::<gdk4::Texture>()
            .map_err(|_| "The current video frame cannot be downloaded.".to_string())?;

        let width = texture.width();
        let height = texture.height();
        if width <= 0 || height <= 0 {
            return Err("The current video frame has invalid dimensions.".to_string());
        }

        // GDK downloads textures in Cairo ARGB32 layout. This deliberately
        // performs readback only on an explicit screenshot request; playback
        // itself remains on the existing GPU-backed presentation path.
        let mut source = gtk4::cairo::ImageSurface::create(
            gtk4::cairo::Format::ARgb32,
            width,
            height,
        ).map_err(|error| format!("Could not allocate screenshot buffer: {error}"))?;
        let stride = source.stride() as usize;
        {
            let mut pixels = source.data()
                .map_err(|error| format!("Could not access screenshot pixels: {error}"))?;
            texture.download(&mut pixels, stride);
        }
        source.mark_dirty();

        let rotation = self.rotation();
        let (output_width, output_height) = screenshot_dimensions(width, height, rotation);
        let output = gtk4::cairo::ImageSurface::create(
            gtk4::cairo::Format::ARgb32,
            output_width,
            output_height,
        ).map_err(|error| format!("Could not allocate rotated screenshot: {error}"))?;
        let context = gtk4::cairo::Context::new(&output)
            .map_err(|error| format!("Could not prepare screenshot: {error}"))?;

        match rotation {
            90 => {
                context.translate(output_width as f64, 0.0);
                context.rotate(std::f64::consts::FRAC_PI_2);
            }
            180 => {
                context.translate(output_width as f64, output_height as f64);
                context.rotate(std::f64::consts::PI);
            }
            270 => {
                context.translate(0.0, output_height as f64);
                context.rotate(-std::f64::consts::FRAC_PI_2);
            }
            _ => {}
        }
        context.set_source_surface(&source, 0.0, 0.0)
            .map_err(|error| format!("Could not prepare screenshot image: {error}"))?;
        context.paint()
            .map_err(|error| format!("Could not render screenshot: {error}"))?;
        drop(context);
        output.flush();

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "The home directory could not be determined.".to_string())?;
        let directory = home.join("Pictures").join("Screenshots");
        fs::create_dir_all(&directory)
            .map_err(|error| format!("Could not create {}: {error}", directory.display()))?;

        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|error| format!("System clock is before the Unix epoch: {error}"))?;
        for attempt in 0..1000 {
            let suffix = if attempt == 0 {
                String::new()
            } else {
                format!("-{attempt}")
            };
            let filename = format!(
                "Glimpse-{}-{:09}{}.png",
                timestamp.as_secs(),
                timestamp.subsec_nanos(),
                suffix,
            );
            let path = directory.join(filename);
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!("Could not create {}: {error}", path.display()));
                }
            };

            if let Err(error) = output.write_to_png(&mut file) {
                let _ = fs::remove_file(&path);
                return Err(format!("Could not write screenshot: {error}"));
            }
            if let Err(error) = file.flush() {
                let _ = fs::remove_file(&path);
                return Err(format!("Could not finish writing screenshot: {error}"));
            }
            return Ok(path);
        }

        Err("Could not allocate a unique screenshot filename.".to_string())
    }
}

fn normalize_rotation(degrees: i32) -> i32 {
    degrees.rem_euclid(360)
}

fn screenshot_dimensions(width: i32, height: i32, rotation: i32) -> (i32, i32) {
    if rotation.rem_euclid(180) == 90 {
        (height, width)
    } else {
        (width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_rotation, screenshot_dimensions};

    #[test]
    fn rotation_wraps_in_both_directions() {
        assert_eq!(normalize_rotation(90), 90);
        assert_eq!(normalize_rotation(360), 0);
        assert_eq!(normalize_rotation(-90), 270);
        assert_eq!(normalize_rotation(450), 90);
    }

    #[test]
    fn screenshot_dimensions_follow_display_rotation() {
        assert_eq!(screenshot_dimensions(1920, 1080, 0), (1920, 1080));
        assert_eq!(screenshot_dimensions(1920, 1080, 90), (1080, 1920));
        assert_eq!(screenshot_dimensions(1920, 1080, 180), (1920, 1080));
        assert_eq!(screenshot_dimensions(1920, 1080, 270), (1080, 1920));
    }
}
