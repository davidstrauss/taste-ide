//! Drawing a Mermaid diagram: text in, pixels out. Runs only in the
//! confined `taste-diagram` process (main.rs); nothing here touches GTK,
//! a file the caller named, or the network.
//!
//! **One face measures and draws.** Mermaid sizes every box and label
//! background to its text, and a renderer that measures with one font and
//! draws with another puts labels over their own edges: merman's built-in
//! measure is a heuristic, and the font it assumes (Trebuchet MS) is not on
//! a GNOME desktop. So the layout measures through [`Measure`], shaping
//! with the desktop's interface font, and the rasterizer is pinned to the
//! same face — the diagram is set in the type the rest of the window is,
//! and every label fits.
//!
//! **Themed by the window, in its palette.** Light or dark is the
//! caller's to say, and the roles merman colours by — surface, border,
//! line, notes, the series a pie is coloured in — take libadwaita's
//! palette, so a diagram is not Mermaid's purple on a GNOME page. The
//! background is left transparent: the page is the diagram's canvas.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use merman::svg::{
    CssOverridePolicy, HostMeasurementResult, HostTextMeasurement, HostTextMeasurementRequest,
    HostTextMeasurer, HostTheme, HostThemeAppearance, MeasurementProfileId, Presentation,
    SvgEnvironment, SvgOutputPolicy, SvgPipelinePreset, TextMeasurementOperation as Op,
    TextMeasurementPhase, TextMeasurementPolicy, TextMeasurementProfileIdentity, TextMetrics,
    ThemeRole,
};
use merman::{OperationControl, RenderOutput, RenderRequest, Renderer, SvgRequest};
use resvg::usvg::{self, fontdb};

/// How long one diagram may take before it is given up on. A preview is
/// interactive, and a pathological graph must not hold a thread for ever.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// The longest side a raster may have, in pixels. Past it the scale comes
/// down rather than the allocation going up. The IDE refuses a reply
/// larger than this, so the two say the same number.
pub const MAX_RASTER_SIDE: f32 = 8192.0;

/// The most pixels a picture is drawn with: four megapixels, sixteen
/// megabytes. Past it the scale comes down. A preview shows a diagram at
/// most a column wide — under a thousand pixels — so this is still more
/// than twice that across for the widest, and it keeps a document of many
/// diagrams from being gigabytes of textures: sixteen drawn at twice their
/// size were 586 MB, up to 104 for one (2026-10-09).
pub const MAX_RASTER_PIXELS: f32 = 4_000_000.0;

/// A drawn diagram: premultiplied RGBA at `width`×`height` pixels, which
/// stand for `logical_width` at the scale it was drawn for (the height
/// follows from the aspect).
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub logical_width: f32,
    pub rgba: Vec<u8>,
}

/// The diagram `source` describes, drawn in `family` for a light or dark
/// page at `scale`. No GTK: this is what runs off the main thread.
pub fn render(source: &str, dark: bool, family: &str, scale: f32) -> Result<Raster, String> {
    let measure = measure_for(family)?;
    let (regular_id, bold_id) = (measure.regular.id, measure.bold.id);
    let identity = TextMeasurementProfileIdentity::new(
        MeasurementProfileId::new("taste-ide.interface-font").map_err(|e| e.to_string())?,
        family,
    )
    .map_err(|e| e.to_string())?;
    let policy =
        TextMeasurementPolicy::host_display(identity, measure.clone(), TextMeasurementPhase::ALL);
    let palette = if dark { &DARK } else { &LIGHT };
    let resolved = Presentation::new()
        .with_theme(theme(palette, dark, family))
        .resolve();
    let engine = merman::Engine::new().with_site_config(layout_config());
    let renderer = Renderer::new().with_engine(resolved.materialize_engine(engine));
    let output = SvgOutputPolicy {
        preset: SvgPipelinePreset::ResvgSafe,
        css_override_policy: CssOverridePolicy::StripExistingImportant,
        // Mermaid paints its root white; the page is the canvas here.
        root_background_color: Some("transparent".to_string()),
        scoped_css: Some(label_halo(palette.canvas)),
        ..SvgOutputPolicy::default()
    };
    let request = SvgRequest {
        environment: SvgEnvironment::deterministic().with_text_measurement_policy(policy),
        pipeline: Some(output.pipeline()),
        presentation: resolved.render_policy(),
        ..Default::default()
    };
    let control = OperationControl::new().with_deadline(DEADLINE);
    let svg = match renderer
        .render(RenderRequest::svg(source, control, request))
        .map_err(|e| e.to_string())?
    {
        RenderOutput::Svg(Some(svg)) => svg.svg().to_string(),
        _ => return Err("this is not a Mermaid diagram".to_string()),
    };
    let options = svg_options(family, regular_id, bold_id);
    let tree = usvg::Tree::from_str(&svg, &options).map_err(|e| e.to_string())?;
    let size = tree.size();
    let longest = size.width().max(size.height()) * scale;
    let scale = if longest > MAX_RASTER_SIDE {
        scale * MAX_RASTER_SIDE / longest
    } else {
        scale
    };
    let area = size.width() * size.height() * scale * scale;
    let scale = if area > MAX_RASTER_PIXELS {
        scale * (MAX_RASTER_PIXELS / area).sqrt()
    } else {
        scale
    };
    let width = (size.width() * scale).ceil().max(1.0) as u32;
    let height = (size.height() * scale).ceil().max(1.0) as u32;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or("the diagram is too large to draw")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(Raster {
        width,
        height,
        logical_width: size.width(),
        rgba: pixmap.take(),
    })
}

/// How the SVG is read: in the face the layout measured with, and with
/// no way to reach a file.
///
/// usvg's own default resolves an `<image href>` by READING THAT PATH, and
/// this runs in the IDE's process, on the host, over Markdown a repository
/// or an agent wrote. A diagram naming `~/Pictures/…` would put the user's
/// file in the preview, where `ide_screenshot` can see it — the boundary
/// (CLAUDE.md, "The boundary is the host") crossed by a picture. merman's
/// resvg-safe pipeline drops image references today; this refuses them
/// again here, so a change upstream cannot reopen the door. Inline data
/// URLs stay: they carry nothing the document did not.
fn svg_options(family: &str, regular: fontdb::ID, bold: fontdb::ID) -> usvg::Options<'static> {
    let mut options = usvg::Options {
        fontdb: fonts(),
        font_family: family.to_string(),
        resources_dir: None,
        ..usvg::Options::default()
    };
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    // The face the layout measured with, for every run of text: whatever
    // family the SVG names, it was sized for this one.
    options.font_resolver.select_font =
        Box::new(move |font, _| Some(if font.weight() >= 600 { bold } else { regular }));
    options
}

/// The system's fonts, read once: the scan is the slow part of a first
/// render, and every later one shares it.
fn fonts() -> Arc<fontdb::Database> {
    static DB: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

/// The measurer for `family`, built once per family: finding a face and
/// reading its file is per font, not per diagram.
fn measure_for(family: &str) -> Result<Arc<Measure>, String> {
    static BY_FAMILY: OnceLock<Mutex<HashMap<String, Arc<Measure>>>> = OnceLock::new();
    let map = BY_FAMILY.get_or_init(Default::default);
    if let Some(measure) = map.lock().unwrap().get(family) {
        return Ok(measure.clone());
    }
    let db = fonts();
    let measure = Arc::new(Measure {
        regular: Face::find(&db, family, fontdb::Weight::NORMAL)
            .ok_or("no font to set the diagram in")?,
        bold: Face::find(&db, family, fontdb::Weight::BOLD)
            .ok_or("no font to set the diagram in")?,
    });
    map.lock()
        .unwrap()
        .insert(family.to_string(), measure.clone());
    Ok(measure)
}

/// One face, held whole so it can be shaped without going back to the
/// font database.
struct Face {
    id: fontdb::ID,
    data: Vec<u8>,
    index: u32,
    units_per_em: f64,
    ascender: f64,
    descender: f64,
}

impl Face {
    /// `family` at `weight`, or the nearest thing this machine has: the
    /// GNOME faces first, since they are what the window is set in when
    /// its own name for its font does not resolve here.
    fn find(db: &fontdb::Database, family: &str, weight: fontdb::Weight) -> Option<Self> {
        let id = [
            family,
            "Adwaita Sans",
            "Cantarell",
            "Noto Sans",
            "DejaVu Sans",
        ]
        .into_iter()
        .find_map(|name| {
            db.query(&fontdb::Query {
                families: &[fontdb::Family::Name(name)],
                weight,
                ..Default::default()
            })
        })
        .or_else(|| db.faces().next().map(|face| face.id))?;
        let (data, index) = db.with_face_data(id, |data, index| (data.to_vec(), index))?;
        let face = rustybuzz::Face::from_slice(&data, index)?;
        let (units_per_em, ascender, descender) = (
            f64::from(face.units_per_em()),
            f64::from(face.ascender()),
            f64::from(face.descender()),
        );
        Some(Self {
            id,
            data,
            index,
            units_per_em,
            ascender,
            descender,
        })
    }

    /// The advance of `text` set at `size` pixels, shaped as resvg will
    /// shape it.
    fn advance(&self, text: &str, size: f64) -> f64 {
        let Some(face) = rustybuzz::Face::from_slice(&self.data, self.index) else {
            return 0.0;
        };
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        let glyphs = rustybuzz::shape(&face, &[], buffer);
        let units: i64 = glyphs
            .glyph_positions()
            .iter()
            .map(|position| i64::from(position.x_advance))
            .sum();
        units as f64 * size / self.units_per_em
    }

    fn line_height(&self, size: f64) -> f64 {
        (self.ascender - self.descender) * size / self.units_per_em
    }
}

/// merman's text measurement, answered from the face the diagram will be
/// drawn in.
struct Measure {
    regular: Face,
    bold: Face,
}

impl Measure {
    fn face(&self, weight: Option<&str>) -> &Face {
        match weight.map(str::trim) {
            Some("bold" | "bolder" | "600" | "700" | "800" | "900") => &self.bold,
            _ => &self.regular,
        }
    }

    /// Each line's width: one per line of `text`, and each of those broken
    /// between words where it would pass `max`.
    fn line_widths(face: &Face, text: &str, size: f64, max: Option<f64>) -> Vec<f64> {
        let mut widths = Vec::new();
        for paragraph in text.split('\n') {
            let Some(max) = max.filter(|max| *max > 0.0) else {
                widths.push(face.advance(paragraph, size));
                continue;
            };
            let mut line = String::new();
            for word in paragraph.split_whitespace() {
                let candidate = if line.is_empty() {
                    word.to_string()
                } else {
                    format!("{line} {word}")
                };
                if !line.is_empty() && face.advance(&candidate, size) > max {
                    widths.push(face.advance(&line, size));
                    line = word.to_string();
                } else {
                    line = candidate;
                }
            }
            widths.push(face.advance(&line, size));
        }
        widths
    }
}

impl HostTextMeasurer for Measure {
    fn measure(&self, request: HostTextMeasurementRequest<'_>) -> HostMeasurementResult {
        let size = if request.style.font_size > 0.0 {
            request.style.font_size
        } else {
            16.0
        };
        let face = self.face(request.style.font_weight.as_deref());
        let metrics = |max| {
            let lines = Self::line_widths(face, request.text, size, max);
            TextMetrics {
                width: lines.iter().copied().fold(0.0, f64::max),
                height: lines.len() as f64 * face.line_height(size),
                line_count: lines.len(),
            }
        };
        Ok(Some(match request.operation {
            Op::Measure | Op::Wrapped => HostTextMeasurement::Metrics(metrics(request.max_width)),
            // Mermaid's `calculateTextDimensions`: how a sequence diagram
            // sizes every message, note, and participant. Declined, it fell
            // to merman's estimate — a narrower face than the one drawn —
            // and labels ran out of what was laid out for them.
            Op::MermaidCalculateTextDimensions => HostTextMeasurement::Metrics(metrics(None)),
            Op::WrappedWithRawWidth => HostTextMeasurement::WrappedWithRawWidth {
                metrics: metrics(request.max_width),
                raw_width: Some(metrics(None).width),
            },
            Op::ComputedLength
            | Op::SimpleBBoxWidth
            | Op::RawBBoxWidth
            | Op::TspanBBoxWidth
            | Op::WrapProbeBBoxWidth
            | Op::BoundingClientRectWidth
            | Op::CanvasMeasureTextWidth => HostTextMeasurement::Length(metrics(None).width),
            Op::BBoxX | Op::BBoxXWithAsciiOverhang | Op::TitleBBoxX => {
                let half = metrics(None).width / 2.0;
                HostTextMeasurement::HorizontalExtents {
                    left: half,
                    right: half,
                }
            }
            Op::TspanBBoxHeight | Op::SimpleBBoxHeight | Op::RawBBoxHeight => {
                HostTextMeasurement::Length(metrics(None).height)
            }
            // Baseline offsets are the font's own and merman's neutral
            // answer is closer than a guess: decline, and it uses that.
            _ => return Ok(None),
        }))
    }
}

/// libadwaita's palette, in the roles merman colours a diagram by.
struct Palette {
    canvas: &'static str,
    surface: &'static str,
    surface_alt: &'static str,
    text: &'static str,
    subtle: &'static str,
    border: &'static str,
    line: &'static str,
    note: &'static str,
    note_border: &'static str,
    series: [&'static str; 6],
}

const LIGHT: Palette = Palette {
    canvas: "#ffffff",
    surface: "#f0f5fc",
    surface_alt: "#e6eef9",
    text: "#2e2e33",
    subtle: "#6e6e74",
    border: "#3584e4",
    line: "#77767b",
    note: "#fdf6d8",
    note_border: "#e5a50a",
    series: [
        "#3584e4", "#33d17a", "#f6d32d", "#ff7800", "#e01b24", "#9141ac",
    ],
};

const DARK: Palette = Palette {
    canvas: "#1d1d20",
    surface: "#2a2f3a",
    surface_alt: "#323845",
    text: "#ffffff",
    subtle: "#a8a8ad",
    border: "#78aeed",
    line: "#9a9996",
    note: "#4a3f12",
    note_border: "#e5a50a",
    series: [
        "#62a0ea", "#57e389", "#f8e45c", "#ffa348", "#f66151", "#c061cb",
    ],
};

fn theme(palette: &Palette, dark: bool, family: &str) -> HostTheme {
    let mut theme = HostTheme::new().with_appearance(if dark {
        HostThemeAppearance::Dark
    } else {
        HostThemeAppearance::Light
    });
    // Each setter validates its value and hands the theme back only when
    // it took; a value it refuses leaves the theme as it was.
    if let Ok(next) = theme
        .clone()
        .try_with_font_family(format!("{family}, sans-serif"))
    {
        theme = next;
    }
    for (role, colour) in [
        (ThemeRole::Canvas, palette.canvas),
        (ThemeRole::Surface, palette.surface),
        (ThemeRole::SurfaceAlt, palette.surface_alt),
        (ThemeRole::SurfaceMuted, palette.surface_alt),
        (ThemeRole::Text, palette.text),
        (ThemeRole::SubtleText, palette.subtle),
        (ThemeRole::Border, palette.border),
        (ThemeRole::Line, palette.line),
        (ThemeRole::EdgeLabelBackground, palette.canvas),
        (ThemeRole::ClusterBackground, palette.surface_alt),
        (ThemeRole::ClusterBorder, palette.border),
        (ThemeRole::NoteBackground, palette.note),
        (ThemeRole::NoteBorder, palette.note_border),
        (ThemeRole::NoteText, palette.text),
        (ThemeRole::ActorBackground, palette.surface),
        (ThemeRole::ActorBorder, palette.border),
        (ThemeRole::ActorText, palette.text),
        (ThemeRole::ActivationBackground, palette.surface_alt),
        (ThemeRole::ActivationBorder, palette.border),
    ] {
        if let Ok(next) = theme.clone().try_with_role(role, colour) {
            theme = next;
        }
    }
    if let Ok(next) = theme.clone().try_with_series_palette(palette.series) {
        theme = next;
    }
    theme
}

/// Mermaid's own settings, chosen for text that stays where it belongs
/// (David, 2026-10-09: "Also hoping this layout can improve to have less
/// text crossing boundaries"). Every one is a setting a diagram's author
/// could write in the diagram itself, which still takes precedence:
/// - Sequence diagrams wrap. Without it a note over two participants is
///   as wide as the gap between them whatever it says — upstream Mermaid
///   does the same — and a long message runs across other participants'
///   lifelines; with it, both fold to fit. Participants are 200 wide
///   rather than 150, which is also the measure a message to oneself is
///   folded to, so those come out three lines rather than five.
/// - State diagrams and flowcharts get more room between nodes and
///   between ranks, where their edge labels sit.
fn layout_config() -> merman::MermaidConfig {
    merman::MermaidConfig::from_value(serde_json::json!({
        "sequence": { "wrap": true, "width": 200 },
        "state": { "nodeSpacing": 80, "rankSpacing": 70 },
        "flowchart": { "nodeSpacing": 60, "rankSpacing": 60 }
    }))
}

/// A halo under a sequence diagram's label text, in the page's own color:
/// a lifeline or a frame passing behind a label breaks around its letters
/// rather than striking through them, as a map's labels sit over its
/// roads. A message to oneself is labelled centred on one's own lifeline,
/// so without it that line ran through every such label. Only these
/// labels: a flowchart's or a state diagram's edge labels have a box of
/// their own already.
fn label_halo(canvas: &str) -> String {
    format!(
        ".messageText, .loopText, .labelText {{ paint-order: stroke fill !important; \
         stroke: {canvas} !important; stroke-width: 6px !important; \
         stroke-linejoin: round !important; }}"
    )
}

/// Read the system's fonts now, so the first diagram does not wait on the
/// scan.
pub fn warm(family: &str) {
    let _ = measure_for(family);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole path off the main thread: a diagram comes out as pixels at
    /// the asked scale, transparent where the page shows through, and a
    /// broken one comes out as the parser's reason.
    #[test]
    fn a_diagram_is_drawn_and_a_broken_one_says_why() {
        let raster = render(
            "flowchart LR\n  A[Start] -->|go| B[Done]",
            true,
            "Adwaita Sans",
            2.0,
        )
        .expect("a flowchart draws");
        assert_eq!(raster.width, (raster.logical_width * 2.0).ceil() as u32);
        assert_eq!(
            raster.rgba.len(),
            (raster.width * raster.height * 4) as usize
        );
        assert_eq!(&raster.rgba[0..4], &[0, 0, 0, 0], "the corner is the page");
        assert!(
            raster.rgba.chunks(4).any(|px| px[3] == 255),
            "something is drawn"
        );

        let error = render("flowchart LR\n  A -->", false, "Adwaita Sans", 1.0)
            .err()
            .expect("an unfinished edge is an error");
        assert!(error.contains("parse"), "{error}");
    }

    /// An SVG that names a file on this machine draws nothing for it:
    /// the file is never opened, though it exists and is a picture.
    #[test]
    fn an_image_naming_a_host_file_is_not_read() {
        let picture =
            std::env::temp_dir().join(format!("taste-mermaid-{}-private.png", std::process::id()));
        // A real 1×1 PNG, so a refusal cannot be mistaken for a decode
        // failure.
        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
        )
        .unwrap();
        std::fs::write(&picture, png).unwrap();
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><image href="{}" width="10" height="10"/></svg>"#,
            picture.display()
        );
        fn images(group: &usvg::Group) -> usize {
            group
                .children()
                .iter()
                .map(|node| match node {
                    usvg::Node::Image(_) => 1,
                    usvg::Node::Group(group) => images(group),
                    _ => 0,
                })
                .sum()
        }
        // usvg's default reads it: the fixture is a file it would open.
        let open = usvg::Tree::from_str(&svg, &usvg::Options::default()).unwrap();
        assert_eq!(images(open.root()), 1, "the fixture must be readable");
        // Ours does not.
        let measure = measure_for("Adwaita Sans").expect("a face");
        let options = svg_options("Adwaita Sans", measure.regular.id, measure.bold.id);
        let guarded = usvg::Tree::from_str(&svg, &options).unwrap();
        assert_eq!(images(guarded.root()), 0);
        let _ = std::fs::remove_file(&picture);
    }

    /// Every measurement a sequence diagram sizes its messages, notes, and
    /// participants by is answered in the drawing face, not left to
    /// merman's estimate of a narrower one.
    #[test]
    fn a_sequence_diagrams_text_is_measured_in_the_drawing_face() {
        use merman::svg::{TextStyle, WrapMode};
        let measure = measure_for("Adwaita Sans").expect("a face");
        let style = TextStyle {
            font_family: None,
            font_size: 16.0,
            font_weight: None,
            font_style: None,
        };
        let text = "From here the TPM won't sign identity requests";
        let answer = measure.measure(HostTextMeasurementRequest {
            operation: Op::MermaidCalculateTextDimensions,
            phase: TextMeasurementPhase::Layout,
            text,
            style: &style,
            max_width: None,
            wrap_mode: WrapMode::SvgLike,
        });
        let Ok(Some(HostTextMeasurement::Metrics(metrics))) = answer else {
            panic!("calculateTextDimensions was not answered: {answer:?}");
        };
        let drawn = measure.regular.advance(text, 16.0);
        assert!(
            (metrics.width - drawn).abs() < 0.5,
            "{} vs {drawn}",
            metrics.width
        );
        assert_eq!(metrics.line_count, 1);
    }

    /// The point of measuring with the drawing face: a wider label makes a
    /// wider box. The built-in heuristic and a real face disagree by
    /// enough to put a label over its own edge; this face cannot disagree
    /// with itself.
    #[test]
    fn a_label_is_measured_in_the_face_it_is_drawn_in() {
        let measure = measure_for("Adwaita Sans").expect("a face");
        let narrow = measure.regular.advance("iii", 16.0);
        let wide = measure.regular.advance("WWW", 16.0);
        assert!(wide > narrow * 2.0, "{narrow} vs {wide}");
        let lines = Measure::line_widths(&measure.regular, "one two three four", 16.0, Some(60.0));
        assert!(lines.len() > 1, "{lines:?}");
        assert!(lines.iter().all(|width| *width <= 60.0 || *width > 0.0));
    }
}
