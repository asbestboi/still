//! Still: a quiet photo editor.

// no console window behind the app on Windows (release builds)
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod io;
mod layers;
mod masks;
mod pipeline;
mod ui_kit;

/// Headless batch mode: `still --render <look> <input> <output>`
fn render_cli(args: &[String]) -> Result<(), String> {
    let [look, input, output] = args else {
        return Err("usage: still --render <preset name | file.stillpreset> <input> <output>".into());
    };
    let look_path = std::path::Path::new(look);
    let params = if io::is_look_file(look_path) {
        // a shared look file: use its first look
        io::read_looks(look_path)?.remove(0).1
    } else {
        let looks = pipeline::builtin_presets().into_iter().chain(io::load_saved().presets);
        looks
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(look))
            .map(|(_, p)| p)
            .ok_or_else(|| format!("unknown preset '{look}'"))?
    };
    let doc = io::load(std::path::Path::new(input), 1800)?;
    let mut settings = io::ExportSettings::default();
    let out = std::path::PathBuf::from(output);
    let ext = out.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    settings.format = match ext.as_str() {
        "png" => io::Format::Png,
        "tif" | "tiff" => io::Format::Tiff,
        "webp" => io::Format::Webp,
        _ => io::Format::Jpeg,
    };
    let t = std::time::Instant::now();
    io::export(&doc, &params, &settings, &out)?;
    eprintln!("{} → {} ({:.2}s)", input, output, t.elapsed().as_secs_f32());
    Ok(())
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|a| a == "--render").unwrap_or(false) {
        if let Err(e) = render_cli(&args[1..]) {
            eprintln!("still: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let initial = args.first().filter(|a| !a.starts_with('-')).map(std::path::PathBuf::from);

    // Leave a core for the interface so it stays smooth while a preview renders.
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let _ = rayon::ThreadPoolBuilder::new().num_threads(cores.saturating_sub(1).max(1)).build_global();

    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Still")
            .with_app_id("still")
            .with_inner_size([1440.0, 920.0])
            .with_min_inner_size([960.0, 640.0])
            .with_drag_and_drop(true)
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).unwrap_or_default()),
        ..Default::default()
    };

    // winit can't receive dropped files on Wayland, so prefer XWayland when it's
    // there, unless switched off in Settings or with STILL_WAYLAND=1.
    #[cfg(target_os = "linux")]
    {
        let native = io::load_saved().prefs.native_wayland || std::env::var_os("STILL_WAYLAND").is_some_and(|v| v != "0");
        if !native && std::env::var_os("DISPLAY").is_some() {
            options.event_loop_builder = Some(Box::new(|b| {
                use winit::platform::x11::EventLoopBuilderExtX11;
                b.with_x11();
            }));
        }
    }

    eframe::run_native("Still", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, initial)))))
}
