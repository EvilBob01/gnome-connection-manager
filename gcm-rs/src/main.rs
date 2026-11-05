#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod config;
mod fonts;
mod gcm_import;
mod launch;
mod mobaxterm;
mod putty;
mod sessionlog;
mod sftp;
mod telnet;
mod terminal;
mod tunnel;

const USAGE: &str = "usage: gcm-rs [SESSION | [proto://][user@]host[:port]]...

Opens a tab for each argument: the name of a saved session, or an address
in quick-connect form (ssh is the default protocol).";

fn main() -> eframe::Result {
    if let Some(code) = launch::askpass() {
        std::process::exit(code);
    }
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("gcm-rs {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let store = match config::Store::load_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gcm: cannot load configuration: {e:#}");
            std::process::exit(1);
        }
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("GNOME Connection Manager")
            .with_app_id("gcm-rs")
            .with_inner_size([1200.0, 760.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../packaging/gcm-rs.png"))
                    .unwrap_or_default(),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "gcm-rs",
        options,
        Box::new(|cc| {
            let mut app = app::App::new(cc, store);
            app.open_args(&args);
            Ok(Box::new(app))
        }),
    )
}
