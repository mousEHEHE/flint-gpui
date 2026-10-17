use gpui::prelude::*;
use gpui::{div, px, Div, ElementId, Stateful, Window, WindowControlArea};

#[cfg(not(target_os = "macos"))]
use gpui_component::{h_flex, Icon, IconName};

/// Height of the title bar strip, on the main screen and the lock screens.
pub const TITLE_BAR_HEIGHT: gpui::Pixels = px(32.);

/// An empty strip that moves the window when dragged. It grows to fill the
/// row it sits in.
pub fn drag_region(id: impl Into<ElementId>) -> Stateful<Div> {
    let region = div()
        .id(id)
        .flex_1()
        .h_full()
        .window_control_area(WindowControlArea::Drag);

    // macOS: the window is not movable (see `main`), so title-bar clicks land
    // instantly, and dragging must be started here.
    // Linux: gpui does nothing with the control-area marking, so the same
    // call starts the move there. Windows drags natively from the marking.
    #[cfg(not(target_os = "windows"))]
    let region = region.on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
        window.start_window_move()
    });

    region
}

/// Minimize, maximize/restore and close buttons. macOS keeps its native
/// traffic lights, so these are Windows/Linux only. On Windows the platform
/// performs the action from the control-area marking, so there is no click
/// handler.
#[cfg(not(target_os = "macos"))]
pub fn window_controls(window: &Window) -> Div {
    let text_dim = crate::theme::text_dim();
    let text = crate::theme::text();
    let hover_bg = crate::theme::element_hover();
    let active_bg = crate::theme::element_active();
    let close_bg = crate::theme::red_error();
    let is_max = window.is_maximized();

    let btn = move |id: &'static str, icon: IconName, area: WindowControlArea, is_close: bool| {
        let button = div()
            .id(id)
            .size(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .text_color(text_dim)
            .cursor_pointer()
            .window_control_area(area)
            .child(Icon::new(icon).size(px(12.)));
        if is_close {
            button.hover(move |s| s.bg(close_bg).text_color(gpui::white()))
        } else {
            button
                .hover(move |s| s.bg(hover_bg).text_color(text))
                .active(move |s| s.bg(active_bg))
        }
    };

    let minimize = btn(
        "win-minimize",
        IconName::WindowMinimize,
        WindowControlArea::Min,
        false,
    );
    let maximize = btn(
        "win-maximize",
        if is_max {
            IconName::WindowRestore
        } else {
            IconName::WindowMaximize
        },
        WindowControlArea::Max,
        false,
    );
    let close = btn(
        "win-close",
        IconName::WindowClose,
        WindowControlArea::Close,
        true,
    );

    // Linux: gpui does nothing with the control-area marking, so the
    // buttons act on a click. flint has one window, so close quits.
    #[cfg(not(target_os = "windows"))]
    let (minimize, maximize, close) = (
        minimize.on_click(|_, window, _| window.minimize_window()),
        maximize.on_click(|_, window, _| window.zoom_window()),
        close.on_click(|_, _, cx| cx.quit()),
    );

    h_flex()
        .flex_none()
        .gap(px(4.))
        .child(minimize)
        .child(maximize)
        .child(close)
}

/// The title bar of the lock screens: nothing to show, but the window still
/// has to move, and on Windows/Linux it still needs its buttons.
pub fn bare_title_bar(window: &Window) -> Div {
    let bar = gpui_component::h_flex()
        .flex_none()
        .w_full()
        .h(TITLE_BAR_HEIGHT)
        .pr(px(6.))
        .items_center()
        .child(drag_region("lock-titlebar-drag"));

    #[cfg(not(target_os = "macos"))]
    let bar = bar.child(window_controls(window));
    #[cfg(target_os = "macos")]
    let _ = window;

    bar
}
