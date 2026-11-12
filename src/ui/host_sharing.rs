//! Sharing a host with a team, as the host menus of the main list and the
//! sidebar offer it.

use gpui::{App, Entity};
use gpui_component::WindowExt as _;
use uuid::Uuid;

use crate::ui::sync_service::SyncService;
use crate::ui::VaultState;
use crate::vault::{AuthMethod, VaultManager};

/// The sharing entries of a host's menu, each with the vault the host goes
/// to, or `None` for back to personal.
pub fn menu_choices(
    vault_state: &Entity<VaultState>,
    id: Uuid,
    cx: &App,
) -> Vec<(String, Option<Uuid>)> {
    let Some(vault) = vault_state.read(cx).manager() else {
        return Vec::new();
    };
    sharing_choices(vault.vault_of(id), &vault.sharing_targets())
}

/// Put a host into a team vault, or with `None` take it back out of its team.
pub fn set_shared(
    vault_state: &Entity<VaultState>,
    id: Uuid,
    vault_id: Option<Uuid>,
    cx: &mut App,
) {
    let outcome = vault_state.update(cx, |vault_state, cx| {
        let vault = vault_state.manager_mut()?;
        let key_name = vault_id.and_then(|vault_id| key_taken_along(vault, id, vault_id));
        let (key_before, keys_before) = (login_key(vault, id), key_count(vault));
        let saved = match vault_id {
            Some(vault_id) => vault.share_entry(id, vault_id),
            None => vault.unshare_entry(id),
        };
        // A host switched to a copy the vault already held gives the team no new key.
        let is_copy_reused = login_key(vault, id) != key_before && key_count(vault) == keys_before;
        let key_name = key_name.filter(|_| !is_copy_reused);
        cx.notify();
        saved
            .inspect_err(|e| tracing::error!(?e, "failed to save the vault after sharing a host"))
            .ok()?;
        let team = vault.sharing_label(id);
        Some(key_name.zip(team))
    });
    // Nothing changed, so there is nothing to send.
    let Some(shared_key) = outcome else {
        return;
    };
    SyncService::global(cx).update(cx, |service, cx| service.sync_now(cx));
    if let (Some((key_name, team)), Some(window)) = (shared_key, cx.active_window()) {
        let message = format!(
            "The key \"{key_name}\" is now shared with {team} too: every member who sees the \
             host can use it."
        );
        // The menu click still holds the window, so wait until it lets go.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| window.push_notification(message, cx));
        });
    }
}

/// The name of the login key that sharing a host into `vault_id` gives the team as well.
fn key_taken_along(vault: &VaultManager, id: Uuid, vault_id: Uuid) -> Option<String> {
    let key_id = login_key(vault, id)?;
    if vault.vault_of(key_id) == Some(vault_id) {
        return None;
    }
    Some(vault.get_key(key_id).ok()?.name.clone())
}

fn login_key(vault: &VaultManager, id: Uuid) -> Option<Uuid> {
    match vault.get_entry(id).ok()?.auth {
        AuthMethod::PrivateKey { key_id } => Some(key_id),
        _ => None,
    }
}

fn key_count(vault: &VaultManager) -> usize {
    vault.list_keys().map(|keys| keys.len()).unwrap_or_default()
}

/// A personal host can go into any team vault. A shared one can come back,
/// or move to another vault of its team.
fn sharing_choices(
    home: Option<Uuid>,
    targets: &[(Uuid, Uuid, String)],
) -> Vec<(String, Option<Uuid>)> {
    let Some(home) = home else {
        let share = |(_, vault_id, label): &(Uuid, Uuid, String)| {
            (format!("Share with {label}"), Some(*vault_id))
        };
        return targets.iter().map(share).collect();
    };
    let Some((team_id, _, label)) = targets.iter().find(|(_, vault_id, _)| *vault_id == home)
    else {
        return vec![("Stop sharing".to_string(), None)];
    };
    let mut choices = vec![(format!("Stop sharing with {label}"), None)];
    for (_, vault_id, label) in targets
        .iter()
        .filter(|(team, vault, _)| team == team_id && *vault != home)
    {
        choices.push((format!("Move to {label}"), Some(*vault_id)));
    }
    choices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_personal_host_can_go_into_any_vault_and_a_shared_one_back_or_to_a_sibling() {
        let (ops, dev) = (Uuid::new_v4(), Uuid::new_v4());
        let (general, servers, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let targets = [
            (dev, other, "dev".to_string()),
            (ops, general, "ops / General".to_string()),
            (ops, servers, "ops / Servers".to_string()),
        ];

        let personal = sharing_choices(None, &targets);
        let labels: Vec<&str> = personal.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Share with dev",
                "Share with ops / General",
                "Share with ops / Servers"
            ]
        );
        assert_eq!(personal[2].1, Some(servers));

        let shared = sharing_choices(Some(general), &targets);
        assert_eq!(
            shared[0],
            ("Stop sharing with ops / General".to_string(), None)
        );
        assert_eq!(
            shared[1],
            ("Move to ops / Servers".to_string(), Some(servers))
        );
        assert_eq!(shared.len(), 2);
    }
}
