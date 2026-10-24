//! Renders every single-photo effect with default settings side by side, and says which found nothing:
//! cargo run --release --example effects_sheet -- photo out.jpg
use alpha_studio::{img::Image, io, pipeline};
use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let session = pipeline::Session::open(Path::new(&args[0])).expect("open");
    let effects: Vec<usize> = (1..pipeline::EFFECTS.len()).filter(|&e| !pipeline::is_multi(e)).collect();
    let mut tiles = Vec::new();
    for &e in &effects {
        let mut s = pipeline::Settings::default();
        s.effect = e;
        let t = Instant::now();
        tiles.push(session.render(&s, 560).expect("render"));
        let note = session.nothing_found(&s).unwrap_or_default();
        println!("{:14} {:5.2} s  {}", pipeline::EFFECTS[e], t.elapsed().as_secs_f32(), note);
    }
    let cols = 4;
    let (tw, th) = (tiles[0].w, tiles[0].h);
    let mut sheet = Image::new(cols * (tw + 8), effects.len().div_ceil(cols) * (th + 8));
    sheet.px.fill(0xffffff);
    for (i, t) in tiles.iter().enumerate() {
        let (ox, oy) = ((i % cols) * (tw + 8), (i / cols) * (th + 8));
        for y in 0..t.h.min(th) {
            for x in 0..t.w.min(tw) {
                sheet.px[(oy + y) * sheet.w + ox + x] = t.px[y * t.w + x];
            }
        }
    }
    io::save_jpeg(&sheet, Path::new(&args[1]), 88).expect("save");
}
