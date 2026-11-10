//! Static BT.2020/PQ test pattern for validating GTK's HDR color-state path.
//!
//! This intentionally uses a 16-bit opaque RGB memory texture. It validates
//! GDK's PQ color-state handling independently of FFmpeg and the playback
//! scheduler. It does not yet validate DMA-BUF import or direct-scanout
//! offloading; those are the next diagnostic step.

use gdk4::{ColorState, MemoryFormat, MemoryTextureBuilder};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, GraphicsOffload,
    GraphicsOffloadEnabled, Label, Orientation, Picture,
};

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const LEVELS_NITS: [f64; 14] = [
    0.1, 1.0, 5.0, 10.0, 25.0, 50.0, 75.0,
    100.0, 203.0, 400.0, 600.0, 1000.0, 2000.0, 4000.0,
];

/// SMPTE ST 2084 (PQ) OETF: absolute display luminance in cd/m² to [0, 1].
fn pq_encode(nits: f64) -> u16 {
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;

    let luminance = (nits.clamp(0.0, 10_000.0) / 10_000.0).powf(m1);
    let encoded = ((c1 + c2 * luminance) / (1.0 + c3 * luminance)).powf(m2);
    (encoded * u16::MAX as f64).round() as u16
}

fn build_pq_pattern() -> gdk4::Texture {
    let mut pixels = Vec::with_capacity(WIDTH * HEIGHT * 6);
    for _y in 0..HEIGHT {
        for x in 0..WIDTH {
            let band = (x * LEVELS_NITS.len() / WIDTH).min(LEVELS_NITS.len() - 1);
            let value = pq_encode(LEVELS_NITS[band]);
            pixels.extend_from_slice(&value.to_ne_bytes());
            pixels.extend_from_slice(&value.to_ne_bytes());
            pixels.extend_from_slice(&value.to_ne_bytes());
        }
    }

    let bytes = glib::Bytes::from_owned(pixels);
    let pq = ColorState::rec2100_pq();

    MemoryTextureBuilder::new()
        .set_bytes(Some(&bytes))
        .set_color_state(&pq)
        .set_format(MemoryFormat::R16g16b16)
        .set_width(WIDTH as i32)
        .set_height(HEIGHT as i32)
        .set_stride(WIDTH * 6)
        .build()
}

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id("com.github.Mel34.Glimpse.HDRProbe")
        .build();

    app.connect_activate(|app| {
        let texture = build_pq_pattern();

        let picture = Picture::new();
        picture.set_paintable(Some(&texture));
        picture.set_content_fit(gtk4::ContentFit::Contain);
        picture.set_hexpand(true);
        picture.set_vexpand(true);

        let offload = GraphicsOffload::new(Some(&picture));
        offload.set_enabled(GraphicsOffloadEnabled::Enabled);
        offload.set_hexpand(true);
        offload.set_vexpand(true);

        let levels = LEVELS_NITS
            .iter()
            .map(|level| format!("{level}"))
            .collect::<Vec<_>>()
            .join("  ·  ");
        let explanation = Label::new(Some(&format!(
            "BT.2020 / ST 2084 PQ · left to right: {levels} cd/m²\n             16-bit RGB texture · GTK graphics offload requested\n             This is a color-pipeline probe, not yet a DMA-BUF/direct-scanout test."
        )));
        explanation.set_wrap(true);
        explanation.set_selectable(true);

        let content = GtkBox::new(Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&offload);
        content.append(&explanation);

        let window = ApplicationWindow::builder()
            .application(app)
            .title("Glimpse — HDR / PQ probe")
            .default_width(1200)
            .default_height(760)
            .child(&content)
            .build();
        window.present();

        println!(
            "Created Rec. 2100 PQ texture: {}x{}",
            texture.width(),
            texture.height()
        );
    });

    app.run()
}

#[cfg(test)]
mod tests {
    use super::pq_encode;

    #[test]
    fn pq_reference_codes_are_close_to_st2084_values() {
        // Reference PQ code values, allowing for 16-bit quantization.
        assert!((pq_encode(100.0) as i32 - 33_297).abs() <= 1);
        assert!((pq_encode(203.0) as i32 - 38_055).abs() <= 1);
        assert_eq!(pq_encode(10_000.0), u16::MAX);
    }
}
