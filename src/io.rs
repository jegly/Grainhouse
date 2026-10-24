//! Loading photos (JPEG, PNG, TIFF, and camera RAW such as Sony ARW) and saving JPEGs.

use crate::img::{pack, Image};
use image::{codecs::jpeg::JpegEncoder, DynamicImage, ImageDecoder, ImageReader};
use rawler::decoders::RawDecodeParams;
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::rawsource::RawSource;
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
pub fn render_raw(raw: &RawLinear, opt: RawOptions) -> Image {
    let gain = 2f32.powf(opt.exposure);
    // tone lookup over linear 0..4 (anything brighter rolls off into white)
    const N: usize = 4096;
    let lut: Vec<u8> = (0..N)
        .map(|i| {
            let v = i as f32 / (N - 1) as f32 * 4.0;
            let s = shoulder(v);
            let g = srgb_gamma(s);
            let c = g + opt.contrast * 0.55 * (g * g * (3.0 - 2.0 * g) - g);
            (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
        })
        .collect();
    let look = |v: f32| -> u8 {
        let x = (v * gain).max(0.0) / 4.0;
        lut[((x * (N - 1) as f32) as usize).min(N - 1)]
    };
    let mut img = Image::new(raw.w, raw.h);
    for (i, p) in img.px.iter_mut().enumerate() {
        let c = &raw.rgb[i * 3..i * 3 + 3];
        *p = pack(look(c[0]), look(c[1]), look(c[2]));
    }
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
