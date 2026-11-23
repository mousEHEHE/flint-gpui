//! The Shortcuts section of Settings: every command a shortcut can run, and
//! its chord. Click a chord, then press the new keys.

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, Keystroke, Subscription, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{InputEvent, InputState},
    v_flex, Disableable as _, Sizable as _,
};

use crate::ui::keymap::{self, Clash, Command, Group, COMMANDS};
use crate::ui::rems_from_px;

use super::view::{empty_note, filter_field, list_card, page_head};

/// A recorded chord that is already taken, waiting for Replace or Cancel.
struct Conflict {
    id: &'static str,
    clash: Clash,
    chord: String,
}

pub struct ShortcutsPane {
    search: Entity<InputState>,
    /// The command whose chord the next key press sets.
    recording: Option<&'static str>,
    conflict: Option<Conflict>,
    _keystrokes: Subscription,
}

impl ShortcutsPane {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("filter shortcuts…"));
        cx.subscribe(&search, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => cx.notify(),
            InputEvent::Focus => this.stop_recording(cx),
            _ => {}
        })
        .detach();

        // Interceptors run before gpui matches key bindings, so no shortcut
        // fires while a new one is recorded.
        let pane = cx.weak_entity();
        let keystrokes = cx.intercept_keystrokes(move |event, _, cx| {
            let is_taken = pane
                .update(cx, |this, cx| this.record(&event.keystroke, cx))
                .unwrap_or(false);
            if is_taken {
                cx.stop_propagation();
            }
        });

        Self {
            search,
            recording: None,
            conflict: None,
            _keystrokes: keystrokes,
        }
    }

    /// Called when the page is left: the key interceptor is app-wide.
    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.recording = None;
        self.conflict = None;
        cx.notify();
    }

    /// Escape cancels, Backspace or Delete removes the shortcut, and a chord
    /// sets it. Every key press is swallowed while recording.
    fn record(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.recording else {
            return false;
        };
        let is_plain = !keystroke.modifiers.modified();
        match keystroke.key.as_str() {
            "escape" if is_plain => self.recording = None,
            "backspace" | "delete" if is_plain => {
                self.recording = None;
                keymap::set_chord(id, None, cx);
            }
            _ => {
                let Some(chord) = keymap::recorded_chord(keystroke) else {
                    return true;
                };
                self.recording = None;
                match keymap::conflict(id, &chord, cx) {
                    Some(clash) => self.conflict = Some(Conflict { id, clash, chord }),
                    None => keymap::set_chord(id, Some(chord), cx),
                }
            }
        }
        cx.notify();
        true
    }

    fn toggle_recording(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.conflict = None;
        self.recording = if self.recording == Some(id) {
            None
        } else {
            Some(id)
        };
        cx.notify();
    }

    fn replace(&mut self, cx: &mut Context<Self>) {
        if let Some(Conflict {
            id,
            clash: Clash::Commands(others),
            chord,
        }) = self.conflict.take()
        {
            keymap::replace_chord(id, &others, chord, cx);
        }
        cx.notify();
    }

    fn reset(&mut self, id: Option<&str>, cx: &mut Context<Self>) {
        self.recording = None;
        self.conflict = None;
        keymap::reset(id, cx);
        cx.notify();
    }

    fn render_group(
        &self,
        group: Group,
        commands: Vec<&'static Command>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .child(
                div()
                    .px(rems_from_px(20.))
                    .pt(rems_from_px(4.))
                    .pb(rems_from_px(8.))
                    .text_size(rems_from_px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(crate::theme::text())
                    .child(group.label()),
            )
            .child(
                list_card().children(
                    commands
                        .into_iter()
                        .enumerate()
                        .map(|(ix, command)| self.render_row(ix, command, cx)),
                ),
            )
    }

    fn render_row(
        &self,
        ix: usize,
        command: &'static Command,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = command.id;
        let note = self.row_note(command, cx);
        v_flex()
            .px(rems_from_px(12.))
            .py(rems_from_px(8.))
            .gap(rems_from_px(6.))
            .when(ix > 0, |row| {
                row.border_t(px(1.)).border_color(crate::theme::border())
            })
            .child(
                h_flex()
                    .gap(rems_from_px(8.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(rems_from_px(13.))
                            .text_color(crate::theme::text())
                            .child(command.label),
                    )
                    .when(keymap::is_customized(id, cx), |el| {
                        el.child(
                            Button::new(("shortcut-reset", ix))
                                .ghost()
                                .xsmall()
                                .child("Reset")
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.reset(Some(id), cx)),
                                ),
                        )
                    })
                    .child(self.render_chip(command, cx)),
            )
            .when_some(note, |el, note| el.child(note))
    }

    fn render_chip(&self, command: &'static Command, cx: &mut Context<Self>) -> impl IntoElement {
        let id = command.id;
        let is_recording = self.recording == Some(id);
        let label = keymap::label(id, cx);
        let (text, color) = match (&label, is_recording) {
            (_, true) => ("Press keys\u{2026}".into(), crate::theme::accent()),
            (Some(label), false) => (label.clone(), crate::theme::text()),
            (None, false) => ("Not set".into(), crate::theme::text_dim()),
        };
        div()
            .id(gpui::SharedString::from(format!("shortcut-chip-{id}")))
            .flex_none()
            .min_w(rems_from_px(72.))
            .h(rems_from_px(24.))
            .px(rems_from_px(8.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(rems_from_px(6.))
            .bg(crate::theme::field())
            .border_1()
            .border_color(if is_recording {
                crate::theme::accent()
            } else {
                crate::theme::border()
            })
            .text_size(rems_from_px(12.))
            .text_color(color)
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::element()))
            .child(text)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_recording(id, cx)))
    }

    /// What the row says under its label: the recording help, a conflict to
    /// settle, or a warning that the shell loses the key.
    fn row_note(&self, command: &Command, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let note = |text: String, color| {
            div()
                .text_size(rems_from_px(11.5))
                .text_color(color)
                .child(text)
        };
        if self.recording == Some(command.id) {
            let help = "Press the new shortcut. Esc cancels, Backspace removes it.";
            return Some(note(help.into(), crate::theme::text_dim()).into_any_element());
        }
        if let Some(conflict) = self.conflict.as_ref().filter(|c| c.id == command.id) {
            return Some(self.render_conflict(conflict, cx).into_any_element());
        }
        keymap::steals_from_terminal(command.id, cx).then(|| {
            let warning = "The terminal no longer gets these keys.".to_string();
            note(warning, crate::theme::warning()).into_any_element()
        })
    }

    fn render_conflict(&self, conflict: &Conflict, cx: &mut Context<Self>) -> impl IntoElement {
        let chord = Keystroke::parse(&conflict.chord)
            .map(|keystroke| keymap::format_chord(&keystroke, cfg!(target_os = "macos")))
            .unwrap_or_else(|_| conflict.chord.clone());
        let (text, can_replace) = match &conflict.clash {
            Clash::BuiltIn => (format!("{chord} is used by a built-in key."), false),
            Clash::Commands(others) => {
                let labels: Vec<String> = others
                    .iter()
                    .map(|other| format!("\u{201C}{}\u{201D}", other.label))
                    .collect();
                (format!("{chord} is used by {}.", labels.join(", ")), true)
            }
        };
        h_flex()
            .gap(rems_from_px(8.))
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(rems_from_px(11.5))
                    .text_color(crate::theme::warning())
                    .child(text),
            )
            .child(
                Button::new("shortcut-conflict-cancel")
                    .ghost()
                    .xsmall()
                    .child("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.conflict = None;
                        cx.notify();
                    })),
            )
            .when(can_replace, |el| {
                el.child(
                    Button::new("shortcut-conflict-replace")
                        .primary()
                        .xsmall()
                        .child("Replace")
                        .on_click(cx.listener(|this, _, _, cx| this.replace(cx))),
                )
            })
    }
}

fn matches_query(command: &Command, query: &str, cx: &gpui::App) -> bool {
    if query.is_empty() {
        return true;
    }
    let label = keymap::label(command.id, cx).unwrap_or_default();
    command.label.to_lowercase().contains(query) || label.to_lowercase().contains(query)
}

impl Render for ShortcutsPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let head = page_head("Shortcuts", None)
            .child(div().flex_1())
            .child(filter_field(&self.search))
            .child(
                Button::new("shortcuts-reset-all")
                    .ghost()
                    .small()
                    .child("Reset all")
                    .disabled(!keymap::has_customized(cx))
                    .on_click(cx.listener(|this, _, _, cx| this.reset(None, cx))),
            );

        let groups: Vec<_> = Group::ALL
            .into_iter()
            .filter_map(|group| {
                let commands: Vec<&'static Command> = COMMANDS
                    .iter()
                    .filter(|command| command.group == group && matches_query(command, &query, cx))
                    .collect();
                (!commands.is_empty()).then(|| self.render_group(group, commands, cx))
            })
            .collect();

        v_flex().size_full().child(head).child(
            v_flex()
                .id("shortcuts-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .when(groups.is_empty(), |el| {
                    el.child(empty_note("No shortcuts match the filter."))
                })
                .children(groups),
        )
    }
}
