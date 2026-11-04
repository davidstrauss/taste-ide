//! Spinners that turn.
//!
//! `GtkSpinner` turned by a CSS animation, and on this desktop it stopped:
//! the chat's working line sat on a frozen arc while everything around it
//! answered (David, 2026-11-03: "The spinner still hangs, even though the
//! UI is responsive"). libadwaita's spinner turns on the frame clock of
//! the widget it is drawn in instead, and is what GNOME's own apps use
//! now. It is drawn here as a `SpinnerPaintable` in a `gtk::Image`, not as
//! an `AdwSpinner`, because an `AdwSpinner` grows to the space it is given,
//! from 16 pixels up to 64, and every spinner here has a size of its own: a
//! file tree row's twelve, a step's mark, a button's face.

/// A spinner `size` pixels square that turns whenever it is on screen:
/// for a place that shows and hides it as a whole — a stack's page, a
/// row that comes and goes, a child added and removed.
pub fn image(size: i32) -> gtk::Image {
    let image = gtk::Image::builder()
        .pixel_size(size)
        .width_request(size)
        .height_request(size)
        .build();
    image.set_paintable(Some(&adw::SpinnerPaintable::new(Some(&image))));
    image
}

/// A spinner that stays where it is and is started and stopped there, as
/// `GtkSpinner` was: stopped, it keeps its square and draws nothing, so
/// nothing beside it moves.
#[derive(Clone)]
pub struct Spinner {
    image: gtk::Image,
    turning: adw::SpinnerPaintable,
}

impl Spinner {
    pub fn new(size: i32) -> Self {
        let image = gtk::Image::builder()
            .pixel_size(size)
            .width_request(size)
            .height_request(size)
            .build();
        let turning = adw::SpinnerPaintable::new(Some(&image));
        Self { image, turning }
    }

    pub fn start(&self) {
        self.image.set_paintable(Some(&self.turning));
    }

    pub fn stop(&self) {
        self.image.set_paintable(gtk::gdk::Paintable::NONE);
    }
}

impl std::ops::Deref for Spinner {
    type Target = gtk::Image;

    fn deref(&self) -> &gtk::Image {
        &self.image
    }
}
