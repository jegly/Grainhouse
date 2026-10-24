//! Command-line use of Alpha Studio, mainly for testing:
//!   alpha-cli photo.ARW -o out.jpg --film "Portra 400" --effect Ghosts --look Dreamy --diffusion "White Mist 1/2"
//!   alpha-cli a.jpg b.jpg c.jpg -o out.jpg --effect Trails
//!   alpha-cli --films

use alpha_studio::{ai, develop, export, film, fx, pipeline, upscale};
use std::path::PathBuf;
use std::time::Instant;

fn pick(list: &[&str], name: &str, what: &str) -> usize {
    list.iter().position(|n| n.eq_ignore_ascii_case(name)).unwrap_or_else(|| {
        eprintln!("unknown {what} {name:?}; choose from: {}", list.join(", "));
        std::process::exit(2)
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
            "--strength" => s.strength = val().parse().unwrap_or(1.0),
            "--effect" => s.effect = pick(&pipeline::EFFECTS, &val(), "effect"),
            "--target" => s.target = pick(&ai::TARGET_NAMES, &val(), "target"),
            "--model" => s.model = pick(&ai::MODEL_NAMES, &val(), "model"),
            "--look" => s.look = pick(&fx::ghost::LOOKS, &val(), "look"),
            "--face" => s.face_style = pick(&fx::mesh::FACE_STYLES, &val(), "face style"),
            "--amount" => s.amount = val().parse().unwrap_or(0.85),
            "--colour" => s.colour = pick(&fx::COLOURS, &val(), "colour"),
            "--glow" => s.glow = pick(&fx::GLOW_NAMES, &val(), "glow"),
            "--diffusion" => s.diffusion = pick(&fx::diffusion::NAMES, &val(), "diffusion"),
            "--halation" => s.develop.halation = true,
            "--leak" => s.develop.light_leak = true,
            "--grain" => s.develop.grain = pick(&develop::GRAIN_NAMES, &val(), "grain"),
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
    if inputs.is_empty() {
        eprintln!("usage: alpha-cli PHOTO... -o out.jpg [--film NAME] [--effect NAME] ... (--films lists the films)");
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
    let t1 = Instant::now();
    let result = if pipeline::is_multi(s.effect) { pipeline::render_multi(&sessions, &s, long) } else { sessions[0].render(&s, long) };
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
