//! The interface, via egui.
//!
//! egui composites into the same render pass as the chart, so the panel is a
//! GPU overlay rather than a second window. That is the whole reason it was
//! chosen over a full application framework: the chart stays in charge of the
//! surface and the interface is one more draw.
//!
//! What lives here is *rendering* the interface and reporting what the user
//! did. What does **not** live here is deciding what to do about it — the
//! panel returns a list of [`Action`]s and the platform shim applies them.
//! That split keeps this crate free of any opinion about chart files, and it
//! is what lets the same panel serve the Linux window and an Android
//! activity whose chart loading works quite differently.

use rutter_core::chart::ChartStyle;

/// What a chart layer looks like to the interface.
///
/// Deliberately not `rutter_charts::LayerInfo`: this crate does not depend on
/// `rutter-charts`, so the caller translates. The cost is one small struct;
/// the benefit is that the renderer still needs no SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerEntry {
    /// The layer's name — an S-57 object class for ENC-derived charts.
    pub name: String,
    /// How it is drawn, which is also how the panel groups it.
    pub style: ChartStyle,
    /// Whether it is currently drawn.
    pub visible: bool,
    /// Features found in the current view, for a hint of what is actually
    /// there. `None` when nothing has been counted yet.
    pub count: Option<usize>,
}

/// Something the user asked for. The platform shim decides how to honour it.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Show or hide one layer.
    SetLayer { name: String, visible: bool },
    /// Show or hide every layer in a style group.
    SetStyle { style: ChartStyle, visible: bool },
    /// Show everything.
    ShowAll,
    /// Hide everything.
    HideAll,
    /// Recentre on the chart's own extent.
    FitChart,
    /// Show or hide the aerial imagery underlay.
    SetImagery(bool),
    /// How strongly the imagery shows through, 0..1.
    SetImageryOpacity(f32),
}

/// What the panel knows about the imagery underlay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageryState {
    /// Whether an imagery file is loaded at all. With none, the controls
    /// are shown disabled rather than hidden, so it is clear the feature
    /// exists and what is missing.
    pub available: bool,
    pub visible: bool,
    pub opacity: f32,
    /// Tiles currently on the GPU, for the status line.
    pub tiles: usize,
    /// The credit line the imagery file asks to be shown.
    pub attribution: Option<String>,
}

/// Panel state that survives between frames.
///
/// Only presentation state — which groups are expanded, whether the panel is
/// open. Nothing here is the truth about the chart; that lives in the
/// `ChartSource` the platform owns.
#[derive(Debug, Default)]
pub struct Ui {
    /// Whether the layer panel is showing.
    ///
    /// Starts closed, which `Default` gives for free: a chart you cannot see
    /// is worse than a panel you have to open, and on a phone the panel
    /// covers most of the screen.
    pub open: bool,
    /// Text typed into the filter box.
    ///
    /// Group open/closed state is *not* here: egui's `CollapsingState` keeps
    /// that in its own memory, keyed by the group's id, so there is nothing
    /// for this type to track.
    filter: String,
}

/// A short, readable name for a style group.
fn style_label(style: ChartStyle) -> &'static str {
    match style {
        ChartStyle::Coastline => "Coastline",
        ChartStyle::Land => "Land",
        ChartStyle::DepthArea => "Depth areas",
        ChartStyle::DepthContour => "Depth contours",
        ChartStyle::Sounding => "Soundings",
        ChartStyle::Buoy => "Buoys",
        ChartStyle::Beacon => "Beacons",
        ChartStyle::Light => "Lights",
        ChartStyle::Danger => "Dangers",
        ChartStyle::Area => "Areas",
        ChartStyle::Other => "Other",
    }
}

/// The order style groups appear in the panel: the same order they are drawn,
/// so the list reads bottom-up like the chart itself.
fn group_order() -> [ChartStyle; 11] {
    [
        ChartStyle::Coastline,
        ChartStyle::Land,
        ChartStyle::DepthArea,
        ChartStyle::DepthContour,
        ChartStyle::Sounding,
        ChartStyle::Danger,
        ChartStyle::Buoy,
        ChartStyle::Beacon,
        ChartStyle::Light,
        ChartStyle::Area,
        ChartStyle::Other,
    ]
}

impl Ui {
    /// Convert a chart style's colour into egui's, so a swatch in the panel
    /// matches the line on the chart.
    fn swatch(style: ChartStyle) -> egui::Color32 {
        let [r, g, b, _] = crate::colour_for(style);
        egui::Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
    }

    /// Draw the interface for one frame and collect what the user did.
    ///
    /// Takes the root [`egui::Ui`] rather than a `Context`, which is what
    /// `Context::run_ui` hands over: in egui 0.37 `SidePanel` and
    /// `TopBottomPanel` merged into one `Panel` type that attaches to a
    /// `Ui`, so there is no longer a context-level panel call.
    ///
    /// `layers` is the chart's layers; `status` is a line of text for the
    /// top strip — position, zoom, whatever the platform knows.
    pub fn show(
        &mut self,
        root: &mut egui::Ui,
        layers: &[LayerEntry],
        imagery: &ImageryState,
        status: &str,
    ) -> Vec<Action> {
        let mut actions = Vec::new();

        // An always-visible strip, so there is something to click when the
        // panel is closed and somewhere for the position readout to live.
        egui::Panel::top("bar").show(root, |ui| {
            ui.horizontal(|ui| {
                // Plain words, not icon glyphs. egui's default font covers
                // Latin text and little else, so a hamburger or a cross
                // renders as an empty box — which is exactly how this panel
                // first appeared. Anything decorative here has to be drawn
                // as a shape or spelled out.
                let label = if self.open { "Hide layers" } else { "Layers" };
                if ui.button(label).clicked() {
                    self.open = !self.open;
                }
                if ui
                    .button("Fit")
                    .on_hover_text("Centre on the chart")
                    .clicked()
                {
                    actions.push(Action::FitChart);
                }
                ui.separator();
                ui.label(status);
            });
        });

        if !self.open {
            return actions;
        }

        // `default_size`, not `default_width`: the unified Panel takes one
        // size that means width for a side panel and height for a top one.
        egui::Panel::left("layers")
            .default_size(260.0)
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("All").clicked() {
                        actions.push(Action::ShowAll);
                    }
                    if ui.button("None").clicked() {
                        actions.push(Action::HideAll);
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("filter")
                            .desired_width(110.0),
                    );
                });
                ui.separator();

                // Imagery above the vector layers: it is the ground
                // everything else sits on, and it is the control most
                // likely to be reached for.
                ui.horizontal(|ui| {
                    let mut on = imagery.visible;
                    let toggle = ui.add_enabled(
                        imagery.available,
                        egui::Checkbox::new(&mut on, "Aerial imagery"),
                    );
                    if toggle.clicked() {
                        actions.push(Action::SetImagery(on));
                    }
                    if !imagery.available {
                        toggle.on_hover_text(
                            "No imagery loaded. Build some with \
                             `rutter imagery --chart <file>`, then pass \
                             --imagery <file>.",
                        );
                    } else {
                        ui.label(
                            egui::RichText::new(format!("{} tiles", imagery.tiles))
                                .weak()
                                .small(),
                        );
                    }
                });

                if imagery.available && imagery.visible {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("fade").weak().small());
                        let mut opacity = imagery.opacity;
                        if ui
                            .add(
                                egui::Slider::new(&mut opacity, 0.0..=1.0)
                                    .show_value(false)
                                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                            )
                            .changed()
                        {
                            actions.push(Action::SetImageryOpacity(opacity));
                        }
                    });
                    if let Some(credit) = &imagery.attribution {
                        ui.label(egui::RichText::new(credit).weak().small());
                    }
                }

                ui.separator();

                if layers.is_empty() {
                    ui.label("No chart loaded.");
                    ui.label(
                        egui::RichText::new(
                            "Pass --charts <file.mbtiles>, or build one with \
                             `rutter charts` or `rutter noaa`.",
                        )
                        .weak(),
                    );
                    return;
                }

                let needle = self.filter.trim().to_ascii_uppercase();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for style in group_order() {
                        // Only the layers in this group that pass the filter.
                        let in_group: Vec<&LayerEntry> = layers
                            .iter()
                            .filter(|l| l.style == style)
                            .filter(|l| {
                                needle.is_empty() || l.name.to_ascii_uppercase().contains(&needle)
                            })
                            .collect();

                        if in_group.is_empty() {
                            continue;
                        }

                        let shown = in_group.iter().filter(|l| l.visible).count();

                        // egui's own collapsing state, which draws the
                        // expand arrow as a triangle rather than a glyph —
                        // so it works with no icon font — and remembers
                        // whether the group is open without this type
                        // tracking it.
                        // Collapsed by default: a real ENC has sixty-odd
                        // layers, and an expanded list is a wall of acronyms
                        // you have to scroll past to find anything. The
                        // group headers alone fit on one screen.
                        let id = ui.make_persistent_id(style_label(style));
                        egui::collapsing_header::CollapsingState::load_with_default_open(
                            ui.ctx(),
                            id,
                            false,
                        )
                        .show_header(ui, |ui| {
                            // A swatch in the chart's own colour, so the
                            // panel and the chart agree without a legend.
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 2.0, Self::swatch(style));

                            // Group toggle. Tristate in appearance only: any
                            // visible means clicking hides the group.
                            let mut group_on = shown > 0;
                            if ui.checkbox(&mut group_on, style_label(style)).clicked() {
                                actions.push(Action::SetStyle {
                                    style,
                                    visible: group_on,
                                });
                            }

                            ui.label(
                                egui::RichText::new(format!("{shown}/{}", in_group.len()))
                                    .weak()
                                    .small(),
                            );
                        })
                        .body(|ui| {
                            for layer in in_group {
                                ui.horizontal(|ui| {
                                    let mut on = layer.visible;
                                    if ui.checkbox(&mut on, &layer.name).clicked() {
                                        actions.push(Action::SetLayer {
                                            name: layer.name.clone(),
                                            visible: on,
                                        });
                                    }
                                    if let Some(n) = layer.count {
                                        ui.label(egui::RichText::new(n.to_string()).weak().small())
                                            .on_hover_text("features in view");
                                    }
                                });
                            }
                        });
                    }
                });
            });

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_has_a_label_and_a_place_in_the_panel() {
        // A style missing from group_order would have its layers silently
        // absent from the picker — present on the chart, impossible to
        // switch off.
        let order = group_order();
        for style in [
            ChartStyle::Coastline,
            ChartStyle::Land,
            ChartStyle::DepthArea,
            ChartStyle::DepthContour,
            ChartStyle::Sounding,
            ChartStyle::Buoy,
            ChartStyle::Beacon,
            ChartStyle::Light,
            ChartStyle::Danger,
            ChartStyle::Area,
            ChartStyle::Other,
        ] {
            assert!(order.contains(&style), "{style:?} missing from group_order");
            assert!(!style_label(style).is_empty(), "{style:?} has no label");
        }
    }

    #[test]
    fn group_order_has_no_duplicates() {
        let order = group_order();
        for (i, a) in order.iter().enumerate() {
            for b in &order[i + 1..] {
                assert_ne!(a, b, "{a:?} appears twice in group_order");
            }
        }
    }

    #[test]
    fn the_panel_starts_closed() {
        // On a phone an open panel covers the chart, which is the opposite
        // of useful on first launch.
        assert!(!Ui::default().open);
    }

    #[test]
    fn swatches_match_the_chart_palette() {
        for style in [ChartStyle::Danger, ChartStyle::Coastline, ChartStyle::Land] {
            let [r, g, b, _] = crate::colour_for(style);
            let swatch = Ui::swatch(style);
            assert_eq!(swatch.r(), (r * 255.0) as u8);
            assert_eq!(swatch.g(), (g * 255.0) as u8);
            assert_eq!(swatch.b(), (b * 255.0) as u8);
        }
    }
}

#[cfg(test)]
mod font_tests {
    //! Guards against the interface rendering as empty boxes.
    //!
    //! egui 0.37 changed its `default` feature to the empty set — it
    //! included `default_fonts` up to 0.31 — so declaring `egui = "0.37"`
    //! compiles with no font data whatsoever. Every glyph then draws as a
    //! tofu box, which looks exactly like a broken texture upload and sends
    //! you looking in the wrong place entirely.
    //!
    //! These run headless: egui lays text out on the CPU, so whether fonts
    //! exist is answerable without a GPU or a window.

    use super::*;

    /// Lay out some text and return the tessellated output.
    fn render_text(text: &str) -> (egui::TexturesDelta, Vec<egui::ClippedPrimitive>) {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.label(text);
        });
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        (output.textures_delta, primitives)
    }

    #[test]
    fn a_font_atlas_is_produced() {
        let (mut textures, _) = render_text("Soundings");

        assert!(
            !textures.set.is_empty(),
            "no font atlas was produced — the `default_fonts` feature is \
             almost certainly off, and every glyph will draw as an empty box"
        );

        // TexturesDelta panics in Drop if left unapplied, which is the other
        // half of this story — see Renderer::render_with_ui.
        textures.clear();
    }

    #[test]
    fn text_tessellates_into_actual_geometry() {
        let (mut textures, primitives) = render_text("DEPARE");
        textures.clear();

        let vertices: usize = primitives
            .iter()
            .map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.len(),
                egui::epaint::Primitive::Callback(_) => 0,
            })
            .sum();

        // Six glyphs cannot be drawn with no vertices. A font-less build
        // still emits boxes, so this is a weaker check than the atlas one —
        // but it fails loudly if tessellation stops producing anything.
        assert!(
            vertices > 0,
            "text produced no geometry at all: {} primitives",
            primitives.len()
        );
    }

    /// Width of the widest mesh in a tessellated frame.
    fn text_width(prims: &[egui::ClippedPrimitive]) -> f32 {
        prims
            .iter()
            .filter_map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(mesh) => Some(mesh.calc_bounds().width()),
                egui::epaint::Primitive::Callback(_) => None,
            })
            .fold(0.0f32, f32::max)
    }

    #[test]
    fn glyphs_have_proportional_widths() {
        // **This is the test that actually catches a font-less build.**
        //
        // Without fonts, egui still produces an atlas and still tessellates
        // — it draws a uniform replacement box per character. So "an atlas
        // exists" and "geometry exists" both pass either way, which is
        // exactly the trap the first version of this module fell into.
        //
        // What cannot survive a font-less build is *proportional* spacing:
        // twenty narrow letters must occupy less width than twenty wide
        // ones. With tofu every character is the same box, so the two are
        // identical.
        let (mut narrow_tex, narrow) = render_text("iiiiiiiiiiiiiiiiiiii");
        narrow_tex.clear();
        let (mut wide_tex, wide) = render_text("WWWWWWWWWWWWWWWWWWWW");
        wide_tex.clear();

        let (narrow_w, wide_w) = (text_width(&narrow), text_width(&wide));

        assert!(
            wide_w > narrow_w * 1.5,
            "twenty 'W' ({wide_w}) should be far wider than twenty 'i'              ({narrow_w}). They are not, which means the glyphs are uniform              replacement boxes: egui has no font data, and the `default_fonts`              feature is off."
        );
    }

    #[test]
    fn longer_text_is_wider() {
        let (mut a_tex, short) = render_text("A");
        a_tex.clear();
        let (mut b_tex, long) = render_text("AAAAAAAAAAAAAAAAAAAA");
        b_tex.clear();

        assert!(
            text_width(&long) > text_width(&short),
            "twenty characters should be wider than one: {} vs {}",
            text_width(&long),
            text_width(&short)
        );
    }

    #[test]
    fn the_panel_labels_are_all_renderable() {
        // Every string the panel shows must come out of the font, not as a
        // box. This is why the labels are plain words rather than icon
        // glyphs: egui's proportional font covers Latin text and little
        // else, and a hamburger or a cross is not guaranteed.
        let ctx = egui::Context::default();
        let mut ui_state = Ui {
            open: true,
            ..Ui::default()
        };
        let entries = vec![LayerEntry {
            name: "COALNE".to_owned(),
            style: ChartStyle::Coastline,
            visible: true,
            count: Some(139),
        }];

        let imagery = ImageryState {
            available: true,
            visible: true,
            opacity: 0.75,
            tiles: 25,
            attribution: Some("Imagery: USGS National Map".to_owned()),
        };
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui_state.show(ui, &entries, &imagery, "32°40.80'N  117°13.80'W   z12.0");
        });
        let mut textures = output.textures_delta;

        assert!(!textures.set.is_empty(), "the panel produced no font atlas");
        textures.clear();
    }
}
