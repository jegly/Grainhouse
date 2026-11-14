//! Spot healing and cloning: click a blemish or dust spot and it is covered with a matching patch from nearby.
//! Positions are fractions of the picture, so a spot fits any output size.

use crate::img::Image;
use crate::util::smooth;

pub const MODES: [&str; 3] = ["Heal", "Clone", "Remove"];
pub const HEAL: usize = 0;
pub const CLONE: usize = 1;
pub const REMOVE: usize = 2;

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Spot {
    /// centre, as fractions of the picture
    pub x: f32,
    pub y: f32,
    /// radius as a fraction of the long edge
    pub radius: f32,
    /// 0 hard edge .. 1 soft
    pub feather: f32,
    pub opacity: f32,
    /// index into MODES
    pub mode: usize,
    /// where to copy from, in fractions; picked automatically when `auto` is set
    pub sx: f32,
    pub sy: f32,
    pub auto: bool,
}

impl Default for Spot {
    fn default() -> Self {
        Spot { x: 0.5, y: 0.5, radius: 0.02, feather: 0.5, opacity: 1.0, mode: HEAL, sx: 0.5, sy: 0.5, auto: true }
    }
}

fn px(img: &Image, x: i32, y: i32) -> [f32; 3] {
    let (x, y) = (x.clamp(0, img.w as i32 - 1) as usize, y.clamp(0, img.h as i32 - 1) as usize);
    let p = img.px[y * img.w + x];
    [((p >> 16) & 255) as f32, ((p >> 8) & 255) as f32, (p & 255) as f32]
}

/// The average colour round a ring of radius r about (cx, cy).
fn ring_mean(img: &Image, cx: f32, cy: f32, r: f32) -> [f32; 3] {
    let mut sum = [0f32; 3];
    let n = 16;
    for k in 0..n {
        let a = k as f32 / n as f32 * std::f32::consts::TAU;
        let c = px(img, (cx + r * a.cos()) as i32, (cy + r * a.sin()) as i32);
        for i in 0..3 {
            sum[i] += c[i];
        }
    }
    sum.map(|v| v / n as f32)
}

/// A source centre for a spot: the nearby ring that best matches the ring round the spot, with a little texture
/// kept so the patch doesn't look flat.
fn pick_source(img: &Image, cx: f32, cy: f32, r: f32) -> (f32, f32) {
    let mut best = (f32::MAX, cx + r * 3.0, cy);
    let ring_r = r * 1.3;
    let target: Vec<[f32; 3]> = (0..16)
        .map(|k| {
            let a = k as f32 / 16.0 * std::f32::consts::TAU;
            px(img, (cx + ring_r * a.cos()) as i32, (cy + ring_r * a.sin()) as i32)
        })
        .collect();
    for ring in 0..3 {
        let dist = r * (2.8 + ring as f32 * 1.2);
        for k in 0..16 {
            let a = k as f32 / 16.0 * std::f32::consts::TAU;
            let (sx, sy) = (cx + dist * a.cos(), cy + dist * a.sin());
            if sx - ring_r < 0.0 || sy - ring_r < 0.0 || sx + ring_r >= img.w as f32 || sy + ring_r >= img.h as f32 {
                continue;
            }
            let mut cost = 0.0;
            for (j, t) in target.iter().enumerate() {
                let b = j as f32 / 16.0 * std::f32::consts::TAU;
                let c = px(img, (sx + ring_r * b.cos()) as i32, (sy + ring_r * b.sin()) as i32);
                cost += (c[0] - t[0]).abs() + (c[1] - t[1]).abs() + (c[2] - t[2]).abs();
            }
            cost += dist * 0.05;
            if cost < best.0 {
                best = (cost, sx, sy);
            }
        }
    }
    (best.1, best.2)
}

pub fn apply(img: &mut Image, spots: &[Spot]) {
    if spots.is_empty() {
        return;
    }
    let (w, h) = (img.w as f32, img.h as f32);
    let long = w.max(h);
    for s in spots {
        let (cx, cy) = (s.x * w, s.y * h);
        let r = (s.radius * long).max(2.0);
        let (sx, sy) = if s.auto || s.mode == REMOVE { pick_source(img, cx, cy, r) } else { (s.sx * w, s.sy * h) };
        let off = if s.mode == CLONE {
            [0.0; 3]
        } else {
            let d = ring_mean(img, cx, cy, r * 1.3);
            let c = ring_mean(img, sx, sy, r * 1.3);
            [d[0] - c[0], d[1] - c[1], d[2] - c[2]]
        };
        let hard = (1.0 - s.feather.clamp(0.0, 1.0)) * 0.9;
        let reach = r * 1.15;
        let (x0, x1) = ((cx - reach).floor().max(0.0) as usize, ((cx + reach).ceil() as usize).min(img.w.saturating_sub(1)));
        let (y0, y1) = ((cy - reach).floor().max(0.0) as usize, ((cy + reach).ceil() as usize).min(img.h.saturating_sub(1)));
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() / r;
                if d >= 1.15 {
                    continue;
                }
                let m = (1.0 - smooth(hard, 1.0, d)) * s.opacity.clamp(0.0, 1.0);
                if m <= 0.003 {
                    continue;
                }
                let from = px(img, (x as f32 + sx - cx) as i32, (y as f32 + sy - cy) as i32);
                let cur = px(img, x as i32, y as i32);
                let q = |i: usize| ((cur[i] + (from[i] + off[i] - cur[i]) * m).clamp(0.0, 255.0) + 0.5) as u32;
                img.px[y * img.w + x] = (q(0) << 16) | (q(1) << 8) | q(2);
            }
        }
    }
}
