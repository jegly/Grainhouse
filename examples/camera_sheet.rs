//! Renders every camera look side by side, for tuning: cargo run --release --example camera_sheet -- photo out.jpg
use alpha_studio::{camera, img::Image, io, pipeline};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (input, out) = (Path::new(&args[0]), Path::new(&args[1]));
    let session = pipeline::Session::open(input).expect("open");
    let cols = 4;
    let cams = camera::CAMERAS.len();
    let mut tiles = Vec::new();
    for c in 0..cams {
        let mut s = pipeline::Settings::default();
        s.camera = c;
        s.date_stamp = c == 10 || c == 9;
        tiles.push(session.render(&s, 560).expect("render"));
    }
    let (tw, th) = (tiles[0].w, tiles[0].h);
    let rows = cams.div_ceil(cols);
    let mut sheet = Image::new(cols * (tw + 8), rows * (th + 8));
    sheet.px.fill(0xffffff);
    for (i, t) in tiles.iter().enumerate() {
        let (ox, oy) = ((i % cols) * (tw + 8), (i / cols) * (th + 8));
        for y in 0..t.h.min(th) {
            for x in 0..t.w.min(tw) {
                sheet.px[(oy + y) * sheet.w + ox + x] = t.px[y * t.w + x];
            }
        }
    }
    io::save_jpeg(&sheet, out, 88).expect("save");
    for (i, c) in camera::CAMERAS.iter().enumerate() {
        println!("{:2} row {} col {}: {}", i, i / cols + 1, i % cols + 1, c.name);
    }
}
