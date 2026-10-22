//! **A place in an artifact**, named the way a URI names one: a reference's
//! path says which file, and its fragment says where in it, in that kind
//! of file's own convention (David, 2026-10-21: "Can we make it so agents
//! can link me to specific slides/pages of a PDF? … a general artifact
//! reference model, maybe as URIs?").
//!
//! An agent links the user to a place the way any Markdown does —
//! `[the call](src/main.rs#L42)`, `[slide 3](deck/main.pdf#page=3)` — and
//! a click opens the file there; `ide_open_file` takes the same places.
//!
//! - **A line** of a text file: `#L42` (GitHub's), `#42`, or `:42` after
//!   the path, as compilers print a location.
//! - **A page** of a PDF: `#page=3`, RFC 8118's fragment identifier for
//!   PDF, which browsers and PDF viewers read the same way; its other
//!   parameters (`&zoom=…`) are passed over.
//!
//! A kind of place not here yet — a notebook's cell, a time in a video —
//! joins as a variant, with its own convention's fragment.

/// Where in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// A 1-based line.
    Line(u32),
    /// A 1-based page.
    Page(u32),
}

impl Anchor {
    /// A URI fragment (without its `#`) as a place: `L12` or `12` a line,
    /// `page=3` — among `&`-joined parameters — a page. `None` for a
    /// fragment that names neither, a heading's for one.
    pub fn from_fragment(fragment: &str) -> Option<Self> {
        for param in fragment.split('&') {
            if let Some(page) = param.strip_prefix("page=") {
                return page.parse().ok().filter(|n| *n > 0).map(Anchor::Page);
            }
        }
        fragment
            .strip_prefix('L')
            .unwrap_or(fragment)
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .map(Anchor::Line)
    }

    /// The line, for a place that is one.
    pub fn line(self) -> Option<u32> {
        match self {
            Anchor::Line(line) => Some(line),
            Anchor::Page(_) => None,
        }
    }

    /// The fragment that names this place, without its `#`.
    pub fn fragment(self) -> String {
        match self {
            Anchor::Line(line) => format!("L{line}"),
            Anchor::Page(page) => format!("page={page}"),
        }
    }
}

/// A reference split into its path and the place in it: `path#fragment`,
/// or `path:12` for a line. A fragment that names no place leaves the
/// path whole and no place.
pub fn split_reference(reference: &str) -> (&str, Option<Anchor>) {
    if let Some((path, fragment)) = reference.split_once('#') {
        return (path, Anchor::from_fragment(fragment));
    }
    match reference.rsplit_once(':') {
        Some((path, digits))
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) =>
        {
            (
                path,
                digits.parse().ok().filter(|n| *n > 0).map(Anchor::Line),
            )
        }
        _ => (reference, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fragment_names_a_line_or_a_page() {
        assert_eq!(Anchor::from_fragment("L42"), Some(Anchor::Line(42)));
        assert_eq!(Anchor::from_fragment("42"), Some(Anchor::Line(42)));
        assert_eq!(Anchor::from_fragment("page=3"), Some(Anchor::Page(3)));
        assert_eq!(
            Anchor::from_fragment("zoom=150&page=7"),
            Some(Anchor::Page(7)),
            "RFC 8118's other parameters are passed over"
        );
        assert_eq!(Anchor::from_fragment("heading"), None);
        assert_eq!(Anchor::from_fragment("page=0"), None);
        assert_eq!(Anchor::from_fragment("L0"), None);
    }

    #[test]
    fn a_reference_splits_into_its_path_and_its_place() {
        assert_eq!(
            split_reference("src/main.rs#L42"),
            ("src/main.rs", Some(Anchor::Line(42)))
        );
        assert_eq!(
            split_reference("src/main.rs:42"),
            ("src/main.rs", Some(Anchor::Line(42)))
        );
        assert_eq!(
            split_reference("deck/main.pdf#page=3"),
            ("deck/main.pdf", Some(Anchor::Page(3)))
        );
        assert_eq!(split_reference("README.md"), ("README.md", None));
        assert_eq!(split_reference("README.md#usage"), ("README.md", None));
        for anchor in [Anchor::Line(9), Anchor::Page(4)] {
            assert_eq!(Anchor::from_fragment(&anchor.fragment()), Some(anchor));
        }
    }
}
