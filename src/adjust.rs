//! Colour and detail for any photo: vibrance, saturation, a colour mixer, colour grading (worked out in Oklab and
//! baked into a 3D LUT, so it's quick), then clarity, texture, dehaze, sharpening and vignette.

use crate::img::Image;
use crate::lut::Lut;
use crate::util::{gaussian, smooth};
use rayon::prelude::*;

pub const MIXER_NAMES: [&str; 8] = ["Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta"];
/// Oklab hue (degrees) of each mixer colour.
const MIXER_HUES: [f32; 8] = [29.0, 55.0, 110.0, 142.0, 195.0, 264.0, 293.0, 328.0];
pub const GRADE_NAMES: [&str; 3] = ["Shadows", "Midtones", "Highlights"];

#[derive(Clone, Copy, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Adjust {
    /// -1..1: lifts muted colours more than strong ones, and skin less
    pub vibrance: f32,
    /// -1 (black and white) .. 1
    pub saturation: f32,
    /// per mixer colour: hue shift, saturation, luminance, each -1..1
    pub mixer: [[f32; 3]; 8],
    /// shadows, midtones, highlights: hue (degrees) and amount (0..1)
    pub grade: [[f32; 2]; 3],
    /// -1 favours shadows .. 1 favours highlights
    pub balance: f32,
    /// -1..1 midtone local contrast
    pub clarity: f32,
    /// -1..1 fine detail
    pub texture: f32,
    /// -1 (adds haze) .. 1 (removes haze)
    pub dehaze: f32,
    /// 0..1
    pub sharpen: f32,
    /// -1 darker corners .. 1 lighter
    pub vignette: f32,
}

impl Adjust {
    fn colour_identity(&self) -> bool {
        self.vibrance == 0.0
            && self.saturation == 0.0
            && self.mixer.iter().all(|m| m.iter().all(|v| *v == 0.0))
            && self.grade.iter().all(|g| g[1] == 0.0)
    }
}

fn to_lin(x: f32) -> f32 {
    if x <= 0.04045 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.0031308 { x * 12.92 } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 }
}

pub fn oklab(c: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = c.map(to_lin);
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    [
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    ]
}

pub fn from_oklab(c: [f32; 3]) -> [f32; 3] {
    let l = (c[0] + 0.3963377774 * c[1] + 0.2158037573 * c[2]).powi(3);
    let m = (c[0] - 0.1055613458 * c[1] - 0.0638541728 * c[2]).powi(3);
    let s = (c[0] - 0.0894841775 * c[1] - 1.2914855480 * c[2]).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    ]
    .map(to_srgb)
}

/// How much each mixer colour applies to a hue (they add up to 1).
fn mixer_weights(h: f32) -> [f32; 8] {
    let mut w = [0f32; 8];
    for i in 0..8 {
        let (a, b) = (MIXER_HUES[i], MIXER_HUES[(i + 1) % 8]);
        let span = (b - a).rem_euclid(360.0);
        let d = (h - a).rem_euclid(360.0);
        if d <= span {
            let t = d / span;
            w[i] += 1.0 - t;
            w[(i + 1) % 8] += t;
            break;
        }
    }
    w
}

fn colour(a: &Adjust, c: [f32; 3]) -> [f32; 3] {
    let [mut l, ca, cb] = oklab(c);
    let mut chroma = (ca * ca + cb * cb).sqrt();
    let mut hue = cb.atan2(ca).to_degrees();
    if chroma > 1e-4 {
        let w = mixer_weights(hue.rem_euclid(360.0));
        let (mut dh, mut ds, mut dl) = (0.0, 0.0, 0.0);
        for i in 0..8 {
            dh += w[i] * a.mixer[i][0];
            ds += w[i] * a.mixer[i][1];
            dl += w[i] * a.mixer[i][2];
        }
        hue += dh * 30.0;
        chroma *= (1.0 + ds).max(0.0);
        // only coloured areas change brightness, so greys stay put
        l += dl * 0.2 * (chroma / 0.12).min(1.0);
        // vibrance: muted colours more, skin tones (orange hues) less
        let skin = 1.0 - 0.6 * (1.0 - ((hue.rem_euclid(360.0) - 55.0) / 30.0).abs().min(1.0));
        let v = if a.vibrance > 0.0 { a.vibrance * (1.0 - (chroma / 0.2).min(1.0)) * skin } else { a.vibrance };
        chroma *= (1.0 + v).max(0.0) * (1.0 + a.saturation).max(0.0);
    }
    // colour grading: tint shadows, midtones and highlights, keeping brightness
    let t = (l - a.balance * 0.2).clamp(0.0, 1.0);
    let ws = (1.0 - t).powi(2);
    let wh = t.powi(2);
    let wm = (1.0 - ws - wh).max(0.0);
    let (mut na, mut nb) = (chroma * hue.to_radians().cos(), chroma * hue.to_radians().sin());
    for (g, w) in a.grade.iter().zip([ws, wm, wh]) {
        if g[1] > 0.0 {
            let r = g[0].to_radians();
            na += g[1] * 0.06 * w * r.cos();
            nb += g[1] * 0.06 * w * r.sin();
        }
    }
    from_oklab([l, na, nb])
}

/// Vibrance, saturation, mixer and grading.
pub fn apply_colour(img: &mut Image, a: &Adjust) {
    if a.colour_identity() {
        return;
    }
    let lut = Lut::from_fn(33, |c| colour(a, c));
    lut.apply(&mut img.px, 1.0);
}

fn luma(p: u32) -> f32 {
    (0.299 * ((p >> 16) & 255) as f32 + 0.587 * ((p >> 8) & 255) as f32 + 0.114 * (p & 255) as f32) / 255.0
}

fn pack(c: [f32; 3]) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    (q(c[0]) << 16) | (q(c[1]) << 8) | q(c[2])
}

fn unpack(p: u32) -> [f32; 3] {
    [((p >> 16) & 255) as f32 / 255.0, ((p >> 8) & 255) as f32 / 255.0, (p & 255) as f32 / 255.0]
}

/// Clarity, texture, dehaze and sharpening. Sizes follow the picture's size, so a preview matches the export.
pub fn apply_detail(img: &mut Image, a: &Adjust) {
    let (w, h) = (img.w, img.h);
    let long = w.max(h) as f32;
    if a.dehaze != 0.0 {
        let mut haze: Vec<f32> = img.px.iter().map(|&p| unpack(p).into_iter().fold(1.0, f32::min)).collect();
        gaussian(&mut haze, w, h, ((0.015 * long) as usize).max(1));
        let d = a.dehaze.clamp(-1.0, 1.0);
        img.px.par_iter_mut().zip(haze.par_iter()).for_each(|(p, &hz)| {
            let c = unpack(*p);
            let o = if d > 0.0 {
                let t = (1.0 - d * 0.85 * hz).max(0.15);
                c.map(|v| (v - (1.0 - t)) / t)
            } else {
                let k = -d * 0.5 * (0.4 + hz);
                c.map(|v| v * (1.0 - k) + 0.82 * k)
            };
            *p = pack(o);
        });
    }
    if a.clarity != 0.0 || a.texture != 0.0 || a.sharpen > 0.0 {
        let l: Vec<f32> = img.px.par_iter().map(|&p| luma(p)).collect();
        let blur = |r: f32| {
            let mut b = l.clone();
            gaussian(&mut b, w, h, (r as usize).max(1));
            b
        };
        let big = (a.clarity != 0.0).then(|| blur(0.012 * long));
        let mid = (a.texture != 0.0).then(|| blur(0.0025 * long));
        let fine = (a.sharpen > 0.0).then(|| blur((long / 3000.0).max(1.0)));
        img.px.par_iter_mut().enumerate().for_each(|(i, p)| {
            let v = l[i];
            let mut d = 0.0;
            if let Some(b) = &big {
                d += a.clarity * 0.9 * (v - b[i]) * (4.0 * v * (1.0 - v)).max(0.0);
            }
            if let Some(b) = &mid {
                d += a.texture * 1.2 * (v - b[i]);
            }
            if let Some(b) = &fine {
                let e = v - b[i];
                // leave flat areas alone so noise isn't sharpened
                d += a.sharpen * 1.8 * e * smooth(0.004, 0.02, e.abs());
            }
            if d != 0.0 {
                *p = pack(unpack(*p).map(|c| c + d));
            }
        });
    }
}

/// Vignette after cropping: darker (negative) or lighter (positive) corners.
pub fn apply_vignette(img: &mut Image, amount: f32) {
    if amount == 0.0 {
        return;
    }
    let (w, h) = (img.w, img.h);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let norm = (cx * cx + cy * cy).sqrt();
    img.px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, p) in row.iter_mut().enumerate() {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let f = smooth(0.35, 1.05, (dx * dx + dy * dy).sqrt() / norm).powf(1.4);
            let c = unpack(*p);
            *p = pack(if amount < 0.0 { c.map(|v| v * (1.0 + amount * 0.85 * f)) } else { c.map(|v| v + (1.0 - v) * amount * 0.6 * f) });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oklab_round_trip_and_identity() {
        for c in [[0.2, 0.5, 0.8], [1.0, 0.0, 0.0], [0.5, 0.5, 0.5]] {
            let back = from_oklab(oklab(c));
            assert!(c.iter().zip(back).all(|(a, b)| (a - b).abs() < 1e-3), "{c:?} {back:?}");
        }
        let a = Adjust::default();
        let c = colour(&a, [0.3, 0.6, 0.2]);
        assert!((c[0] - 0.3).abs() < 1e-3 && (c[1] - 0.6).abs() < 1e-3);
        let w = mixer_weights(100.0);
        assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        let grey = colour(&Adjust { saturation: 1.0, ..a }, [0.5, 0.5, 0.5]);
        assert!((grey[0] - grey[2]).abs() < 1e-3);
    }
}
