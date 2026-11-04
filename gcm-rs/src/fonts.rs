//! Terminal fonts: egui's built-in Hack has no CJK and few symbols, so add
//! common system fonts as fallbacks, plus an optional user-chosen primary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

fn fallback_candidates() -> Vec<PathBuf> {
    if cfg!(windows) {
        let fonts = PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()))
            .join("Fonts");
        ["seguisym.ttf", "msgothic.ttc", "msyh.ttc", "malgun.ttf"]
            .iter()
            .map(|f| fonts.join(f))
            .collect()
    } else {
        [
            // Symbols and box drawing
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
            // CJK
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSerifCJK-Regular.ttc",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
}

fn load(path: &Path) -> Option<Arc<FontData>> {
    let bytes = std::fs::read(path).ok()?;
    Some(Arc::new(FontData::from_owned(bytes)))
}

/// `primary` is a font file path; empty keeps the built-in monospace font.
pub fn install(ctx: &egui::Context, primary: &str) {
    let mut defs = FontDefinitions::default();
    if !primary.is_empty() {
        match load(Path::new(primary)) {
            Some(data) => {
                defs.font_data.insert("user-mono".into(), data);
                defs.families
                    .entry(FontFamily::Monospace)
                    .or_default()
                    .insert(0, "user-mono".into());
            }
            None => log::warn!("cannot load font {primary}"),
        }
    }
    for (i, path) in fallback_candidates().iter().enumerate() {
        if let Some(data) = load(path) {
            let name = format!("fallback-{i}");
            defs.font_data.insert(name.clone(), data);
            for family in [FontFamily::Monospace, FontFamily::Proportional] {
                defs.families.entry(family).or_default().push(name.clone());
            }
        }
    }
    ctx.set_fonts(defs);
}
