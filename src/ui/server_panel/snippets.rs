//! The Snippets tab: the host's own commands, which are edited here, and the
//! shared snippets. Each runs in this session's terminal with one click.

use std::cmp::Reverse;

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context, Div, Entity, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    tooltip::Tooltip,
    v_flex, ActiveTheme as _, Icon, IconName, Sizable as _,
};
use uuid::Uuid;

use super::{note, section_title, ServerPanel, ServerPanelEvent};
use crate::theme;
use crate::vault::{HostSnippet, Snippet, SnippetBody, VaultError};

/// The form that adds or edits one of the host's own snippets, in place of
/// its row.
pub(super) struct SnippetEditor {
    /// `None` while adding a new one.
    id: Option<Uuid>,
    name: Entity<InputState>,
    script: Entity<TextareaState>,
    runs_on_connect: bool,
    error: Option<String>,
}

/// What a row shows, for a host's own snippet and a shared one alike.
struct Row {
    id: Uuid,
    name: String,
    script: String,
}

impl ServerPanel {
    pub(super) fn render_snippets(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(server_id) = self.server_id(cx) else {
            return Vec::new();
        };
        let Some(vault) = self.vault_state.read(cx).manager() else {
            return Vec::new();
        };
        let Ok(entry) = vault.get_entry(server_id) else {
            return Vec::new();
        };
        let startup = entry.startup_snippet;

        let mut children = vec![h_flex()
            .justify_between()
            .items_center()
            .child(section_title("This host"))
            .child(
                Button::new("server-snippet-add")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Plus)
                    .child("Add")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_snippet_editor(None, window, cx)
                    })),
            )
            .into_any_element()];
        let editing = self.snippet_editor.as_ref();
        if let Some(editor) = editing.filter(|editor| editor.id.is_none()) {
            children.push(self.render_editor(editor, cx).into_any_element());
        }
        for (ix, own) in entry.snippets.iter().enumerate() {
            match editing.filter(|editor| editor.id == Some(own.id)) {
                Some(editor) => children.push(self.render_editor(editor, cx).into_any_element()),
                None => {
                    let row = Row {
                        id: own.id,
                        name: own.name.clone(),
                        script: own.script.clone(),
                    };
                    let is_startup = startup == Some(own.id);
                    children.push(snippet_row(ix, &row, is_startup, true, cx).into_any_element());
                }
            }
        }
        if entry.snippets.is_empty() && editing.is_none() {
            children
                .push(note("Commands only this host needs, like cd /srv/app.").into_any_element());
        }

        children.push(section_title("Shared").into_any_element());
        let mut shared = vault.list_snippets().unwrap_or_default();
        if shared.is_empty() {
            children.push(note("No shared snippets yet. Add them in Settings.").into_any_element());
            return children;
        }
        // What ran on this host last comes first.
        shared.sort_by_key(|snippet| {
            (
                Reverse(last_run_on(snippet, server_id)),
                snippet.name.to_lowercase(),
            )
        });
        children.extend(shared.iter().enumerate().map(|(ix, snippet)| {
            let SnippetBody::Script { content } = &snippet.body;
            let row = Row {
                id: snippet.id,
                name: snippet.name.clone(),
                script: content.clone(),
            };
            let is_startup = startup == Some(snippet.id);
            snippet_row(ix, &row, is_startup, false, cx).into_any_element()
        }));
        children
    }

    fn open_snippet_editor(
        &mut self,
        snippet: Option<(HostSnippet, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (id, name, script, runs_on_connect) = match snippet {
            Some((snippet, is_startup)) => {
                (Some(snippet.id), snippet.name, snippet.script, is_startup)
            }
            None => (None, String::new(), String::new(), false),
        };
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name (optional)")
                .default_value(SharedString::from(name))
        });
        let script = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(3)
                .placeholder("cd /srv/app")
                .default_value(SharedString::from(script))
        });
        script.update(cx, |state, cx| state.focus(window, cx));
        self.snippet_editor = Some(SnippetEditor {
            id,
            name,
            script,
            runs_on_connect,
            error: None,
        });
        cx.notify();
    }

    fn close_snippet_editor(&mut self, cx: &mut Context<Self>) {
        self.snippet_editor = None;
        cx.notify();
    }

    fn save_snippet_editor(&mut self, cx: &mut Context<Self>) {
        let Some(server_id) = self.server_id(cx) else {
            return;
        };
        let Some(editor) = &self.snippet_editor else {
            return;
        };
        let script = editor.script.read(cx).value().trim().to_string();
        if script.is_empty() {
            self.set_editor_error("Type the command first.".to_string(), cx);
            return;
        }
        let name = match editor.name.read(cx).value().trim() {
            "" => default_name(&script),
            name => name.to_string(),
        };
        let snippet = HostSnippet {
            id: editor.id.unwrap_or_else(Uuid::new_v4),
            name,
            script,
        };
        let runs_on_connect = editor.runs_on_connect;
        let saved = self.vault_state.update(cx, |vault_state, cx| {
            cx.notify();
            let vault = vault_state.manager_mut()?;
            Some(save_host_snippet(
                vault,
                server_id,
                snippet,
                runs_on_connect,
            ))
        });
        match saved {
            Some(Err(e)) => {
                tracing::error!(?e, "failed to save the host's snippet");
                self.set_editor_error(format!("Not saved: {e}"), cx);
            }
            _ => self.close_snippet_editor(cx),
        }
    }

    fn delete_edited_snippet(&mut self, cx: &mut Context<Self>) {
        let Some(server_id) = self.server_id(cx) else {
            return;
        };
        let Some(id) = self.snippet_editor.as_ref().and_then(|editor| editor.id) else {
            return;
        };
        let deleted = self.vault_state.update(cx, |vault_state, cx| {
            cx.notify();
            Some(
                vault_state
                    .manager_mut()?
                    .delete_host_snippet(server_id, id),
            )
        });
        match deleted {
            Some(Err(e)) => {
                tracing::error!(?e, "failed to delete the host's snippet");
                self.set_editor_error(format!("Not deleted: {e}"), cx);
            }
            _ => self.close_snippet_editor(cx),
        }
    }

    fn set_editor_error(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.snippet_editor {
            editor.error = Some(error);
        }
        cx.notify();
    }

    fn render_editor(&self, editor: &SnippetEditor, cx: &Context<Self>) -> Div {
        let is_new = editor.id.is_none();
        v_flex()
            .gap(px(8.))
            .p(px(8.))
            .rounded(px(7.))
            .border_1()
            .border_color(theme::border_focused())
            .bg(theme::card())
            .child(Input::new(&editor.name).small())
            .child(
                Textarea::new(&editor.script)
                    .h(px(70.))
                    .font_family(theme::MONO_FONT),
            )
            .child(
                Checkbox::new("server-snippet-on-connect")
                    .checked(editor.runs_on_connect)
                    .label("Run on connect")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(editor) = &mut this.snippet_editor {
                            editor.runs_on_connect = *checked;
                        }
                        cx.notify();
                    })),
            )
            .children(editor.error.clone().map(|error| {
                div()
                    .text_size(px(11.5))
                    .text_color(cx.theme().danger)
                    .child(error)
            }))
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        Button::new("server-snippet-save")
                            .small()
                            .primary()
                            .child("Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save_snippet_editor(cx))),
                    )
                    .child(
                        Button::new("server-snippet-cancel")
                            .small()
                            .ghost()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.close_snippet_editor(cx))),
                    )
                    .child(div().flex_1())
                    .when(!is_new, |el| {
                        el.child(
                            Button::new("server-snippet-delete")
                                .small()
                                .ghost()
                                .text_color(cx.theme().danger)
                                .child("Delete")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.delete_edited_snippet(cx)),
                                ),
                        )
                    }),
            )
    }
}

/// The snippet, and whether it runs on connect: it takes the place of what
/// ran before, and stops when unticked.
fn save_host_snippet(
    vault: &mut crate::vault::VaultManager,
    server_id: Uuid,
    snippet: HostSnippet,
    runs_on_connect: bool,
) -> Result<(), VaultError> {
    let id = snippet.id;
    vault.save_host_snippet(server_id, snippet)?;
    let startup = vault.get_entry(server_id)?.startup_snippet;
    if runs_on_connect && startup != Some(id) {
        vault.set_startup_snippet(server_id, Some(id))?;
    }
    if !runs_on_connect && startup == Some(id) {
        vault.set_startup_snippet(server_id, None)?;
    }
    Ok(())
}

/// A name for a snippet saved without one: its first line, cut short.
fn default_name(script: &str) -> String {
    let line = script.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(32) {
        Some((cut, _)) => format!("{}\u{2026}", &line[..cut]),
        None => line.to_string(),
    }
}

fn last_run_on(snippet: &Snippet, server_id: Uuid) -> i64 {
    snippet
        .usages
        .iter()
        .filter(|usage| usage.server_id == server_id)
        .map(|usage| usage.at)
        .max()
        .unwrap_or(0)
}

/// The first line that says something, for a hint of what the snippet does.
fn first_line(script: &str) -> SharedString {
    script
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
        .into()
}

/// Only the button runs it: a stray click on the row must not send a
/// command to the server. A host's own snippet also has an edit button.
fn snippet_row(
    ix: usize,
    row: &Row,
    is_startup: bool,
    is_own: bool,
    cx: &Context<ServerPanel>,
) -> Div {
    let snippet_id = row.id;
    let text = cx.theme().foreground;
    let group: SharedString = format!("server-snippet-{is_own}-{ix}").into();
    let icon_button = |id: (&'static str, usize), icon: &'static str, tip: &'static str| {
        div()
            .id(id)
            .flex_none()
            .size(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .text_color(cx.theme().muted_foreground)
            .cursor_pointer()
            .hover(move |s| s.bg(theme::field()).text_color(text))
            .active(|s| s.bg(theme::element_active()))
            .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
            .child(Icon::empty().path(icon).size(px(13.)))
    };
    let run_id = if is_own {
        ("server-own-snippet-run", ix)
    } else {
        ("server-shared-snippet-run", ix)
    };
    h_flex()
        .group(group.clone())
        .gap(px(4.))
        .pl(px(8.))
        .pr(px(4.))
        .py(px(7.))
        .rounded(px(7.))
        .border_1()
        .border_color(theme::border())
        .bg(theme::card())
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    h_flex()
                        .gap(px(6.))
                        .items_center()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.5))
                                .text_color(text)
                                .child(row.name.clone()),
                        )
                        .when(is_startup, |el| el.child(on_connect_tag(cx))),
                )
                .child(
                    div()
                        .truncate()
                        .font_family(theme::MONO_FONT)
                        .text_size(px(11.))
                        .text_color(theme::text_dim())
                        .child(first_line(&row.script)),
                ),
        )
        .when(is_own, |el| {
            let snippet = HostSnippet {
                id: row.id,
                name: row.name.clone(),
                script: row.script.clone(),
            };
            el.child(
                icon_button(
                    ("server-own-snippet-edit", ix),
                    "icons/pen-line.svg",
                    "Edit",
                )
                .invisible()
                .group_hover(group.clone(), |s| s.visible())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_snippet_editor(Some((snippet.clone(), is_startup)), window, cx)
                })),
            )
        })
        .child(
            icon_button(run_id, "icons/play.svg", "Run in the terminal").on_click(cx.listener(
                move |_, _, _, cx| {
                    cx.emit(ServerPanelEvent::RunSnippet(snippet_id));
                },
            )),
        )
}

fn on_connect_tag(cx: &Context<ServerPanel>) -> Div {
    div()
        .flex_none()
        .px(px(5.))
        .rounded(px(4.))
        .bg(theme::accent_soft())
        .text_size(px(10.5))
        .text_color(cx.theme().primary)
        .child("on connect")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snippet_without_a_name_is_named_after_its_first_line() {
        assert_eq!(default_name("cd /srv/app\nls"), "cd /srv/app");
        assert_eq!(
            default_name("docker compose -f /srv/app/compose.yml logs -f web"),
            "docker compose -f /srv/app/compo\u{2026}"
        );
    }
}
