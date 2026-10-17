//! A transfer whose target name is already taken: what the user is asked, the
//! rules behind the answers, and the dialog that asks.

use gpui::prelude::*;
use gpui::{div, px, App, WeakEntity};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::{Dialog, DialogFooter, DialogHeader, DialogTitle},
    h_flex, v_flex, ActiveTheme as _, WindowExt as _,
};

use crate::ui::dialog_chrome;
use crate::ui::file_pane::{fmt_mtime, fmt_size};
use crate::ui::sftp_browser::{SftpBrowser, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Overwrite,
    Skip,
    KeepBoth,
}

/// What is known about one side of a conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileFacts {
    pub is_folder: bool,
    pub size: u64,
    /// Seconds since the epoch; 0 when unknown.
    pub mtime: u32,
}

/// One end of a transfer: this computer or one of the two servers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Local,
    Server(Side),
}

#[derive(Clone, Debug)]
pub struct TransferConflict {
    /// Tells this conflict apart from the next one in the queue, so a late
    /// answer from a closed dialog cannot resolve the wrong one.
    pub id: u64,
    pub name: String,
    pub from: Endpoint,
    pub to: Endpoint,
    /// How the dialog names the place the transfer lands in: "this computer"
    /// or the server's name.
    pub target_label: String,
    /// The path of the item at `from`.
    pub source_path: String,
    /// The folder at `to` the transfer lands in.
    pub target_dir: String,
    pub source: FileFacts,
    pub existing: FileFacts,
}

impl TransferConflict {
    /// A file cannot replace a folder, nor a folder a file.
    pub fn can_overwrite(&self) -> bool {
        self.source.is_folder == self.existing.is_folder
    }

    /// True when the server on `side` is one end of the transfer.
    pub fn involves(&self, side: Side) -> bool {
        [self.from, self.to].contains(&Endpoint::Server(side))
    }

    fn is_folder_merge(&self) -> bool {
        self.source.is_folder && self.existing.is_folder
    }
}

/// Archive endings that count as one extension, so a second `backup.tar.gz`
/// becomes `backup (1).tar.gz`.
const DOUBLE_EXTENSIONS: [&str; 4] = [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"];

/// Where the extension of a file name starts, dot included.
fn extension_start(name: &str) -> Option<usize> {
    // ASCII lowercasing keeps every byte in place, so the lengths match.
    let lower = name.to_ascii_lowercase();
    let double = DOUBLE_EXTENSIONS
        .iter()
        .find(|ending| lower.ends_with(**ending));
    match double {
        Some(ending) => Some(name.len() - ending.len()),
        None => name.rfind('.'),
    }
}

/// First name of the form `report (1).txt`, `report (2).txt`, ... that is not
/// taken. A folder name is never split at a dot.
pub fn free_name(name: &str, is_folder: bool, is_taken: impl Fn(&str) -> bool) -> String {
    let (stem, extension) = match extension_start(name) {
        // A leading dot is a hidden file, not an extension.
        Some(dot) if dot > 0 && !is_folder => name.split_at(dot),
        _ => (name, ""),
    };
    (1u32..)
        .map(|number| format!("{stem} ({number}){extension}"))
        .find(|candidate| !is_taken(candidate))
        .unwrap_or_else(|| format!("{stem} (copy){extension}"))
}

fn facts_line(label: &str, facts: &FileFacts) -> String {
    let mut parts = Vec::new();
    if facts.is_folder {
        parts.push("folder".to_string());
    } else {
        parts.push(fmt_size(facts.size));
    }
    if facts.mtime != 0 {
        parts.push(format!("changed {}", fmt_mtime(facts.mtime)));
    }
    format!("{label}: {}", parts.join(", "))
}

fn for_all_label(other_count: usize) -> String {
    if other_count == 1 {
        return "Do the same for the 1 other conflict".to_string();
    }
    format!("Do the same for the {other_count} other conflicts")
}

fn explanation(conflict: &TransferConflict) -> Option<&'static str> {
    if !conflict.can_overwrite() {
        return Some(
            "A file cannot replace a folder, or a folder a file. \
             Skip it or keep both under a new name.",
        );
    }
    if conflict.is_folder_merge() {
        return Some(
            "Merge copies into the existing folder. Files inside it that have \
             the same name are replaced without asking.",
        );
    }
    None
}

fn place_line(conflict: &TransferConflict) -> String {
    format!("In {} on {}.", conflict.target_dir, conflict.target_label)
}

fn resolve(browser: &WeakEntity<SftpBrowser>, id: u64, choice: ConflictChoice, cx: &mut App) {
    browser
        .update(cx, |browser, cx| {
            browser.resolve_transfer_conflict(id, choice, cx)
        })
        .ok();
}

fn choice_button(
    button: Button,
    browser: &WeakEntity<SftpBrowser>,
    id: u64,
    choice: ConflictChoice,
) -> Button {
    let browser = browser.clone();
    button.on_click(move |_, window, cx| {
        window.close_dialog(cx);
        resolve(&browser, id, choice, cx);
    })
}

/// Build the dialog for `conflict`. Called on every repaint of the dialog, so
/// the count and the checkbox are read from the browser each time.
pub fn transfer_conflict_dialog(
    dialog: Dialog,
    conflict: &TransferConflict,
    browser: &WeakEntity<SftpBrowser>,
    cx: &mut App,
) -> Dialog {
    let (pending_count, is_for_all) = browser
        .read_with(cx, |browser, _| {
            (
                browser.transfer_conflict_count(),
                browser.is_conflict_choice_for_all,
            )
        })
        .unwrap_or((1, false));
    let other_count = pending_count.saturating_sub(1);
    let muted = cx.theme().muted_foreground;
    let id = conflict.id;

    let overwrite_label = if conflict.is_folder_merge() {
        "Merge"
    } else {
        "Overwrite"
    };

    let toggle_browser = browser.clone();
    let close_browser = browser.clone();

    dialog_chrome(dialog)
        .w(px(480.))
        // Escape, Enter, the close button or a click outside: this one item is
        // not transferred.
        .on_close(move |_, _, cx| {
            close_browser
                .update(cx, |browser, cx| browser.dismiss_transfer_conflict(id, cx))
                .ok();
        })
        .child(
            v_flex()
                .p(px(12.))
                .gap(px(8.))
                .child(DialogHeader::new().child(
                    DialogTitle::new().child(format!("\"{}\" already exists", conflict.name)),
                ))
                .child(div().text_sm().child(place_line(conflict)))
                .child(
                    v_flex()
                        .gap(px(2.))
                        .text_sm()
                        .text_color(muted)
                        .child(facts_line("Incoming", &conflict.source))
                        .child(facts_line("Existing", &conflict.existing)),
                )
                .children(explanation(conflict).map(|text| div().text_sm().child(text)))
                .when(other_count > 0, |this| {
                    this.child(
                        Checkbox::new("transfer-conflict-all")
                            .checked(is_for_all)
                            .label(for_all_label(other_count))
                            .on_click(move |checked: &bool, window, cx| {
                                toggle_browser
                                    .update(cx, |browser, _| {
                                        browser.is_conflict_choice_for_all = *checked;
                                    })
                                    .ok();
                                window.refresh();
                            }),
                    )
                })
                // No `DialogAction` around the buttons: it turns every click
                // into a second, delayed close that can hit the next dialog.
                .child(
                    DialogFooter::new().child(
                        h_flex()
                            .gap_2()
                            .child(choice_button(
                                Button::new("transfer-conflict-skip").ghost().child("Skip"),
                                browser,
                                id,
                                ConflictChoice::Skip,
                            ))
                            .child(choice_button(
                                Button::new("transfer-conflict-keep-both").child("Keep both"),
                                browser,
                                id,
                                ConflictChoice::KeepBoth,
                            ))
                            .when(conflict.can_overwrite(), |this| {
                                this.child(choice_button(
                                    Button::new("transfer-conflict-overwrite")
                                        .primary()
                                        .child(overwrite_label),
                                    browser,
                                    id,
                                    ConflictChoice::Overwrite,
                                ))
                            }),
                    ),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(is_folder: bool) -> FileFacts {
        FileFacts {
            is_folder,
            size: 0,
            mtime: 0,
        }
    }

    fn conflict(source_is_folder: bool, existing_is_folder: bool) -> TransferConflict {
        TransferConflict {
            id: 1,
            name: "item".to_string(),
            from: Endpoint::Local,
            to: Endpoint::Server(Side::Right),
            target_label: "web-01".to_string(),
            source_path: "/home/me/item".to_string(),
            target_dir: "/srv".to_string(),
            source: facts(source_is_folder),
            existing: facts(existing_is_folder),
        }
    }

    #[test]
    fn free_name_numbers_before_the_extension() {
        assert_eq!(free_name("config.yml", false, |_| false), "config (1).yml");
    }

    #[test]
    fn free_name_skips_taken_numbers() {
        let taken = ["config (1).yml", "config (2).yml"];
        assert_eq!(
            free_name("config.yml", false, |name| taken.contains(&name)),
            "config (3).yml"
        );
    }

    #[test]
    fn free_name_splits_only_the_last_extension() {
        assert_eq!(
            free_name("site.min.js", false, |_| false),
            "site.min (1).js"
        );
    }

    #[test]
    fn free_name_keeps_a_tar_archive_ending_whole() {
        assert_eq!(
            free_name("backup.tar.gz", false, |_| false),
            "backup (1).tar.gz"
        );
        assert_eq!(
            free_name("Backup.TAR.XZ", false, |_| false),
            "Backup (1).TAR.XZ"
        );
        // Nothing but the ending: a hidden file, as `.bashrc` is.
        assert_eq!(free_name(".tar.gz", false, |_| false), ".tar.gz (1)");
        assert_eq!(free_name("logs.tar.gz", true, |_| false), "logs.tar.gz (1)");
    }

    #[test]
    fn free_name_keeps_hidden_and_plain_names_whole() {
        assert_eq!(free_name(".bashrc", false, |_| false), ".bashrc (1)");
        assert_eq!(free_name("Makefile", false, |_| false), "Makefile (1)");
    }

    #[test]
    fn free_name_never_splits_a_folder_name() {
        assert_eq!(free_name("site.d", true, |_| false), "site.d (1)");
    }

    #[test]
    fn overwrite_needs_the_same_kind_on_both_sides() {
        assert!(conflict(false, false).can_overwrite());
        assert!(conflict(true, true).can_overwrite());
        assert!(!conflict(false, true).can_overwrite());
        assert!(!conflict(true, false).can_overwrite());
    }

    #[test]
    fn the_dialog_names_the_place_the_transfer_lands_in() {
        assert_eq!(place_line(&conflict(false, false)), "In /srv on web-01.");
    }

    #[test]
    fn a_conflict_involves_the_servers_at_its_two_ends() {
        let upload = conflict(false, false);
        assert!(upload.involves(Side::Right));
        assert!(!upload.involves(Side::Left));
        let copy = TransferConflict {
            from: Endpoint::Server(Side::Left),
            ..upload
        };
        assert!(copy.involves(Side::Left));
    }

    #[test]
    fn only_two_folders_merge() {
        assert!(conflict(true, true).is_folder_merge());
        assert!(!conflict(false, false).is_folder_merge());
    }

    #[test]
    fn explanation_covers_merge_and_mismatch_only() {
        assert!(explanation(&conflict(false, false)).is_none());
        assert!(explanation(&conflict(true, true)).is_some_and(|text| text.contains("Merge")));
        assert!(explanation(&conflict(true, false)).is_some_and(|text| text.contains("cannot")));
    }

    #[test]
    fn facts_line_hides_what_is_unknown() {
        let file = FileFacts {
            is_folder: false,
            size: 2048,
            mtime: 0,
        };
        assert_eq!(facts_line("Existing", &file), "Existing: 2 KB");
        assert_eq!(facts_line("Incoming", &facts(true)), "Incoming: folder");
    }
}
