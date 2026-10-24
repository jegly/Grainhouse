//! Alpha Studio's window (GTK4 + libadwaita): drop in ARW or JPEG photos, pick a film, an effect, diffusion and
//! develop effects, and export with optional Real-ESRGAN upscaling and sharpening.

use adw::prelude::*;
use alpha_studio::{
    ai, camera, develop, export,
    export::ExportOptions,
    film, fx,
    geometry::{self, Geometry, Grab, Rect},
    img::Image,
    lut,
    pipeline::{self, Session, Settings},
    upscale, whitebalance,
};
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
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
        s.raw.kelvin = if u.wb.selected() == 0 { None } else { Some(u.kelvin.value() as f32) };
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
        let multi = pipeline::is_multi(e);
        let subjects = e != pipeline::NONE && e != pipeline::FACE_MESH && e != pipeline::SKELETON && !multi;
        u.target.set_visible(subjects);
        u.model.set_visible(subjects || e == pipeline::SKELETON);
        u.look.set_visible(e == pipeline::GHOSTS);
        u.face.set_visible(e == pipeline::WIREFRAME || e == pipeline::FACE_MESH);
        u.amount.set_visible(e != pipeline::NONE);
        u.amount.set_title(match e {
            pipeline::GHOSTS => "Opacity",
            pipeline::WIREFRAME | pipeline::CONTOURS | pipeline::POINTS | pipeline::SKELETON | pipeline::FACE_MESH => "Dim behind",
            pipeline::TRAILS => "Paler trails",
            _ => "Amount",
        });
        let lit = matches!(e, pipeline::GHOSTS | pipeline::WIREFRAME | pipeline::CONTOURS | pipeline::FACE_MESH | pipeline::POINTS | pipeline::SKELETON | pipeline::TRAILS)
            || e == pipeline::SCENE_FIRST + fx::scene::RIM
            || e == pipeline::SCENE_FIRST + fx::scene::COMIC;
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
        } else if brand > brands.len() {
            lut::list().into_iter().map(|(n, p)| (format!("lut:{}", p.display()), n)).unzip()
        } else {
            film::films()
                .into_iter()
                .filter(|f| f.brand == brands[brand - 1])
                .map(|f| (f.file.to_string(), if f.error < 0.0 { format!("{}  \u{25c6}", f.name) } else { f.name.to_string() }))
                .unzip()
        };
        let empty_luts = brand > brands.len() && ids.is_empty();
        self.ui.film.set_subtitle(if empty_luts { "Press Add LUT above to bring in .cube files" } else { "" });
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        self.updating.set(true);
        self.ui.film.set_model(Some(&gtk::StringList::new(&refs)));
        self.ui.film.set_selected(0);
        self.updating.set(false);
        *self.brand_films.borrow_mut() = ids;
    }

    /// With "As shot" chosen, shows the photo's own white balance on the Temperature slider.
    fn sync_wb(&self) {
        let sessions = self.sessions.borrow();
        let Some(s) = sessions.get(self.current.get()) else { return };
        let shot = s.as_shot_kelvin();
        self.ui.kelvin.set_subtitle(&shot.map(|k| format!("Kelvin. As shot: {:.0} K", (k / 50.0).round() * 50.0)).unwrap_or("Kelvin".into()));
        if self.ui.wb.selected() == 0 {
            if let Some(k) = shot {
                self.updating.set(true);
                self.ui.kelvin.set_value(((k / 50.0).round() * 50.0) as f64);
                self.updating.set(false);
            }
        }
    }

    fn request_render(&self) {
        if self.updating.get() {
            return;
        }
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
        let (title, sub) = if pipeline::is_multi(settings.effect) {
            (format!("Combining {} photos", sessions.len()), String::new())
        } else if shown.needs_ai(&settings) {
            (ai_message(&settings), "The AI runs once, then changes are quick".to_string())
        } else {
            ("Updating".to_string(), String::new())
        };
        self.set_busy(Some((title, sub)));
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
        let sub = if paths.iter().any(|p| alpha_studio::io::is_raw(p)) { "Developing the RAW" } else { "" };
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
        let raw = self.sessions.borrow()[0].is_raw();
        self.ui.raw_group.set_visible(raw);
        self.sync_wb();
        self.ui.crop_group.set_visible(true);
        self.reset_crop();
        self.ui.crop_button.set_sensitive(true);
        self.rebuild_strip();
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
                    let raw = app.sessions.borrow()[i].is_raw();
                    app.ui.raw_group.set_visible(raw);
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
        self.updating.set(true);
        self.ui.aspect.set_selected(0);
        self.ui.straighten.set_value(0.0);
        self.ui.crop_button.set_active(false);
        self.updating.set(false);
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

    fn rendered(&self, seq: u64, result: Result<Image, String>, ms: u128, note: Option<String>) {
        if seq != self.seq.get() {
            return; // a newer render is on its way
        }
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
    let window = adw::ApplicationWindow::builder().application(app).title("Alpha Studio").default_width(1400).default_height(900).build();
    let toasts = adw::ToastOverlay::new();

    // the photo
    let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).hexpand(true).vexpand(true).build();
    let empty = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .title("Drop photos here")
        .description("Sony ARW and other RAW files, JPEG, PNG or TIFF.\nDrop several for Crowd remover or Trails.")
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
    let raw_group = adw::PreferencesGroup::builder().title("RAW").visible(false).build();
    let wb = combo("White balance", &whitebalance::PRESETS, 0);
    let kelvin = slider("Temperature", 2000.0, 12000.0, 50.0, 5500.0, 0);
    kelvin.set_subtitle("Kelvin");
    let tint = slider("Tint", -100.0, 100.0, 1.0, 0.0, 0);
    tint.set_subtitle("Green to magenta");
    let exposure = slider("Exposure", -3.0, 3.0, 0.05, 0.0, 2);
    let contrast = slider("Contrast", 0.0, 100.0, 1.0, 100.0, 0);
    contrast.set_subtitle("0 is flat, 100 is like the camera's JPEG");
    let shadows = slider("Shadows", -100.0, 100.0, 1.0, 0.0, 0);
    shadows.set_subtitle("Lift or deepen the dark areas (D-Range)");
    let highlights = slider("Highlights", -100.0, 100.0, 1.0, 0.0, 0);
    highlights.set_subtitle("Below 0 brings back bright detail");
    raw_group.add(&wb);
    for r in [&kelvin, &tint, &exposure, &contrast, &shadows, &highlights] {
        raw_group.add(&r.row);
    }

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
        .description("Film Sim's looks. \u{25c6} = designed for the camera. My LUTs = your .cube files")
        .build();
    let add_lut = gtk::Button::builder().label("Add LUT\u{2026}").css_classes(["flat"]).valign(gtk::Align::Center).build();
    film_group.set_header_suffix(Some(&add_lut));
    let mut brands = vec!["None"];
    brands.extend(film::brands());
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

    let dev_group = adw::PreferencesGroup::builder().title("Develop").description("Film Sim's after-shot effects").build();
    let halation = switch("Halation", "CineStill-style red glow round bright lights");
    let grain = combo("Grain", &develop::GRAIN_NAMES, 0);
    let leak = switch("Light leak", "Random each photo");
    let mono = switch("Black & white grain", "No colour in the grain");
    dev_group.add(&halation);
    dev_group.add(&grain);
    dev_group.add(&leak);
    dev_group.add(&mono);

    let page = adw::PreferencesPage::new();
    for g in [&raw_group, &crop_group, &camera_group, &film_group, &fx_group, &lens_group, &dev_group] {
        page.add(g);
    }
    page.set_width_request(390);

    let paned = gtk::Paned::builder().orientation(gtk::Orientation::Horizontal).start_child(&left).end_child(&page).resize_end_child(false).shrink_end_child(false).build();
    toasts.set_child(Some(&paned));

    // header
    let header = adw::HeaderBar::new();
    let open_button = gtk::Button::builder().icon_name("document-open-symbolic").tooltip_text("Open photos").build();
    let before = gtk::ToggleButton::builder().label("Before").tooltip_text("Show the photo without the edit").build();
    let export_button = gtk::Button::builder().label("Export\u{2026}").css_classes(["suggested-action"]).sensitive(false).build();
    header.pack_start(&open_button);
    header.pack_start(&before);
    let crop_button = gtk::ToggleButton::builder().label("Crop").tooltip_text("Crop, straighten and rotate (Enter when done)").sensitive(false).build();
    header.pack_start(&crop_button);
    header.pack_end(&export_button);
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
    });
    app.update_rows();

    // any setting change re-renders
    let u = &app.ui;
    for row in [&u.camera, &u.film, &u.effect, &u.target, &u.model, &u.look, &u.face, &u.colour, &u.glow, &u.diffusion, &u.grain] {
        let a = Rc::downgrade(&app);
        row.connect_selected_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.request_render();
            }
        });
    }
    for row in [&u.exposure, &u.contrast, &u.tint, &u.shadows, &u.highlights, &u.cam_strength, &u.strength, &u.amount, &u.diff_strength, &u.diff_size, &u.bloom, &u.warmth] {
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
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(a) = a.upgrade() else { return glib::Propagation::Proceed };
            if a.cropping() && (key == gdk::Key::Return || key == gdk::Key::KP_Enter) {
                a.ui.crop_button.set_active(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
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
    let exif = adw::SwitchRow::builder().title("Keep EXIF").subtitle("Camera, lens, date and settings (from JPEG originals)").active(opts.keep_exif).build();
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

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id("io.github.jegly.AlphaStudio").flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_activate(build);
    app.connect_open(|app, _, _| build(app));
    app.run_with_args::<&str>(&[])
}
