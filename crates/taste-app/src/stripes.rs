//! The construction stripes: hazard bands at 45°, drawn across a region
//! from its left edge to a fraction of its width, the rest a flat grey,
//! sliding left with a phase the caller advances. Dark bands and a dark
//! grey under the dark scheme's light text, pale bands and a light grey
//! under the light scheme's dark text (David, 2026-09-22). One drawing
//! for every surface that says "under construction": the startup page
//! wears it whole and slow; a bar would wear it narrow and quicker.

/// The stripes' pitch — one yellow band and one dark, in pixels.
pub const PERIOD: f64 = 28.0;

/// Draw the stripes over `filled` of the width, and the remainder grey.
pub fn draw(
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    fraction: f64,
    offset: f64,
    dark: bool,
) {
    let filled = f64::from(width) * fraction.clamp(0.0, 1.0);
    if height <= 0 || width <= 0 {
        return;
    }
    let h = f64::from(height);
    let (yellow, other, remainder) = if dark {
        (
            (0.22, 0.18, 0.03, 1.0),
            (0.08, 0.08, 0.08, 1.0),
            (0.14, 0.14, 0.14, 1.0),
        )
    } else {
        (
            (1.0, 0.96, 0.80, 1.0),
            (0.99, 0.99, 0.99, 1.0),
            (0.92, 0.92, 0.91, 1.0),
        )
    };
    cr.set_source_rgba(remainder.0, remainder.1, remainder.2, remainder.3);
    cr.rectangle(0.0, 0.0, f64::from(width), h);
    let _ = cr.fill();
    if filled <= 0.5 {
        return;
    }
    cr.save().ok();
    cr.rectangle(0.0, 0.0, filled, h);
    cr.clip();
    let half = PERIOD / 2.0;
    // Each band is a parallelogram leaning left: its top edge `half` wide
    // at `x`, its bottom edge shifted by the height, so the bands run at
    // 45° and a leftward slide of the phase reads as leftward motion.
    let mut x = (offset % PERIOD) - PERIOD - h;
    let mut yellow_band = true;
    while x < filled + h {
        let (r, g, b, a) = if yellow_band { yellow } else { other };
        cr.set_source_rgba(r, g, b, a);
        cr.move_to(x, 0.0);
        cr.line_to(x + half, 0.0);
        cr.line_to(x + half - h, h);
        cr.line_to(x - h, h);
        cr.close_path();
        let _ = cr.fill();
        x += half;
        yellow_band = !yellow_band;
    }
    cr.restore().ok();
}
