//! Hosts the user just deleted. They stay in the vault while the "Undo" toast
//! shows, so an undo keeps the same record, its team vault and its links.
//! The lists leave them out.

use std::collections::HashSet;

use gpui::{App, Global};
use uuid::Uuid;

#[derive(Default)]
struct HiddenHosts(HashSet<Uuid>);

impl Global for HiddenHosts {}

pub fn is_hidden(id: Uuid, cx: &App) -> bool {
    cx.try_global::<HiddenHosts>()
        .is_some_and(|hidden| hidden.0.contains(&id))
}

pub fn hide(ids: &[Uuid], cx: &mut App) {
    cx.default_global::<HiddenHosts>().0.extend(ids);
}

/// Show them again. Returns the ones that were still hidden.
pub fn unhide(ids: &[Uuid], cx: &mut App) -> Vec<Uuid> {
    let hidden = &mut cx.default_global::<HiddenHosts>().0;
    ids.iter().copied().filter(|id| hidden.remove(id)).collect()
}

pub fn all(cx: &App) -> Vec<Uuid> {
    cx.try_global::<HiddenHosts>()
        .map(|hidden| hidden.0.iter().copied().collect())
        .unwrap_or_default()
}
