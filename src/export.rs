//! Export: format, quality, size, Real-ESRGAN upscaling, sharpening, and keeping the original's EXIF.

use crate::img::{pack_f, unpack, Image};
use crate::upscale;
use crate::util::gaussian;
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, ImageFormat};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const FORMATS: [&str; 3] = ["JPEG", "PNG", "TIFF"];
pub const SIZES: [&str; 3] = ["Original size", "Long edge", "Percent"];
pub const UPSCALES: [&str; 3] = ["No upscaling", "2x (Real-ESRGAN)", "4x (Real-ESRGAN)"];
pub const SHARPEN: [&str; 3] = ["No sharpening", "Smart sharpen", "AI detail (Real-ESRGAN)"];

#[derive(Clone, Debug, PartialEq)]
pub struct ExportOptions {
    pub format: usize,
    pub quality: u8,
    pub size: usize,
    pub long_edge: usize,
    pub percent: f32,
    pub upscale: usize,
    pub upscale_model: usize,
    pub sharpen: usize,
    /// Smart sharpen strength, 0..2
    pub sharpen_amount: f32,
    pub keep_exif: bool,
    pub suffix: String,
    /// Folder to save into; None = next to the original.
    pub folder: Option<PathBuf>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            format: 0,
            quality: 92,
            size: 0,
            long_edge: 3000,
            percent: 50.0,
            upscale: 0,
            upscale_model: upscale::FAST,
            sharpen: 0,
            sharpen_amount: 0.8,
            keep_exif: true,
            suffix: "_alpha".into(),
            folder: None,
        }
    }
}

impl ExportOptions {
    pub fn extension(&self) -> &'static str {
        ["jpg", "png", "tif"][self.format.min(2)]
    }

    /// Where a photo exported from `src` goes.
    pub fn target(&self, src: &Path) -> PathBuf {
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("photo");
        let dir = self.folder.clone().unwrap_or_else(|| src.parent().map(Path::to_path_buf).unwrap_or_default());
        dir.join(format!("{stem}{}.{}", self.suffix, self.extension()))
    }

    /// Rough seconds for the slow (AI) steps on an image of this size.
    pub fn estimate(&self, w: usize, h: usize) -> f32 {
        if self.upscale > 0 {
            upscale::estimate(w, h, self.upscale_model)
        } else if self.sharpen == 2 {
            upscale::estimate(w, h, upscale::FAST)
        } else {
            0.0
        }
    }
}

/// Applies the export steps to a finished edit. `progress` (0..1) returns false to cancel.
pub fn process(img: Image, opt: &ExportOptions, progress: &dyn Fn(f32) -> bool) -> Result<Image, String> {
    let (w0, h0) = (img.w, img.h);
    let mut img = img;
    if opt.upscale > 0 {
        let up = upscale::upscale4(&img, opt.upscale_model, progress)?;
        img = if opt.upscale == 1 { resize(&up, w0 * 2, h0 * 2) } else { up };
    } else if opt.sharpen == 2 {
        // AI detail: Real-ESRGAN at 4x, then back to the same size
        let up = upscale::upscale4(&img, upscale::FAST, progress)?;
        img = resize(&up, w0, h0);
    }
    let (w, h) = (img.w, img.h);
    let target = match opt.size {
        1 => {
            let k = opt.long_edge as f32 / w.max(h) as f32;
            Some(((w as f32 * k).round() as usize, (h as f32 * k).round() as usize))
        }
        2 => Some(((w as f32 * opt.percent / 100.0).round() as usize, (h as f32 * opt.percent / 100.0).round() as usize)),
        _ => None,
    };
    if let Some((tw, th)) = target {
        if tw >= 1 && th >= 1 && (tw, th) != (w, h) {
            img = resize(&img, tw, th);
        }
    }
    if opt.sharpen == 1 {
        smart_sharpen(&mut img, opt.sharpen_amount);
    }
    Ok(img)
}

fn resize(img: &Image, w: usize, h: usize) -> Image {
    Image::from_rgb(&image::imageops::resize(&img.to_rgb(), w as u32, h as u32, FilterType::Lanczos3))
}

/// Sharpening on brightness only, at two scales (fine detail and local contrast), with soft limits so
/// edges don't get halos and flat areas don't get noisy.
pub fn smart_sharpen(img: &mut Image, amount: f32) {
    let (w, h) = (img.w, img.h);
    let lum: Vec<f32> = img.px.par_iter().map(|&p| {
        let (r, g, b) = unpack(p);
        0.299 * r + 0.587 * g + 0.114 * b
    }).collect();
    let mut fine = lum.clone();
    let mut wide = lum.clone();
    gaussian(&mut fine, w, h, 1);
    gaussian(&mut wide, w, h, ((w.max(h) as f32 / 1500.0).round() as usize).max(2));
    img.px.par_iter_mut().enumerate().for_each(|(i, p)| {
        let d1 = lum[i] - fine[i];
        let d2 = fine[i] - wide[i];
        // ignore tiny differences (noise), limit big ones (halos)
        let soft = |d: f32, t: f32, lim: f32| {
            let m = d.abs();
            if m < t { d * (m / t) } else { d.signum() * (m.min(lim)) }
        };
        let delta = amount * (1.1 * soft(d1, 2.0, 24.0) + 0.35 * soft(d2, 1.0, 30.0));
        let (r, g, b) = unpack(*p);
        *p = pack_f(r + delta, g + delta, b + delta);
    });
}

pub fn save(img: &Image, path: &Path, opt: &ExportOptions, source: Option<&Path>) -> Result<(), String> {
    match opt.format {
        0 => {
            let mut buf = Vec::new();
            JpegEncoder::new_with_quality(&mut buf, opt.quality.clamp(1, 100))
                .encode_image(&img.to_rgb())
                .map_err(|e| format!("JPEG encode failed: {e}"))?;
            if opt.keep_exif {
                if let Some(exif) = source.and_then(read_exif) {
                    buf = insert_exif(&buf, &exif);
                }
            }
            let mut f = BufWriter::new(File::create(path).map_err(|e| format!("cannot write {}: {e}", path.display()))?);
            f.write_all(&buf).map_err(|e| e.to_string())
        }
        1 => img.to_rgb().save_with_format(path, ImageFormat::Png).map_err(|e| format!("PNG save failed: {e}")),
        _ => img.to_rgb().save_with_format(path, ImageFormat::Tiff).map_err(|e| format!("TIFF save failed: {e}")),
    }
}

/// The source JPEG's EXIF (APP1) segment, with its orientation set to 1 (the pixels are already upright).
fn read_exif(path: &Path) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path).ok()?.read_to_end(&mut data).ok()?;
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if marker == 0xE1 && data.get(i + 4..i + 10) == Some(b"Exif\0\0") {
            let mut seg = data[i..i + 2 + len].to_vec();
            set_orientation(&mut seg[10..], 1);
            return Some(seg);
        }
        i += 2 + len;
    }
    None
}

/// Sets the orientation tag (0x0112) in IFD0 of a TIFF-format EXIF block.
fn set_orientation(t: &mut [u8], value: u16) {
    if t.len() < 8 {
        return;
    }
    let le = &t[0..2] == b"II";
    let r16 = |b: &[u8], o: usize| if le { u16::from_le_bytes([b[o], b[o + 1]]) } else { u16::from_be_bytes([b[o], b[o + 1]]) };
    let r32 = |b: &[u8], o: usize| if le { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) } else { u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) };
    let ifd = r32(t, 4) as usize;
    if ifd + 2 > t.len() {
        return;
    }
    let n = r16(t, ifd) as usize;
    for k in 0..n {
        let e = ifd + 2 + k * 12;
        if e + 12 > t.len() {
            return;
        }
        if r16(t, e) == 0x0112 {
            let v = if le { value.to_le_bytes() } else { value.to_be_bytes() };
            t[e + 8] = v[0];
            t[e + 9] = v[1];
            return;
        }
    }
}

fn insert_exif(jpeg: &[u8], exif: &[u8]) -> Vec<u8> {
    // after SOI, and after a JFIF APP0 if the encoder wrote one
    let mut at = 2;
    if jpeg.len() > 4 && jpeg[2] == 0xFF && jpeg[3] == 0xE0 {
        at += 2 + u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
    }
    let mut out = Vec::with_capacity(jpeg.len() + exif.len());
    out.extend_from_slice(&jpeg[..at]);
    out.extend_from_slice(exif);
    out.extend_from_slice(&jpeg[at..]);
    out
}
