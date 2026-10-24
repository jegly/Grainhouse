//! Grainhouse's window (GTK4 + libadwaita): drop in ARW or JPEG photos, pick a film, an effect, diffusion and
//! develop effects, and export with optional Real-ESRGAN upscaling and sharpening.

use adw::prelude::*;
use grainhouse::{
    adjust, ai, camera, creative, develop, export,
    export::ExportOptions,
    film, fx,
    geometry::{self, Geometry, Grab, Rect},
    img::Image,
    frame, local, lut,
    pipeline::{self, Session, Settings},
    presets,
    upscale, whitebalance,
};
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

const PREVIEW: usize = 1800;

const ASPECTS: [&str; 8] = ["Free", "Original", "1:1", "3:2", "4:3", "5:4", "16:9", "65:24 (XPan)"];
const RATIOS: [f32; 8] = [0.0, 0.0, 1.0, 1.5, 4.0 / 3.0, 1.25, 16.0 / 9.0, 65.0 / 24.0];

enum Job {
    Open(Vec<PathBuf>),
    Render { seq: u64, sessions: Vec<Arc<Session>>, current: usize, settings: Settings },
}

enum Done {
    Opened(Vec<Arc<Session>>, Vec<String>),
    Rendered { seq: u64, result: Result<Image, String>, ms: u128, note: Option<String> },
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    picture: gtk::Picture,
    empty: adw::StatusPage,
    /// the "working" box over the photo, with what's happening
    status: gtk::Box,
    status_title: gtk::Label,
    status_sub: gtk::Label,
    info: gtk::Label,
    strip: gtk::Box,
    strip_scroll: gtk::ScrolledWindow,
    before: gtk::ToggleButton,
    export_button: gtk::Button,
    raw_group: adw::PreferencesGroup,
    whites: Slider,
    blacks: Slider,
    vibrance: Slider,
    saturation: Slider,
    mix_colour: adw::ComboRow,
    mix_hue: Slider,
    mix_sat: Slider,
    mix_lum: Slider,
    grade_zone: adw::ComboRow,
    grade_hue: Slider,
    grade_amount: Slider,
    balance: Slider,
    clarity: Slider,
    texture: Slider,
    dehaze: Slider,
    sharpen: Slider,
    vignette: Slider,
    frame: adw::ComboRow,
    curve_channel: adw::ComboRow,
    curve_area: gtk::DrawingArea,
    sharpen_radius: Slider,
    masking: Slider,
    noise: Slider,
    colour_noise: Slider,
    defringe: Slider,
    vig_mid: Slider,
    vig_feather: Slider,
    distortion: Slider,
    vertical: Slider,
    horizontal: Slider,
    local_kind: adw::ComboRow,
    local_size: Slider,
    local_feather: Slider,
    local_exposure: Slider,
    local_contrast: Slider,
    local_saturation: Slider,
    local_warmth: Slider,
    histogram: gtk::DrawingArea,
    undo: gtk::Button,
    redo: gtk::Button,
    presets_box: gtk::Box,
    wb: adw::ComboRow,
    kelvin: Slider,
    tint: Slider,
    shadows: Slider,
    highlights: Slider,
    camera: adw::ComboRow,
    cam_strength: Slider,
    stamp: adw::SwitchRow,
    exposure: Slider,
    contrast: Slider,
    brand: adw::ComboRow,
    film: adw::ComboRow,
    strength: Slider,
    effect: adw::ComboRow,
    target: adw::ComboRow,
    model: adw::ComboRow,
    look: adw::ComboRow,
    face: adw::ComboRow,
    amount: Slider,
    colour: adw::ComboRow,
    glow: adw::ComboRow,
    diffusion: adw::ComboRow,
    diff_strength: Slider,
    diff_size: Slider,
    bloom: Slider,
    warmth: Slider,
    crop_button: gtk::ToggleButton,
    crop_group: adw::PreferencesGroup,
    aspect: adw::ComboRow,
    straighten: Slider,
    /// draws the crop frame over the photo and takes the drags
    crop_area: gtk::DrawingArea,
    halation: adw::SwitchRow,
    grain: adw::ComboRow,
    leak: adw::SwitchRow,
    mono: adw::SwitchRow,
}

struct App {
    ui: Ui,
    jobs: async_channel::Sender<Job>,
    sessions: RefCell<Vec<Arc<Session>>>,
    current: Cell<usize>,
    seq: Cell<u64>,
    /// bumped on every busy change, so a delayed "show the status box" knows if it's still wanted
    busy_token: Rc<Cell<u64>>,
    /// films in the current brand, as shown in the Film row
    brand_films: RefCell<Vec<String>>,
    updating: Cell<bool>,
    export_opts: RefCell<ExportOptions>,
    geometry: RefCell<Geometry>,
    /// crop shape turned to portrait
    portrait: Cell<bool>,
    /// the crop drag in progress: what was grabbed and the crop when it started
    drag: RefCell<Option<(Grab, Rect)>>,
    /// whether the newest render request / the picture on screen is the whole uncropped frame
    frame_requested: Cell<bool>,
    frame_shown: Cell<bool>,
    /// the last "found nothing" note shown
    last_note: RefCell<Option<String>>,
    /// colour mixer and grading values (the panel shows one colour / zone at a time)
    mixer: RefCell<[[f32; 3]; 8]>,
    grade: RefCell<[[f32; 2]; 3]>,
    /// undo history and where we are in it
    history: RefCell<Vec<Settings>>,
    history_pos: Cell<usize>,
    history_token: Rc<Cell<u64>>,
    /// red, green, blue and brightness counts of the picture on screen
    hist: RefCell<Option<[Vec<f32>; 4]>>,
    show_hist: Cell<bool>,
    me: RefCell<Weak<App>>,
    /// a preview render is on its way
    rendering: Cell<bool>,
    /// tone curve points per channel, and the point being dragged
    curve: RefCell<[[f32; 5]; 4]>,
    curve_drag: Cell<Option<(usize, f32)>>,
}

fn combo(title: &str, items: &[&str], selected: usize) -> adw::ComboRow {
    let row = adw::ComboRow::builder().title(title).use_markup(false).model(&gtk::StringList::new(items)).build();
    row.set_selected(selected as u32);
    row
}

fn spin(title: &str, min: f64, max: f64, step: f64, value: f64, digits: u32) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_use_markup(false);
    row.set_title(title);
    row.set_digits(digits);
    row.set_value(value);
    row
}

/// A row with a slider and its value; double-click the row to put it back.
#[derive(Clone)]
struct Slider {
    row: adw::ActionRow,
    scale: gtk::Scale,
}

impl std::ops::Deref for Slider {
    type Target = adw::ActionRow;
    fn deref(&self) -> &adw::ActionRow {
        &self.row
    }
}

impl Slider {
    fn value(&self) -> f64 {
        self.scale.value()
    }

    fn set_value(&self, v: f64) {
        self.scale.set_value(v);
    }

    fn connect_value_notify<F: Fn(&Slider) + 'static>(&self, f: F) {
        let me = self.clone();
        self.scale.connect_value_changed(move |_| f(&me));
    }
}

fn slider(title: &str, min: f64, max: f64, step: f64, value: f64, digits: u32) -> Slider {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    scale.set_value(value);
    scale.set_digits(digits as i32);
    scale.set_round_digits(digits as i32);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Left);
    scale.set_hexpand(true);
    scale.set_width_request(200);
    scale.set_valign(gtk::Align::Center);
    if value > min && value < max {
        scale.add_mark(value, gtk::PositionType::Bottom, None);
    }
    // the wheel scrolls the panel, not the slider (too easy to change a setting by accident)
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = scale.downgrade();
    wheel.connect_scroll(move |c, _, dy| {
        let sw = weak.upgrade().and_then(|s| s.ancestor(gtk::ScrolledWindow::static_type())).and_downcast::<gtk::ScrolledWindow>();
        if let Some(sw) = sw {
            let adj = sw.vadjustment();
            let px = if c.unit() == gdk::ScrollUnit::Surface { dy } else { dy * 60.0 };
            adj.set_value(adj.value() + px);
        }
        glib::Propagation::Stop
    });
    scale.add_controller(wheel);
    let row = adw::ActionRow::builder().title(title).use_markup(false).build();
    row.add_suffix(&scale);
    let reset = gtk::GestureClick::new();
    let weak = scale.downgrade();
    reset.connect_pressed(move |_, n, _, _| {
        if n == 2 {
            if let Some(s) = weak.upgrade() {
                s.set_value(value);
            }
        }
    });
    row.add_controller(reset);
    Slider { row, scale }
}

fn switch(title: &str, subtitle: &str) -> adw::SwitchRow {
    adw::SwitchRow::builder().title(title).subtitle(subtitle).use_markup(false).build()
}

fn texture(img: &Image) -> gdk::MemoryTexture {
    let bytes = glib::Bytes::from_owned(img.rgb_bytes());
    gdk::MemoryTexture::new(img.w as i32, img.h as i32, gdk::MemoryFormat::R8g8b8, &bytes, img.w * 3)
}

impl App {
    fn settings(&self) -> Settings {
        let u = &self.ui;
        let mut s = Settings::default();
        s.raw.exposure = u.exposure.value() as f32;
        s.raw.contrast = (u.contrast.value() / 100.0) as f32;
        let raw = self.sessions.borrow().get(self.current.get()).is_none_or(|x| x.is_raw());
        s.raw.kelvin = if raw {
            if u.wb.selected() == 0 { None } else { Some(u.kelvin.value() as f32) }
        } else if (u.kelvin.value() - JPEG_KELVIN).abs() > 1.0 {
            Some(u.kelvin.value() as f32)
        } else {
            None
        };
        s.raw.whites = (u.whites.value() / 100.0) as f32;
        s.raw.blacks = (u.blacks.value() / 100.0) as f32;
        let pc = |r: &Slider| (r.value() / 100.0) as f32;
        s.adjust = adjust::Adjust {
            vibrance: pc(&u.vibrance),
            saturation: pc(&u.saturation),
            mixer: *self.mixer.borrow(),
            grade: *self.grade.borrow(),
            balance: pc(&u.balance),
            clarity: pc(&u.clarity),
            texture: pc(&u.texture),
            dehaze: pc(&u.dehaze),
            sharpen: pc(&u.sharpen),
            vignette: pc(&u.vignette),
            curve: *self.curve.borrow(),
            vignette_mid: pc(&u.vig_mid),
            vignette_feather: pc(&u.vig_feather),
            sharpen_radius: u.sharpen_radius.value() as f32,
            sharpen_masking: pc(&u.masking),
            noise: pc(&u.noise),
            colour_noise: pc(&u.colour_noise),
            defringe: pc(&u.defringe),
        };
        s.local = local::Local {
            kind: u.local_kind.selected() as usize,
            exposure: u.local_exposure.value() as f32,
            contrast: pc(&u.local_contrast),
            saturation: pc(&u.local_saturation),
            warmth: pc(&u.local_warmth),
            size: pc(&u.local_size),
            feather: pc(&u.local_feather),
        };
        s.frame = u.frame.selected() as usize;
        s.raw.tint = (u.tint.value() / 100.0) as f32;
        s.raw.shadows = (u.shadows.value() / 100.0) as f32;
        s.raw.highlights = (u.highlights.value() / 100.0) as f32;
        s.camera = u.camera.selected() as usize;
        s.camera_strength = (u.cam_strength.value() / 100.0) as f32;
        s.date_stamp = u.stamp.is_active();
        let films = self.brand_films.borrow();
        s.film = if u.brand.selected() == 0 { None } else { films.get(u.film.selected() as usize).cloned() };
        s.strength = (u.strength.value() / 100.0) as f32;
        s.effect = u.effect.selected() as usize;
        s.target = u.target.selected() as usize;
        s.model = u.model.selected() as usize;
        s.look = u.look.selected() as usize;
        s.face_style = u.face.selected() as usize;
        s.amount = (u.amount.value() / 100.0) as f32;
        s.colour = u.colour.selected() as usize;
        s.glow = u.glow.selected() as usize;
        s.diffusion = u.diffusion.selected() as usize;
        s.diffusion_opt.strength = (u.diff_strength.value() / 100.0) as f32;
        s.diffusion_opt.size = (u.diff_size.value() / 100.0) as f32;
        s.diffusion_opt.bloom = (u.bloom.value() / 100.0) as f32;
        s.diffusion_opt.warmth = (u.warmth.value() / 100.0) as f32;
        s.geometry = *self.geometry.borrow();
        s.develop.halation = u.halation.is_active();
        s.develop.grain = u.grain.selected() as usize;
        s.develop.light_leak = u.leak.is_active();
        s.develop.monochrome = u.mono.is_active();
        s
    }

    /// Shows only the rows that matter for the chosen effect.
    fn update_rows(&self) {
        let u = &self.ui;
        let e = u.effect.selected() as usize;
        let subjects = pipeline::needs_model(e) && e != pipeline::FACE_MESH && e != pipeline::SKELETON;
        u.target.set_visible(subjects);
        u.model.set_visible(subjects || e == pipeline::SKELETON);
        u.look.set_visible(!pipeline::looks(e).is_empty());
        u.face.set_visible(e == pipeline::WIREFRAME || e == pipeline::FACE_MESH);
        u.amount.set_visible(e != pipeline::NONE && e != pipeline::EXTRA_FIRST + fx::extra::MIRROR);
        u.amount.set_title(match e {
            pipeline::GHOSTS => "Opacity",
            pipeline::WIREFRAME | pipeline::CONTOURS | pipeline::POINTS | pipeline::SKELETON | pipeline::FACE_MESH => "Dim behind",
            pipeline::TRAILS => "Paler trails",
            e if e >= pipeline::EXTRA_FIRST => "Strength",
            _ => "Amount",
        });
        let lit = matches!(e, pipeline::GHOSTS | pipeline::WIREFRAME | pipeline::CONTOURS | pipeline::FACE_MESH | pipeline::POINTS | pipeline::SKELETON | pipeline::TRAILS)
            || e == pipeline::SCENE_FIRST + fx::scene::RIM
            || e == pipeline::SCENE_FIRST + fx::scene::COMIC
            || e == pipeline::EXTRA_FIRST + fx::extra::NEON;
        let raw = self.sessions.borrow().get(self.current.get()).is_some_and(|x| x.is_raw());
        u.wb.set_visible(raw);
        let lk = u.local_kind.selected() as usize;
        u.local_size.set_visible(local::has_shape(lk));
        u.local_feather.set_visible(local::has_shape(lk));
        for r in [&u.local_exposure, &u.local_contrast, &u.local_saturation, &u.local_warmth] {
            r.set_visible(lk != 0);
        }
        u.sharpen_radius.set_visible(u.sharpen.value() > 0.0);
        u.masking.set_visible(u.sharpen.value() > 0.0);
        u.vig_mid.set_visible(u.vignette.value() != 0.0);
        u.vig_feather.set_visible(u.vignette.value() != 0.0);
        u.colour.set_visible(lit);
        u.glow.set_visible(matches!(e, pipeline::GHOSTS | pipeline::WIREFRAME | pipeline::CONTOURS | pipeline::FACE_MESH | pipeline::SKELETON));
        let diffusing = u.diffusion.selected() != 0;
        for r in [&u.diff_strength, &u.diff_size, &u.bloom, &u.warmth] {
            r.set_visible(diffusing);
        }
        u.cam_strength.set_sensitive(u.camera.selected() != 0);
        u.film.set_sensitive(u.brand.selected() != 0);
        u.strength.set_sensitive(u.brand.selected() != 0);
    }

    fn fill_films(&self) {
        let brand = self.ui.brand.selected() as usize;
        let brands = film::brands();
        let (ids, names): (Vec<String>, Vec<String>) = if brand == 0 {
            (Vec::new(), Vec::new())
        } else if brand == brands.len() + 1 {
            creative::NAMES.iter().enumerate().map(|(i, n)| (format!("gen:{i}"), n.to_string())).unzip()
        } else if brand > brands.len() {
            lut::list().into_iter().map(|(n, p)| (format!("lut:{}", p.display()), n)).unzip()
        } else {
            film::films()
                .into_iter()
                .filter(|f| f.brand == brands[brand - 1])
                .map(|f| (f.file.to_string(), if f.error < 0.0 { format!("{}  \u{25c6}", f.name) } else { f.name.to_string() }))
                .unzip()
        };
        let empty_luts = brand > brands.len() + 1 && ids.is_empty();
        self.ui.film.set_subtitle(if empty_luts { "Use Add above to bring in .cube files or a folder of them" } else { "" });
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let was = self.updating.replace(true);
        self.ui.film.set_model(Some(&gtk::StringList::new(&refs)));
        self.ui.film.set_selected(0);
        self.updating.set(was);
        *self.brand_films.borrow_mut() = ids;
    }

    /// With "As shot" chosen, shows the photo's own white balance on the Temperature slider.
    fn sync_wb(&self) {
        let sessions = self.sessions.borrow();
        let Some(s) = sessions.get(self.current.get()) else { return };
        if !s.is_raw() {
            self.ui.kelvin.set_subtitle("Kelvin. 6500 leaves the photo as it is");
            if self.ui.wb.selected() == 0 {
                let was = self.updating.replace(true);
                self.ui.kelvin.set_value(JPEG_KELVIN);
                self.updating.set(was);
            }
            return;
        }
        let shot = s.as_shot_kelvin();
        self.ui.kelvin.set_subtitle(&shot.map(|k| format!("Kelvin. As shot: {:.0} K", (k / 50.0).round() * 50.0)).unwrap_or("Kelvin".into()));
        if self.ui.wb.selected() == 0 {
            if let Some(k) = shot {
                let was = self.updating.replace(true);
                self.ui.kelvin.set_value(((k / 50.0).round() * 50.0) as f64);
                self.updating.set(was);
            }
        }
    }

    fn request_render(&self) {
        if self.updating.get() {
            return;
        }
        self.schedule_history();
        self.update_rows();
        let sessions = self.sessions.borrow().clone();
        if sessions.is_empty() {
            return;
        }
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        let mut settings = self.settings();
        if self.ui.before.is_active() {
            let (raw, geometry) = (settings.raw, settings.geometry);
            settings = Settings::default();
            settings.raw = raw;
            settings.geometry = geometry;
        }
        // while cropping: the whole frame, and no effect (they can be slow and would run again for each change)
        let cropping = self.cropping();
        if cropping {
            settings.geometry = settings.geometry.uncropped();
            settings.effect = pipeline::NONE;
        }
        self.frame_requested.set(cropping);
        let shown = &sessions[self.current.get().min(sessions.len() - 1)];
        let (title, sub) = if settings.effect == pipeline::DOUBLE && shown.needs_ai(&settings) {
            ("Double exposure: finding people".to_string(), "In the first photo; the second fills them".to_string())
        } else if pipeline::is_multi(settings.effect) {
            (format!("Combining {} photos", sessions.len()), String::new())
        } else if shown.needs_ai(&settings) {
            (ai_message(&settings), "The AI runs once, then changes are quick".to_string())
        } else {
            ("Updating".to_string(), String::new())
        };
        self.set_busy(Some((title, sub)));
        self.rendering.set(true);
        let _ = self.jobs.send_blocking(Job::Render { seq, sessions, current: self.current.get(), settings });
    }

    /// Shows what's being worked on (None = done). The box appears only if the work takes over a quarter second.
    fn set_busy(&self, what: Option<(String, String)>) {
        let token = self.busy_token.get() + 1;
        self.busy_token.set(token);
        let u = &self.ui;
        let Some((title, sub)) = what else {
            u.status.set_visible(false);
            u.picture.set_opacity(1.0);
            return;
        };
        u.status_title.set_text(&title);
        u.status_sub.set_text(&sub);
        u.status_sub.set_visible(!sub.is_empty());
        let (status, picture, current) = (u.status.clone(), u.picture.clone(), self.busy_token.clone());
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            if current.get() == token {
                status.set_visible(true);
                picture.set_opacity(0.55);
            }
        });
    }

    fn open(&self, paths: Vec<PathBuf>) {
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_file()).collect();
        if paths.is_empty() {
            return;
        }
        let first = paths[0].file_name().and_then(|n| n.to_str()).unwrap_or("photo").to_string();
        let title = if paths.len() == 1 { format!("Opening {first}") } else { format!("Opening {} photos", paths.len()) };
        let sub = if paths.iter().any(|p| grainhouse::io::is_raw(p)) { "Developing the RAW" } else { "" };
        self.set_busy(Some((title, sub.to_string())));
        self.ui.info.set_text(&format!("Opening {} photo{}...", paths.len(), if paths.len() == 1 { "" } else { "s" }));
        let _ = self.jobs.send_blocking(Job::Open(paths));
    }

    fn toast(&self, msg: &str) {
        self.ui.toasts.add_toast(adw::Toast::new(msg));
    }

    fn opened(self: &Rc<Self>, sessions: Vec<Arc<Session>>, errors: Vec<String>) {
        for e in &errors {
            self.toast(e);
        }
        if sessions.is_empty() {
            self.set_busy(None);
            return;
        }
        *self.sessions.borrow_mut() = sessions;
        self.current.set(0);
        self.ui.empty.set_visible(false);
        self.ui.export_button.set_sensitive(true);
        self.ui.raw_group.set_visible(true);
        self.sync_wb();
        self.ui.crop_group.set_visible(true);
        self.reset_crop();
        self.ui.crop_button.set_sensitive(true);
        self.rebuild_strip();
        *self.history.borrow_mut() = vec![self.settings()];
        self.history_pos.set(0);
        self.update_history_buttons();
        self.request_render();
    }

    fn rebuild_strip(self: &Rc<Self>) {
        let strip = &self.ui.strip;
        while let Some(c) = strip.first_child() {
            strip.remove(&c);
        }
        let sessions = self.sessions.borrow();
        self.ui.strip_scroll.set_visible(sessions.len() > 1);
        for (i, s) in sessions.iter().enumerate() {
            let name = s.path.file_name().and_then(|n| n.to_str()).unwrap_or("photo").to_string();
            let b = gtk::ToggleButton::with_label(&name);
            b.set_active(i == self.current.get());
            let app = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(app) = app.upgrade() {
                    app.current.set(i);
                    app.sync_wb();
                    app.rebuild_strip();
                    app.request_render();
                }
            });
            strip.append(&b);
        }
    }

    fn cropping(&self) -> bool {
        self.ui.crop_button.is_active()
    }

    /// Size of the turned and straightened frame of the photo on screen.
    fn frame(&self) -> (f32, f32) {
        let sessions = self.sessions.borrow();
        let Some(s) = sessions.get(self.current.get()) else { return (1.0, 1.0) };
        let (w, h) = s.size();
        self.geometry.borrow().frame(w, h)
    }

    /// The chosen crop shape as width / height in frame fractions (None = free).
    fn shape(&self) -> Option<f32> {
        let i = (self.ui.aspect.selected() as usize).min(RATIOS.len() - 1);
        if i == 0 {
            return None;
        }
        let (fw, fh) = self.frame();
        let r = if i == 1 { fw.max(fh) / fw.min(fh) } else { RATIOS[i] };
        let r = if self.portrait.get() { 1.0 / r } else { r };
        Some(r * fh / fw)
    }

    /// Makes the crop the chosen shape (as big as fits, around where it was).
    fn apply_shape(&self) {
        if let Some(k) = self.shape() {
            let mut g = self.geometry.borrow_mut();
            let r = geometry::fit_shape(g.rect(), k);
            g.set_rect(r);
        }
        self.ui.crop_area.queue_draw();
    }

    fn reset_crop(&self) {
        let was = self.updating.replace(true);
        self.ui.aspect.set_selected(0);
        self.ui.straighten.set_value(0.0);
        self.ui.crop_button.set_active(false);
        self.updating.set(was);
        *self.geometry.borrow_mut() = Geometry::default();
        self.portrait.set(false);
    }

    /// Starts cropping if not already (which re-renders), otherwise just re-renders.
    fn crop_changed(&self) {
        if self.cropping() {
            self.request_render();
        } else {
            self.ui.crop_button.set_active(true);
        }
    }

    /// Where the picture sits in the photo area, as (x, y, w, h) in pixels.
    fn shown_rect(&self) -> Option<(f64, f64, f64, f64)> {
        let p = self.ui.picture.paintable()?;
        let (iw, ih) = (p.intrinsic_width() as f64, p.intrinsic_height() as f64);
        let (ww, wh) = (self.ui.crop_area.width() as f64, self.ui.crop_area.height() as f64);
        if iw <= 0.0 || ih <= 0.0 || ww <= 0.0 || wh <= 0.0 {
            return None;
        }
        let k = (ww / iw).min(wh / ih);
        Some(((ww - iw * k) / 2.0, (wh - ih * k) / 2.0, iw * k, ih * k))
    }

    /// What a press at (px, py) grabs: a corner, an edge, the inside (move) or outside (draw a new crop).
    fn grab_at(&self, px: f64, py: f64) -> Option<(Grab, Rect, &'static str)> {
        let (ox, oy, dw, dh) = self.shown_rect()?;
        let r = self.geometry.borrow().rect();
        let (x0, y0) = (ox + r.x as f64 * dw, oy + r.y as f64 * dh);
        let (x1, y1) = (x0 + r.w as f64 * dw, y0 + r.h as f64 * dh);
        let edges = |reach: f64| {
            let left = (px - x0).abs() <= reach && (px - x0).abs() <= (px - x1).abs();
            let right = (px - x1).abs() <= reach && !left;
            let top = (py - y0).abs() <= reach && (py - y0).abs() <= (py - y1).abs();
            let bottom = (py - y1).abs() <= reach && !top;
            (left, top, right, bottom)
        };
        let (l, t, rt, b) = edges(22.0);
        if (l || rt) && (t || b) {
            let cursor = if l == t { "nwse-resize" } else { "nesw-resize" };
            return Some((Grab::Edges { left: l, top: t, right: rt, bottom: b }, r, cursor));
        }
        let (l, t, rt, b) = edges(12.0);
        let within_x = px > x0 && px < x1;
        let within_y = py > y0 && py < y1;
        if (l || rt) && within_y {
            return Some((Grab::Edges { left: l, top: false, right: rt, bottom: false }, r, "ew-resize"));
        }
        if (t || b) && within_x {
            return Some((Grab::Edges { left: false, top: t, right: false, bottom: b }, r, "ns-resize"));
        }
        if within_x && within_y {
            return Some((Grab::Move, r, "move"));
        }
        let at = Rect { x: ((px - ox) / dw).clamp(0.0, 1.0) as f32, y: ((py - oy) / dh).clamp(0.0, 1.0) as f32, w: 0.0, h: 0.0 };
        Some((Grab::Edges { left: false, top: false, right: true, bottom: true }, at, "crosshair"))
    }

    fn draw_crop(&self, cr: &gtk::cairo::Context) {
        if !self.cropping() || !self.frame_shown.get() {
            return;
        }
        let Some((ox, oy, dw, dh)) = self.shown_rect() else { return };
        let g = *self.geometry.borrow();
        let (x, y, w, h) = (ox + g.x as f64 * dw, oy + g.y as f64 * dh, g.w as f64 * dw, g.h as f64 * dh);
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        cr.rectangle(ox, oy, dw, dh);
        cr.rectangle(x, y, w, h);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
        let _ = cr.fill();
        // thirds, or a fine grid while straightening
        let n = if g.angle != 0.0 { 8 } else { 3 };
        cr.set_source_rgba(1.0, 1.0, 1.0, if n == 3 { 0.4 } else { 0.25 });
        cr.set_line_width(1.0);
        for i in 1..n {
            let f = i as f64 / n as f64;
            cr.move_to(x + w * f, y);
            cr.line_to(x + w * f, y + h);
            cr.move_to(x, y + h * f);
            cr.line_to(x + w, y + h * f);
        }
        let _ = cr.stroke();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
        cr.set_line_width(1.5);
        cr.rectangle(x, y, w, h);
        let _ = cr.stroke();
        cr.set_line_width(4.0);
        let l = 20f64.min(w / 2.0).min(h / 2.0);
        for (cx, cy, sx, sy) in [(x, y, 1.0, 1.0), (x + w, y, -1.0, 1.0), (x, y + h, 1.0, -1.0), (x + w, y + h, -1.0, -1.0)] {
            cr.move_to(cx, cy + sy * l);
            cr.line_to(cx, cy);
            cr.line_to(cx + sx * l, cy);
        }
        let _ = cr.stroke();
    }

    /// Fills the Look row with the effect's choices, keeping `keep` if it fits.
    fn fill_looks(&self, keep: Option<usize>) {
        let e = self.ui.effect.selected() as usize;
        let names = pipeline::looks(e);
        let was = self.updating.replace(true);
        self.ui.look.set_model(Some(&gtk::StringList::new(names)));
        let default = if e == pipeline::GHOSTS { 1 } else { 0 };
        let pick = keep.filter(|&k| k < names.len()).unwrap_or(default.min(names.len().saturating_sub(1)));
        self.ui.look.set_selected(pick as u32);
        self.updating.set(was);
    }

    /// Shows the mixer and grading sliders for the chosen colour and zone.
    fn show_mixer(&self) {
        let u = &self.ui;
        let was = self.updating.replace(true);
        let m = self.mixer.borrow()[(u.mix_colour.selected() as usize).min(7)];
        u.mix_hue.set_value((m[0] * 100.0) as f64);
        u.mix_sat.set_value((m[1] * 100.0) as f64);
        u.mix_lum.set_value((m[2] * 100.0) as f64);
        let g = self.grade.borrow()[(u.grade_zone.selected() as usize).min(2)];
        u.grade_hue.set_value(g[0] as f64);
        u.grade_amount.set_value((g[1] * 100.0) as f64);
        self.updating.set(was);
    }

    /// Puts settings into the panel (for presets, undo and reset), then renders.
    fn apply_settings(&self, s: &Settings) {
        let u = &self.ui;
        let was = self.updating.replace(true);
        u.exposure.set_value(s.raw.exposure as f64);
        u.contrast.set_value((s.raw.contrast * 100.0) as f64);
        let raw = self.sessions.borrow().get(self.current.get()).is_some_and(|x| x.is_raw());
        match s.raw.kelvin {
            None => {
                u.wb.set_selected(0);
            }
            Some(k) => {
                let preset = whitebalance::PRESET_KELVIN.iter().position(|p| *p > 0.0 && (*p - k).abs() < 1.0);
                u.wb.set_selected(if raw { preset.unwrap_or(whitebalance::CUSTOM) as u32 } else { whitebalance::CUSTOM as u32 });
                u.kelvin.set_value(k as f64);
            }
        }
        let pc = |r: &Slider, v: f32| r.set_value((v * 100.0) as f64);
        pc(&u.tint, s.raw.tint);
        pc(&u.shadows, s.raw.shadows);
        pc(&u.highlights, s.raw.highlights);
        pc(&u.whites, s.raw.whites);
        pc(&u.blacks, s.raw.blacks);
        u.camera.set_selected(s.camera as u32);
        pc(&u.cam_strength, s.camera_strength);
        u.stamp.set_active(s.date_stamp);
        // film: find its brand, fill the list, pick it
        let brands = film::brands();
        let brand = match s.film.as_deref() {
            None => 0,
            Some(f) if f.starts_with("gen:") => brands.len() + 1,
            Some(f) if f.starts_with("lut:") => brands.len() + 2,
            Some(f) => film::films().into_iter().find(|x| x.file == f).and_then(|x| brands.iter().position(|b| *b == x.brand)).map_or(0, |i| i + 1),
        };
        u.brand.set_selected(brand as u32);
        self.fill_films();
        if let Some(f) = &s.film {
            let pos = self.brand_films.borrow().iter().position(|x| x == f);
            if let Some(pos) = pos {
                u.film.set_selected(pos as u32);
            }
        }
        pc(&u.strength, s.strength);
        let a = &s.adjust;
        pc(&u.vibrance, a.vibrance);
        pc(&u.saturation, a.saturation);
        *self.mixer.borrow_mut() = a.mixer;
        *self.grade.borrow_mut() = a.grade;
        pc(&u.balance, a.balance);
        pc(&u.clarity, a.clarity);
        pc(&u.texture, a.texture);
        pc(&u.dehaze, a.dehaze);
        pc(&u.sharpen, a.sharpen);
        pc(&u.vignette, a.vignette);
        *self.curve.borrow_mut() = a.curve;
        self.ui.curve_area.queue_draw();
        pc(&u.vig_mid, a.vignette_mid);
        pc(&u.vig_feather, a.vignette_feather);
        u.sharpen_radius.set_value(a.sharpen_radius as f64);
        pc(&u.masking, a.sharpen_masking);
        pc(&u.noise, a.noise);
        pc(&u.colour_noise, a.colour_noise);
        pc(&u.defringe, a.defringe);
        let l = &s.local;
        u.local_kind.set_selected(l.kind as u32);
        u.local_exposure.set_value(l.exposure as f64);
        pc(&u.local_contrast, l.contrast);
        pc(&u.local_saturation, l.saturation);
        pc(&u.local_warmth, l.warmth);
        pc(&u.local_size, l.size);
        pc(&u.local_feather, l.feather);
        pc(&u.distortion, s.geometry.distortion);
        pc(&u.vertical, s.geometry.vertical);
        pc(&u.horizontal, s.geometry.horizontal);
        u.effect.set_selected(s.effect as u32);
        self.fill_looks(Some(s.look));
        u.target.set_selected(s.target as u32);
        u.model.set_selected(s.model as u32);
        u.face.set_selected(s.face_style as u32);
        pc(&u.amount, s.amount);
        u.colour.set_selected(s.colour as u32);
        u.glow.set_selected(s.glow as u32);
        u.diffusion.set_selected(s.diffusion as u32);
        pc(&u.diff_strength, s.diffusion_opt.strength);
        pc(&u.diff_size, s.diffusion_opt.size);
        pc(&u.bloom, s.diffusion_opt.bloom);
        pc(&u.warmth, s.diffusion_opt.warmth);
        u.halation.set_active(s.develop.halation);
        u.grain.set_selected(s.develop.grain as u32);
        u.leak.set_active(s.develop.light_leak);
        u.mono.set_active(s.develop.monochrome);
        u.frame.set_selected(s.frame as u32);
        *self.geometry.borrow_mut() = s.geometry;
        u.straighten.set_value(s.geometry.angle as f64);
        if !s.geometry.is_cropped() {
            u.aspect.set_selected(0);
        }
        self.updating.set(was);
        self.show_mixer();
        if s.raw.kelvin.is_none() {
            self.sync_wb();
        }
        self.request_render();
    }

    /// Remembers the settings for undo once they've stopped changing for a moment.
    fn schedule_history(&self) {
        let token = self.history_token.get() + 1;
        self.history_token.set(token);
        let (current, me) = (self.history_token.clone(), self.me.borrow().clone());
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            if current.get() != token {
                return;
            }
            let Some(a) = me.upgrade() else { return };
            if a.sessions.borrow().is_empty() {
                return;
            }
            let s = a.settings();
            let pos = a.history_pos.get();
            let mut h = a.history.borrow_mut();
            if h.get(pos) == Some(&s) {
                return;
            }
            h.truncate(pos + 1);
            h.push(s);
            if h.len() > 200 {
                h.remove(0);
            }
            a.history_pos.set(h.len() - 1);
            drop(h);
            a.update_history_buttons();
        });
    }

    fn update_history_buttons(&self) {
        let pos = self.history_pos.get();
        self.ui.undo.set_sensitive(pos > 0);
        self.ui.redo.set_sensitive(pos + 1 < self.history.borrow().len());
    }

    fn step_history(&self, back: bool) {
        let pos = self.history_pos.get();
        let len = self.history.borrow().len();
        let to = if back { pos.checked_sub(1) } else { (pos + 1 < len).then_some(pos + 1) };
        let Some(to) = to else { return };
        self.history_pos.set(to);
        let s = self.history.borrow()[to].clone();
        self.update_history_buttons();
        self.apply_settings(&s);
    }

    fn draw_histogram(&self, cr: &gtk::cairo::Context, w: f64, h: f64) {
        if !self.show_hist.get() || self.sessions.borrow().is_empty() {
            return;
        }
        let Some(hist) = &*self.hist.borrow() else { return };
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
        let r = 8.0;
        cr.new_sub_path();
        cr.arc(w - r, r, r, -std::f64::consts::FRAC_PI_2, 0.0);
        cr.arc(w - r, h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
        cr.arc(r, h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
        cr.arc(r, r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
        cr.close_path();
        let _ = cr.fill();
        let (pad, gw, gh) = (8.0, w - 16.0, h - 16.0);
        let colours = [(1.0, 0.3, 0.3), (0.3, 1.0, 0.4), (0.35, 0.55, 1.0), (0.9, 0.9, 0.9)];
        for (c, (r, g, b)) in hist.iter().zip(colours) {
            cr.set_source_rgba(r, g, b, 0.4);
            cr.move_to(pad, pad + gh);
            for (i, v) in c.iter().enumerate() {
                cr.line_to(pad + gw * i as f64 / 255.0, pad + gh * (1.0 - *v as f64));
            }
            cr.line_to(pad + gw, pad + gh);
            cr.close_path();
            let _ = cr.fill();
        }
    }

    fn draw_curve(&self, cr: &gtk::cairo::Context, w: f64, h: f64) {
        let ch = (self.ui.curve_channel.selected() as usize).min(3);
        let (pad, gw, gh) = (8.0, w - 16.0, h - 16.0);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
        cr.rectangle(pad, pad, gw, gh);
        let _ = cr.fill();
        // the brightness histogram behind, faintly
        if let Some(hist) = &*self.hist.borrow() {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
            cr.move_to(pad, pad + gh);
            for (i, v) in hist[3].iter().enumerate() {
                cr.line_to(pad + gw * i as f64 / 255.0, pad + gh * (1.0 - *v as f64));
            }
            cr.line_to(pad + gw, pad + gh);
            cr.close_path();
            let _ = cr.fill();
        }
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
        cr.set_line_width(1.0);
        for i in 1..4 {
            let f = i as f64 / 4.0;
            cr.move_to(pad + gw * f, pad);
            cr.line_to(pad + gw * f, pad + gh);
            cr.move_to(pad, pad + gh * f);
            cr.line_to(pad + gw, pad + gh * f);
        }
        cr.move_to(pad, pad + gh);
        cr.line_to(pad + gw, pad);
        let _ = cr.stroke();
        let (r, g, b) = [(0.95, 0.95, 0.95), (1.0, 0.4, 0.4), (0.4, 1.0, 0.5), (0.45, 0.6, 1.0)][ch];
        let pts = self.curve.borrow()[ch];
        let table = adjust::curve_table(&pts);
        cr.set_source_rgba(r, g, b, 0.95);
        cr.set_line_width(2.0);
        for (i, v) in table.iter().enumerate() {
            let (x, y) = (pad + gw * i as f64 / 255.0, pad + gh * (1.0 - *v as f64));
            if i == 0 { cr.move_to(x, y) } else { cr.line_to(x, y) }
        }
        let _ = cr.stroke();
        for (i, v) in pts.iter().enumerate() {
            cr.arc(pad + gw * adjust::CURVE_X[i] as f64, pad + gh * (1.0 - *v as f64), 5.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    }

    /// Fills the presets popover: built-in looks, the user's own (with delete), and saving the current look.
    fn fill_presets(&self) {
        let b = &self.ui.presets_box;
        while let Some(c) = b.first_child() {
            b.remove(&c);
        }
        let me = self.me.borrow().clone();
        let heading = |t: &str| gtk::Label::builder().label(t).halign(gtk::Align::Start).css_classes(["heading"]).margin_top(6).margin_bottom(2).build();
        b.append(&heading("Built in"));
        let flow = gtk::FlowBox::builder().max_children_per_line(2).min_children_per_line(2).selection_mode(gtk::SelectionMode::None).column_spacing(4).row_spacing(4).build();
        for (name, preset) in presets::builtin() {
            let button = gtk::Button::builder().label(name).css_classes(["flat"]).build();
            let me = me.clone();
            button.connect_clicked(move |_| {
                if let Some(a) = me.upgrade() {
                    let s = presets::apply(&a.settings(), &preset);
                    a.apply_settings(&s);
                    a.toast(&format!("Preset: {name}"));
                }
            });
            flow.append(&button);
        }
        b.append(&flow);
        let user = presets::user();
        if !user.is_empty() {
            b.append(&heading("Yours"));
            for (name, path) in user {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                let apply = gtk::Button::builder().label(&name).css_classes(["flat"]).hexpand(true).build();
                let del = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Delete this preset").css_classes(["flat"]).build();
                let (me2, p2, n2) = (me.clone(), path.clone(), name.clone());
                apply.connect_clicked(move |_| {
                    let Some(a) = me2.upgrade() else { return };
                    match presets::load(&p2) {
                        Ok(p) => {
                            let s = presets::apply(&a.settings(), &p);
                            a.apply_settings(&s);
                            a.toast(&format!("Preset: {n2}"));
                        }
                        Err(e) => a.toast(&e),
                    }
                });
                let me3 = me.clone();
                del.connect_clicked(move |_| {
                    let _ = std::fs::remove_file(&path);
                    if let Some(a) = me3.upgrade() {
                        a.fill_presets();
                    }
                });
                row.append(&apply);
                row.append(&del);
                b.append(&row);
            }
        }
        b.append(&heading("Save the current look"));
        let save_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let entry = gtk::Entry::builder().placeholder_text("Preset name").hexpand(true).build();
        let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
        let do_save = {
            let (me, entry) = (me.clone(), entry.clone());
            move || {
                let Some(a) = me.upgrade() else { return };
                match presets::save(&entry.text(), &a.settings()) {
                    Ok(_) => {
                        a.toast(&format!("Saved preset {}", entry.text()));
                        a.fill_presets();
                    }
                    Err(e) => a.toast(&e),
                }
            }
        };
        let f = Rc::new(do_save);
        let f2 = f.clone();
        save.connect_clicked(move |_| f());
        entry.connect_activate(move |_| f2());
        save_row.append(&entry);
        save_row.append(&save);
        b.append(&save_row);
    }

    fn rendered(&self, seq: u64, result: Result<Image, String>, ms: u128, note: Option<String>) {
        if seq != self.seq.get() {
            return; // a newer render is on its way
        }
        self.rendering.set(false);
        self.set_busy(None);
        // say once when an effect finds nothing (not again on every slider move)
        if note != *self.last_note.borrow() {
            if let Some(n) = &note {
                self.toast(n);
            }
            *self.last_note.borrow_mut() = note;
        }
        match result {
            Ok(img) => {
                self.ui.picture.set_paintable(Some(&texture(&img)));
                *self.hist.borrow_mut() = Some(histogram(&img));
                self.ui.histogram.queue_draw();
                self.ui.curve_area.queue_draw();
                self.frame_shown.set(self.frame_requested.get());
                self.ui.crop_area.queue_draw();
                let sessions = self.sessions.borrow();
                if let Some(s) = sessions.get(self.current.get()) {
                    let (w, h) = s.output_size(&self.settings());
                    let name = s.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let raw = if s.is_raw() { "RAW, " } else { "" };
                    self.ui.info.set_text(&format!("{name}   {raw}{w} x {h}   preview in {:.1} s", ms as f32 / 1000.0));
                }
            }
            Err(e) => self.toast(&e),
        }
    }
}

/// Temperature that leaves a JPEG as it is.
const JPEG_KELVIN: f64 = 6500.0;

/// Red, green, blue and brightness histograms (0..1, scaled to the busiest level), from every few pixels.
fn histogram(img: &Image) -> [Vec<f32>; 4] {
    let mut c = [vec![0u32; 256], vec![0u32; 256], vec![0u32; 256], vec![0u32; 256]];
    let step = (img.px.len() / 200_000).max(1);
    for p in img.px.iter().step_by(step) {
        let (r, g, b) = ((p >> 16) & 255, (p >> 8) & 255, p & 255);
        c[0][r as usize] += 1;
        c[1][g as usize] += 1;
        c[2][b as usize] += 1;
        c[3][((r * 77 + g * 150 + b * 29) >> 8) as usize] += 1;
    }
    // ignore the very ends when scaling, so clipped black or white doesn't flatten the rest
    let max = c.iter().map(|h| h[1..255].iter().copied().max().unwrap_or(1)).max().unwrap_or(1).max(1) as f32;
    c.map(|h| h.iter().map(|&v| (v as f32 / max).min(1.0).sqrt()).collect())
}

/// What the AI is looking for, for the status box.
fn ai_message(s: &Settings) -> String {
    let effect = pipeline::EFFECTS[s.effect];
    match s.effect {
        pipeline::SKELETON => format!("{effect}: finding poses"),
        pipeline::FACE_MESH => format!("{effect}: finding faces"),
        _ => format!("{effect}: finding {}", ai::TARGET_NAMES[s.target.min(ai::TARGET_NAMES.len() - 1)].to_lowercase()),
    }
}

/// The background worker: opens photos and renders previews, always skipping to the newest request.
fn worker(jobs: async_channel::Receiver<Job>, done: async_channel::Sender<Done>) {
    std::thread::spawn(move || {
        while let Ok(mut job) = jobs.recv_blocking() {
            // only the newest render matters; opens are never skipped
            if matches!(job, Job::Render { .. }) {
                while let Ok(next) = jobs.try_recv() {
                    if matches!(next, Job::Open(_)) {
                        let _ = done.send_blocking(run_open(next));
                        continue;
                    }
                    job = next;
                }
            }
            let out = match job {
                Job::Open(_) => run_open(job),
                Job::Render { seq, sessions, current, settings } => {
                    let t0 = Instant::now();
                    let shown = &sessions[current.min(sessions.len() - 1)];
                    let result = if pipeline::is_multi(settings.effect) {
                        let refs: Vec<&Session> = sessions.iter().map(|s| s.as_ref()).collect();
                        pipeline::render_multi(&refs, &settings, PREVIEW)
                    } else {
                        shown.render(&settings, PREVIEW)
                    };
                    let note = if result.is_ok() { shown.nothing_found(&settings) } else { None };
                    Done::Rendered { seq, result, ms: t0.elapsed().as_millis(), note }
                }
            };
            if done.send_blocking(out).is_err() {
                return;
            }
        }
    });
}

fn run_open(job: Job) -> Done {
    let Job::Open(paths) = job else { unreachable!() };
    let mut sessions = Vec::new();
    let mut errors = Vec::new();
    for p in paths {
        match Session::open(&p) {
            Ok(s) => sessions.push(Arc::new(s)),
            Err(e) => errors.push(e),
        }
    }
    Done::Opened(sessions, errors)
}

fn build(app: &adw::Application) {
    let application = app.clone();
    // the icon comes from the icon theme once installed; when run from the source folder, from data/
    if let Some(display) = gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        if !theme.has_icon(APP_ID) {
            theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data"));
        }
    }
    gtk::Window::set_default_icon_name(APP_ID);
    let window = adw::ApplicationWindow::builder().application(app).title("Grainhouse").default_width(1400).default_height(900).build();
    let toasts = adw::ToastOverlay::new();

    // the photo
    let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).hexpand(true).vexpand(true).build();
    let empty = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .title("Drop photos here")
        .description("RAW files (ARW, DNG, CR2, NEF and more), JPEG, PNG or TIFF.\nDrop several for Crowd remover, Trails or Double exposure.")
        .build();
    let status = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["osd", "card"])
        .visible(false)
        .build();
    let status_spinner = adw::Spinner::new();
    status_spinner.set_size_request(40, 40);
    status_spinner.set_margin_top(18);
    let status_title = gtk::Label::builder().css_classes(["title-4"]).margin_start(28).margin_end(28).build();
    let status_sub = gtk::Label::builder().css_classes(["dim-label", "caption"]).margin_start(28).margin_end(28).build();
    let status_pad = gtk::Box::new(gtk::Orientation::Vertical, 0);
    status_pad.set_margin_bottom(8);
    status.append(&status_spinner);
    status.append(&status_title);
    status.append(&status_sub);
    status.append(&status_pad);
    let info = gtk::Label::builder().halign(gtk::Align::Start).valign(gtk::Align::End).margin_start(12).margin_bottom(8).css_classes(["dim-label", "caption"]).build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    let crop_area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).can_target(false).build();
    overlay.add_overlay(&crop_area);
    let histogram = gtk::DrawingArea::builder().height_request(96).margin_start(12).margin_end(12).margin_top(10).can_target(false).build();
    overlay.add_overlay(&empty);
    overlay.add_overlay(&status);
    overlay.add_overlay(&info);
    let strip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    strip.set_margin_start(8);
    strip.set_margin_end(8);
    strip.set_margin_top(6);
    strip.set_margin_bottom(6);
    let strip_scroll = gtk::ScrolledWindow::builder().child(&strip).vscrollbar_policy(gtk::PolicyType::Never).visible(false).build();
    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.append(&overlay);
    left.append(&strip_scroll);

    // the settings
    let raw_group = adw::PreferencesGroup::builder().title("Light").visible(false).build();
    let auto_button = gtk::Button::builder().label("Auto").tooltip_text("Set exposure, shadows, highlights, whites and blacks from the photo").css_classes(["flat"]).valign(gtk::Align::Center).build();
    raw_group.set_header_suffix(Some(&auto_button));
    let wb = combo("White balance", &whitebalance::PRESETS, 0);
    let kelvin = slider("Temperature", 2000.0, 12000.0, 50.0, 5500.0, 0);
    kelvin.set_subtitle("Kelvin");
    let tint = slider("Tint", -100.0, 100.0, 1.0, 0.0, 0);
    tint.set_subtitle("Green to magenta");
    let exposure = slider("Exposure", -3.0, 3.0, 0.05, 0.0, 2);
    let contrast = slider("Contrast", 0.0, 200.0, 1.0, 100.0, 0);
    contrast.set_subtitle("100 is as shot (for RAW, the camera's own look)");
    let shadows = slider("Shadows", -100.0, 100.0, 1.0, 0.0, 0);
    shadows.set_subtitle("Lift or deepen the dark areas (D-Range)");
    let highlights = slider("Highlights", -100.0, 100.0, 1.0, 0.0, 0);
    highlights.set_subtitle("Below 0 brings back bright detail");
    let whites = slider("Whites", -100.0, 100.0, 1.0, 0.0, 0);
    let blacks = slider("Blacks", -100.0, 100.0, 1.0, 0.0, 0);
    blacks.set_subtitle("Below 0 deepens, above 0 fades");
    raw_group.add(&wb);
    for r in [&kelvin, &tint, &exposure, &contrast, &shadows, &highlights, &whites, &blacks] {
        raw_group.add(&r.row);
    }

    let curve_group = adw::PreferencesGroup::builder().title("Tone curve").description("Drag the points; double-click to reset this channel").build();
    let curve_channel = combo("Channel", &adjust::CURVE_CHANNELS, 0);
    let curve_area = gtk::DrawingArea::builder().height_request(230).margin_top(6).build();
    curve_group.add(&curve_channel);
    curve_group.add(&curve_area);

    let lens_fix_group = adw::PreferencesGroup::builder().title("Lens corrections").description("Crops in a little so there are no empty corners").build();
    let distortion = slider("Distortion", -100.0, 100.0, 1.0, 0.0, 0);
    distortion.set_subtitle("Above 0 straightens barrel distortion");
    let vertical = slider("Vertical", -100.0, 100.0, 1.0, 0.0, 0);
    vertical.set_subtitle("Straightens buildings that lean in");
    let horizontal = slider("Horizontal", -100.0, 100.0, 1.0, 0.0, 0);
    let defringe = slider("Defringe", 0.0, 100.0, 1.0, 0.0, 0);
    defringe.set_subtitle("Removes purple and green edges");
    for r in [&distortion, &vertical, &horizontal, &defringe] {
        lens_fix_group.add(&r.row);
    }

    let local_group = adw::PreferencesGroup::builder().title("Local adjustment").description("Change just part of the picture").build();
    let local_kind = combo("Mask", &local::KINDS, 0);
    let local_size = slider("Size", 0.0, 100.0, 1.0, 50.0, 0);
    local_size.set_subtitle("How far in, or how big");
    let local_feather = slider("Feather", 0.0, 100.0, 1.0, 50.0, 0);
    let local_exposure = slider("Exposure", -3.0, 3.0, 0.05, 0.0, 2);
    let local_contrast = slider("Contrast", -100.0, 100.0, 1.0, 0.0, 0);
    let local_saturation = slider("Saturation", -100.0, 100.0, 1.0, 0.0, 0);
    let local_warmth = slider("Warmth", -100.0, 100.0, 1.0, 0.0, 0);
    local_group.add(&local_kind);
    for r in [&local_size, &local_feather, &local_exposure, &local_contrast, &local_saturation, &local_warmth] {
        local_group.add(&r.row);
    }

    let colour_group = adw::PreferencesGroup::builder().title("Colour").build();
    let vibrance = slider("Vibrance", -100.0, 100.0, 1.0, 0.0, 0);
    vibrance.set_subtitle("Muted colours more, skin less");
    let saturation = slider("Saturation", -100.0, 100.0, 1.0, 0.0, 0);
    let mixer_row = adw::ExpanderRow::builder().title("Colour mixer").subtitle("Hue, saturation and brightness of each colour").build();
    let mix_colour = combo("Colour", &adjust::MIXER_NAMES, 0);
    let mix_hue = slider("Hue", -100.0, 100.0, 1.0, 0.0, 0);
    let mix_sat = slider("Saturation", -100.0, 100.0, 1.0, 0.0, 0);
    let mix_lum = slider("Luminance", -100.0, 100.0, 1.0, 0.0, 0);
    mixer_row.add_row(&mix_colour);
    for r in [&mix_hue, &mix_sat, &mix_lum] {
        mixer_row.add_row(&r.row);
    }
    let grade_row = adw::ExpanderRow::builder().title("Colour grading").subtitle("Tint the shadows, midtones and highlights").build();
    let grade_zone = combo("Zone", &adjust::GRADE_NAMES, 0);
    let grade_hue = slider("Hue", 0.0, 360.0, 1.0, 0.0, 0);
    let grade_amount = slider("Amount", 0.0, 100.0, 1.0, 0.0, 0);
    let balance = slider("Balance", -100.0, 100.0, 1.0, 0.0, 0);
    balance.set_subtitle("Towards the shadows or the highlights");
    grade_row.add_row(&grade_zone);
    for r in [&grade_hue, &grade_amount, &balance] {
        grade_row.add_row(&r.row);
    }
    colour_group.add(&vibrance.row);
    colour_group.add(&saturation.row);
    colour_group.add(&mixer_row);
    colour_group.add(&grade_row);

    let detail_group = adw::PreferencesGroup::builder().title("Detail").build();
    let clarity = slider("Clarity", -100.0, 100.0, 1.0, 0.0, 0);
    clarity.set_subtitle("Midtone punch");
    let texture_s = slider("Texture", -100.0, 100.0, 1.0, 0.0, 0);
    texture_s.set_subtitle("Fine detail; below 0 smooths skin");
    let dehaze = slider("Dehaze", -100.0, 100.0, 1.0, 0.0, 0);
    let sharpen = slider("Sharpen", 0.0, 100.0, 1.0, 0.0, 0);
    let vignette = slider("Vignette", -100.0, 100.0, 1.0, 0.0, 0);
    vignette.set_subtitle("Below 0 darkens the corners");
    let sharpen_radius = slider("Radius", 0.5, 3.0, 0.1, 1.0, 1);
    let masking = slider("Masking", 0.0, 100.0, 1.0, 0.0, 0);
    masking.set_subtitle("Sharpen edges only, not skin or sky");
    let noise = slider("Noise reduction", 0.0, 100.0, 1.0, 0.0, 0);
    let colour_noise = slider("Colour noise", 0.0, 100.0, 1.0, 0.0, 0);
    let vig_mid = slider("Midpoint", 0.0, 100.0, 1.0, 50.0, 0);
    let vig_feather = slider("Feather", 0.0, 100.0, 1.0, 50.0, 0);
    for r in [&clarity, &texture_s, &dehaze, &sharpen, &sharpen_radius, &masking, &noise, &colour_noise, &vignette, &vig_mid, &vig_feather] {
        detail_group.add(&r.row);
    }

    let frame_group = adw::PreferencesGroup::builder().title("Frame").build();
    let frame_row = combo("Frame", &frame::NAMES, 0);
    frame_row.set_subtitle("Polaroid, Instax and 6x6 crop to their shape");
    frame_group.add(&frame_row);

    let camera_group = adw::PreferencesGroup::builder().title("Camera").description("Looks in the spirit of other cameras' colour and character").build();
    let camera_row = combo("Camera", &camera::names(), 0);
    camera_row.set_enable_search(true);
    let cam_strength = slider("Strength", 0.0, 100.0, 1.0, 100.0, 0);
    let stamp = switch("Date stamp", "Orange date in the corner, from when the photo was taken");
    camera_group.add(&camera_row);
    camera_group.add(&cam_strength.row);
    camera_group.add(&stamp);

    let film_group = adw::PreferencesGroup::builder()
        .title("Film")
        .description("153 film stocks (\u{25c6} = designed looks), 24 Creative looks, and your own .cube files under My LUTs")
        .build();
    let lut_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    let add_lut = gtk::Button::builder().label("Add LUTs\u{2026}").tooltip_text("Add .cube files to My LUTs").css_classes(["flat"]).valign(gtk::Align::Center).build();
    let add_folder = gtk::Button::builder().icon_name("folder-symbolic").tooltip_text("Add every .cube file in a folder").css_classes(["flat"]).valign(gtk::Align::Center).build();
    lut_buttons.append(&add_lut);
    lut_buttons.append(&add_folder);
    film_group.set_header_suffix(Some(&lut_buttons));
    let mut brands = vec!["None"];
    brands.extend(film::brands());
    brands.push("Creative");
    brands.push("My LUTs");
    let lut_brand = brands.len() as u32 - 1;
    let brand = combo("Brand", &brands, 0);
    let film_row = combo("Film", &[], 0);
    film_row.set_enable_search(true);
    let strength = slider("Strength", 0.0, 100.0, 1.0, 100.0, 0);
    strength.set_subtitle("How much of the film look");
    film_group.add(&brand);
    film_group.add(&film_row);
    film_group.add(&strength.row);

    let fx_group = adw::PreferencesGroup::builder().title("Effect").build();
    let effect = combo("Effect", &pipeline::EFFECTS, 0);
    effect.set_enable_search(true);
    let target = combo("Target", &ai::TARGET_NAMES, 0);
    let model = combo("AI model", &ai::MODEL_NAMES, 1);
    let look = combo("Look", &fx::ghost::LOOKS, 1);
    let face = combo("Face", &fx::mesh::FACE_STYLES, 0);
    let amount = slider("Amount", 0.0, 100.0, 1.0, 85.0, 0);
    let colour = combo("Colour", &fx::COLOURS, 0);
    let glow = combo("Glow", &fx::GLOW_NAMES, 2);
    for r in [&effect, &target, &model, &look, &face] {
        fx_group.add(r);
    }
    fx_group.add(&amount.row);
    fx_group.add(&colour);
    fx_group.add(&glow);

    let lens_group = adw::PreferencesGroup::builder().title("Diffusion filter").build();
    let diffusion = combo("Filter", &fx::diffusion::NAMES, 0);
    let diff_strength = slider("Strength", 0.0, 200.0, 1.0, 100.0, 0);
    let diff_size = slider("Glow size", 25.0, 300.0, 1.0, 100.0, 0);
    let bloom = slider("Highlight bloom", 0.0, 200.0, 1.0, 100.0, 0);
    let warmth = slider("Warmth", -100.0, 100.0, 1.0, 0.0, 0);
    diff_strength.set_subtitle("How much light spreads");
    diff_size.set_subtitle("How far the glow reaches");
    bloom.set_subtitle("Extra glow round lamps and bright spots");
    warmth.set_subtitle("Cool blue to warm orange glow");
    lens_group.add(&diffusion);
    for r in [&diff_strength, &diff_size, &bloom, &warmth] {
        lens_group.add(&r.row);
    }

    let crop_group = adw::PreferencesGroup::builder()
        .title("Crop")
        .description("Press Crop at the top, then drag on the photo. Effects are hidden while cropping.")
        .visible(false)
        .build();
    let reset_crop = gtk::Button::builder().label("Reset").css_classes(["flat"]).valign(gtk::Align::Center).build();
    crop_group.set_header_suffix(Some(&reset_crop));
    let aspect = combo("Aspect", &ASPECTS, 0);
    let swap = gtk::Button::builder().label("Swap").tooltip_text("Portrait or landscape").css_classes(["flat"]).valign(gtk::Align::Center).build();
    aspect.add_suffix(&swap);
    let straighten = slider("Straighten", -20.0, 20.0, 0.1, 0.0, 1);
    straighten.set_subtitle("Degrees, the grid helps line it up");
    let rotate_row = adw::ActionRow::builder().title("Rotate").build();
    let rot_left = gtk::Button::builder().icon_name("object-rotate-left-symbolic").tooltip_text("Turn left").css_classes(["flat"]).valign(gtk::Align::Center).build();
    let rot_right = gtk::Button::builder().icon_name("object-rotate-right-symbolic").tooltip_text("Turn right").css_classes(["flat"]).valign(gtk::Align::Center).build();
    rotate_row.add_suffix(&rot_left);
    rotate_row.add_suffix(&rot_right);
    crop_group.add(&aspect);
    crop_group.add(&straighten.row);
    crop_group.add(&rotate_row);

    let dev_group = adw::PreferencesGroup::builder().title("Develop").description("Halation, grain and light leaks").build();
    let halation = switch("Halation", "CineStill-style red glow round bright lights");
    let grain = combo("Grain", &develop::GRAIN_NAMES, 0);
    let leak = switch("Light leak", "Random each photo");
    let mono = switch("Black & white grain", "No colour in the grain");
    dev_group.add(&halation);
    dev_group.add(&grain);
    dev_group.add(&leak);
    dev_group.add(&mono);

    let page = adw::PreferencesPage::new();
    for g in [
        &raw_group, &curve_group, &crop_group, &lens_fix_group, &camera_group, &film_group, &colour_group, &detail_group, &local_group, &fx_group, &lens_group,
        &dev_group, &frame_group,
    ] {
        page.add(g);
    }
    page.set_width_request(390);
    page.set_vexpand(true);
    let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
    side.append(&histogram);
    side.append(&page);

    let paned = gtk::Paned::builder().orientation(gtk::Orientation::Horizontal).start_child(&left).end_child(&side).resize_end_child(false).shrink_end_child(false).build();
    toasts.set_child(Some(&paned));

    // header
    let header = adw::HeaderBar::new();
    let open_button = gtk::Button::builder().icon_name("document-open-symbolic").tooltip_text("Open photos").build();
    let before = gtk::ToggleButton::builder().label("Before").tooltip_text("Show the photo without the edit").build();
    let export_button = gtk::Button::builder().label("Export\u{2026}").css_classes(["suggested-action"]).sensitive(false).build();
    let undo = gtk::Button::builder().icon_name("edit-undo-symbolic").tooltip_text("Undo (Ctrl+Z)").sensitive(false).build();
    let redo = gtk::Button::builder().icon_name("edit-redo-symbolic").tooltip_text("Redo (Ctrl+Shift+Z)").sensitive(false).build();
    header.pack_start(&open_button);
    header.pack_start(&undo);
    header.pack_start(&redo);
    header.pack_start(&before);
    let crop_button = gtk::ToggleButton::builder().label("Crop").tooltip_text("Crop, straighten and rotate (Enter when done)").sensitive(false).build();
    header.pack_start(&crop_button);
    header.pack_end(&export_button);
    let menu = gio::Menu::new();
    menu.append(Some("Show histogram"), Some("win.histogram"));
    menu.append(Some("Reset all edits"), Some("win.reset"));
    menu.append(Some("Keyboard shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About Grainhouse"), Some("win.about"));
    let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Menu").build();
    header.pack_end(&menu_button);
    let presets_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_start(8).margin_end(8).margin_top(8).margin_bottom(8).build();
    let presets_scroll = gtk::ScrolledWindow::builder().child(&presets_box).hscrollbar_policy(gtk::PolicyType::Never).min_content_height(480).min_content_width(380).build();
    let presets_pop = gtk::Popover::builder().child(&presets_scroll).build();
    let presets_button = gtk::MenuButton::builder().label("Presets").popover(&presets_pop).tooltip_text("Whole looks in one click, and your own saved looks").build();
    header.pack_end(&presets_button);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));
    window.set_content(Some(&view));

    let (job_tx, job_rx) = async_channel::unbounded();
    let (done_tx, done_rx) = async_channel::unbounded();
    worker(job_rx, done_tx);

    let app = Rc::new(App {
        ui: Ui {
            window: window.clone(),
            toasts,
            picture,
            empty,
            status,
            status_title,
            status_sub,
            info,
            strip,
            strip_scroll,
            before: before.clone(),
            export_button: export_button.clone(),
            raw_group,
            whites,
            blacks,
            vibrance,
            saturation,
            mix_colour,
            mix_hue,
            mix_sat,
            mix_lum,
            grade_zone,
            grade_hue,
            grade_amount,
            balance,
            clarity,
            texture: texture_s,
            dehaze,
            sharpen,
            vignette,
            frame: frame_row,
            curve_channel,
            curve_area: curve_area.clone(),
            sharpen_radius,
            masking,
            noise,
            colour_noise,
            defringe,
            vig_mid,
            vig_feather,
            distortion,
            vertical,
            horizontal,
            local_kind,
            local_size,
            local_feather,
            local_exposure,
            local_contrast,
            local_saturation,
            local_warmth,
            histogram: histogram.clone(),
            undo: undo.clone(),
            redo: redo.clone(),
            presets_box,
            wb,
            kelvin,
            tint,
            shadows,
            highlights,
            camera: camera_row,
            cam_strength,
            stamp,
            exposure,
            contrast,
            brand,
            film: film_row,
            strength,
            effect,
            target,
            model,
            look,
            face,
            amount,
            colour,
            glow,
            diffusion,
            diff_strength,
            diff_size,
            bloom,
            warmth,
            crop_button: crop_button.clone(),
            crop_group,
            aspect,
            straighten,
            crop_area: crop_area.clone(),
            halation,
            grain,
            leak,
            mono,
        },
        jobs: job_tx,
        sessions: RefCell::new(Vec::new()),
        current: Cell::new(0),
        seq: Cell::new(0),
        busy_token: Rc::new(Cell::new(0)),
        brand_films: RefCell::new(Vec::new()),
        updating: Cell::new(false),
        export_opts: RefCell::new(ExportOptions::default()),
        geometry: RefCell::new(Geometry::default()),
        portrait: Cell::new(false),
        drag: RefCell::new(None),
        frame_requested: Cell::new(false),
        frame_shown: Cell::new(false),
        last_note: RefCell::new(None),
        mixer: RefCell::new([[0.0; 3]; 8]),
        grade: RefCell::new([[0.0; 2]; 3]),
        history: RefCell::new(Vec::new()),
        history_pos: Cell::new(0),
        history_token: Rc::new(Cell::new(0)),
        hist: RefCell::new(None),
        show_hist: Cell::new(true),
        me: RefCell::new(Weak::new()),
        rendering: Cell::new(false),
        curve: RefCell::new([adjust::CURVE_X; 4]),
        curve_drag: Cell::new(None),
    });
    *app.me.borrow_mut() = Rc::downgrade(&app);
    app.update_rows();

    // any setting change re-renders
    let u = &app.ui;
    for row in [&u.camera, &u.film, &u.target, &u.model, &u.look, &u.face, &u.colour, &u.glow, &u.diffusion, &u.grain, &u.frame, &u.local_kind] {
        let a = Rc::downgrade(&app);
        row.connect_selected_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.request_render();
            }
        });
    }
    for row in [
        &u.exposure, &u.contrast, &u.tint, &u.shadows, &u.highlights, &u.whites, &u.blacks, &u.cam_strength, &u.strength, &u.amount, &u.diff_strength,
        &u.diff_size, &u.bloom, &u.warmth, &u.vibrance, &u.saturation, &u.balance, &u.clarity, &u.texture, &u.dehaze, &u.sharpen, &u.vignette,
        &u.sharpen_radius, &u.masking, &u.noise, &u.colour_noise, &u.defringe, &u.vig_mid, &u.vig_feather, &u.local_size, &u.local_feather,
        &u.local_exposure, &u.local_contrast, &u.local_saturation, &u.local_warmth,
    ] {
        let a = Rc::downgrade(&app);
        row.connect_value_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.request_render();
            }
        });
    }
    for row in [&u.halation, &u.leak, &u.mono, &u.stamp] {
        let a = Rc::downgrade(&app);
        row.connect_active_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.request_render();
            }
        });
    }
    {
        let a = Rc::downgrade(&app);
        u.brand.connect_selected_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.fill_films();
                a.request_render();
            }
        });
    }
    {
        let a = Rc::downgrade(&app);
        before.connect_toggled(move |_| {
            if let Some(a) = a.upgrade() {
                a.request_render();
            }
        });
    }

    {
        let a = Rc::downgrade(&app);
        u.effect.connect_selected_notify(move |_| {
            if let Some(a) = a.upgrade() {
                if a.updating.get() {
                    return;
                }
                a.fill_looks(None);
                a.request_render();
            }
        });
    }
    // lens corrections live in the geometry
    for (i, row) in [&u.distortion, &u.vertical, &u.horizontal].into_iter().enumerate() {
        let a = Rc::downgrade(&app);
        row.connect_value_notify(move |r| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            let v = (r.value() / 100.0) as f32;
            {
                let mut g = a.geometry.borrow_mut();
                match i {
                    0 => g.distortion = v,
                    1 => g.vertical = v,
                    _ => g.horizontal = v,
                }
            }
            a.request_render();
        });
    }
    // Auto light
    {
        let a = Rc::downgrade(&app);
        auto_button.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            let Some(sess) = a.sessions.borrow().get(a.current.get()).cloned() else { return };
            let r = sess.auto_light(a.settings().raw);
            let u = &a.ui;
            let was = a.updating.replace(true);
            u.exposure.set_value(r.exposure as f64);
            u.shadows.set_value((r.shadows * 100.0) as f64);
            u.highlights.set_value((r.highlights * 100.0) as f64);
            u.whites.set_value((r.whites * 100.0) as f64);
            u.blacks.set_value((r.blacks * 100.0) as f64);
            a.updating.set(was);
            a.request_render();
        });
    }
    // the tone curve editor
    {
        let a = Rc::downgrade(&app);
        curve_area.set_draw_func(move |_, cr, w, h| {
            if let Some(a) = a.upgrade() {
                a.draw_curve(cr, w as f64, h as f64);
            }
        });
        let a = Rc::downgrade(&app);
        u.curve_channel.connect_selected_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.ui.curve_area.queue_draw();
            }
        });
        let drag = gtk::GestureDrag::new();
        let a = Rc::downgrade(&app);
        drag.connect_drag_begin(move |_, x, y| {
            let Some(a) = a.upgrade() else { return };
            let w = a.ui.curve_area.width() as f64 - 16.0;
            let i = (((x - 8.0) / w * 4.0).round().clamp(0.0, 4.0)) as usize;
            a.curve_drag.set(Some((i, y as f32)));
        });
        let a = Rc::downgrade(&app);
        drag.connect_drag_update(move |_, _, dy| {
            let Some(a) = a.upgrade() else { return };
            let Some((i, y0)) = a.curve_drag.get() else { return };
            let h = a.ui.curve_area.height() as f32 - 16.0;
            let ch = (a.ui.curve_channel.selected() as usize).min(3);
            a.curve.borrow_mut()[ch][i] = (1.0 - (y0 + dy as f32 - 8.0) / h).clamp(0.0, 1.0);
            a.ui.curve_area.queue_draw();
            a.request_render();
        });
        let a = Rc::downgrade(&app);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(a) = a.upgrade() {
                a.curve_drag.set(None);
            }
        });
        curve_area.add_controller(drag);
        let reset = gtk::GestureClick::new();
        let a = Rc::downgrade(&app);
        reset.connect_pressed(move |_, n, _, _| {
            let Some(a) = a.upgrade() else { return };
            if n == 2 {
                let ch = (a.ui.curve_channel.selected() as usize).min(3);
                a.curve.borrow_mut()[ch] = adjust::CURVE_X;
                a.ui.curve_area.queue_draw();
                a.request_render();
            }
        });
        curve_area.add_controller(reset);
    }
    // the colour mixer and grading show one colour / zone at a time
    for row in [&u.mix_colour, &u.grade_zone] {
        let a = Rc::downgrade(&app);
        row.connect_selected_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.show_mixer();
            }
        });
    }
    for (i, row) in [&u.mix_hue, &u.mix_sat, &u.mix_lum].into_iter().enumerate() {
        let a = Rc::downgrade(&app);
        row.connect_value_notify(move |r| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            let c = (a.ui.mix_colour.selected() as usize).min(7);
            a.mixer.borrow_mut()[c][i] = (r.value() / 100.0) as f32;
            a.request_render();
        });
    }
    for (i, row) in [&u.grade_hue, &u.grade_amount].into_iter().enumerate() {
        let a = Rc::downgrade(&app);
        row.connect_value_notify(move |r| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            let z = (a.ui.grade_zone.selected() as usize).min(2);
            a.grade.borrow_mut()[z][i] = if i == 0 { r.value() as f32 } else { (r.value() / 100.0) as f32 };
            a.request_render();
        });
    }
    {
        let a = Rc::downgrade(&app);
        histogram.set_draw_func(move |_, cr, w, h| {
            if let Some(a) = a.upgrade() {
                a.draw_histogram(cr, w as f64, h as f64);
            }
        });
    }
    for (button, back) in [(&undo, true), (&redo, false)] {
        let a = Rc::downgrade(&app);
        button.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.step_history(back);
            }
        });
    }
    {
        let a = Rc::downgrade(&app);
        presets_pop.connect_show(move |_| {
            if let Some(a) = a.upgrade() {
                a.fill_presets();
            }
        });
    }
    {
        let a = Rc::downgrade(&app);
        add_folder.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            let dialog = gtk::FileDialog::builder().title("Add a folder of LUTs").build();
            let win = a.ui.window.clone();
            glib::spawn_future_local(async move {
                let Ok(folder) = dialog.select_folder_future(Some(&win)).await else { return };
                let Some(dir) = folder.path() else { return };
                let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("cube")))
                    .collect();
                files.sort();
                let (mut ok, mut bad) = (0, 0);
                for f in &files {
                    if lut::add(f).is_ok() { ok += 1 } else { bad += 1 }
                }
                a.toast(&if files.is_empty() {
                    "No .cube files in that folder".to_string()
                } else if bad > 0 {
                    format!("Added {ok} LUTs ({bad} couldn't be read)")
                } else {
                    format!("Added {ok} LUTs to My LUTs")
                });
                if ok > 0 {
                    a.ui.brand.set_selected(lut_brand);
                    a.fill_films();
                    a.request_render();
                }
            });
        });
    }
    // menu actions and shortcuts
    {
        let act = gio::SimpleAction::new_stateful("histogram", None, &true.to_variant());
        let a = Rc::downgrade(&app);
        act.connect_activate(move |act, _| {
            let Some(a) = a.upgrade() else { return };
            let on = !a.show_hist.get();
            a.show_hist.set(on);
            act.set_state(&on.to_variant());
            a.ui.histogram.set_visible(on);
            a.ui.histogram.queue_draw();
        });
        window.add_action(&act);
        let reset = gio::SimpleAction::new("reset", None);
        let a = Rc::downgrade(&app);
        reset.connect_activate(move |_, _| {
            if let Some(a) = a.upgrade() {
                a.reset_crop();
                a.apply_settings(&Settings::default());
                a.toast("All edits reset (Undo brings them back)");
            }
        });
        window.add_action(&reset);
        let shortcuts = gio::SimpleAction::new("shortcuts", None);
        let w = window.clone();
        shortcuts.connect_activate(move |_, _| shortcuts_dialog(&w));
        window.add_action(&shortcuts);
        let about = gio::SimpleAction::new("about", None);
        let w = window.clone();
        about.connect_activate(move |_, _| {
            let d = adw::AboutDialog::builder()
                .application_name("Grainhouse")
                .application_icon(APP_ID)
                .developer_name("jegly")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Film looks, AI effects, Lightroom-style editing and Real-ESRGAN upscaling for your photos.")
                .build();
            d.present(Some(&w));
        });
        window.add_action(&about);
        for (name, back) in [("undo", true), ("redo", false)] {
            let act = gio::SimpleAction::new(name, None);
            let a = Rc::downgrade(&app);
            act.connect_activate(move |_, _| {
                if let Some(a) = a.upgrade() {
                    a.step_history(back);
                }
            });
            window.add_action(&act);
        }
        let open_act = gio::SimpleAction::new("open", None);
        let ob = open_button.clone();
        open_act.connect_activate(move |_, _| ob.emit_clicked());
        window.add_action(&open_act);
        let export_act = gio::SimpleAction::new("export", None);
        let eb = export_button.clone();
        export_act.connect_activate(move |_, _| {
            if eb.is_sensitive() {
                eb.emit_clicked();
            }
        });
        window.add_action(&export_act);
        application.set_accels_for_action("win.open", &["<Ctrl>o"]);
        application.set_accels_for_action("win.export", &["<Ctrl>e"]);
        application.set_accels_for_action("win.undo", &["<Ctrl>z"]);
        application.set_accels_for_action("win.redo", &["<Ctrl><Shift>z", "<Ctrl>y"]);
    }

    // white balance: presets set the temperature, moving the temperature makes it Custom
    {
        let a = Rc::downgrade(&app);
        u.wb.connect_selected_notify(move |row| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            let i = row.selected() as usize;
            if i == 0 {
                a.sync_wb();
            } else if whitebalance::PRESET_KELVIN[i.min(whitebalance::CUSTOM)] > 0.0 {
                a.updating.set(true);
                a.ui.kelvin.set_value(whitebalance::PRESET_KELVIN[i] as f64);
                a.updating.set(false);
            }
            a.request_render();
        });
    }
    {
        let a = Rc::downgrade(&app);
        u.kelvin.connect_value_notify(move |_| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            if a.ui.wb.selected() as usize != whitebalance::CUSTOM {
                a.updating.set(true);
                a.ui.wb.set_selected(whitebalance::CUSTOM as u32);
                a.updating.set(false);
            }
            a.request_render();
        });
    }
    // My LUTs: copy .cube files into the LUT folder and pick the last one
    {
        let a = Rc::downgrade(&app);
        add_lut.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Cube LUTs"));
            filter.add_suffix("cube");
            filter.add_suffix("CUBE");
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let dialog = gtk::FileDialog::builder().title("Add LUTs").filters(&filters).build();
            let win = a.ui.window.clone();
            glib::spawn_future_local(async move {
                let Ok(list) = dialog.open_multiple_future(Some(&win)).await else { return };
                let mut last = None;
                for f in (0..list.n_items()).filter_map(|i| list.item(i).and_downcast::<gio::File>().and_then(|f| f.path())) {
                    match lut::add(&f) {
                        Ok(to) => last = Some(to),
                        Err(e) => a.toast(&e),
                    }
                }
                let Some(to) = last else { return };
                a.updating.set(true);
                a.ui.brand.set_selected(lut_brand);
                a.updating.set(false);
                a.fill_films();
                let id = format!("lut:{}", to.display());
                let pos = a.brand_films.borrow().iter().position(|f| *f == id);
                if let Some(pos) = pos {
                    a.ui.film.set_selected(pos as u32);
                }
                a.request_render();
            });
        });
    }

    // the crop tool
    {
        let a = Rc::downgrade(&app);
        crop_button.connect_toggled(move |b| {
            let Some(a) = a.upgrade() else { return };
            b.set_label(if b.is_active() { "Done" } else { "Crop" });
            a.ui.crop_area.set_can_target(b.is_active());
            if b.is_active() {
                b.add_css_class("suggested-action");
            } else {
                b.remove_css_class("suggested-action");
            }
            a.ui.crop_area.queue_draw();
            a.request_render();
        });
    }
    {
        let a = Rc::downgrade(&app);
        crop_area.set_draw_func(move |_, cr, _, _| {
            if let Some(a) = a.upgrade() {
                a.draw_crop(cr);
            }
        });
    }
    {
        let drag = gtk::GestureDrag::new();
        let a = Rc::downgrade(&app);
        drag.connect_drag_begin(move |_, x, y| {
            if let Some(a) = a.upgrade() {
                *a.drag.borrow_mut() = a.grab_at(x, y).map(|(g, r, _)| (g, r));
            }
        });
        let a = Rc::downgrade(&app);
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(a) = a.upgrade() else { return };
            let Some((grab, start)) = *a.drag.borrow() else { return };
            let Some((_, _, dw, dh)) = a.shown_rect() else { return };
            let r = geometry::drag(start, grab, (dx / dw) as f32, (dy / dh) as f32, a.shape());
            a.geometry.borrow_mut().set_rect(r);
            a.ui.crop_area.queue_draw();
        });
        let a = Rc::downgrade(&app);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(a) = a.upgrade() {
                *a.drag.borrow_mut() = None;
            }
        });
        crop_area.add_controller(drag);
        let motion = gtk::EventControllerMotion::new();
        let a = Rc::downgrade(&app);
        motion.connect_motion(move |_, x, y| {
            let Some(a) = a.upgrade() else { return };
            if a.drag.borrow().is_some() {
                return;
            }
            let cursor = a.grab_at(x, y).map(|g| g.2).unwrap_or("default");
            a.ui.crop_area.set_cursor_from_name(Some(cursor));
        });
        crop_area.add_controller(motion);
    }
    {
        let a = Rc::downgrade(&app);
        u.aspect.connect_selected_notify(move |_| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            let (fw, fh) = a.frame();
            a.portrait.set(fh > fw);
            a.apply_shape();
            a.crop_changed();
        });
    }
    {
        let a = Rc::downgrade(&app);
        swap.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            a.portrait.set(!a.portrait.get());
            a.apply_shape();
            a.crop_changed();
        });
    }
    {
        let a = Rc::downgrade(&app);
        u.straighten.connect_value_notify(move |r| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            a.geometry.borrow_mut().angle = r.value() as f32;
            a.crop_changed();
        });
    }
    for (button, cw) in [(&rot_left, false), (&rot_right, true)] {
        let a = Rc::downgrade(&app);
        button.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            a.geometry.borrow_mut().turn(cw);
            a.portrait.set(!a.portrait.get());
            a.request_render();
        });
    }
    {
        let a = Rc::downgrade(&app);
        reset_crop.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.reset_crop();
                a.request_render();
            }
        });
    }
    {
        let keys = gtk::EventControllerKey::new();
        let a = Rc::downgrade(&app);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(a) = a.upgrade() else { return glib::Propagation::Proceed };
            if a.cropping() && (key == gdk::Key::Return || key == gdk::Key::KP_Enter || key == gdk::Key::Escape) {
                a.ui.crop_button.set_active(false);
                return glib::Propagation::Stop;
            }
            // single-letter shortcuts, unless typing somewhere
            let typing = gtk::prelude::GtkWindowExt::focus(&a.ui.window).is_some_and(|w| w.is::<gtk::Text>() || w.is::<gtk::Entry>());
            if typing || state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) || a.sessions.borrow().is_empty() {
                return glib::Propagation::Proceed;
            }
            match key.to_unicode().map(|c| c.to_ascii_lowercase()) {
                Some('c') => a.ui.crop_button.set_active(!a.ui.crop_button.is_active()),
                Some('b') | Some('\\') => a.ui.before.set_active(!a.ui.before.is_active()),
                Some('h') => {
                    let _ = gtk::prelude::WidgetExt::activate_action(&a.ui.window, "win.histogram", None);
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        window.add_controller(keys);
    }

    // opening photos: the button, and dropping files on the window
    {
        let a = Rc::downgrade(&app);
        open_button.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Photos"));
            for ext in ["arw", "ARW", "jpg", "JPG", "jpeg", "png", "tif", "tiff", "dng", "DNG", "cr2", "cr3", "nef", "raf", "orf", "rw2"] {
                filter.add_suffix(ext);
            }
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let dialog = gtk::FileDialog::builder().title("Open photos").filters(&filters).build();
            let win = a.ui.window.clone();
            glib::spawn_future_local(async move {
                if let Ok(list) = dialog.open_multiple_future(Some(&win)).await {
                    let paths: Vec<PathBuf> = (0..list.n_items())
                        .filter_map(|i| list.item(i).and_downcast::<gio::File>().and_then(|f| f.path()))
                        .collect();
                    a.open(paths);
                }
            });
        });
    }
    {
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let a = Rc::downgrade(&app);
        drop.connect_drop(move |_, value, _, _| {
            let Some(a) = a.upgrade() else { return false };
            if let Ok(list) = value.get::<gdk::FileList>() {
                let mut paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
                paths.sort();
                a.open(paths);
                return true;
            }
            false
        });
        window.add_controller(drop);
    }
    {
        let a = Rc::downgrade(&app);
        export_button.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                export_dialog(&a);
            }
        });
    }

    // results from the worker; this loop holds the only strong reference, keeping the app alive with the window
    {
        let a = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = done_rx.recv().await {
                match msg {
                    Done::Opened(s, errors) => a.opened(s, errors),
                    Done::Rendered { seq, result, ms, note } => a.rendered(seq, result, ms, note),
                }
            }
        });
    }

    // photos given on the command line
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if !args.is_empty() {
        app.open(args);
    }
    window.present();
    // GRAINHOUSE_TOUR=<folder>: drive the window through every feature, saving a screenshot after each step
    if let Some(dir) = std::env::var_os("GRAINHOUSE_TOUR") {
        glib::spawn_future_local(tour::run(app.clone(), PathBuf::from(dir)));
    }
    // GRAINHOUSE_SHOTS=<folder>: screenshots of the interface with no photo, for the README
    if let Some(dir) = std::env::var_os("GRAINHOUSE_SHOTS") {
        glib::spawn_future_local(tour::shots(app.clone(), PathBuf::from(dir)));
    }
}

fn shortcuts_dialog(window: &adw::ApplicationWindow) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    for (keys, what) in [
        ("Ctrl+O", "Open photos"),
        ("Ctrl+E", "Export"),
        ("Ctrl+Z", "Undo"),
        ("Ctrl+Shift+Z or Ctrl+Y", "Redo"),
        ("B", "Before and after"),
        ("C", "Crop, straighten and rotate"),
        ("Enter or Esc", "Finish cropping"),
        ("H", "Histogram on or off"),
        ("Double-click a slider", "Put it back to its default"),
    ] {
        group.add(&adw::ActionRow::builder().title(what).subtitle(keys).use_markup(false).build());
    }
    page.add(&group);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    let d = adw::Dialog::builder().title("Keyboard shortcuts").content_width(420).content_height(520).child(&view).build();
    d.present(Some(window));
}

/// The export dialog: format, size, upscaling, sharpening, EXIF, naming and folder; runs in the background.
fn export_dialog(app: &Rc<App>) {
    let opts = app.export_opts.borrow().clone();
    let sessions = app.sessions.borrow().clone();
    let settings = app.settings();
    let multi = pipeline::is_multi(settings.effect);
    let current = app.current.get().min(sessions.len().saturating_sub(1));

    let dialog = adw::Dialog::builder().title("Export").content_width(480).build();
    let page = adw::PreferencesPage::new();

    let out_group = adw::PreferencesGroup::builder().title("File").build();
    let which = combo("Photos", &["This photo", &format!("All {} photos (same settings)", sessions.len())], 0);
    which.set_visible(sessions.len() > 1 && !multi);
    let format = combo("Format", &export::FORMATS, opts.format);
    let quality = spin("JPEG quality", 50.0, 100.0, 1.0, opts.quality as f64, 0);
    let exif = adw::SwitchRow::builder().title("Keep EXIF").subtitle("Camera, lens, date and exposure (JPEG export)").active(opts.keep_exif).build();
    let suffix = adw::EntryRow::builder().title("Name ending").text(&opts.suffix).build();
    let folder_row = adw::ActionRow::builder().title("Folder").subtitle(opts.folder.as_ref().map(|f| f.display().to_string()).unwrap_or("Next to the original".into())).build();
    let folder_button = gtk::Button::builder().label("Choose\u{2026}").valign(gtk::Align::Center).build();
    folder_row.add_suffix(&folder_button);
    for r in [&which, &format] {
        out_group.add(r);
    }
    out_group.add(&quality);
    out_group.add(&exif);
    out_group.add(&suffix);
    out_group.add(&folder_row);

    let size_group = adw::PreferencesGroup::builder().title("Size").build();
    let size = combo("Size", &export::SIZES, opts.size);
    let long_edge = spin("Long edge (pixels)", 100.0, 40000.0, 100.0, opts.long_edge as f64, 0);
    let percent = spin("Percent", 5.0, 400.0, 5.0, opts.percent as f64, 0);
    size_group.add(&size);
    size_group.add(&long_edge);
    size_group.add(&percent);

    let ai_group = adw::PreferencesGroup::builder().title("AI").description("Real-ESRGAN runs on the CPU: the estimate is shown below").build();
    let up = combo("Upscale", &export::UPSCALES, opts.upscale);
    let up_model = combo("Upscaler", &upscale::MODELS, opts.upscale_model);
    let sharpen = combo("Sharpen", &export::SHARPEN, opts.sharpen);
    let sharpen_amount = spin("Sharpen amount", 0.0, 200.0, 10.0, (opts.sharpen_amount * 100.0) as f64, 0);
    let estimate = adw::ActionRow::builder().title("Time").subtitle("").build();
    for r in [&up, &up_model, &sharpen] {
        ai_group.add(r);
    }
    ai_group.add(&sharpen_amount);
    ai_group.add(&estimate);

    let progress = gtk::ProgressBar::builder().show_text(true).visible(false).margin_start(12).margin_end(12).build();
    let go = gtk::Button::builder().label("Export").css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).margin_top(6).margin_bottom(12).build();
    let cancel_flag = Arc::new(AtomicBool::new(false));

    for g in [&out_group, &size_group, &ai_group] {
        page.add(g);
    }
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.append(&page);
    body.append(&progress);
    body.append(&go);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&body));
    dialog.set_child(Some(&view));

    let folder: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(opts.folder.clone()));
    let read = {
        let (format, quality, exif, suffix, size, long_edge, percent, up, up_model, sharpen, sharpen_amount, folder) = (
            format.clone(), quality.clone(), exif.clone(), suffix.clone(), size.clone(), long_edge.clone(), percent.clone(), up.clone(), up_model.clone(),
            sharpen.clone(), sharpen_amount.clone(), folder.clone(),
        );
        move || ExportOptions {
            format: format.selected() as usize,
            quality: quality.value() as u8,
            size: size.selected() as usize,
            long_edge: long_edge.value() as usize,
            percent: percent.value() as f32,
            upscale: up.selected() as usize,
            upscale_model: up_model.selected() as usize,
            sharpen: sharpen.selected() as usize,
            sharpen_amount: (sharpen_amount.value() / 100.0) as f32,
            keep_exif: exif.is_active(),
            suffix: suffix.text().to_string(),
            folder: folder.borrow().clone(),
        }
    };
    let read = Rc::new(read);
    let (w, h) = sessions.get(current).map(|s| s.output_size(&settings)).unwrap_or((0, 0));
    let refresh = {
        let (read, quality, long_edge, percent, up_model, sharpen, sharpen_amount, estimate, which) =
            (read.clone(), quality.clone(), long_edge.clone(), percent.clone(), up_model.clone(), sharpen.clone(), sharpen_amount.clone(), estimate.clone(), which.clone());
        let count = sessions.len();
        move || {
            let o = read();
            quality.set_visible(o.format == 0);
            long_edge.set_visible(o.size == 1);
            percent.set_visible(o.size == 2);
            up_model.set_visible(o.upscale > 0);
            sharpen.set_sensitive(o.upscale == 0);
            sharpen_amount.set_visible(o.sharpen == 1 && o.upscale == 0);
            let n = if which.selected() == 1 { count } else { 1 };
            let secs = o.estimate(w, h) * n as f32;
            let out = match o.upscale {
                1 => (w * 2, h * 2),
                2 => (w * 4, h * 4),
                _ => (w, h),
            };
            let note = if secs < 2.0 { "A few seconds".to_string() } else if secs < 120.0 { format!("About {:.0} seconds", secs) } else { format!("About {:.0} minutes", secs / 60.0) };
            estimate.set_subtitle(&format!("{note}   ({} x {} before resizing)", out.0, out.1));
        }
    };
    let refresh = Rc::new(refresh);
    refresh();
    for r in [&format, &size, &up, &up_model, &sharpen, &which] {
        let f = refresh.clone();
        r.connect_selected_notify(move |_| f());
    }
    for r in [&long_edge, &percent, &sharpen_amount] {
        let f = refresh.clone();
        r.connect_value_notify(move |_| f());
    }
    {
        let (folder, folder_row, dialog2) = (folder.clone(), folder_row.clone(), dialog.clone());
        folder_button.connect_clicked(move |_| {
            let fd = gtk::FileDialog::builder().title("Export to").build();
            let (folder, folder_row) = (folder.clone(), folder_row.clone());
            let root = dialog2.root().and_downcast::<gtk::Window>();
            glib::spawn_future_local(async move {
                if let Ok(f) = fd.select_folder_future(root.as_ref()).await {
                    if let Some(p) = f.path() {
                        folder_row.set_subtitle(&p.display().to_string());
                        *folder.borrow_mut() = Some(p);
                    }
                }
            });
        });
    }

    let running = Rc::new(Cell::new(false));
    {
        let (app, dialog2, progress, read, which, cancel_flag, running) = (app.clone(), dialog.clone(), progress.clone(), read.clone(), which.clone(), cancel_flag.clone(), running.clone());
        go.connect_clicked(move |go| {
            if running.get() {
                cancel_flag.store(true, Ordering::Relaxed);
                go.set_label("Cancelling\u{2026}");
                return;
            }
            let o = read();
            *app.export_opts.borrow_mut() = o.clone();
            let sessions = app.sessions.borrow().clone();
            let targets: Vec<usize> = if multi || which.selected() == 0 { vec![current] } else { (0..sessions.len()).collect() };
            running.set(true);
            cancel_flag.store(false, Ordering::Relaxed);
            go.set_label("Cancel");
            go.remove_css_class("suggested-action");
            go.add_css_class("destructive-action");
            progress.set_visible(true);
            let (tx, rx) = async_channel::unbounded::<Result<(f64, String), String>>();
            let settings = settings.clone();
            let cancel = cancel_flag.clone();
            std::thread::spawn(move || {
                let n = targets.len();
                for (k, &i) in targets.iter().enumerate() {
                    let s = &sessions[i];
                    let name = s.path.file_name().and_then(|n| n.to_str()).unwrap_or("photo").to_string();
                    let _ = tx.send_blocking(Ok((k as f64 / n as f64, format!("Rendering {name}"))));
                    let img = if multi {
                        let refs: Vec<&Session> = sessions.iter().map(|s| s.as_ref()).collect();
                        pipeline::render_multi(&refs, &settings, usize::MAX)
                    } else {
                        s.render(&settings, usize::MAX)
                    };
                    let img = match img {
                        Ok(i) => i,
                        Err(e) => {
                            let _ = tx.send_blocking(Err(e));
                            return;
                        }
                    };
                    let tx2 = tx.clone();
                    let cancel2 = cancel.clone();
                    let label = format!("Upscaling {name}");
                    let result = export::process(img, &o, &move |p| {
                        let _ = tx2.send_blocking(Ok(((k as f64 + p as f64) / n as f64, label.clone())));
                        !cancel2.load(Ordering::Relaxed)
                    });
                    let img = match result {
                        Ok(i) => i,
                        Err(e) => {
                            let _ = tx.send_blocking(Err(e));
                            return;
                        }
                    };
                    let path = o.target(&s.path);
                    if let Err(e) = export::save(&img, &path, &o, Some(&s.path)) {
                        let _ = tx.send_blocking(Err(e));
                        return;
                    }
                    let _ = tx.send_blocking(Ok(((k + 1) as f64 / n as f64, format!("Saved {}", path.display()))));
                }
                let _ = tx.send_blocking(Ok((2.0, "done".into())));
            });
            let (app, dialog2, progress, go, running) = (app.clone(), dialog2.clone(), progress.clone(), go.clone(), running.clone());
            glib::spawn_future_local(async move {
                let mut last = String::new();
                while let Ok(msg) = rx.recv().await {
                    match msg {
                        Ok((f, _)) if f > 1.5 => {
                            app.toast(&last);
                            dialog2.close();
                            break;
                        }
                        Ok((f, text)) => {
                            progress.set_fraction(f.clamp(0.0, 1.0));
                            progress.set_text(Some(&text));
                            if text.starts_with("Saved") {
                                last = text;
                            }
                        }
                        Err(e) => {
                            app.toast(&if e == "cancelled" { "Export cancelled".to_string() } else { e });
                            progress.set_visible(false);
                            go.set_label("Export");
                            go.remove_css_class("destructive-action");
                            go.add_css_class("suggested-action");
                            running.set(false);
                            break;
                        }
                    }
                }
            });
        });
    }
    dialog.present(Some(&app.ui.window));
}

const APP_ID: &str = "io.github.jegly.Grainhouse";

/// The command-line tool, built into the same program so the models are only shipped once: run as
/// `grainhouse-cli` (a link to this program) or `grainhouse --cli`.
#[path = "bin/grainhouse-cli.rs"]
#[allow(dead_code)]
mod cli;

fn main() -> glib::ExitCode {
    let name = std::env::args().next().and_then(|a| std::path::Path::new(&a).file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
    if name.starts_with("grainhouse-cli") {
        cli::main();
        return glib::ExitCode::SUCCESS;
    }
    if std::env::args().nth(1).as_deref() == Some("--cli") {
        cli::run(std::env::args().skip(2).collect());
        return glib::ExitCode::SUCCESS;
    }
    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_activate(build);
    app.connect_open(|app, _, _| build(app));
    app.run_with_args::<&str>(&[])
}

/// A scripted tour of the window for testing: it works the real widgets (as clicks would) and saves a
/// screenshot of the window after each step, plus a log. Run with GRAINHOUSE_TOUR=<folder>.
mod tour {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    async fn pause(ms: u64) {
        glib::timeout_future(Duration::from_millis(ms)).await;
    }

    /// Waits for the preview to finish (and the undo history to catch up).
    async fn idle(a: &Rc<App>) {
        pause(200).await;
        for _ in 0..900 {
            if !a.rendering.get() {
                break;
            }
            pause(100).await;
        }
        pause(700).await;
    }

    fn snap(widget: &gtk::Widget, path: &std::path::Path) -> bool {
        let (w, h) = (widget.width() as f32, widget.height() as f32);
        let paintable = gtk::WidgetPaintable::new(Some(widget));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, w as f64, h as f64);
        let (Some(node), Some(native)) = (snapshot.to_node(), widget.native()) else { return false };
        let Some(renderer) = native.renderer() else { return false };
        let tex = renderer.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w, h)));
        tex.save_to_png(path).is_ok()
    }

    /// The first button under `root` whose label is `label`.
    fn find_button(root: &gtk::Widget, label: &str) -> Option<gtk::Button> {
        if let Some(b) = root.downcast_ref::<gtk::Button>() {
            if b.label().as_deref() == Some(label) {
                return Some(b.clone());
            }
        }
        let mut c = root.first_child();
        while let Some(w) = c {
            if let Some(b) = find_button(&w, label) {
                return Some(b);
            }
            c = w.next_sibling();
        }
        None
    }

    fn pick(row: &adw::ComboRow, name: &str) -> bool {
        let Some(model) = row.model().and_downcast::<gtk::StringList>() else { return false };
        for i in 0..model.n_items() {
            if model.string(i).is_some_and(|s| s.trim_end_matches(" \u{25c6}").trim() == name) {
                row.set_selected(i);
                return true;
            }
        }
        false
    }

    fn find<T: IsA<gtk::Widget>>(root: &gtk::Widget) -> Option<T> {
        if let Some(t) = root.downcast_ref::<T>() {
            return Some(t.clone());
        }
        let mut c = root.first_child();
        while let Some(w) = c {
            if let Some(t) = find::<T>(&w) {
                return Some(t);
            }
            c = w.next_sibling();
        }
        None
    }

    fn expand_all(root: &gtk::Widget) {
        if let Some(e) = root.downcast_ref::<adw::ExpanderRow>() {
            e.set_expanded(true);
        }
        let mut c = root.first_child();
        while let Some(w) = c {
            expand_all(&w);
            c = w.next_sibling();
        }
    }

    /// The interface with no photo open: the start screen, every panel group, presets and shortcuts.
    pub async fn shots(a: Rc<App>, dir: PathBuf) {
        let _ = std::fs::create_dir_all(&dir);
        pause(1500).await;
        let win: gtk::Widget = a.ui.window.clone().upcast();
        let u = &a.ui;
        u.histogram.set_visible(false);
        for _ in 0..20 {
            if snap(&win, &dir.join("window.png")) {
                break;
            }
            pause(250).await;
        }
        // show the groups that normally wait for a photo, and open the expanders
        u.raw_group.set_visible(true);
        u.crop_group.set_visible(true);
        pick(&u.effect, "Ghosts");
        pick(&u.diffusion, "Black Mist 1/4");
        pick(&u.local_kind, "Sky gradient");
        u.sharpen.set_value(40.0);
        u.vignette.set_value(-30.0);
        a.update_rows();
        let page: gtk::Widget = u.vibrance.row.ancestor(adw::PreferencesPage::static_type()).unwrap();
        expand_all(&page);
        pause(800).await;
        let sw = u.vibrance.row.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>().unwrap();
        let adj = sw.vadjustment();
        let side = u.histogram.parent().unwrap();
        let mut n = 0;
        let mut at = 0.0;
        loop {
            adj.set_value(at);
            pause(400).await;
            n += 1;
            // the whole window (a widget alone loses the window's background); cropped to the panel later
            let _ = &side;
            snap(&win, &dir.join(format!("panel-{n}.png")));
            if at >= adj.upper() - adj.page_size() - 1.0 || n > 12 {
                break;
            }
            at = (at + adj.page_size() * 0.9).min(adj.upper() - adj.page_size());
        }
        if let Some(mb) = find::<gtk::MenuButton>(&win).filter(|m| m.label().as_deref() == Some("Presets")).or_else(|| {
            // the first MenuButton is the main menu; find the one labelled Presets
            let mut found = None;
            let mut stack = vec![win.clone()];
            while let Some(w) = stack.pop() {
                if let Some(m) = w.downcast_ref::<gtk::MenuButton>() {
                    if m.label().as_deref() == Some("Presets") {
                        found = Some(m.clone());
                    }
                }
                let mut c = w.first_child();
                while let Some(x) = c {
                    stack.push(x.clone());
                    c = x.next_sibling();
                }
            }
            found
        }) {
            mb.popup();
            pause(800).await;
            if let Some(pop) = mb.popover() {
                let pw: gtk::Widget = pop.upcast();
                snap(&pw, &dir.join("presets.png"));
            }
            mb.popdown();
        }
        let _ = WidgetExt::activate_action(&a.ui.window, "win.shortcuts", None);
        pause(800).await;
        snap(&win, &dir.join("shortcuts.png"));
        if let Some(app) = a.ui.window.application() {
            app.quit();
        }
    }

    pub async fn run(a: Rc<App>, dir: PathBuf) {
        let _ = std::fs::create_dir_all(&dir);
        let mut log = std::fs::File::create(dir.join("tour.log")).unwrap();
        let t0 = Instant::now();
        let mut n = 0;
        // wait for the photo to open
        for _ in 0..300 {
            if !a.sessions.borrow().is_empty() {
                break;
            }
            pause(100).await;
        }
        idle(&a).await;
        let win: gtk::Widget = a.ui.window.clone().upcast();
        let mut shot = |name: &str, log: &mut std::fs::File, a: &Rc<App>| {
            n += 1;
            let file = dir.join(format!("{n:02}-{name}.png"));
            let ok = snap(&win, &file);
            let info = a.ui.info.text();
            let _ = writeln!(log, "{:6.1}s  {n:02} {name:28} screenshot {}  [{}]", t0.elapsed().as_secs_f32(), if ok { "ok" } else { "FAILED" }, info);
        };
        macro_rules! step {
            ($name:expr, $body:block) => {{
                $body;
                idle(&a).await;
                shot($name, &mut log, &a);
            }};
        }
        let u = &a.ui;
        shot("opened", &mut log, &a);
        step!("film-portra-400", {
            pick(&u.brand, "Kodak");
            pick(&u.film, "Portra 400");
        });
        step!("light-shadows-highlights-wb", {
            u.shadows.set_value(40.0);
            u.highlights.set_value(-40.0);
            u.kelvin.set_value(4800.0);
        });
        step!("colour-grading", {
            u.grade_zone.set_selected(0);
            u.grade_hue.set_value(200.0);
            u.grade_amount.set_value(50.0);
            u.grade_zone.set_selected(2);
            u.grade_hue.set_value(45.0);
            u.grade_amount.set_value(40.0);
            u.vibrance.set_value(25.0);
        });
        for (effect, extra) in [("Ghosts", ""), ("Skeleton", ""), ("Zoom burst", ""), ("Neon edges", "Cyan"), ("Infrared", ""), ("VHS", ""), ("Oil paint", "")] {
            step!(&format!("effect-{}", effect.to_lowercase().replace(' ', "-")), {
                pick(&u.effect, effect);
                if !extra.is_empty() {
                    pick(&u.colour, extra);
                }
            });
        }
        step!("auto-light", {
            if let Some(b) = find_button(&a.ui.window.clone().upcast(), "Auto") {
                b.emit_clicked();
            }
        });
        step!("tone-curve", {
            a.curve.borrow_mut()[0] = [0.04, 0.2, 0.5, 0.82, 0.97];
            a.curve.borrow_mut()[3] = [0.06, 0.27, 0.5, 0.72, 0.95];
            a.ui.curve_area.queue_draw();
            a.request_render();
        });
        step!("lens-corrections", {
            u.distortion.set_value(40.0);
            u.vertical.set_value(30.0);
            u.defringe.set_value(60.0);
        });
        step!("local-sky-gradient", {
            pick(&u.local_kind, "Sky gradient");
            u.local_exposure.set_value(-1.0);
            u.local_warmth.set_value(40.0);
        });
        step!("local-subject", {
            pick(&u.local_kind, "Subject (AI)");
            u.local_exposure.set_value(0.8);
            u.local_saturation.set_value(30.0);
        });
        step!("noise-sharpen", {
            u.noise.set_value(50.0);
            u.colour_noise.set_value(50.0);
            u.sharpen.set_value(60.0);
            u.masking.set_value(40.0);
        });
        step!("effect-off-diffusion", {
            pick(&u.effect, "None");
            pick(&u.diffusion, "White Mist 1/2");
        });
        step!("crop-mode-square", {
            u.crop_button.set_active(true);
            pick(&u.aspect, "1:1");
        });
        step!("cropped", {
            u.crop_button.set_active(false);
        });
        step!("undo-x3", {
            for _ in 0..3 {
                let _ = WidgetExt::activate_action(&a.ui.window, "win.undo", None);
                pause(300).await;
            }
        });
        step!("redo", {
            let _ = WidgetExt::activate_action(&a.ui.window, "win.redo", None);
        });
        step!("preset-cinestill-night", {
            a.fill_presets();
            let root: gtk::Widget = a.ui.presets_box.clone().upcast();
            if let Some(b) = find_button(&root, "CineStill night") {
                b.emit_clicked();
            }
        });
        {
            let pb: gtk::Widget = a.ui.presets_box.clone().upcast();
            let _ = pb;
        }
        step!("camera-disposable-stamp", {
            pick(&u.camera, "Disposable camera");
            u.stamp.set_active(true);
        });
        step!("frame-polaroid", {
            pick(&u.frame, "Polaroid");
        });
        step!("creative-teal-orange", {
            pick(&u.camera, "None");
            u.stamp.set_active(false);
            pick(&u.frame, "35mm film strip");
            pick(&u.brand, "Creative");
            pick(&u.film, "Teal & Orange");
        });
        step!("before", {
            u.before.set_active(true);
        });
        step!("after", {
            u.before.set_active(false);
        });
        // export through the dialog to the tour folder
        let out = dir.join("export");
        let _ = std::fs::create_dir_all(&out);
        a.export_opts.borrow_mut().folder = Some(out.clone());
        step!("export-dialog", {
            u.export_button.emit_clicked();
        });
        let before_files = std::fs::read_dir(&out).map(|d| d.count()).unwrap_or(0);
        if let Some(go) = find_button(&win, "Export") {
            go.emit_clicked();
            let _ = writeln!(log, "{:6.1}s  pressed Export", t0.elapsed().as_secs_f32());
        } else {
            let _ = writeln!(log, "export button not found");
        }
        let mut done = false;
        for _ in 0..600 {
            pause(200).await;
            let now: Vec<_> = std::fs::read_dir(&out).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            if now.len() > before_files {
                pause(800).await;
                let _ = writeln!(log, "{:6.1}s  exported {:?} ({} bytes)", t0.elapsed().as_secs_f32(), now, now.iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum::<u64>());
                done = true;
                break;
            }
        }
        if !done {
            let _ = writeln!(log, "export did not produce a file");
        }
        step!("after-export", {});
        step!("reset-all", {
            let _ = WidgetExt::activate_action(&a.ui.window, "win.reset", None);
        });
        // the side panel, scrolled through, with the mixer and grading open
        let sw = u.vibrance.row.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>();
        let mut c: Option<gtk::Widget> = u.vibrance.row.parent();
        while let Some(w) = c {
            if let Some(e) = w.downcast_ref::<gtk::ListBox>() {
                let mut r = e.first_child();
                while let Some(row) = r {
                    if let Some(x) = row.downcast_ref::<adw::ExpanderRow>() {
                        x.set_expanded(true);
                    }
                    r = row.next_sibling();
                }
                break;
            }
            c = w.parent();
        }
        if let Some(sw) = sw {
            let adj = sw.vadjustment();
            for (i, f) in [0.3, 0.55, 0.8, 1.0].into_iter().enumerate() {
                step!(&format!("panel-{}", i + 1), {
                    adj.set_value((adj.upper() - adj.page_size()) * f);
                });
            }
        }
        step!("histogram-off", {
            let _ = WidgetExt::activate_action(&a.ui.window, "win.histogram", None);
        });
        let _ = writeln!(log, "{:6.1}s  tour finished", t0.elapsed().as_secs_f32());
        a.ui.window.close();
    }
}
