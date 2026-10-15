use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
// The app icon is for the bundle and the .exe, the running app never loads it.
#[exclude = "app-icon/*"]
struct AppAssets;

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(file) = AppAssets::get(path) {
            return Ok(Some(file.data));
        }
        gpui_kit_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths: Vec<SharedString> = AppAssets::iter()
            .filter(|p| p.starts_with(path))
            .map(Into::into)
            .collect();
        paths.extend(gpui_kit_assets::Assets.list(path)?);
        Ok(paths)
    }
}
