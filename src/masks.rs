//! Masks that go where you point, like Lightroom's masking panel. A photo can have several mask layers. Each layer is
//! built from components (a brush, a linear or radial gradient, a luminance or colour range, sky, subject or
//! background) that are combined with Add, Subtract or Intersect, can be inverted, and carry 17 adjustments plus an
//! overall Amount. Positions are fractions of the picture, so a mask fits any output size.

use crate::adjust::{self, Adjust};
use crate::fx::extra::Mask as AiMask;
use crate::img::Image;
use crate::util::smooth;
use rayon::prelude::*;

pub const KINDS: [&str; 8] = ["Brush", "Linear gradient", "Radial gradient", "Luminance range", "Colour range", "Sky", "Subject", "Background"];
pub const BRUSH: usize = 0;
pub const LINEAR: usize = 1;
pub const RADIAL: usize = 2;
pub const LUMINANCE: usize = 3;
pub const COLOUR: usize = 4;
pub const SKY: usize = 5;
pub const SUBJECT: usize = 6;
pub const BACKGROUND: usize = 7;
pub const MODES: [&str; 3] = ["Add", "Subtract", "Intersect"];

/// Names of the 17 adjustments, in the order of `MaskAdjust::values`.
pub const ADJUSTMENTS: [&str; 17] = [
    "Temp", "Tint", "Exposure", "Contrast", "Highlights", "Shadows", "Whites", "Blacks", "Texture", "Clarity", "Dehaze", "Hue", "Saturation",
    "Sharpness", "Noise", "Moire", "Defringe",
];

/// One brush stroke: points along it, as fractions of the picture.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Stroke {
    pub pts: Vec<[f32; 2]>,
    /// brush size as a fraction of the picture's long edge
    pub size: f32,
    /// 0 hard edge .. 1 very soft
    pub feather: f32,
    /// how much each dab adds, 0..1
    pub flow: f32,
    pub erase: bool,
}

impl Default for Stroke {
    fn default() -> Self {
        Stroke { pts: Vec::new(), size: 0.08, feather: 0.5, flow: 0.7, erase: false }
    }
}

#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Component {
    pub kind: usize,
    /// index into MODES
    pub mode: usize,
    pub invert: bool,
    /// Linear: start x, y, end x, y. Radial: centre x, y, radius x, radius y. Range kinds and the rest: see `p`.
    pub a: [f32; 4],
    /// Linear: feather. Radial: feather, rotation (turns). Luminance: low, high, smoothness.
    /// Colour: hue (degrees), range 0..1, minimum saturation. Sky: how far down it reaches. Brush: density.
    pub p: [f32; 3],
    pub strokes: Vec<Stroke>,
}

impl Default for Component {
    fn default() -> Self {
        Component { kind: LINEAR, mode: 0, invert: false, a: [0.5, 0.2, 0.5, 0.6], p: [0.5, 0.0, 0.0], strokes: Vec::new() }
    }
}

impl Component {
    pub fn new(kind: usize) -> Component {
        let mut c = Component { kind, ..Default::default() };
        match kind {
            BRUSH => c.p = [1.0, 0.0, 0.0],
            LINEAR => {
                c.a = [0.5, 0.25, 0.5, 0.6];
                c.p = [0.5, 0.0, 0.0];
            }
            RADIAL => {
                c.a = [0.5, 0.5, 0.3, 0.3];
                c.p = [0.5, 0.0, 0.0];
            }
            LUMINANCE => c.p = [0.0, 0.5, 0.3],
            COLOUR => c.p = [200.0, 0.4, 0.05],
            SKY => c.p = [0.5, 0.0, 0.0],
            _ => {}
        }
        c
    }

    pub fn uses_subject(&self) -> bool {
        matches!(self.kind, SUBJECT | BACKGROUND)
    }
}

/// The 17 adjustments, each -1..1 (Sharpness and Noise 0..1), 0 = no change.
#[derive(Clone, Copy, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MaskAdjust {
    pub values: [f32; 17],
}

impl MaskAdjust {
    pub fn any(&self) -> bool {
        self.values.iter().any(|v| *v != 0.0)
    }
}

#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MaskLayer {
    pub name: String,
    pub visible: bool,
    pub comps: Vec<Component>,
    pub adj: MaskAdjust,
    /// overall strength of the layer, 0..1
    pub amount: f32,
}

impl Default for MaskLayer {
    fn default() -> Self {
        MaskLayer { name: "Mask".into(), visible: true, comps: Vec::new(), adj: MaskAdjust::default(), amount: 1.0 }
    }
}

impl MaskLayer {
    pub fn uses_subject(&self) -> bool {
        self.comps.iter().any(|c| c.uses_subject())
    }
}

pub fn any_subject(layers: &[MaskLayer]) -> bool {
    layers.iter().any(|l| l.visible && l.adj.any() && l.uses_subject())
}

fn to_lin(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// A coarse map of how much each part of the picture is selected.
pub struct Field {
    w: usize,
    h: usize,
    v: Vec<f32>,
}

impl Field {
    fn new(w: usize, h: usize, fill: f32) -> Field {
        Field { w, h, v: vec![fill; w * h] }
    }

    pub fn at(&self, fx: f32, fy: f32) -> f32 {
        let x = (fx * self.w as f32 - 0.5).clamp(0.0, self.w as f32 - 1.0);
        let y = (fy * self.h as f32 - 0.5).clamp(0.0, self.h as f32 - 1.0);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (tx, ty) = (x - x0 as f32, y - y0 as f32);
        let a = self.v[y0 * self.w + x0] * (1.0 - tx) + self.v[y0 * self.w + x1] * tx;
        let b = self.v[y1 * self.w + x0] * (1.0 - tx) + self.v[y1 * self.w + x1] * tx;
        a * (1.0 - ty) + b * ty
    }
}

fn brush_field(c: &Component, w: usize, h: usize, long: f32) -> Vec<f32> {
    let mut v = vec![0f32; w * h];
    let density = c.p[0].clamp(0.0, 1.0);
    for s in &c.strokes {
        let radius = (s.size.clamp(0.002, 0.5) * long).max(1.0);
        let hard = (1.0 - s.feather.clamp(0.0, 1.0)).max(0.0);
        let spacing = (radius * 0.2).max(1.0);
        let mut stamps: Vec<(f32, f32)> = Vec::new();
        let px: Vec<(f32, f32)> = s.pts.iter().map(|p| (p[0] * w as f32, p[1] * h as f32)).collect();
        for (i, &(x, y)) in px.iter().enumerate() {
            if i == 0 {
                stamps.push((x, y));
                continue;
            }
            let (x0, y0) = px[i - 1];
            let len = ((x - x0).powi(2) + (y - y0).powi(2)).sqrt();
            let n = (len / spacing).ceil().max(1.0) as usize;
            for k in 1..=n {
                let t = k as f32 / n as f32;
                stamps.push((x0 + (x - x0) * t, y0 + (y - y0) * t));
            }
        }
        for (cx, cy) in stamps {
            let x0 = ((cx - radius).floor().max(0.0)) as usize;
            let x1 = ((cx + radius).ceil() as usize).min(w.saturating_sub(1));
            let y0 = ((cy - radius).floor().max(0.0)) as usize;
            let y1 = ((cy + radius).ceil() as usize).min(h.saturating_sub(1));
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() / radius;
                    if d >= 1.0 {
                        continue;
                    }
                    let fall = 1.0 - smooth(hard, 1.0, d);
                    let a = (s.flow.clamp(0.0, 1.0) * fall).clamp(0.0, 1.0);
                    let o = &mut v[y * w + x];
                    if s.erase {
                        *o *= 1.0 - a;
                    } else {
                        *o += (density - *o).max(0.0) * a;
                    }
                }
            }
        }
    }
    v
}

fn luma_of(p: u32) -> f32 {
    (0.299 * ((p >> 16) & 255) as f32 + 0.587 * ((p >> 8) & 255) as f32 + 0.114 * (p & 255) as f32) / 255.0
}

/// The selection of one component at the coarse size (mw x mh), before Add / Subtract / Intersect.
fn component_field(c: &Component, small: &Image, mw: usize, mh: usize, ai: Option<&AiMask>, long: f32) -> Vec<f32> {
    let aspect = small.w as f32 / small.h as f32;
    let mut v = match c.kind {
        BRUSH => brush_field(c, mw, mh, long),
        _ => {
            let mut out = vec![0f32; mw * mh];
            out.par_chunks_mut(mw).enumerate().for_each(|(y, row)| {
                let fy = (y as f32 + 0.5) / mh as f32;
                for (x, o) in row.iter_mut().enumerate() {
                    let fx = (x as f32 + 0.5) / mw as f32;
                    let px = small.px[((fy * small.h as f32) as usize).min(small.h - 1) * small.w + ((fx * small.w as f32) as usize).min(small.w - 1)];
                    *o = match c.kind {
                        LINEAR => {
                            // 1 at the start point, fading to 0 at the end point
                            let (dx, dy) = ((c.a[2] - c.a[0]) * aspect, c.a[3] - c.a[1]);
                            let len2 = (dx * dx + dy * dy).max(1e-6);
                            let t = (((fx - c.a[0]) * aspect) * dx + (fy - c.a[1]) * dy) / len2;
                            let soft = 0.5 * c.p[0].clamp(0.0, 1.0) + 0.02;
                            1.0 - smooth(0.5 - soft, 0.5 + soft, t)
                        }
                        RADIAL => {
                            let (rx, ry) = (c.a[2].max(0.01), c.a[3].max(0.01));
                            let rot = c.p[1] * std::f32::consts::TAU;
                            let (dx, dy) = ((fx - c.a[0]) * aspect, fy - c.a[1]);
                            let (u, w) = (dx * rot.cos() + dy * rot.sin(), -dx * rot.sin() + dy * rot.cos());
                            let d = ((u / (rx * aspect)).powi(2) + (w / ry).powi(2)).sqrt();
                            let soft = 0.6 * c.p[0].clamp(0.0, 1.0) + 0.02;
                            1.0 - smooth(1.0 - soft, 1.0 + soft * 0.3, d)
                        }
                        LUMINANCE => {
                            let l = luma_of(px);
                            let (lo, hi, sm) = (c.p[0].min(c.p[1]), c.p[0].max(c.p[1]), c.p[2].clamp(0.0, 1.0) * 0.3 + 0.01);
                            smooth(lo - sm, lo + sm, l) * (1.0 - smooth(hi - sm, hi + sm, l))
                        }
                        COLOUR => {
                            let rgb = [((px >> 16) & 255) as f32 / 255.0, ((px >> 8) & 255) as f32 / 255.0, (px & 255) as f32 / 255.0];
                            let [_, a, b] = adjust::oklab(rgb);
                            let chroma = (a * a + b * b).sqrt();
                            let hue = b.atan2(a).to_degrees();
                            let width = 8.0 + 60.0 * c.p[1].clamp(0.0, 1.0);
                            let d = ((hue - c.p[0] + 540.0).rem_euclid(360.0) - 180.0).abs();
                            let by_hue = 1.0 - smooth(width * 0.5, width, d);
                            by_hue * smooth(c.p[2] * 0.3, c.p[2] * 0.3 + 0.03, chroma)
                        }
                        SKY => {
                            // approximate: blue or pale and bright, near the top of the picture
                            let rgb = [((px >> 16) & 255) as f32 / 255.0, ((px >> 8) & 255) as f32 / 255.0, (px & 255) as f32 / 255.0];
                            let blue = (rgb[2] - rgb[0].max(rgb[1]) + 0.08).clamp(0.0, 0.2) / 0.2;
                            let pale = if rgb[0].min(rgb[1]).min(rgb[2]) > 0.6 { 1.0 } else { 0.0 } * (1.0 - smooth(0.0, 0.5, fy));
                            let reach = 0.2 + 0.8 * c.p[0].clamp(0.0, 1.0);
                            (blue.max(pale * 0.7)) * (1.0 - smooth(reach * 0.6, reach, fy)) * smooth(0.25, 0.5, luma_of(px) + 0.1)
                        }
                        SUBJECT => ai.map_or(0.0, |k| k.at(fx, fy)),
                        BACKGROUND => 1.0 - ai.map_or(0.0, |k| k.at(fx, fy)),
                        _ => 0.0,
                    };
                }
            });
            out
        }
    };
    if c.invert {
        for x in v.iter_mut() {
            *x = 1.0 - *x;
        }
    }
    v
}

/// The finished selection of a layer: its components folded together with Add / Subtract / Intersect.
pub fn layer_field(layer: &MaskLayer, small: &Image, ai: Option<&AiMask>, mw: usize, mh: usize, long: f32) -> Field {
    let mut f = Field::new(mw, mh, 0.0);
    for (i, c) in layer.comps.iter().enumerate() {
        let m = component_field(c, small, mw, mh, ai, long);
        if i == 0 {
            f.v.copy_from_slice(&m);
            continue;
        }
        for (r, m) in f.v.iter_mut().zip(m) {
            *r = match c.mode {
                1 => *r * (1.0 - m),
                2 => *r * m,
                _ => *r + m - *r * m,
            };
        }
    }
    f
}

/// The 17 adjustments applied to the whole picture at full strength; the mask then decides where they show.
fn adjusted_copy(img: &Image, a: &MaskAdjust) -> Image {
    let v = a.values;
    let mut out = img.clone();
    // detail: texture, clarity, dehaze, sharpness, noise, moire (colour blotches), defringe
    let mut d = Adjust::default();
    d.texture = v[8];
    d.clarity = v[9];
    d.dehaze = v[10];
    d.sharpen = v[13].max(0.0);
    d.noise = v[14].max(0.0);
    d.colour_noise = v[15].max(0.0);
    d.defringe = v[16].max(0.0);
    if [d.texture, d.clarity, d.dehaze, d.sharpen, d.noise, d.colour_noise, d.defringe].iter().any(|x| *x != 0.0) {
        adjust::apply_detail(&mut out, &d);
    }
    let tone = v[..8].iter().any(|x| *x != 0.0) || v[11] != 0.0 || v[12] != 0.0;
    if !tone {
        return out;
    }
    let gain = (v[2] * 2.0).exp2();
    let (temp, tint) = (v[0] * 0.25, v[1] * 0.12);
    let hue = v[11] * 30.0f32.to_radians() * 3.0;
    let (ch, sh) = (hue.cos(), hue.sin());
    out.px.par_iter_mut().for_each(|p| {
        let c = [((*p >> 16) & 255) as f32 / 255.0, ((*p >> 8) & 255) as f32 / 255.0, (*p & 255) as f32 / 255.0];
        // white balance and exposure in linear light
        let lin = [
            to_lin(c[0]) * gain * (1.0 + temp),
            to_lin(c[1]) * gain * (1.0 - tint),
            to_lin(c[2]) * gain * (1.0 - temp),
        ];
        let mut o = lin.map(to_srgb);
        let l = 0.299 * o[0] + 0.587 * o[1] + 0.114 * o[2];
        // contrast: an S-curve around the middle
        o = o.map(|x| x + v[3] * 0.6 * (x * x * (3.0 - 2.0 * x) - x));
        // highlights, shadows, whites and blacks weigh the tones by brightness
        let w_sh = (1.0 - l).powi(3);
        let w_hi = l.powi(3);
        let w_wh = smooth(0.6, 1.0, l);
        let w_bl = 1.0 - smooth(0.0, 0.4, l);
        let delta = 0.25 * v[4] * w_hi + 0.25 * v[5] * w_sh + 0.2 * v[6] * w_wh + 0.2 * v[7] * w_bl;
        o = o.map(|x| x + delta);
        // hue (a rotation of the colour plane) and saturation
        if v[11] != 0.0 {
            let y = 0.299 * o[0] + 0.587 * o[1] + 0.114 * o[2];
            let (i, q) = (0.596 * o[0] - 0.274 * o[1] - 0.322 * o[2], 0.211 * o[0] - 0.523 * o[1] + 0.312 * o[2]);
            let (i2, q2) = (i * ch - q * sh, i * sh + q * ch);
            o = [y + 0.956 * i2 + 0.621 * q2, y - 0.272 * i2 - 0.647 * q2, y - 1.106 * i2 + 1.703 * q2];
        }
        if v[12] != 0.0 {
            let y = 0.299 * o[0] + 0.587 * o[1] + 0.114 * o[2];
            o = o.map(|x| y + (x - y) * (1.0 + v[12]).max(0.0));
        }
        let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        *p = (q(o[0]) << 16) | (q(o[1]) << 8) | q(o[2]);
    });
    out
}

/// Applies every visible layer that has adjustments, in order. `ai` is the subject mask (people), if there is one.
pub fn apply_all(img: &mut Image, layers: &[MaskLayer], ai: Option<&AiMask>) {
    let long = img.w.max(img.h) as f32;
    // masks are worked out on a coarse grid, which is plenty for soft edges
    let k = (1024.0 / long).min(1.0);
    let (mw, mh) = (((img.w as f32 * k) as usize).max(2), ((img.h as f32 * k) as usize).max(2));
    let small = img.resized(mw, mh);
    for layer in layers {
        if !layer.visible || !layer.adj.any() || layer.comps.is_empty() || layer.amount <= 0.0 {
            continue;
        }
        if layer.uses_subject() && ai.is_none() {
            continue;
        }
        let field = layer_field(layer, &small, ai, mw, mh, mw.max(mh) as f32);
        let adjusted = adjusted_copy(img, &layer.adj);
        let (w, h) = (img.w, img.h);
        let amount = layer.amount.clamp(0.0, 1.0);
        img.px.par_chunks_mut(w).zip(adjusted.px.par_chunks(w)).enumerate().for_each(|(y, (row, adj))| {
            let fy = (y as f32 + 0.5) / h as f32;
            for x in 0..w {
                let m = field.at((x as f32 + 0.5) / w as f32, fy) * amount;
                if m <= 0.002 {
                    continue;
                }
                let (a, b) = (row[x], adj[x]);
                let mix = |s: u32| {
                    let (ca, cb) = (((a >> s) & 255) as f32, ((b >> s) & 255) as f32);
                    ((ca + (cb - ca) * m).clamp(0.0, 255.0) + 0.5) as u32
                };
                row[x] = (mix(16) << 16) | (mix(8) << 8) | mix(0);
            }
        });
    }
}

/// A grayscale picture of the layer's selection, for drawing a red overlay on screen (0..255 per pixel).
pub fn preview(layer: &MaskLayer, img: &Image, ai: Option<&AiMask>, out_w: usize, out_h: usize) -> Vec<u8> {
    let small = img.resized(out_w.max(2), out_h.max(2));
    let f = layer_field(layer, &small, ai, out_w.max(2), out_h.max(2), out_w.max(out_h) as f32);
    f.v.iter().map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8).collect()
}
