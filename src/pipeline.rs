//! The whole edit: RAW develop, film look, AI effect, diffusion, then halation, light leak and grain.
//! A Session holds one photo and caches the slow parts (RAW decoding, the models' results).

use crate::ai::{self, Pose, Subjects, MODEL_SIZES};
use crate::develop::{develop, DevelopOptions};
use crate::film;
use crate::fx::{self, contours::Contours, diffusion::{Diffusion, DiffusionOptions}, ghost::Ghost, mesh::Mesh, scene::Scene, skeleton::Skeleton};
use crate::geometry::{self, Geometry};
use crate::img::Image;
use crate::io::{self, RawLinear, RawOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const EFFECTS: [&str; 15] = [
    "None", "Ghosts", "Wireframe", "Contours", "Face mesh", "Points", "Noir pop", "Bokeh", "Rim light", "Glitch", "Thermal", "Comic", "Skeleton",
    "Crowd remover", "Trails",
];
pub const NONE: usize = 0;
pub const GHOSTS: usize = 1;
pub const WIREFRAME: usize = 2;
pub const CONTOURS: usize = 3;
pub const FACE_MESH: usize = 4;
pub const POINTS: usize = 5;
pub const SCENE_FIRST: usize = 6; // Noir pop .. Comic, in Scene's order
pub const SCENE_LAST: usize = 11;
pub const SKELETON: usize = 12;
pub const CROWD: usize = 13;
pub const TRAILS: usize = 14;

/// Effects made from several photos (the others work on one).
pub fn is_multi(effect: usize) -> bool {
    effect == CROWD || effect == TRAILS
}

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub raw: RawOptions,
    /// Film file name from the library (e.g. "57.bin"), "lut:" + the path of a .cube file, or None.
    pub film: Option<String>,
    pub strength: f32,
    pub develop: DevelopOptions,
    pub effect: usize,
    pub target: usize,
    pub model: usize,
    pub look: usize,
    pub face_style: usize,
    /// Ghosts: opacity; Wireframe/Contours: how dim the subject is; scene effects: strength. 0..1
    pub amount: f32,
    pub colour: usize,
    pub glow: usize,
    pub diffusion: usize,
    pub diffusion_opt: DiffusionOptions,
    /// turn, straighten and crop
    pub geometry: Geometry,
    /// camera look (index into camera::CAMERAS, 0 = none) and how much of it
    pub camera: usize,
    pub camera_strength: f32,
    /// the orange date stamp
    pub date_stamp: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            raw: RawOptions::default(),
            film: None,
            strength: 1.0,
            develop: DevelopOptions::default(),
            effect: NONE,
            target: 0,
            model: 1,
            look: 1,
            face_style: 0,
            amount: 0.85,
            colour: 0,
            glow: 2,
            diffusion: 0,
            diffusion_opt: DiffusionOptions::default(),
            geometry: Geometry::default(),
            camera: 0,
            camera_strength: 1.0,
            date_stamp: false,
        }
    }
}

/// What the models found, in fractions of the photo's size (so it fits any output size).
#[derive(Clone)]
struct Found {
    subjects: Option<Subjects>,
    poses: Vec<Pose>,
    /// face landmarks, 478 x/y pairs as fractions
    faces: Vec<Vec<f32>>,
}

pub struct Session {
    pub path: PathBuf,
    raw: Option<RawLinear>,
    base: Mutex<(RawOptions, Image)>,
    cache: Mutex<HashMap<String, Found>>,
    /// when the photo was taken, for the date stamp
    pub date: Option<(i32, u32, u32)>,
}

/// Long side of the copy the models look at.
const DETECT_SIZE: usize = 2048;

impl Session {
    pub fn open(path: &Path) -> Result<Session, String> {
        let (raw, base) = if io::is_raw(path) {
            let raw = io::decode_raw(path)?;
            let img = io::render_raw(&raw, RawOptions::default());
            (Some(raw), img)
        } else {
            (None, io::load_image(path)?)
        };
        let date = crate::camera::photo_date(path);
        Ok(Session { path: path.to_path_buf(), raw, base: Mutex::new((RawOptions::default(), base)), cache: Mutex::new(HashMap::new()), date })
    }

    pub fn is_raw(&self) -> bool {
        self.raw.is_some()
    }

    /// The camera's own white balance as a colour temperature (RAW files only).
    pub fn as_shot_kelvin(&self) -> Option<f32> {
        self.raw.as_ref().and_then(|r| r.as_shot_kelvin())
    }

    /// The photo developed with these RAW settings (non-RAW photos ignore them).
    pub fn base(&self, raw_opts: RawOptions) -> Image {
        let mut b = self.base.lock().unwrap();
        if let Some(raw) = &self.raw {
            if b.0 != raw_opts {
                *b = (raw_opts, io::render_raw(raw, raw_opts));
            }
        }
        b.1.clone()
    }

    /// Size of the photo as opened (upright, before any crop).
    pub fn size(&self) -> (usize, usize) {
        let b = self.base.lock().unwrap();
        (b.1.w, b.1.h)
    }

    /// Size of the finished picture at full resolution, after the crop.
    pub fn output_size(&self, s: &Settings) -> (usize, usize) {
        let (w, h) = self.size();
        s.geometry.size(w, h)
    }

    /// The developed photo turned, straightened and cropped, long side at most `long`.
    fn shaped(&self, s: &Settings, long: usize) -> Image {
        let mut b = self.base.lock().unwrap();
        if let Some(raw) = &self.raw {
            if b.0 != s.raw {
                *b = (s.raw, io::render_raw(raw, s.raw));
            }
        }
        geometry::apply(&b.1, &s.geometry, long)
    }

    /// True when rendering with these settings will run the AI models first (the slow part).
    pub fn needs_ai(&self, s: &Settings) -> bool {
        s.effect != NONE && !is_multi(s.effect) && !self.cache.lock().unwrap().contains_key(&found_key(s))
    }

    /// After a render: a note when the effect found nothing to work on, with what to try.
    pub fn nothing_found(&self, s: &Settings) -> Option<String> {
        if s.effect == NONE || is_multi(s.effect) {
            return None;
        }
        let cache = self.cache.lock().unwrap();
        let f = cache.get(&found_key(s))?;
        let empty = match s.effect {
            SKELETON => f.poses.is_empty(),
            FACE_MESH => f.faces.is_empty(),
            _ => f.subjects.as_ref().is_none_or(|x| x.count == 0),
        };
        if !empty {
            return None;
        }
        let bigger = if s.model < 2 { " Try AI model Fine." } else { "" };
        Some(match s.effect {
            SKELETON => format!("Skeleton found no people.{bigger}"),
            FACE_MESH => "Face mesh found no faces. They need to be fairly big in the picture and facing the camera.".to_string(),
            _ => {
                let what = ai::TARGET_NAMES[s.target.min(ai::TARGET_NAMES.len() - 1)].to_lowercase();
                let other = if s.target == ai::TARGET_NAMES.len() - 1 { "" } else { " Or set Target to Everything." };
                format!("{} found no {what}.{bigger}{other}", EFFECTS[s.effect])
            }
        })
    }

    /// The models' results for these settings, from the cache or by running them.
    fn found(&self, s: &Settings) -> Result<Found, String> {
        let key = found_key(s);
        if let Some(f) = self.cache.lock().unwrap().get(&key) {
            return Ok(f.clone());
        }
        let det = self.shaped(s, DETECT_SIZE);
        let size = MODEL_SIZES[s.model.min(2)];
        let (fw, fh) = (det.w as f32, det.h as f32);
        let to_frac = |pts: Vec<f32>| -> Vec<f32> { pts.chunks(2).flat_map(|p| [p[0] / fw, p[1] / fh]).collect() };
        let mut found = Found { subjects: None, poses: Vec::new(), faces: Vec::new() };
        match s.effect {
            SKELETON => {
                found.poses = ai::poses(&det, size)?
                    .into_iter()
                    .map(|mut p| {
                        for k in 0..17 {
                            p.kp[3 * k] /= fw;
                            p.kp[3 * k + 1] /= fh;
                        }
                        p.height /= fh;
                        p
                    })
                    .collect();
            }
            FACE_MESH => {
                found.faces = ai::faces(&det, None, 8)?.into_iter().map(to_frac).collect();
            }
            NONE | CROWD | TRAILS => {}
            _ => {
                let subj = ai::subjects(&det, size, s.target)?;
                if (s.effect == WIREFRAME || s.effect == POINTS) && s.target == 0 {
                    found.faces = ai::faces(&det, Some(&subj.boxes), 6)?.into_iter().map(to_frac).collect();
                }
                found.subjects = Some(subj);
            }
        }
        self.cache.lock().unwrap().insert(key, found.clone());
        Ok(found)
    }

    /// Renders the edit with the long side at most `long` pixels (usize::MAX for full size).
    pub fn render(&self, s: &Settings, long: usize) -> Result<Image, String> {
        let mut img = self.shaped(s, long);
        crate::camera::colour(&mut img, s.camera, s.camera_strength);
        apply_film(&mut img, s)?;
        if s.effect != NONE && !is_multi(s.effect) {
            let found = self.found(s)?;
            apply_effect(&mut img, s, &found, seed_of(&self.path));
        }
        finish(&mut img, s, seed_of(&self.path), self.date);
        Ok(img)
    }
}

fn found_key(s: &Settings) -> String {
    let g = &s.geometry;
    format!("{}-{}-{}-{}-{}-{}-{}-{}-{}-{}", s.effect, s.model, s.target, s.raw.exposure, g.quarter, g.angle, g.x, g.y, g.w, g.h)
}

fn seed_of(path: &Path) -> u64 {
    path.to_string_lossy().bytes().fold(1469598103934665603u64, |h, b| (h ^ b as u64).wrapping_mul(1099511628211))
}

fn apply_film(img: &mut Image, s: &Settings) -> Result<(), String> {
    let Some(name) = s.film.as_deref() else { return Ok(()) };
    if let Some(path) = name.strip_prefix("lut:") {
        crate::lut::load(Path::new(path))?.apply(&mut img.px, s.strength);
    } else if let Some(look) = film::load(name) {
        film::Applied::new(&look, s.strength).apply(&mut img.px);
    }
    Ok(())
}

/// The camera's lens and sensor character, diffusion, the date stamp, then the develop effects (grain last).
fn finish(img: &mut Image, s: &Settings, seed: u64, date: Option<(i32, u32, u32)>) {
    crate::camera::character(img, s.camera, s.camera_strength, seed);
    if s.diffusion > 0 {
        let (dw, dh) = scaled(img, 640);
        let preview = img.resized(dw, dh);
        fx::run(&Diffusion::new(&preview.px, dw, dh, s.diffusion, &s.diffusion_opt), img);
    }
    if s.date_stamp {
        if let Some(date) = date {
            crate::camera::date_stamp(img, date);
        }
    }
    let mut d = s.develop;
    d.seed = seed;
    develop(img, &d);
}

fn scaled(img: &Image, long: usize) -> (usize, usize) {
    let k = long as f32 / img.w.max(img.h) as f32;
    (((img.w as f32 * k).round() as usize).max(2), ((img.h as f32 * k).round() as usize).max(2))
}

fn apply_effect(img: &mut Image, s: &Settings, found: &Found, seed: u64) {
    let edges = ai::mesh_edges();
    match s.effect {
        SKELETON => {
            let mut sk = Skeleton::new(s.amount, s.glow, s.colour);
            for p in &found.poses {
                sk.add_person(&p.kp, p.height, img.w as f32 / img.h as f32);
            }
            fx::run(&sk, img);
        }
        FACE_MESH => {
            let (mw, mh) = scaled(img, 480);
            let mut mesh = Mesh::new(&vec![0u8; mw * mh], mw, mh, 0.0, s.glow, s.colour, seed, false);
            mesh.set_face_dim(s.amount * 0.5);
            for f in &found.faces {
                mesh.add_face(&to_px(f, mw, mh), edges, s.face_style);
            }
            fx::run(&mesh, img);
        }
        _ => {
            let Some(subj) = &found.subjects else { return };
            if subj.count == 0 {
                return;
            }
            let (mw, mh, mask) = (subj.mw, subj.mh, &subj.mask);
            let preview = img.resized(mw, mh);
            match s.effect {
                GHOSTS => fx::run(&Ghost::new(mask, mw, mh, s.glow, s.look, s.amount, s.colour, &preview.px), img),
                CONTOURS => fx::run(&Contours::new(mask, mw, mh, s.amount, s.glow, s.colour), img),
                WIREFRAME | POINTS => {
                    let mut mesh = Mesh::new(mask, mw, mh, s.amount, s.glow, s.colour, seed, s.effect == POINTS);
                    for f in &found.faces {
                        mesh.add_face(&to_px(f, mw, mh), edges, s.face_style);
                    }
                    fx::run(&mesh, img);
                }
                e if (SCENE_FIRST..=SCENE_LAST).contains(&e) => {
                    let (bw, bh) = scaled(img, 1600.min(img.w.max(img.h)));
                    let big = img.resized(bw, bh);
                    let sc = Scene::new(e - SCENE_FIRST, mask, mw, mh, &big.px, bw, bh, s.amount, s.colour, seed as i32);
                    fx::run(&sc, img);
                }
                _ => {}
            }
        }
    }
}

fn to_px(frac: &[f32], mw: usize, mh: usize) -> Vec<f32> {
    frac.chunks(2).flat_map(|p| [p[0] * mw as f32, p[1] * mh as f32]).collect()
}

/// Crowd remover / Trails over several photos (oldest first), then the film look and finishing.
pub fn render_multi(sessions: &[&Session], s: &Settings, long: usize) -> Result<Image, String> {
    let frames: Vec<Image> = sessions.iter().map(|x| x.shaped(s, long)).collect();
    let kind = if s.effect == TRAILS { fx::multishot::TRAILS } else { fx::multishot::CROWD };
    let mut img = fx::multishot::combine(&frames, kind, s.amount, s.colour)?;
    crate::camera::colour(&mut img, s.camera, s.camera_strength);
    apply_film(&mut img, s)?;
    finish(&mut img, s, sessions.first().map(|x| seed_of(&x.path)).unwrap_or(1), sessions.first().and_then(|x| x.date));
    Ok(img)
}
