//! Real-ESRGAN upscaling (BSD-3, github.com/xinntao/Real-ESRGAN) through ONNX Runtime, in overlapping tiles so
//! any size works in modest memory. Also "AI detail": upscale 4x, then shrink back, for crisper detail.

use crate::img::{pack, Image};
use ort::session::Session;
use ort::value::Tensor;
use std::sync::{Mutex, OnceLock};

pub const MODELS: [&str; 3] = ["Fast (general x4v3)", "Fast, denoised (general wdn x4v3)", "Max quality (x4plus)"];
pub const FAST: usize = 0;
pub const MAX: usize = 2;

static DATA: [&[u8]; 3] = [
    include_bytes!("../assets/upscale/realesr-general-x4v3.onnx"),
    include_bytes!("../assets/upscale/realesr-general-wdn-x4v3.onnx"),
    include_bytes!("../assets/upscale/RealESRGAN_x4plus.onnx"),
];

/// Seconds per input megapixel on the machine it was measured on (i7-10810U, 6 cores); used for estimates.
pub const SECONDS_PER_MP: [f32; 3] = [14.0, 14.0, 230.0];

static SESSIONS: Mutex<[Option<Session>; 3]> = Mutex::new([None, None, None]);

fn environment() -> Result<&'static ort::environment::Environment, String> {
    static ENV: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| ort::init().with_name("grainhouse").build().map_err(|e| format!("ONNX Runtime failed to start: {e}")))
        .as_ref()
        .map_err(|e| e.clone())
}

fn with_session<T>(model: usize, f: impl FnOnce(&mut Session) -> Result<T, String>) -> Result<T, String> {
    let mut all = SESSIONS.lock().unwrap();
    if all[model].is_none() {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let err = |e: String| format!("could not load the upscaler: {e}");
        let b = Session::builder(environment()?).map_err(|e| err(e.to_string()))?;
        let mut b = b.with_intra_threads(threads).map_err(|e| err(e.to_string()))?;
        let s = b.commit_from_memory(DATA[model]).map_err(|e| err(e.to_string()))?;
        all[model] = Some(s);
    }
    f(all[model].as_mut().unwrap())
}

/// Rough time for an upscale of a w x h image, in seconds.
pub fn estimate(w: usize, h: usize, model: usize) -> f32 {
    (w * h) as f32 / 1e6 * SECONDS_PER_MP[model.min(2)]
}

const TILE: usize = 192;
const PAD: usize = 16;

/// Upscales 4x. `progress` gets 0..1 and returns false to cancel.
pub fn upscale4(img: &Image, model: usize, progress: &dyn Fn(f32) -> bool) -> Result<Image, String> {
    let (w, h) = (img.w, img.h);
    let mut out = Image::new(w * 4, h * 4);
    let tiles_x = w.div_ceil(TILE);
    let tiles_y = h.div_ceil(TILE);
    let total = tiles_x * tiles_y;
    let mut done = 0;
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            if !progress(done as f32 / total as f32) {
                return Err("cancelled".into());
            }
            // the tile plus a margin of real pixels, so tile edges don't show
            let (x0, y0) = (tx * TILE, ty * TILE);
            let (x1, y1) = ((x0 + TILE).min(w), (y0 + TILE).min(h));
            let (px0, py0) = (x0.saturating_sub(PAD), y0.saturating_sub(PAD));
            let (px1, py1) = ((x1 + PAD).min(w), (y1 + PAD).min(h));
            let (tw, th) = (px1 - px0, py1 - py0);
            let mut input = vec![0f32; 3 * tw * th];
            for y in 0..th {
                for x in 0..tw {
                    let p = img.px[(py0 + y) * w + px0 + x];
                    let i = y * tw + x;
                    input[i] = ((p >> 16) & 255) as f32 / 255.0;
                    input[tw * th + i] = ((p >> 8) & 255) as f32 / 255.0;
                    input[2 * tw * th + i] = (p & 255) as f32 / 255.0;
                }
            }
            let result = with_session(model, |s| {
                let t = Tensor::from_array(([1usize, 3, th, tw], input)).map_err(|e| e.to_string())?;
                let outs = s.run(ort::inputs!["input" => t]).map_err(|e| format!("upscale failed: {e}"))?;
                let (_, data) = outs["output"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
                Ok(data.to_vec())
            })?;
            let (ow, oh) = (tw * 4, th * 4);
            let plane = ow * oh;
            let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            for y in (y0 - py0) * 4..(y1 - py0) * 4 {
                let row = (py0 * 4 + y) * out.w;
                for x in (x0 - px0) * 4..(x1 - px0) * 4 {
                    let i = y * ow + x;
                    out.px[row + px0 * 4 + x] = pack(to8(result[i]), to8(result[plane + i]), to8(result[2 * plane + i]));
                }
            }
            let _ = oh;
            done += 1;
        }
    }
    progress(1.0);
    Ok(out)
}
