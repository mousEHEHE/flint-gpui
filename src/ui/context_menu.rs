//! Floating menus: a cursor-anchored [`ContextMenu`] for right-click menus, and
//! a trigger-anchored [`PopoverMenu`] dropdown for "more" buttons.
//!
//! [`PopoverMenu`] takes its position from the trigger's window bounds. A
//! [`gpui::canvas`] probe records them at prepaint and [`AnchoredToTrigger`]
//! reads them in its own deferred prepaint, which runs later in the same
//! frame. gpui_component's `Popover` does the same; this is a trimmed copy, so
//! the menu keeps its own styling and stays a plain element with no `Entity`
//! and no focus trap.

use std::cell::Cell;
use std::panic::Location;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    canvas, deferred, div, point, px, Anchor, AnyElement, App, Axis, Bounds, ClickEvent, ElementId,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, MouseDownEvent, Pixels, Point,
    Position, SharedString, Style, Window,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _};

use crate::ui::rems_from_px;

/// One selectable row in a menu.
pub struct ContextMenuItem {
    id: ElementId,
    pub(crate) label: SharedString,
    icon: Option<SharedString>,
    shortcut: Option<SharedString>,
    danger: bool,
    on_click: ClickHandler,
    /// Shown to the right while the row is hovered; the row itself does nothing.
    pub(crate) submenu: Vec<ContextMenuItem>,
}

impl ContextMenuItem {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            shortcut: None,
            danger: false,
            on_click: Box::new(on_click),
            submenu: Vec::new(),
        }
    }

    pub fn submenu(mut self, items: Vec<ContextMenuItem>) -> Self {
        self.submenu = items;
        self
    }

    pub fn icon(mut self, path: impl Into<SharedString>) -> Self {
        self.icon = Some(path.into());
        self
    }

    /// The key that does the same, shown dimmed at the right edge of the row.
    pub fn shortcut(mut self, shortcut: impl Into<SharedString>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// Like `shortcut`, for a key the user can unbind.
    pub fn bound_shortcut(mut self, shortcut: Option<SharedString>) -> Self {
        self.shortcut = shortcut;
        self
    }

    /// Mark a destructive action (e.g. Delete): tinted red, on hover too.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

/// Render the shared menu panel (the bordered box of items). Used by both the
/// cursor-anchored and trigger-anchored menus so they look identical.
fn menu_panel(
    id: ElementId,
    items: Vec<ContextMenuItem>,
    on_dismiss: DismissHandler,
    is_submenu: bool,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Stateful<gpui::Div> {
    let fg = cx.theme().foreground;
    let danger = cx.theme().danger;
    let muted = cx.theme().muted_foreground;
    let radius = cx.theme().radius;
    let inner_radius = (radius - px(4.)).max(px(0.));
    let bg = cx.theme().popover;
    let border_c = crate::theme::border_focused();
    let hover_bg = cx.theme().list_hover;
    let active_bg = cx.theme().list_active;
    let has_icons = items.iter().any(|item| item.icon.is_some());
    // The row whose submenu is open. It stays open until another row of this
    // panel is hovered; a submenu's own rows leave it alone.
    let open_submenu = (!is_submenu)
        .then(|| window.use_keyed_state("menu-open-submenu", cx, |_, _| None::<usize>));
    let open_ix = open_submenu.as_ref().and_then(|state| *state.read(cx));
    // A click in the submenu is outside this panel's bounds, but must not close the menu.
    let submenu_bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));

    let mut rows = Vec::with_capacity(items.len());
    for (ix, item) in items.into_iter().enumerate() {
        let item_fg = if item.danger { danger } else { fg };
        let has_submenu = !item.submenu.is_empty();
        let is_open = has_submenu && open_ix == Some(ix);
        let target = has_submenu.then_some(ix);
        let open_submenu = open_submenu.clone();
        let submenu = is_open.then(|| {
            let panel = menu_panel(
                ("menu-submenu", ix).into(),
                item.submenu,
                Box::new(|_, _, _| {}),
                true,
                window,
                cx,
            );
            let bounds = submenu_bounds.clone();
            let anchor_point = Rc::new(Cell::new(Point::default()));
            let flip_x = Rc::new(Cell::new(px(0.)));
            // The row's corners, past this panel's padding and onto its border: a gap
            // would be outside both panels. The first rows of both line up.
            let row_probe = canvas(
                {
                    let (anchor_point, flip_x) = (anchor_point.clone(), flip_x.clone());
                    move |row: Bounds<Pixels>, window, _| {
                        let padding = rems_from_px(2.).to_pixels(window.rem_size());
                        anchor_point
                            .set(point(row.right() + padding, row.top() - padding - px(1.)));
                        flip_x.set(row.left() - padding);
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full();
            let panel = div().child(panel).child(
                canvas(move |area, _, _| bounds.set(Some(area)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            );
            (
                row_probe,
                AnchoredToTrigger {
                    anchor: Anchor::TopLeft,
                    anchor_point,
                    margin: px(8.),
                    flip_x: Some(flip_x),
                    child: panel.into_any_element(),
                },
            )
        });
        rows.push(
            div()
                .id(item.id)
                .relative()
                .h(rems_from_px(22.))
                .px(rems_from_px(6.))
                .rounded(inner_radius)
                .flex()
                .items_center()
                .gap(rems_from_px(6.))
                .text_size(rems_from_px(12.))
                .text_color(item_fg)
                .cursor_pointer()
                .when(is_open, |el| el.bg(hover_bg))
                .hover(move |s| s.bg(hover_bg))
                .active(move |s| s.bg(active_bg))
                .when_some(open_submenu, |el, open_submenu| {
                    el.on_hover(move |is_hovered, _, cx| {
                        if !*is_hovered {
                            return;
                        }
                        open_submenu.update(cx, |open, cx| {
                            if *open != target {
                                *open = target;
                                cx.notify();
                            }
                        });
                    })
                })
                .on_click(item.on_click)
                .map(|el| match item.icon {
                    Some(path) => el.child(
                        Icon::new(IconName::File)
                            .path(path)
                            .xsmall()
                            .text_color(item_fg),
                    ),
                    // Keep labels on one edge when only some items have icons.
                    None => el.when(has_icons, |el| {
                        el.child(div().size(rems_from_px(12.)).flex_none())
                    }),
                })
                .child(item.label)
                .children(item.shortcut.map(|shortcut| {
                    div()
                        .ml_auto()
                        .pl(rems_from_px(16.))
                        .text_size(rems_from_px(10.5))
                        .text_color(muted)
                        .child(shortcut)
                }))
                .when(has_submenu, |el| {
                    el.child(
                        div()
                            .ml_auto()
                            .pl(rems_from_px(16.))
                            .child(Icon::new(IconName::ChevronRight).xsmall().text_color(muted)),
                    )
                })
                .when_some(submenu, |el, (row_probe, submenu)| {
                    // The probe first: the submenu reads the row's corners in the same frame.
                    el.child(row_probe).child(submenu)
                }),
        );
    }

    div()
        .id(id)
        .occlude()
        .on_mouse_down_out(move |event, window, cx| {
            if !submenu_bounds
                .get()
                .is_some_and(|bounds| bounds.contains(&event.position))
            {
                on_dismiss(event, window, cx)
            }
        })
        .bg(bg)
        .border_1()
        .border_color(border_c)
        .rounded(radius)
        .shadow_md()
        .p(rems_from_px(2.))
        .min_w(rems_from_px(140.))
        .children(rows)
}

/// A menu pinned to a window-absolute point. Use it for right-click context
/// menus, where the cursor location is the anchor.
#[derive(IntoElement)]
pub struct ContextMenu {
    id: ElementId,
    position: Point<Pixels>,
    anchor: Anchor,
    on_dismiss: DismissHandler,
    items: Vec<ContextMenuItem>,
}

impl ContextMenu {
    pub fn new(
        id: impl Into<ElementId>,
        position: Point<Pixels>,
        anchor: Anchor,
        on_dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
        items: Vec<ContextMenuItem>,
    ) -> Self {
        Self {
            id: id.into(),
            position,
            anchor,
            on_dismiss: Box::new(on_dismiss),
            items,
        }
    }
}

impl RenderOnce for ContextMenu {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let panel = menu_panel(self.id, self.items, self.on_dismiss, false, window, cx);
        deferred(
            gpui::anchored()
                .position(self.position)
                .anchor(self.anchor)
                .snap_to_window_with_margin(px(8.))
                .child(panel),
        )
    }
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
type DismissHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;

/// A dropdown menu anchored to its trigger. The caller owns the open state and
/// builds the trigger, so the trigger's styling stays at the call site.
///
/// ```ignore
/// PopoverMenu::new(("host-more", ix), is_open, trigger_button)
///     .item(ContextMenuItem::new("edit", "Edit", on_edit).icon("icons/pen-line.svg"))
///     .item(ContextMenuItem::new("del", "Delete", on_del).icon("icons/trash.svg").danger())
///     .on_dismiss(cx.listener(|this, _, _, cx| { this.close_menu(); cx.notify() }))
/// ```
#[derive(IntoElement)]
pub struct PopoverMenu {
    id: ElementId,
    open: bool,
    trigger: AnyElement,
    /// Which corner of the menu pins to the trigger. `TopRight` (default) drops
    /// the menu below the trigger, right edges aligned.
    anchor: Anchor,
    items: Vec<ContextMenuItem>,
    on_dismiss: DismissHandler,
}

impl PopoverMenu {
    pub fn new(id: impl Into<ElementId>, open: bool, trigger: impl IntoElement) -> Self {
        Self {
            id: id.into(),
            open,
            trigger: trigger.into_any_element(),
            anchor: Anchor::TopRight,
            items: Vec::new(),
            on_dismiss: Box::new(|_, _, _| {}),
        }
    }

    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    pub fn item(mut self, item: ContextMenuItem) -> Self {
        self.items.push(item);
        self
    }

    pub fn items(mut self, items: impl IntoIterator<Item = ContextMenuItem>) -> Self {
        self.items.extend(items);
        self
    }

    pub fn on_dismiss(
        mut self,
        on_dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_dismiss = Box::new(on_dismiss);
        self
    }
}

impl RenderOnce for PopoverMenu {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // The trigger probe writes its anchor point here at prepaint. The
        // deferred anchor element reads it at its own, later, prepaint, so the
        // menu lands on the current trigger bounds.
        let anchor_point = Rc::new(Cell::new(Point::default()));
        let anchor = self.anchor;
        // Gap between the trigger and the menu, along the drop direction.
        let gap = px(4.);
        // The trigger corner the menu pins *from* (e.g. menu's TopRight pins to
        // the trigger's BottomRight so it drops below, right edges aligned).
        let trigger_corner = anchor.other_side_along(Axis::Vertical);
        // Drop direction: top-anchored menus sit below (+gap), bottom-anchored above.
        let gap_y = if matches!(anchor, Anchor::TopLeft | Anchor::TopRight) {
            gap
        } else {
            -gap
        };

        let mut wrapper = div()
            .id(self.id)
            .relative()
            .flex()
            .child(self.trigger)
            // A canvas that fills the trigger and reports its window bounds at
            // prepaint. `size_full` + `absolute` keeps it out of layout, so it
            // never changes the trigger's size.
            .child(
                canvas(
                    {
                        let anchor_point = anchor_point.clone();
                        move |bounds: Bounds<Pixels>, _, _| {
                            let p = bounds.corner(trigger_corner);
                            anchor_point.set(point(p.x, p.y + gap_y));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );

        if self.open {
            let panel = menu_panel(
                "popover-menu-panel".into(),
                self.items,
                self.on_dismiss,
                false,
                window,
                cx,
            );
            wrapper = wrapper.child(deferred(AnchoredToTrigger {
                anchor,
                anchor_point,
                margin: px(8.),
                flip_x: None,
                child: panel.into_any_element(),
            }));
        }

        wrapper
    }
}

/// A trimmed fork of gpui's `anchored`. It puts the `anchor` corner of `child`
/// at a point that another element writes at prepaint, then keeps the result
/// inside the window with a margin. Pair it with a `canvas` probe over the
/// trigger (see [`PopoverMenu`]).
pub struct AnchoredToTrigger {
    pub anchor: Anchor,
    pub anchor_point: Rc<Cell<Point<Pixels>>>,
    pub margin: Pixels,
    /// Where the child's right edge goes when it would leave the window on the
    /// right: a submenu opens on the other side of its menu instead.
    pub flip_x: Option<Rc<Cell<Pixels>>>,
    pub child: AnyElement,
}

impl IntoElement for AnchoredToTrigger {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for AnchoredToTrigger {
    type RequestLayoutState = LayoutId;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let child_layout_id = self.child.request_layout(window, cx);
        let style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        let layout_id = window.request_layout(style, [child_layout_id], cx);
        (layout_id, child_layout_id)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child_layout_id: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let size = window.layout_bounds(*child_layout_id).size;
        let mut desired = Bounds::from_anchor_and_size(self.anchor, self.anchor_point.get(), size);

        // Keep the menu inside the window, leaving `margin` at the edges.
        let viewport = window.viewport_size();
        let m = self.margin;
        if let Some(flip_x) = &self.flip_x {
            if desired.right() > viewport.width - m {
                desired.origin.x = flip_x.get() - size.width;
            }
        }
        if desired.right() > viewport.width - m {
            desired.origin.x -= desired.right() - (viewport.width - m);
        }
        if desired.origin.x < m {
            desired.origin.x = m;
        }
        if desired.bottom() > viewport.height - m {
            desired.origin.y -= desired.bottom() - (viewport.height - m);
        }
        if desired.origin.y < m {
            desired.origin.y = m;
        }

        let offset = desired.origin - bounds.origin;
        let offset = point(offset.x.round(), offset.y.round());
        window.with_element_offset(offset, |window| self.child.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _child_layout_id: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}
