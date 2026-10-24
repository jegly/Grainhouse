//! Command-line use of Grainhouse, mainly for testing:
//!   grainhouse-cli photo.ARW -o out.jpg --film "Portra 400" --effect Ghosts --look Dreamy --diffusion "White Mist 1/2"
//!   grainhouse-cli a.jpg b.jpg c.jpg -o out.jpg --effect Trails
//!   grainhouse-cli photo.ARW -o out.jpg --rotate cw --straighten 2.5 --crop 0.1,0.1,0.8,0.8 (fractions x,y,w,h)
//!   grainhouse-cli photo.ARW -o out.jpg --auto              (automatic light, like the Auto button)
//!   grainhouse-cli photo.ARW -o out.jpg --settings look.json (a saved preset or any settings file)
//!   grainhouse-cli --films

use grainhouse::{ai, develop, export, film, fx, pipeline, upscale};
use std::path::PathBuf;
use std::time::Instant;

fn pick(list: &[&str], name: &str, what: &str) -> usize {
    list.iter().position(|n| n.eq_ignore_ascii_case(name)).unwrap_or_else(|| {
        eprintln!("unknown {what} {name:?}; choose from: {}", list.join(", "));
        std::process::exit(2)
    })
}

pub fn main() {
    run(std::env::args().skip(1).collect());
}

/// Runs with these arguments (without the program name).
pub fn run(args: Vec<String>) {
    if args.iter().any(|a| a == "--films") {
        for f in film::films() {
            println!("{:8} {:14} {}", f.file, f.brand, f.name);
        }
        return;
    }
    let mut s = pipeline::Settings::default();
    let mut inputs = Vec::new();
    let mut out = PathBuf::from("out.jpg");
    let mut ex = export::ExportOptions::default();
    let mut long = usize::MAX;
    let mut look: Option<String> = None;
    let mut auto = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut val = || {
            i += 1;
            args.get(i).cloned().unwrap_or_default()
        };
        match a {
            "-o" => out = PathBuf::from(val()),
            "--size" => long = val().parse().unwrap_or(usize::MAX),
            "--film" => {
                let name = val();
                let f = film::films().into_iter().find(|f| f.name.eq_ignore_ascii_case(&name) || f.file == name);
                s.film = Some(f.map(|f| f.file.to_string()).unwrap_or_else(|| {
                    eprintln!("no film called {name:?} (see --films)");
                    std::process::exit(2)
                }));
            }
            "--lut" => s.film = Some(format!("lut:{}", val())),
            "--camera" => s.camera = pick(&grainhouse::camera::names(), &val(), "camera"),
            "--camera-strength" => s.camera_strength = val().parse().unwrap_or(1.0),
            "--date-stamp" => s.date_stamp = true,
            "--strength" => s.strength = val().parse().unwrap_or(1.0),
            "--effect" => s.effect = pick(&pipeline::EFFECTS, &val(), "effect"),
            "--target" => s.target = pick(&ai::TARGET_NAMES, &val(), "target"),
            "--model" => s.model = pick(&ai::MODEL_NAMES, &val(), "model"),
            "--look" => look = Some(val()),
            "--face" => s.face_style = pick(&fx::mesh::FACE_STYLES, &val(), "face style"),
            "--amount" => s.amount = val().parse().unwrap_or(0.85),
            "--colour" => s.colour = pick(&fx::COLOURS, &val(), "colour"),
            "--glow" => s.glow = pick(&fx::GLOW_NAMES, &val(), "glow"),
            "--diffusion" => s.diffusion = pick(&fx::diffusion::NAMES, &val(), "diffusion"),
            "--halation" => s.develop.halation = true,
            "--leak" => s.develop.light_leak = true,
            "--grain" => s.develop.grain = pick(&develop::GRAIN_NAMES, &val(), "grain"),
            "--diffusion-strength" => s.diffusion_opt.strength = val().parse().unwrap_or(1.0),
            "--diffusion-size" => s.diffusion_opt.size = val().parse().unwrap_or(1.0),
            "--bloom" => s.diffusion_opt.bloom = val().parse().unwrap_or(1.0),
            "--warmth" => s.diffusion_opt.warmth = val().parse().unwrap_or(0.0),
            "--crop" => {
                let v: Vec<f32> = val().split(',').filter_map(|x| x.parse().ok()).collect();
                if v.len() == 4 {
                    (s.geometry.x, s.geometry.y, s.geometry.w, s.geometry.h) = (v[0], v[1], v[2], v[3]);
                }
            }
            "--straighten" => s.geometry.angle = val().parse().unwrap_or(0.0),
            "--rotate" => match val().as_str() {
                "cw" | "90" => s.geometry.turn(true),
                "ccw" | "-90" | "270" => s.geometry.turn(false),
                "180" => {
                    s.geometry.turn(true);
                    s.geometry.turn(true);
                }
                _ => {}
            },
            "--auto" => auto = true,
            "--settings" => {
                let path = val();
                match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<pipeline::Settings>(&t).map_err(|e| e.to_string())) {
                    Ok(loaded) => s = loaded,
                    Err(e) => {
                        eprintln!("cannot read settings {path}: {e}");
                        std::process::exit(2)
                    }
                }
            }
            "--kelvin" => s.raw.kelvin = val().parse().ok(),
            "--tint" => s.raw.tint = val().parse().unwrap_or(0.0),
            "--shadows" => s.raw.shadows = val().parse().unwrap_or(0.0),
            "--highlights" => s.raw.highlights = val().parse().unwrap_or(0.0),
            "--exposure" => s.raw.exposure = val().parse().unwrap_or(0.0),
            "--upscale" => ex.upscale = match val().as_str() { "2x" | "2" => 1, "4x" | "4" => 2, _ => 0 },
            "--upscaler" => ex.upscale_model = pick(&["fast", "clean", "max"], &val(), "upscaler"),
            "--sharpen" => ex.sharpen = match val().as_str() { "smart" => 1, "ai" => 2, _ => 0 },
            "--sharpen-amount" => ex.sharpen_amount = val().parse().unwrap_or(0.8),
            "--quality" => ex.quality = val().parse().unwrap_or(92),
            "--long-edge" => {
                ex.size = 1;
                ex.long_edge = val().parse().unwrap_or(3000);
            }
            _ => inputs.push(PathBuf::from(a)),
        }
        i += 1;
    }
    // the Look names depend on the effect, so they're matched once everything is read
    if let Some(name) = look {
        s.look = pick(pipeline::looks(s.effect), &name, "look");
    } else if s.effect != pipeline::GHOSTS {
        s.look = 0;
    }
    if inputs.is_empty() {
        eprintln!("usage: grainhouse-cli PHOTO... -o out.jpg [--film NAME] [--effect NAME] ... (--films lists the films)");
        std::process::exit(2);
    }
    let t0 = Instant::now();
    let sessions: Vec<pipeline::Session> = inputs
        .iter()
        .map(|p| pipeline::Session::open(p).unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1)
        }))
        .collect();
    if auto {
        s.raw = sessions[0].auto_light(s.raw);
        eprintln!("auto: exposure {:+.2}, shadows {:+.2}, highlights {:+.2}, whites {:+.2}, blacks {:+.2}", s.raw.exposure, s.raw.shadows, s.raw.highlights, s.raw.whites, s.raw.blacks);
    }
    let t1 = Instant::now();
    let result = if pipeline::is_multi(s.effect) { pipeline::render_multi(&sessions.iter().collect::<Vec<_>>(), &s, long) } else { sessions[0].render(&s, long) };
    let img = result.unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    let est = ex.estimate(img.w, img.h);
    if est > 1.0 {
        eprintln!("upscaling with {}: about {:.0} s", upscale::MODELS[ex.upscale_model], est);
    }
    let img = export::process(img, &ex, &|p| {
        eprint!("\r{:3.0}%", p * 100.0);
        true
    })
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    let t2 = Instant::now();
    ex.format = match out.extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_ascii_lowercase().as_str() {
        "png" => 1,
        "tif" | "tiff" => 2,
        _ => 0,
    };
    export::save(&img, &out, &ex, Some(&inputs[0])).unwrap_or_else(|e| eprintln!("{e}"));
    println!(
        "{}x{} -> {}  (open {:.1} s, render {:.1} s, save {:.1} s)",
        img.w,
        img.h,
        out.display(),
        (t1 - t0).as_secs_f32(),
        (t2 - t1).as_secs_f32(),
        t2.elapsed().as_secs_f32()
    );
}
