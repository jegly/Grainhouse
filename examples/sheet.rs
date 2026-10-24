//! Contact sheets for tuning looks on a real photo:
//!   cargo run --release --example sheet -- photo out.jpg cameras|effects|new|creative|adjust|frames
use grainhouse::{adjust::Adjust, camera, creative, img::Image, io, pipeline::{self, Session, Settings}};
use std::path::Path;
use std::time::Instant;

fn variants(mode: &str) -> Vec<(String, Settings)> {
    let d = Settings::default();
    match mode {
        "cameras" => (0..camera::CAMERAS.len())
            .map(|c| (camera::CAMERAS[c].name.to_string(), Settings { camera: c, date_stamp: c == 9 || c == 10, ..d.clone() }))
            .collect(),
        "effects" => (1..pipeline::EFFECTS.len())
            .filter(|&e| !pipeline::is_multi(e))
            .map(|e| (pipeline::EFFECTS[e].to_string(), Settings { effect: e, ..d.clone() }))
            .collect(),
        "new" => (pipeline::EXTRA_FIRST..pipeline::EFFECTS.len())
            .filter(|&e| !pipeline::is_multi(e))
            .map(|e| (pipeline::EFFECTS[e].to_string(), Settings { effect: e, look: 0, ..d.clone() }))
            .collect(),
        "creative" => (0..creative::NAMES.len()).map(|n| (creative::NAMES[n].to_string(), Settings { film: Some(format!("gen:{n}")), ..d.clone() })).collect(),
        "adjust" => {
            let a = Adjust::default();
            let mut grade = a;
            grade.grade = [[200.0, 0.6], [0.0, 0.0], [60.0, 0.5]];
            let mut mixer = a;
            mixer.mixer[3] = [-0.6, -0.4, -0.5];
            mixer.mixer[1] = [0.0, 0.4, 0.3];
            [
                ("none", a),
                ("vibrance +80", Adjust { vibrance: 0.8, ..a }),
                ("saturation -100", Adjust { saturation: -1.0, ..a }),
                ("grade teal/yellow", grade),
                ("mixer greens", mixer),
                ("clarity +80", Adjust { clarity: 0.8, ..a }),
                ("texture +80", Adjust { texture: 0.8, ..a }),
                ("dehaze +70", Adjust { dehaze: 0.7, ..a }),
                ("dehaze -60", Adjust { dehaze: -0.6, ..a }),
                ("sharpen 100", Adjust { sharpen: 1.0, ..a }),
                ("vignette -70", Adjust { vignette: -0.7, ..a }),
                ("vignette +50", Adjust { vignette: 0.5, ..a }),
            ]
            .into_iter()
            .map(|(n, a)| (n.to_string(), Settings { adjust: a, ..d.clone() }))
            .collect()
        }
        "frames" => (0..grainhouse::frame::NAMES.len())
            .map(|f| (grainhouse::frame::NAMES[f].to_string(), Settings { frame: f, film: if f == 6 { Some("57.bin".into()) } else { None }, ..d.clone() }))
            .collect(),
        _ => panic!("unknown mode {mode}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let session = Session::open(Path::new(&args[0])).expect("open");
    let mut tiles = Vec::new();
    for (name, s) in variants(args.get(2).map(|s| s.as_str()).unwrap_or("effects")) {
        let t = Instant::now();
        tiles.push(session.render(&s, 480).expect("render"));
        println!("{:2} {:24} {:5.2} s  {}", tiles.len() - 1, name, t.elapsed().as_secs_f32(), session.nothing_found(&s).unwrap_or_default());
    }
    let cols = 4;
    let (tw, th) = (tiles.iter().map(|t| t.w).max().unwrap(), tiles.iter().map(|t| t.h).max().unwrap());
    let mut sheet = Image::new(cols * (tw + 6), tiles.len().div_ceil(cols) * (th + 6));
    sheet.px.fill(0xffffff);
    for (i, t) in tiles.iter().enumerate() {
        let (ox, oy) = ((i % cols) * (tw + 6), (i / cols) * (th + 6));
        for y in 0..t.h {
            for x in 0..t.w {
                sheet.px[(oy + y) * sheet.w + ox + x] = t.px[y * t.w + x];
            }
        }
    }
    io::save_jpeg(&sheet, Path::new(&args[1]), 88).expect("save");
}
