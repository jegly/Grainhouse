//! Loading photos (JPEG, PNG, TIFF, and camera RAW such as Sony ARW) and saving JPEGs.

use crate::img::Image;
use image::{codecs::jpeg::JpegEncoder, DynamicImage, ImageDecoder, ImageReader};
use rawler::decoders::RawDecodeParams;
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::rawsource::RawSource;
use rayon::prelude::*;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

/// How RAW files are turned into a picture: exposure and the camera-like contrast curve.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RawOptions {
    /// Exposure change in stops.
    pub exposure: f32,
    /// 0 = flat (straight sRGB), 1 = camera-like contrast.
    pub contrast: f32,
}

impl Default for RawOptions {
    fn default() -> Self {
        RawOptions { exposure: 0.0, contrast: 1.0 }
    }
}

pub fn is_raw(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "arw" | "srf" | "sr2" | "dng" | "cr2" | "cr3" | "nef" | "nrw" | "raf" | "orf" | "rw2" | "pef" | "srw" | "3fr" | "iiq" | "erf" | "mos"
    )
}

/// A RAW file decoded to linear camera-white-balanced sRGB, before exposure and tone, kept so the RAW
/// controls can be changed without decoding again.
pub struct RawLinear {
    pub w: usize,
    pub h: usize,
    pub rgb: Vec<f32>,
    pub orientation: u16,
}

pub fn decode_raw(path: &Path) -> Result<RawLinear, String> {
    let src = RawSource::new(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let decoder = rawler::get_decoder(&src).map_err(|e| format!("not a supported RAW file: {e}"))?;
    let params = RawDecodeParams::default();
    let raw = decoder.raw_image(&src, &params, false).map_err(|e| format!("RAW decode failed: {e}"))?;
    let orientation = decoder.raw_metadata(&src, &params).ok().and_then(|m| m.exif.orientation).unwrap_or(1);
    // everything up to (not including) the sRGB gamma: we apply exposure and tone in linear light
    let steps = [
        ProcessingStep::Rescale,
        ProcessingStep::Demosaic,
        ProcessingStep::CropActiveArea,
        ProcessingStep::WhiteBalance,
        ProcessingStep::Calibrate,
        ProcessingStep::CropDefault,
    ];
    let inter = RawDevelop::new_with(&steps).develop_intermediate(&raw).map_err(|e| format!("RAW develop failed: {e}"))?;
    let dim = inter.dim();
    let rgb: Vec<f32> = match inter {
        Intermediate::ThreeColor(p) => p.flatten(),
        Intermediate::Monochrome(p) => p.data.iter().flat_map(|&v| [v, v, v]).collect(),
        Intermediate::FourColor(p) => p.data.iter().flat_map(|c| [c[0], c[1], c[2]]).collect(),
    };
    Ok(RawLinear { w: dim.w, h: dim.h, rgb, orientation })
}

/// Linear RAW data to an 8-bit picture: exposure, a soft highlight shoulder, sRGB, then a gentle S-curve
/// like a camera JPEG's.
/// Sony's look on top of a neutral develop, measured by matching our output to the A7 II's own JPEG of the same
/// shot (Standard creative style): deeper shadows, brighter mid-tones and highlights.
const CAMERA_CURVE: [u8; 256] = [0,1,2,2,3,4,5,6,7,8,9,10,11,12,13,15,16,17,18,20,21,23,25,26,28,30,32,35,37,39,42,44,46,48,50,52,55,57,59,61,63,65,67,69,71,73,75,77,79,81,83,85,87,90,92,94,96,98,100,101,103,105,107,109,111,113,115,116,118,120,121,123,124,126,127,129,130,132,133,135,136,138,139,140,142,143,145,146,147,148,150,151,152,153,155,156,157,158,160,161,162,163,164,165,167,168,169,170,171,172,173,175,176,177,178,179,180,181,182,183,184,185,186,187,188,189,190,191,193,194,195,196,197,198,198,199,200,201,202,203,203,204,205,206,207,207,208,209,210,211,211,212,213,214,214,215,216,216,217,218,218,219,220,220,221,222,222,223,223,224,224,225,225,226,226,227,227,228,228,229,229,230,230,230,231,231,232,232,232,233,233,233,234,234,234,235,235,235,236,236,236,237,237,238,238,238,239,239,239,240,240,240,241,241,241,242,242,242,243,243,243,244,244,244,245,245,245,246,246,246,247,247,247,248,248,248,249,249,249,250,250,250,251,251,251,252,252,252,253,253,253,254,254,254,255,255];
const CAMERA_SATURATION: f32 = 1.05;

pub fn render_raw(raw: &RawLinear, opt: RawOptions) -> Image {
    let gain = 2f32.powf(opt.exposure);
    // tone lookup over linear 0..4 (anything brighter rolls off into white)
    const N: usize = 4096;
    let lut: Vec<u8> = (0..N)
        .map(|i| {
            let v = i as f32 / (N - 1) as f32 * 4.0;
            let s = shoulder(v);
            let g = srgb_gamma(s);
            let c = (g + 0.55 * (g * g * (3.0 - 2.0 * g) - g)).clamp(0.0, 1.0) * 255.0;
            // contrast blends from this neutral develop to the camera's look
            let cam = CAMERA_CURVE[(c + 0.5) as usize] as f32;
            (c + (cam - c) * opt.contrast).clamp(0.0, 255.0).round() as u8
        })
        .collect();
    let look = |v: f32| -> u8 {
        let x = (v * gain).max(0.0) / 4.0;
        lut[((x * (N - 1) as f32) as usize).min(N - 1)]
    };
    let sat = 1.0 + (CAMERA_SATURATION - 1.0) * opt.contrast;
    let mut img = Image::new(raw.w, raw.h);
    img.px.par_iter_mut().enumerate().for_each(|(i, p)| {
        let c = &raw.rgb[i * 3..i * 3 + 3];
        let (r, g, b) = (look(c[0]) as f32, look(c[1]) as f32, look(c[2]) as f32);
        let l = 0.299 * r + 0.587 * g + 0.114 * b;
        *p = crate::img::pack_f(l + (r - l) * sat, l + (g - l) * sat, l + (b - l) * sat);
    });
    orient(img, raw.orientation)
}

fn shoulder(v: f32) -> f32 {
    const K: f32 = 0.82;
    if v <= K {
        v
    } else {
        K + (1.0 - K) * (1.0 - (-(v - K) / (1.0 - K)).exp())
    }
}

fn srgb_gamma(v: f32) -> f32 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Loads a non-RAW photo, turned upright from its EXIF orientation.
pub fn load_image(path: &Path) -> Result<Image, String> {
    let mut dec = ImageReader::open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let orientation = dec.orientation().ok();
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| format!("cannot decode {}: {e}", path.display()))?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(Image::from_rgb(&img.to_rgb8()))
}

/// Turns an image upright from an EXIF orientation value (1..8).
pub fn orient(img: Image, o: u16) -> Image {
    let rgb = img.to_rgb();
    let out = match o {
        2 => image::imageops::flip_horizontal(&rgb),
        3 => image::imageops::rotate180(&rgb),
        4 => image::imageops::flip_vertical(&rgb),
        5 => image::imageops::flip_horizontal(&image::imageops::rotate90(&rgb)),
        6 => image::imageops::rotate90(&rgb),
        7 => image::imageops::flip_horizontal(&image::imageops::rotate270(&rgb)),
        8 => image::imageops::rotate270(&rgb),
        _ => return img,
    };
    Image::from_rgb(&out)
}

pub fn save_jpeg(img: &Image, path: &Path, quality: u8) -> Result<(), String> {
    let f = File::create(path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let mut enc = JpegEncoder::new_with_quality(BufWriter::new(f), quality);
    enc.encode_image(&img.to_rgb()).map_err(|e| format!("JPEG encode failed: {e}"))
}
