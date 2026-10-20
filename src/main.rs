#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use assets::Assets;
use gpui::prelude::*;
use gpui::{
    px, relative, size, AnyView, App, Application, AssetSource, Bounds, TitlebarOptions,
    WindowBounds, WindowOptions,
};
use gpui_component::{theme::ThemeMode, Root};

mod account;
mod app_discovery;
mod assets;
mod connection;
mod file_associations;
mod file_edit;
mod file_open;
mod forward;
mod import;
mod logging;
mod preferences;
mod shell;
mod ssh;
mod ssh_target;
mod term_emulator;
// Reads the Termius app's local database. Only Windows stores the vault key
// in a form flint can use; on macOS and Linux a signed-in Termius encrypts its
// hosts with an account key that is not kept locally, so the feature is
// Windows only. The termius-export JSON importer works on every platform.
#[cfg(windows)]
mod termius_local;
mod theme;
mod ui;
mod update;
mod vault;
mod version;

use preferences::{Preferences, ThemePreference};
use ui::root_view::RootView;

fn main() {
    vault::memory::disable_core_dumps();

    logging::init();

    let platform = gpui_platform::current_platform(false);
    Application::with_platform(platform)
        .with_assets(Assets)
        .run(|cx: &mut App| {
            // The assets folder is embedded whole, so the font comes from
            // there and is not compiled in a second time.
            let font = Assets
                .load("fonts/CascadiaCode.ttf")
                .expect("read the embedded assets")
                .expect("Cascadia Code is among the embedded assets");
            cx.text_system()
                .add_fonts(vec![font])
                .expect("load Cascadia Code font");

            gpui_component::init(cx);
            crate::ui::terminal_view::init(cx);
            crate::ui::file_pane_keys::init(cx);
            crate::ui::app_view::init(cx);
            crate::ui::settings::init(cx);

            // Loaded before the vault so the unlock screen is already themed.
            let prefs = Preferences::load_or_default();
            let theme_preference = prefs.theme;
            cx.set_global(prefs);
            theme::apply_preference(theme_preference, None, cx);

            let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(920.), px(540.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("flint".into()),
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    // macOS: a movable window makes the native title bar drag-track
                    // every click in its strip before delivering it, which lags the
                    // title-bar search. Disable native moving; we drag the window
                    // ourselves via `start_window_move()` on the drag region.
                    // Windows must keep it: there the drag region only works
                    // because the platform maps it to a caption hit-test, and gpui
                    // skips that mapping when the window is not movable.
                    is_movable: !cfg!(target_os = "macos"),
                    ..Default::default()
                },
                |window, cx| {
                    window
                        .observe_window_appearance(|window, cx| {
                            if cx.global::<Preferences>().theme != ThemePreference::System {
                                return;
                            }
                            let mode = ThemeMode::from(window.appearance());
                            theme::apply_mode(mode, Some(window), cx);
                        })
                        .detach();

                    shell::set_window_visible(window.is_visible());
                    window
                        .observe_window_visibility(|visibility, _, _| {
                            tracing::debug!(?visibility, "window visibility changed");
                            shell::set_window_visible(visibility.is_visible());
                        })
                        .detach();

                    let view = cx.new(|cx| RootView::new(window, cx));
                    // gpui defaults every line box to 1.618x the font size, which
                    // makes rows a third taller than the text needs. The whole UI
                    // is chrome, not prose, so it inherits a tight line box here.
                    cx.new(|cx| {
                        Root::new(AnyView::from(view), window, cx).line_height(relative(1.3))
                    })
                },
            )
            .expect("open window");

            // Clicking a transfer notification should bring flint forward, not
            // just dismiss the banner.
            cx.on_system_notification_response(|_response, cx| {
                cx.activate(true);
            });

            cx.activate(true);
        });
}
