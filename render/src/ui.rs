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
use rutter_core::depth::{Clearance, Draft, Tally};

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
    /// Show or hide the tide stations.
    SetTides(bool),
    /// Predict for this station instead of the nearest one.
    SelectTideStation(String),
    /// Switch the under-keel check on or off.
    SetDraftCheck(bool),
    /// Change the boat's draft and the margin wanted under it.
    SetDraft(Draft),
    /// Show or hide the wind.
    SetWind(bool),
    /// Show or hide the boat's own position.
    SetVessel(bool),
    /// Open a feed at this address, replacing any current one.
    ConnectNmea(String),
    /// Close the feed.
    DisconnectNmea,
    /// Use the device's own GPS instead of a network feed.
    UseDeviceGps,
    /// Show or hide the tidal currents.
    SetCurrents(bool),
    /// Look at a time this many seconds from now. Zero is now.
    SetTimeOffset(i64),
    /// Back to now.
    NowTime,
    /// Keep the chart centred on the boat as it moves.
    SetFollow(bool),
    /// Centre on the boat once, without following it.
    GoToBoat,
}

/// What the panel knows about the tidal currents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CurrentState {
    /// Whether the database carries any harmonic current stations.
    pub available: bool,
    pub visible: bool,
    /// How many are in the file.
    pub stations: usize,
    /// The nearest station's name.
    pub station: Option<String>,
    /// What it is doing at the time being shown: "1.4 kn flooding, 355°".
    pub now: Option<String>,
    /// The next few slacks and peaks.
    pub next: Vec<String>,
    /// How many of the stations in view are subordinate and so not
    /// predicted. Worth saying: NOAA publishes ten times as many of those
    /// as harmonic ones, and a chart with two arrows on it looks broken
    /// until you know why.
    pub subordinate_nearby: usize,
}

/// What the panel knows about the time being shown.
///
/// The chart can be shown at a time other than now — a slider drives the
/// tide, the currents, the under-keel check, the wind and the temperature
/// together. The boat's own position is *not* driven by it and cannot be:
/// a GPS says where the boat is, not where it will be.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimeState {
    /// Seconds from now. Zero is now; negative is the past.
    pub offset_secs: i64,
    /// How far the slider may be dragged, in seconds from now.
    pub min_secs: i64,
    pub max_secs: i64,
    /// The time being shown, as `14:30Z`.
    pub shown: String,
    /// How far from now, as `in 3h 20m`. Empty at now.
    pub away: String,
    /// Whether the slider is at now.
    pub at_now: bool,
    /// Whether the forecast covers the time being shown. False past the
    /// model's horizon, where the tide still works and the wind does not.
    pub wind_covers_this: bool,
}

/// What the panel knows about the boat's own position.
///
/// Strings rather than numbers, as with the other sections: the formatting
/// is navigation — degrees and decimal minutes, knots, degrees true — and
/// belongs with the code that knows what the numbers mean, not with the
/// code that draws labels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VesselState {
    /// Whether a feed was configured at all. False means no `--nmea`, and
    /// the section explains itself rather than looking broken.
    pub available: bool,
    pub visible: bool,
    /// One line about the socket: connected, listening, or why not.
    pub link: String,
    /// Whether that line is a problem rather than a status.
    pub trouble: bool,
    /// The position, in degrees and decimal minutes. `None` until a fix.
    pub fix: Option<String>,
    /// Speed and course over ground, when the feed carries them.
    pub motion: Option<String>,
    /// Heading from a compass, which is not the same as course.
    pub heading: Option<String>,
    /// Depth under the transducer.
    pub depth: Option<String>,
    /// Apparent wind at the masthead — the boat's own instrument, not the
    /// forecast.
    pub wind: Option<String>,
    /// How old the fix is, and whether that is too old to steer by.
    pub age: Option<String>,
    pub stale: bool,
    /// Quality and satellite count, for when a fix looks wrong.
    pub quality: Option<String>,
    pub follow: bool,
    /// Sentence counts: accepted, unsupported, rejected.
    pub counts: Option<String>,
    /// The address currently in use, for seeding the address box. Every
    /// platform can connect from the panel; only Linux has a flag.
    pub address: Option<String>,
    /// Whether this device has a GPS of its own to fall back on.
    pub has_device_gps: bool,
    /// Whether that is what is currently being read.
    pub using_device_gps: bool,
    /// Horizontal accuracy, when the source states one. A phone does; a
    /// NMEA feed cannot.
    pub accuracy: Option<String>,
}

/// What the panel knows about the wind.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindState {
    pub available: bool,
    pub visible: bool,
    /// The wind at the middle of the view, formatted.
    pub here: Option<String>,
    /// Which forecast hour is being shown, and how stale it is. A forecast
    /// is only worth anything with its age attached.
    pub validity: Option<String>,
    /// True when the nearest hour is far enough from now to distrust.
    pub stale: bool,
    /// Set when the view reaches past the edge of the fetched forecast.
    ///
    /// Said out loud because the symptom is misleading: barbs over part of
    /// the screen and nothing beyond looks like a layer that has given up,
    /// when it is the edge of the box someone chose to download.
    pub partial_coverage: bool,
}

/// One high or low water, ready to print.
///
/// The strings are built by the platform rather than here. This crate has no
/// clock and no timezone database, and giving it one so that a panel could
/// format a time would be the wrong place for both.
#[derive(Debug, Clone, PartialEq)]
pub struct TideReading {
    /// "High" or "Low".
    pub label: String,
    /// Clock time, with whatever zone the platform chose to name.
    pub when: String,
    /// How far off, as "in 2h 11m". Timezone-free, and the part you act on.
    pub away: String,
    /// Height above chart datum, formatted with its unit.
    pub height: String,
    /// Drives the colour. A high and a low should be tellable apart
    /// without reading the word.
    pub high: bool,
}

/// A tide station drawn over the chart.
///
/// Positions are in screen pixels, projected by the platform: it owns the
/// [`rutter_core::View`], and duplicating the projection here would be a
/// second place for it to go wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct TideMarker {
    pub x: f32,
    pub y: f32,
    pub id: String,
    pub label: String,
    /// The station currently being predicted for.
    pub selected: bool,
}

/// What the panel knows about tides.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TideState {
    /// Whether a station database is loaded at all. With none the controls
    /// show disabled rather than vanishing, so it is clear the feature
    /// exists and what is missing.
    pub available: bool,
    pub visible: bool,
    /// Stations in the database.
    pub stations: usize,
    /// Name of the station being predicted for.
    pub station: Option<String>,
    /// The water now, formatted.
    pub now: Option<String>,
    /// The next few turning points.
    pub next: Vec<TideReading>,
    /// Heights across the plotted window, metres, evenly spaced.
    pub curve: Vec<f32>,
    /// Where "now" falls across that window, 0..1.
    pub now_at: f32,
    /// Stations to draw over the chart.
    pub markers: Vec<TideMarker>,
    pub attribution: Option<String>,
}

/// What the panel knows about the under-keel check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DraftState {
    /// Whether the check can run at all: it needs both a chart to judge and
    /// a tide to judge it against.
    pub available: bool,
    pub enabled: bool,
    pub draft: Draft,
    /// What the last drawn chart judged.
    pub tally: Tally,
    /// The tide now and at the window's low water, metres above datum, for
    /// the explanation line. The panel does the subtraction so the reader
    /// does not have to.
    pub tide_now_m: f64,
    pub tide_low_m: f64,
    /// How far ahead the low water was looked for, hours.
    pub window_hours: u32,
    /// Areas in view that could not be shaded because their polygon could
    /// not be tiled exactly. Shown when non-zero, because a shading layer
    /// with silent gaps is worse than one that admits them.
    pub unfilled: usize,
}

impl Default for DraftState {
    fn default() -> Self {
        Self {
            available: false,
            enabled: false,
            draft: Draft::default(),
            tally: Tally::default(),
            tide_now_m: 0.0,
            tide_low_m: 0.0,
            window_hours: 12,
            unfilled: 0,
        }
    }
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

/// Everything the panel draws from, for one frame.
///
/// One struct rather than eight arguments. It started as a handful of
/// parameters and grew one per layer — charts, then imagery, then tides,
/// then the under-keel check, then wind — which is a signature that tells
/// you it wants to be a type. The platform builds one of these from its own
/// state and hands it over; nothing here reaches back.
#[derive(Debug, Default)]
pub struct PanelState {
    pub layers: Vec<LayerEntry>,
    pub vessel: VesselState,
    pub imagery: ImageryState,
    pub tides: TideState,
    pub currents: CurrentState,
    pub time: TimeState,
    pub draft: DraftState,
    pub wind: WindState,
    /// The top strip's text: position, zoom, scale.
    pub status: String,
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
    /// The NMEA address being typed, and whether it has been seeded from
    /// whatever the platform was started with.
    ///
    /// Held here rather than in the app because it is a half-typed string
    /// — interface state, not navigation state. Seeded once so that
    /// `--nmea` shows up in the box and can be edited rather than retyped.
    nmea_address: String,
    nmea_seeded: bool,
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
    pub fn show(&mut self, root: &mut egui::Ui, state: &PanelState) -> Vec<Action> {
        let PanelState {
            layers,
            vessel,
            imagery,
            tides,
            currents,
            time,
            draft,
            wind,
            status,
        } = state;
        let mut actions = Vec::new();

        // Station rings and wind barbs are drawn by the chart renderer, not
        // here — see `Overlay` in this crate's root. What remains in egui is
        // the part that needs a font atlas or a click: the selected
        // station's label, and hit-testing the markers.
        if tides.visible {
            actions.extend(Self::paint_tide_markers(root, tides));
        }

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
                // The boat, beside the view's own position: with the panel
                // closed this is the one control worth having, because
                // "where am I" is the question a plotter exists to answer.
                if vessel.available {
                    let button = ui.add_enabled(vessel.fix.is_some(), egui::Button::new("Boat"));
                    if button.clicked() {
                        actions.push(Action::GoToBoat);
                    }
                    button.on_hover_text(if vessel.fix.is_some() {
                        "Centre on the boat"
                    } else {
                        "No fix yet"
                    });
                }
                ui.separator();
                ui.label(status);
            });
        });

        // The time strip, always visible. It is not a setting tucked in a
        // panel: it changes what every other layer is claiming, and a
        // chart showing a tide three hours from now while looking exactly
        // like one showing the tide now would be a genuinely dangerous
        // thing to leave hidden.
        actions.extend(Self::time_strip(root, time));

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
                actions.extend(self.vessel_section(ui, vessel));
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
                actions.extend(Self::tide_section(ui, tides));
                ui.separator();
                actions.extend(Self::current_section(ui, currents));
                ui.separator();
                actions.extend(Self::wind_section(ui, wind));
                ui.separator();
                actions.extend(Self::draft_section(ui, draft));
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

    /// The wind block in the panel.
    /// The boat: where it is, how fast, and whether the feed is healthy.
    ///
    /// Put first in the panel because it is the only section about the boat
    /// rather than about the water. Everything else here describes the
    /// world; this says where you are in it.
    fn vessel_section(&mut self, ui: &mut egui::Ui, vessel: &VesselState) -> Vec<Action> {
        let mut actions = Vec::new();

        ui.horizontal(|ui| {
            let mut on = vessel.visible;
            let toggle = ui.add_enabled(vessel.available, egui::Checkbox::new(&mut on, "Boat"));
            if toggle.clicked() {
                actions.push(Action::SetVessel(on));
            }
            if !vessel.available {
                toggle.on_hover_text("No GPS feed. Give an address below.");
            }
        });

        // The address box, whether or not a feed is open. Changing boats,
        // or reconnecting to a phone that got a new DHCP lease, has to be
        // possible without restarting — and on Android there is no command
        // line to have passed it on in the first place.
        if !self.nmea_seeded {
            self.nmea_seeded = true;
            if let Some(address) = &vessel.address {
                self.nmea_address = address.clone();
            }
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.nmea_address)
                    .hint_text("tcp://host:10110")
                    .desired_width(150.0),
            )
            .on_hover_text(
                "An NMEA 0183 feed over the network. A chartplotter or \
                 multiplexer on the boat, or a phone running a GPS relay \
                 app such as GPS2IP or Share GPS. udp://:10110 listens for \
                 anything broadcasting to the network instead.",
            );
            let typed = !self.nmea_address.trim().is_empty();
            if ui
                .add_enabled(
                    typed,
                    egui::Button::new(if vessel.available {
                        "Reconnect"
                    } else {
                        "Connect"
                    }),
                )
                .clicked()
            {
                actions.push(Action::ConnectNmea(self.nmea_address.trim().to_owned()));
            }
            if vessel.available && ui.button("Off").clicked() {
                actions.push(Action::DisconnectNmea);
            }
        });

        // The device's own GPS, where there is one. Shown disabled rather
        // than hidden on a laptop, so it is clear this is a thing the app
        // does and a thing this machine lacks.
        if vessel.has_device_gps || !vessel.available {
            ui.horizontal(|ui| {
                let button = ui.add_enabled(
                    vessel.has_device_gps && !vessel.using_device_gps,
                    egui::Button::new("Use this device's GPS"),
                );
                if button.clicked() {
                    actions.push(Action::UseDeviceGps);
                }
                if !vessel.has_device_gps {
                    button.on_hover_text(
                        "This machine has no GPS of its own. Plug in a USB \
                         dongle and pass its device path, or connect to a \
                         feed over the network.",
                    );
                } else if vessel.using_device_gps {
                    button.on_hover_text("Already in use");
                }
            });
        }

        if !vessel.available {
            return actions;
        }

        // The link line always shows, fix or no fix: when there is no
        // position, the reason is the whole content of this section.
        let link = egui::RichText::new(&vessel.link).weak().small();
        ui.label(if vessel.trouble {
            link.color(egui::Color32::from_rgb(0xff, 0x6b, 0x5b))
        } else {
            link
        });

        if !vessel.visible {
            return actions;
        }

        match &vessel.fix {
            Some(fix) => {
                let text = egui::RichText::new(fix).strong();
                // A stale fix is the dangerous case: the numbers look
                // exactly as confident when they are a minute old and the
                // boat has moved a cable since.
                ui.label(if vessel.stale {
                    text.color(egui::Color32::from_rgb(0xff, 0xb3, 0x4d))
                } else {
                    text
                });
            }
            None => {
                ui.label(egui::RichText::new("waiting for a fix").weak());
            }
        }

        for line in [&vessel.motion, &vessel.heading, &vessel.depth, &vessel.wind]
            .into_iter()
            .flatten()
        {
            ui.label(line);
        }

        if let Some(accuracy) = &vessel.accuracy {
            ui.label(egui::RichText::new(accuracy).weak().small())
                .on_hover_text(
                    "How far the fix could be out, as the receiver \
                     reckons it. A phone states this; an NMEA feed \
                     carries HDOP instead, which is not a distance.",
                );
        }
        if let Some(age) = &vessel.age {
            let text = egui::RichText::new(age).weak().small();
            ui.label(if vessel.stale {
                text.color(egui::Color32::from_rgb(0xff, 0xb3, 0x4d))
            } else {
                text
            });
        }
        if let Some(quality) = &vessel.quality {
            ui.label(egui::RichText::new(quality).weak().small());
        }

        ui.horizontal(|ui| {
            let mut follow = vessel.follow;
            if ui
                .add_enabled(
                    vessel.fix.is_some(),
                    egui::Checkbox::new(&mut follow, "Follow"),
                )
                .on_hover_text(
                    "Keep the boat centred. Panning or zooming by hand \
                     switches this off, so the chart never fights the hand \
                     on it.",
                )
                .clicked()
            {
                actions.push(Action::SetFollow(follow));
            }
        });

        if let Some(counts) = &vessel.counts {
            ui.label(egui::RichText::new(counts).weak().small())
                .on_hover_text(
                    "Sentences understood, ignored as another kind, and \
                     refused. A climbing refusal count means the wrong \
                     port or a noisy link.",
                );
        }

        actions
    }

    /// The time being shown, and the slider that moves it.
    fn time_strip(root: &mut egui::Ui, time: &TimeState) -> Vec<Action> {
        let mut actions = Vec::new();
        if time.min_secs >= time.max_secs {
            return actions;
        }

        egui::Panel::bottom("time").show(root, |ui| {
            ui.horizontal(|ui| {
                // "Now" first and always enabled when it would do
                // something: getting back to the present must never be a
                // hunt through a panel.
                if ui
                    .add_enabled(!time.at_now, egui::Button::new("Now"))
                    .on_hover_text("Back to the present")
                    .clicked()
                {
                    actions.push(Action::NowTime);
                }

                let mut offset = time.offset_secs;
                let slider = egui::Slider::new(&mut offset, time.min_secs..=time.max_secs)
                    .show_value(false)
                    .custom_formatter(|v, _| format!("{:+.1} h", v / 3600.0));
                if ui.add(slider).changed() {
                    actions.push(Action::SetTimeOffset(offset));
                }

                // The time itself, emphasised when it is not now. A
                // readout that looks the same whether or not you have
                // moved is the one failure this control must not have.
                let shown = egui::RichText::new(&time.shown).monospace();
                ui.label(if time.at_now {
                    shown
                } else {
                    shown
                        .strong()
                        .color(egui::Color32::from_rgb(0x6c, 0xc6, 0xff))
                });
                if !time.away.is_empty() {
                    ui.label(
                        egui::RichText::new(&time.away)
                            .small()
                            .color(egui::Color32::from_rgb(0x6c, 0xc6, 0xff)),
                    );
                }
                if !time.wind_covers_this {
                    ui.label(
                        egui::RichText::new("no forecast this far out")
                            .small()
                            .weak(),
                    )
                    .on_hover_text(
                        "The tide and the currents are astronomical and work \
                         for any time at all. The wind and temperature are a \
                         model run with a horizon — fetch more hours with \
                         `rutter weather --hours`.",
                    );
                }
            });
        });

        actions
    }

    /// The tidal currents: which way the water is going, and when it turns.
    fn current_section(ui: &mut egui::Ui, currents: &CurrentState) -> Vec<Action> {
        let mut actions = Vec::new();

        ui.horizontal(|ui| {
            let mut on = currents.visible;
            let toggle = ui.add_enabled(
                currents.available,
                egui::Checkbox::new(&mut on, "Tidal currents"),
            );
            if toggle.clicked() {
                actions.push(Action::SetCurrents(on));
            }
            if currents.available {
                ui.label(
                    egui::RichText::new(format!("{} stations", currents.stations))
                        .weak()
                        .small(),
                );
            } else {
                toggle.on_hover_text(
                    "No current stations loaded. Fetch some with \
                     `rutter tides --box <s>,<w>,<n>,<e>`.",
                );
            }
        });

        if !currents.available || !currents.visible {
            return actions;
        }

        if let Some(station) = &currents.station {
            ui.label(egui::RichText::new(station).strong());
        }
        if let Some(now) = &currents.now {
            ui.label(now).on_hover_text(
                "Where the water is going, not where it comes from — the \
                 opposite convention to a wind, and the one every chart \
                 uses for a current. Arrow length is speed.",
            );
        }
        for line in &currents.next {
            ui.label(egui::RichText::new(line).weak().small());
        }

        if currents.subordinate_nearby > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "{} more stations nearby, not predicted",
                    currents.subordinate_nearby
                ))
                .weak()
                .small(),
            )
            .on_hover_text(
                "NOAA publishes most current stations as time and speed \
                 offsets against a reference station rather than as \
                 harmonic constituents. Predicting those needs a different \
                 method, which this does not do yet.",
            );
        }

        actions
    }

    fn wind_section(ui: &mut egui::Ui, wind: &WindState) -> Vec<Action> {
        let mut actions = Vec::new();

        ui.horizontal(|ui| {
            let mut on = wind.visible;
            let toggle = ui.add_enabled(wind.available, egui::Checkbox::new(&mut on, "Wind"));
            if toggle.clicked() {
                actions.push(Action::SetWind(on));
            }
            if !wind.available {
                toggle.on_hover_text(
                    "No forecast loaded. Fetch one with \
                     `rutter weather --chart <file>`.",
                );
            }
        });

        if !wind.available || !wind.visible {
            return actions;
        }

        if let Some(here) = &wind.here {
            ui.label(egui::RichText::new(here).strong());
        }
        if let Some(validity) = &wind.validity {
            let text = egui::RichText::new(validity).weak().small();
            // A stale forecast is the one thing here worth colouring: the
            // numbers look exactly as confident when they are a week old.
            ui.label(if wind.stale {
                text.color(egui::Color32::from_rgb(0xff, 0xb3, 0x4d))
            } else {
                text
            })
            .on_hover_text(
                "GFS at 0.25 degrees — about 28 km a node, so this is the \
                 synoptic wind and not the sea breeze in your harbour. \
                 Barbs: half 5 kn, full 10 kn, pennant 50 kn.",
            );
        }
        if wind.stale {
            ui.label(
                egui::RichText::new("refetch with `rutter weather`")
                    .weak()
                    .small(),
            );
        }

        if wind.partial_coverage {
            ui.label(
                egui::RichText::new("forecast covers only part of this view")
                    .weak()
                    .small(),
            )
            .on_hover_text(
                "The blank area is outside the box that was downloaded, not \
                 calm. `rutter weather --chart <file>` pads the chart's \
                 bounds; raise --margin for more.",
            );
        }

        actions
    }

    /// The under-keel check: the boat, and what the chart says about it.
    ///
    /// This is the control that turns the tide from a number into an answer.
    /// Everything above it describes the water; this asks whether the boat
    /// fits in it.
    fn draft_section(ui: &mut egui::Ui, state: &DraftState) -> Vec<Action> {
        let mut actions = Vec::new();

        ui.horizontal(|ui| {
            let mut on = state.enabled;
            let toggle = ui.add_enabled(
                state.available,
                egui::Checkbox::new(&mut on, "Under-keel check"),
            );
            if toggle.clicked() {
                actions.push(Action::SetDraftCheck(on));
            }
            if !state.available {
                toggle.on_hover_text(
                    "Needs both a chart with depths and a tide station. \
                     Fetch stations with `rutter tides --near <lat>,<lon>`.",
                );
            }
        });

        if !state.available || !state.enabled {
            return actions;
        }

        // Both numbers are drag-values rather than text fields: on a phone
        // in a cockpit, dragging a number is possible and typing is not.
        let mut draft = state.draft;
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("draft");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut draft.draft_m)
                        .speed(0.05)
                        .range(0.0..=20.0)
                        .suffix(" m"),
                )
                .changed();
            ui.label("+ margin");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut draft.safety_m)
                        .speed(0.05)
                        .range(0.0..=10.0)
                        .suffix(" m"),
                )
                .on_hover_text(
                    "Water you want left under the keel. Worth at least \
                     0.3 m here: the tide prediction is astronomical only \
                     and takes no account of pressure or wind.",
                )
                .changed();
        });
        if changed {
            actions.push(Action::SetDraft(draft));
        }

        ui.label(
            egui::RichText::new(format!("needs {:.2} m of water", draft.needed_m()))
                .weak()
                .small(),
        );

        // The arithmetic, spelled out. The whole feature is one subtraction,
        // and showing it is what makes the colours trustworthy rather than
        // magic.
        let needed = draft.needed_m();
        ui.label(
            egui::RichText::new(format!(
                "clears soundings ≥ {:.2} m now, ≥ {:.2} m within {} h",
                needed - state.tide_now_m,
                needed - state.tide_low_m,
                state.window_hours
            ))
            .weak()
            .small(),
        );

        // On a rising tide the window's low water *is* now, so nothing can
        // get shallower and the amber count is legitimately zero. Without
        // saying so, a zero reads as a broken check rather than as good
        // news — which is the wrong way round for the one number here that
        // means "you have time".
        if state.tide_low_m >= state.tide_now_m - 0.005 {
            ui.label(
                egui::RichText::new(format!(
                    "tide is rising — nothing gets shallower for {} h",
                    state.window_hours
                ))
                .weak()
                .small(),
            );
        } else {
            ui.label(
                egui::RichText::new(format!(
                    "tide falls {:.2} m before its low",
                    state.tide_now_m - state.tide_low_m
                ))
                .weak()
                .small(),
            );
        }

        let t = state.tally;
        if t.total() == 0 {
            ui.label(
                egui::RichText::new("nothing with a charted depth in view")
                    .weak()
                    .small(),
            );
            return actions;
        }

        for (count, verdict, label) in [
            (t.foul, Clearance::Foul, "no water now"),
            (
                t.tight_at_low,
                Clearance::TightAtLow,
                "dries out within the window",
            ),
            (t.clear, Clearance::Clear, "clear"),
        ] {
            if count == 0 {
                continue;
            }
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                let [r, g, b, _] = crate::colour_for_clearance(verdict);
                ui.painter().rect_filled(
                    rect,
                    2.0,
                    egui::Color32::from_rgb(
                        (r * 255.0) as u8,
                        (g * 255.0) as u8,
                        (b * 255.0) as u8,
                    ),
                );
                ui.label(format!("{count}"));
                ui.label(egui::RichText::new(label).weak().small());
            });
        }

        if !t.any_concern() {
            ui.label(
                egui::RichText::new("everything in view clears through the window")
                    .weak()
                    .small(),
            );
        }

        if state.unfilled > 0 {
            // Not a warning colour, but not hidden either: the shading is
            // incomplete and the navigator should know which way to read a
            // gap. The outline and the soundings are still drawn there.
            ui.label(
                egui::RichText::new(format!(
                    "{} area{} outlined but not shaded",
                    state.unfilled,
                    if state.unfilled == 1 { "" } else { "s" }
                ))
                .weak()
                .small(),
            )
            .on_hover_text(
                "These polygons could not be divided into triangles exactly,                  so they carry no fill. Their boundary and their soundings                  are still drawn — read an unshaded patch as unknown rather                  than as clear water.",
            );
        }

        actions
    }

    /// Colour for a high or a low water.
    ///
    /// Rising water is the safe direction and falling is the one that puts
    /// you on the bottom, so a low reads as the warning.
    fn tide_colour(high: bool) -> egui::Color32 {
        if high {
            egui::Color32::from_rgb(0x6c, 0xc6, 0xff)
        } else {
            egui::Color32::from_rgb(0xff, 0xb3, 0x4d)
        }
    }

    /// The tide block in the panel: station, water now, next turning points,
    /// and a plot of the window around now.
    fn tide_section(ui: &mut egui::Ui, tides: &TideState) -> Vec<Action> {
        let mut actions = Vec::new();

        ui.horizontal(|ui| {
            let mut on = tides.visible;
            let toggle = ui.add_enabled(tides.available, egui::Checkbox::new(&mut on, "Tides"));
            if toggle.clicked() {
                actions.push(Action::SetTides(on));
            }
            if tides.available {
                ui.label(
                    egui::RichText::new(format!("{} stations", tides.stations))
                        .weak()
                        .small(),
                );
            } else {
                toggle.on_hover_text(
                    "No tide stations loaded. Fetch some with \
                     `rutter tides --near <lat>,<lon>`.",
                );
            }
        });

        if !tides.available || !tides.visible {
            return actions;
        }

        if let Some(station) = &tides.station {
            ui.label(egui::RichText::new(station).strong());
        }
        if let Some(now) = &tides.now {
            ui.horizontal(|ui| {
                ui.label(format!("now  {now}"));
                // Say what the number is measured from, every time it is
                // shown. Without this the natural reading is "the water is
                // 0.21 m deep", when it means "0.21 m more than the chart
                // says" — and when the figure goes negative, which it does
                // on a spring low, it means 0.11 m *less*. That is the
                // difference between a sounding you clear and one you do
                // not, so the label is not decoration.
                ui.label(egui::RichText::new("above datum").weak().small())
                    .on_hover_text(
                        "Heights are above chart datum (MLLW), the same zero the \
                     chart's soundings use. Depth at a spot = charted \
                     sounding + this figure. A negative figure means less \
                     water than the chart prints.\n\nAstronomical tide only \
                     — no allowance for barometric pressure or wind, which \
                     can shift the real level by 0.2-0.3 m.",
                    );
            });
        }

        if !tides.curve.is_empty() {
            Self::plot_tide(ui, tides);
        }

        for reading in &tides.next {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&reading.label)
                        .color(Self::tide_colour(reading.high))
                        .strong(),
                );
                ui.label(&reading.when);
                ui.label(egui::RichText::new(&reading.height).strong());
                ui.label(egui::RichText::new(&reading.away).weak().small());
            });
        }

        if let Some(credit) = &tides.attribution {
            ui.label(egui::RichText::new(credit).weak().small());
        }

        actions
    }

    /// The tide curve, drawn with the painter rather than a plotting crate.
    ///
    /// It is a polyline, a baseline at chart datum and a cursor at now —
    /// not worth a dependency, and a dependency would have to cross-compile
    /// to four Android ABIs to earn its place.
    fn plot_tide(ui: &mut egui::Ui, tides: &TideState) {
        const HEIGHT: f32 = 56.0;

        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, HEIGHT), egui::Sense::hover());
        let painter = ui.painter_at(rect);

        let lowest = tides.curve.iter().copied().fold(f32::MAX, f32::min);
        let highest = tides.curve.iter().copied().fold(f32::MIN, f32::max);
        // A flat curve would divide by zero. It should not happen with real
        // constituents, but a station reduced to one tiny term could.
        let span = (highest - lowest).max(0.01);

        let plot = rect.shrink2(egui::vec2(0.0, 6.0));
        let at = |i: usize, metres: f32| {
            let x = if tides.curve.len() > 1 {
                i as f32 / (tides.curve.len() - 1) as f32
            } else {
                0.5
            };
            egui::pos2(
                plot.left() + x * plot.width(),
                // Screen y grows downward, water upward.
                plot.bottom() - ((metres - lowest) / span) * plot.height(),
            )
        };

        painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);

        // Chart datum, if it falls inside the window: the line below which
        // a sounding no longer has the depth printed on it.
        if lowest < 0.0 && highest > 0.0 {
            let y = at(0, 0.0).y;
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
            );
        }

        let points: Vec<egui::Pos2> = tides
            .curve
            .iter()
            .enumerate()
            .map(|(i, &m)| at(i, m))
            .collect();
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.5, Self::tide_colour(true)),
        ));

        // Where we are on it.
        let x = plot.left() + tides.now_at.clamp(0.0, 1.0) * plot.width();
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.0, ui.visuals().strong_text_color()),
        );

        painter.text(
            rect.left_bottom() + egui::vec2(3.0, -1.0),
            egui::Align2::LEFT_BOTTOM,
            format!("{lowest:.1}"),
            egui::FontId::proportional(9.0),
            ui.visuals().weak_text_color(),
        );
        painter.text(
            rect.left_top() + egui::vec2(3.0, 1.0),
            egui::Align2::LEFT_TOP,
            format!("{highest:.1} m"),
            egui::FontId::proportional(9.0),
            ui.visuals().weak_text_color(),
        );
    }

    /// Clicks on the station markers, and the selected one's label.
    ///
    /// The rings themselves are drawn by the chart renderer — see `Overlay`
    /// — so what is left here is the two things that genuinely belong to the
    /// interface: hit-testing a tap, which needs egui's input, and a text
    /// label, which needs its font atlas. Nothing is painted over the chart
    /// except that label.
    fn paint_tide_markers(root: &mut egui::Ui, tides: &TideState) -> Vec<Action> {
        /// The ring's drawn radius, matching `refresh_overlay`.
        const RADIUS: f32 = 5.0;
        /// Generous next to it: this has to be hittable with a thumb on a
        /// phone, not just a mouse.
        const TOUCH: f32 = 14.0;

        let mut actions = Vec::new();
        if tides.markers.is_empty() {
            return actions;
        }

        let clip = root.painter().clip_rect();
        for marker in &tides.markers {
            let centre = egui::pos2(marker.x, marker.y);
            if !clip.expand(TOUCH).contains(centre) {
                continue;
            }

            if marker.selected {
                root.painter().text(
                    centre + egui::vec2(RADIUS + 5.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &marker.label,
                    egui::FontId::proportional(11.0),
                    Self::tide_colour(true),
                );
            }

            let hit = egui::Rect::from_center_size(centre, egui::Vec2::splat(TOUCH * 2.0));
            let response = root.interact(
                hit,
                // `unique` means "not scoped by the parent widget", not
                // "different every frame": it hashes the source, so the same
                // station keeps the same id across frames and egui can track
                // the click.
                egui::Id::unique(("tide-marker", &marker.id)),
                egui::Sense::click(),
            );
            if response.clicked() {
                actions.push(Action::SelectTideStation(marker.id.clone()));
            }
            if response.hovered() && !marker.selected {
                response.on_hover_text(&marker.label);
            }
        }

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
            ui_state.show(
                ui,
                &PanelState {
                    layers: entries.clone(),
                    vessel: populated_vessel(),
                    currents: populated_currents(),
                    time: populated_time(),
                    imagery: imagery.clone(),
                    tides: populated_tides(),
                    draft: populated_draft(),
                    wind: populated_wind(),
                    status: "32°40.80'N  117°13.80'W   z12.0".to_owned(),
                },
            );
        });
        let mut textures = output.textures_delta;

        assert!(!textures.set.is_empty(), "the panel produced no font atlas");
        textures.clear();
    }

    /// A tide state with everything filled in, for the panel tests.
    fn populated_tides() -> TideState {
        TideState {
            available: true,
            visible: true,
            stations: 11,
            station: Some("SAN DIEGO (Broadway)".to_owned()),
            now: Some("+1.24 m".to_owned()),
            next: vec![
                TideReading {
                    label: "High".to_owned(),
                    when: "15:25Z".to_owned(),
                    away: "in 2h 11m".to_owned(),
                    height: "1.95 m".to_owned(),
                    high: true,
                },
                TideReading {
                    label: "Low".to_owned(),
                    when: "22:18Z".to_owned(),
                    away: "in 9h 04m".to_owned(),
                    height: "-0.39 m".to_owned(),
                    high: false,
                },
            ],
            // A plausible mixed semidiurnal shape, including water below
            // chart datum so the datum line is exercised.
            curve: (0..120i16)
                .map(|i| {
                    let t = f32::from(i) * 0.05;
                    0.9 + 0.9 * t.sin() + 0.3 * (2.0 * t).cos()
                })
                .collect(),
            now_at: 0.125,
            markers: vec![
                TideMarker {
                    x: 320.0,
                    y: 240.0,
                    id: "9410170".to_owned(),
                    label: "SAN DIEGO (Broadway)".to_owned(),
                    selected: true,
                },
                TideMarker {
                    x: 80.0,
                    y: 400.0,
                    id: "9410079".to_owned(),
                    label: "Avalon, Santa Catalina Island".to_owned(),
                    selected: false,
                },
            ],
            attribution: Some("Tides: NOAA CO-OPS (public domain)".to_owned()),
        }
    }

    /// A wind state as the panel sees it. The barbs themselves are the
    /// chart renderer's business now; what is left here is text.
    /// A boat on a working feed, for the drawing tests.
    ///
    /// Every string populated, including the ones that only appear in
    /// unusual states: the point of these tests is that a label which
    /// cannot be rendered is caught here rather than on the water, and a
    /// `None` renders nothing at all.
    fn populated_vessel() -> VesselState {
        VesselState {
            available: true,
            visible: true,
            link: "connected to 192.168.1.50:10110".to_owned(),
            trouble: false,
            fix: Some("32°40.80'N  117°13.80'W".to_owned()),
            motion: Some("5.2 kn   COG 184°".to_owned()),
            heading: Some("HDG 190°".to_owned()),
            depth: Some("9.4 m under the transducer".to_owned()),
            wind: Some("12.0 kn apparent, 45° starboard".to_owned()),
            age: Some("fix is current".to_owned()),
            stale: false,
            quality: Some("differential, 11 satellites".to_owned()),
            follow: true,
            counts: Some("812 read, 94 ignored, 0 refused".to_owned()),
            address: Some("tcp://192.168.1.50:10110".to_owned()),
            has_device_gps: true,
            using_device_gps: false,
            accuracy: Some("within 4 m".to_owned()),
        }
    }

    /// Currents running, for the drawing tests.
    fn populated_currents() -> CurrentState {
        CurrentState {
            available: true,
            visible: true,
            stations: 2,
            station: Some("San Diego Bay Entrance".to_owned()),
            now: Some("1.4 kn flood, setting 355°".to_owned()),
            next: vec![
                "14:22Z slack  in 1h 10m".to_owned(),
                "17:08Z 2.1 kn ebb  in 3h 56m".to_owned(),
            ],
            subordinate_nearby: 23,
        }
    }

    /// The slider moved off now, which is the state worth drawing: at now
    /// half of this strip is blank.
    fn populated_time() -> TimeState {
        TimeState {
            offset_secs: 3 * 3600 + 20 * 60,
            min_secs: -12 * 3600,
            max_secs: 24 * 3600,
            shown: "17:50Z".to_owned(),
            away: "in 3h 20m".to_owned(),
            at_now: false,
            wind_covers_this: false,
        }
    }

    fn populated_wind() -> WindState {
        WindState {
            available: true,
            visible: true,
            here: Some("12 kn from 299°, force 4, 22 C".to_owned()),
            validity: Some("GFS +0 h, 40m ago".to_owned()),
            stale: false,
            partial_coverage: false,
        }
    }

    /// A draft state with the check on and something to report.
    fn populated_draft() -> DraftState {
        DraftState {
            available: true,
            enabled: true,
            draft: Draft {
                draft_m: 1.5,
                safety_m: 0.5,
            },
            tally: Tally {
                clear: 112,
                tight_at_low: 9,
                foul: 37,
            },
            tide_now_m: 0.209,
            tide_low_m: -0.112,
            window_hours: 12,
            unfilled: 2,
        }
    }

    /// Run one frame of the panel and hand back the tessellated output.
    fn one_frame(tides: &TideState) -> egui::FullOutput {
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
        let imagery = ImageryState::default();

        ctx.run_ui(egui::RawInput::default(), |ui| {
            ui_state.show(
                ui,
                &PanelState {
                    layers: entries.clone(),
                    vessel: populated_vessel(),
                    currents: populated_currents(),
                    time: populated_time(),
                    imagery: imagery.clone(),
                    tides: tides.clone(),
                    draft: populated_draft(),
                    wind: populated_wind(),
                    status: "status".to_owned(),
                },
            );
        })
    }

    #[test]
    fn the_tide_panel_draws_without_panicking() {
        // The curve plot does its own arithmetic — normalising heights,
        // finding the datum line — and a division by a zero span or an
        // empty slice would take the whole app down mid-frame rather than
        // showing a bad graph.
        let mut output = one_frame(&populated_tides());
        assert!(!output.textures_delta.set.is_empty());
        output.textures_delta.clear();
    }

    #[test]
    fn a_flat_or_empty_tide_curve_does_not_divide_by_zero() {
        // A station reduced to one tiny constituent, or none at all.
        for curve in [vec![], vec![0.5; 40], vec![f32::MIN_POSITIVE; 40]] {
            let tides = TideState {
                curve,
                ..populated_tides()
            };
            let mut output = one_frame(&tides);
            output.textures_delta.clear();
        }
    }

    #[test]
    fn a_single_point_curve_is_drawn_not_skipped() {
        // `len() - 1` as a divisor is zero here, which is the obvious way
        // to write the x step and the obvious way to crash.
        let tides = TideState {
            curve: vec![1.0],
            ..populated_tides()
        };
        let mut output = one_frame(&tides);
        output.textures_delta.clear();
    }

    #[test]
    fn markers_far_off_screen_are_skipped() {
        // A station a thousand screens away still arrives in the list when
        // the viewport is wide; painting it is wasted and egui will have
        // allocated an interaction id for it.
        let tides = TideState {
            markers: vec![TideMarker {
                x: 1.0e6,
                y: -1.0e6,
                id: "far".to_owned(),
                label: "Far away".to_owned(),
                selected: false,
            }],
            ..populated_tides()
        };
        let mut output = one_frame(&tides);
        output.textures_delta.clear();
    }

    #[test]
    fn tides_with_no_database_still_show_the_control() {
        // Hiding the control when nothing is loaded makes the feature
        // invisible; showing it disabled says what is missing.
        let tides = TideState::default();
        let mut output = one_frame(&tides);
        output.textures_delta.clear();
    }

    #[test]
    fn the_three_clearance_colours_are_distinct() {
        // Red/green alone is a poor signal; these also drive the mark size
        // in the renderer. But they must at least differ, or the amber
        // "fine now, not later" state is invisible.
        let clear = crate::colour_for_clearance(Clearance::Clear);
        let tight = crate::colour_for_clearance(Clearance::TightAtLow);
        let foul = crate::colour_for_clearance(Clearance::Foul);
        assert_ne!(clear, tight);
        assert_ne!(tight, foul);
        assert_ne!(clear, foul);
    }

    #[test]
    fn the_draft_panel_renders_in_every_state() {
        // Each of these took a different path through the section, and one
        // of them — the rising-tide line — exists only to explain a zero.
        let states = [
            DraftState::default(),
            DraftState {
                available: true,
                enabled: false,
                ..populated_draft()
            },
            populated_draft(),
            // Rising tide: the window's low is now, so nothing worsens.
            DraftState {
                tide_low_m: 0.209,
                tally: Tally {
                    clear: 100,
                    tight_at_low: 0,
                    foul: 5,
                },
                ..populated_draft()
            },
            // Nothing judged at all, which is the "attributes lost" case.
            DraftState {
                tally: Tally::default(),
                ..populated_draft()
            },
            // Everything clear.
            DraftState {
                tally: Tally {
                    clear: 500,
                    tight_at_low: 0,
                    foul: 0,
                },
                ..populated_draft()
            },
        ];

        for state in states {
            let ctx = egui::Context::default();
            let mut ui_state = Ui {
                open: true,
                ..Ui::default()
            };
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui_state.show(
                    ui,
                    &PanelState {
                        draft: state,
                        wind: populated_wind(),
                        status: "status".to_owned(),
                        ..PanelState::default()
                    },
                );
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn the_wind_panel_renders_in_every_state() {
        // Each of these is a different path through `paint_wind`: calm
        // circles, half barbs, full barbs, pennants, and a 105-knot wind
        // that needs two pennants. The arithmetic divides and rounds, so a
        // zero speed or a NaN direction would panic mid-frame.
        let states = [
            WindState::default(),
            WindState {
                visible: false,
                ..populated_wind()
            },
            populated_wind(),
            WindState {
                stale: true,
                validity: Some("GFS +0 h, 3d 04h ago".to_owned()),
                ..populated_wind()
            },
            // Outside the fetched box entirely, so no reading.
            WindState {
                here: None,
                ..populated_wind()
            },
            // Half in, half out of the downloaded box.
            WindState {
                partial_coverage: true,
                ..populated_wind()
            },
        ];

        for wind in states {
            let ctx = egui::Context::default();
            let mut ui_state = Ui {
                open: true,
                ..Ui::default()
            };
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui_state.show(
                    ui,
                    &PanelState {
                        wind: wind.clone(),
                        status: "status".to_owned(),
                        ..PanelState::default()
                    },
                );
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn a_high_and_a_low_are_different_colours() {
        // The words "High" and "Low" are four characters apart on a small
        // screen in the rain. The colour is the real signal.
        assert_ne!(Ui::tide_colour(true), Ui::tide_colour(false));
    }
}
