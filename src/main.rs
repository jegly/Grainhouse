//! Alpha Studio's window (GTK4 + libadwaita): drop in ARW or JPEG photos, pick a film, an effect, diffusion and
//! develop effects, and export with optional Real-ESRGAN upscaling and sharpening.

use adw::prelude::*;
use alpha_studio::{
    ai, develop, export,
    export::ExportOptions,
    film, fx,
    img::Image,
    pipeline::{self, Session, Settings},
    upscale,
};
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

const PREVIEW: usize = 1800;

enum Job {
    Open(Vec<PathBuf>),
    Render { gen: u64, sessions: Vec<Arc<Session>>, current: usize, settings: Settings },
}

enum Done {
    Opened(Vec<Arc<Session>>, Vec<String>),
    Rendered { gen: u64, result: Result<Image, String>, ms: u128 },
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    picture: gtk::Picture,
    empty: adw::StatusPage,
    spinner: gtk::Spinner,
    info: gtk::Label,
    strip: gtk::Box,
    strip_scroll: gtk::ScrolledWindow,
    before: gtk::ToggleButton,
    export_button: gtk::Button,
    raw_group: adw::PreferencesGroup,
    exposure: adw::SpinRow,
    contrast: adw::SpinRow,
    brand: adw::ComboRow,
    film: adw::ComboRow,
    strength: adw::SpinRow,
    effect: adw::ComboRow,
    target: adw::ComboRow,
    model: adw::ComboRow,
    look: adw::ComboRow,
    face: adw::ComboRow,
    amount: adw::SpinRow,
    colour: adw::ComboRow,
    glow: adw::ComboRow,
    diffusion: adw::ComboRow,
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
    gen: Cell<u64>,
    busy: Cell<bool>,
    /// films in the current brand, as shown in the Film row
    brand_films: RefCell<Vec<&'static str>>,
    updating: Cell<bool>,
    export_opts: RefCell<ExportOptions>,
}

fn combo(title: &str, items: &[&str], selected: usize) -> adw::ComboRow {
    let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(items)).build();
    row.set_selected(selected as u32);
    row
}

fn spin(title: &str, min: f64, max: f64, step: f64, value: f64, digits: u32) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    row.set_digits(digits);
    row.set_value(value);
    row
}

fn switch(title: &str, subtitle: &str) -> adw::SwitchRow {
    adw::SwitchRow::builder().title(title).subtitle(subtitle).build()
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
        let films = self.brand_films.borrow();
        s.film = if u.brand.selected() == 0 { None } else { films.get(u.film.selected() as usize).map(|f| f.to_string()) };
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
        u.film.set_sensitive(u.brand.selected() != 0);
        u.strength.set_sensitive(u.brand.selected() != 0);
    }

    fn fill_films(&self) {
        let brand = self.ui.brand.selected() as usize;
        let brands = film::brands();
        let list: Vec<&'static str> =
            if brand == 0 { Vec::new() } else { film::films().into_iter().filter(|f| f.brand == brands[brand - 1]).map(|f| f.file).collect() };
        let names: Vec<String> = film::films()
            .into_iter()
            .filter(|f| list.contains(&f.file))
            .map(|f| if f.error < 0.0 { format!("{}  \u{25c6}", f.name) } else { f.name.to_string() })
            .collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        self.updating.set(true);
        self.ui.film.set_model(Some(&gtk::StringList::new(&refs)));
        self.ui.film.set_selected(0);
        self.updating.set(false);
        *self.brand_films.borrow_mut() = list;
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
        let gen = self.gen.get() + 1;
        self.gen.set(gen);
        let mut settings = self.settings();
        if self.ui.before.is_active() {
            let raw = settings.raw;
            settings = Settings::default();
            settings.raw = raw;
        }
        self.set_busy(true);
        let _ = self.jobs.send_blocking(Job::Render { gen, sessions, current: self.current.get(), settings });
    }

    fn set_busy(&self, b: bool) {
        self.busy.set(b);
        self.ui.spinner.set_spinning(b);
        self.ui.spinner.set_visible(b);
    }

    fn open(&self, paths: Vec<PathBuf>) {
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_file()).collect();
        if paths.is_empty() {
            return;
        }
        self.set_busy(true);
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
            self.set_busy(false);
            return;
        }
        *self.sessions.borrow_mut() = sessions;
        self.current.set(0);
        self.ui.empty.set_visible(false);
        self.ui.export_button.set_sensitive(true);
        let raw = self.sessions.borrow()[0].is_raw();
        self.ui.raw_group.set_visible(raw);
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
                    app.rebuild_strip();
                    app.request_render();
                }
            });
            strip.append(&b);
        }
    }

    fn rendered(&self, gen: u64, result: Result<Image, String>, ms: u128) {
        if gen != self.gen.get() {
            return; // a newer render is on its way
        }
        self.set_busy(false);
        match result {
            Ok(img) => {
                self.ui.picture.set_paintable(Some(&texture(&img)));
                let sessions = self.sessions.borrow();
                if let Some(s) = sessions.get(self.current.get()) {
                    let (w, h) = s.size();
                    let name = s.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let raw = if s.is_raw() { "RAW, " } else { "" };
                    self.ui.info.set_text(&format!("{name}   {raw}{w} x {h}   preview in {:.1} s", ms as f32 / 1000.0));
                }
            }
            Err(e) => self.toast(&e),
        }
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
                Job::Render { gen, sessions, current, settings } => {
                    let t0 = Instant::now();
                    let result = if pipeline::is_multi(settings.effect) {
                        let refs: Vec<&Session> = sessions.iter().map(|s| s.as_ref()).collect();
                        pipeline::render_multi(&refs, &settings, PREVIEW)
                    } else {
                        sessions[current.min(sessions.len() - 1)].render(&settings, PREVIEW)
                    };
                    Done::Rendered { gen, result, ms: t0.elapsed().as_millis() }
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
    let spinner = gtk::Spinner::builder().halign(gtk::Align::End).valign(gtk::Align::Start).margin_top(12).margin_end(12).visible(false).build();
    spinner.set_size_request(28, 28);
    let info = gtk::Label::builder().halign(gtk::Align::Start).valign(gtk::Align::End).margin_start(12).margin_bottom(8).css_classes(["dim-label", "caption"]).build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&empty);
    overlay.add_overlay(&spinner);
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
    let exposure = spin("Exposure", -3.0, 3.0, 0.1, 0.0, 1);
    let contrast = spin("Contrast", 0.0, 100.0, 5.0, 100.0, 0);
    raw_group.add(&exposure);
    raw_group.add(&contrast);

    let film_group = adw::PreferencesGroup::builder().title("Film").description("Film Sim's looks. \u{25c6} = designed for the camera").build();
    let mut brands = vec!["None"];
    brands.extend(film::brands());
    let brand = combo("Brand", &brands, 0);
    let film_row = combo("Film", &[], 0);
    film_row.set_enable_search(true);
    let strength = spin("Strength", 0.0, 100.0, 5.0, 100.0, 0);
    film_group.add(&brand);
    film_group.add(&film_row);
    film_group.add(&strength);

    let fx_group = adw::PreferencesGroup::builder().title("Effect").build();
    let effect = combo("Effect", &pipeline::EFFECTS, 0);
    let target = combo("Target", &ai::TARGET_NAMES, 0);
    let model = combo("AI model", &ai::MODEL_NAMES, 1);
    let look = combo("Look", &fx::ghost::LOOKS, 1);
    let face = combo("Face", &fx::mesh::FACE_STYLES, 0);
    let amount = spin("Amount", 0.0, 100.0, 5.0, 85.0, 0);
    let colour = combo("Colour", &fx::COLOURS, 0);
    let glow = combo("Glow", &fx::GLOW_NAMES, 2);
    for r in [&effect, &target, &model, &look, &face] {
        fx_group.add(r);
    }
    fx_group.add(&amount);
    fx_group.add(&colour);
    fx_group.add(&glow);

    let lens_group = adw::PreferencesGroup::builder().title("Diffusion filter").build();
    let diffusion = combo("Filter", &fx::diffusion::NAMES, 0);
    lens_group.add(&diffusion);

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
    for g in [&raw_group, &film_group, &fx_group, &lens_group, &dev_group] {
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
            spinner,
            info,
            strip,
            strip_scroll,
            before: before.clone(),
            export_button: export_button.clone(),
            raw_group,
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
            halation,
            grain,
            leak,
            mono,
        },
        jobs: job_tx,
        sessions: RefCell::new(Vec::new()),
        current: Cell::new(0),
        gen: Cell::new(0),
        busy: Cell::new(false),
        brand_films: RefCell::new(Vec::new()),
        updating: Cell::new(false),
        export_opts: RefCell::new(ExportOptions::default()),
    });
    app.update_rows();

    // any setting change re-renders
    let u = &app.ui;
    for row in [&u.film, &u.effect, &u.target, &u.model, &u.look, &u.face, &u.colour, &u.glow, &u.diffusion, &u.grain] {
        let a = Rc::downgrade(&app);
        row.connect_selected_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.request_render();
            }
        });
    }
    for row in [&u.exposure, &u.contrast, &u.strength, &u.amount] {
        let a = Rc::downgrade(&app);
        row.connect_value_notify(move |_| {
            if let Some(a) = a.upgrade() {
                a.request_render();
            }
        });
    }
    for row in [&u.halation, &u.leak, &u.mono] {
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

    // results from the worker
    {
        let a = Rc::downgrade(&app);
        glib::spawn_future_local(async move {
            while let Ok(msg) = done_rx.recv().await {
                let Some(a) = a.upgrade() else { return };
                match msg {
                    Done::Opened(s, errors) => a.opened(s, errors),
                    Done::Rendered { gen, result, ms } => a.rendered(gen, result, ms),
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
    let (w, h) = sessions.get(current).map(|s| s.size()).unwrap_or((0, 0));
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
                        Ok((f, text)) if f > 1.5 => {
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
