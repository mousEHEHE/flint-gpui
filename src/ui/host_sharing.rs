//! Sharing a host with a team, as the host menus of the main list and the
//! sidebar offer it.

use gpui::{App, Entity};
use uuid::Uuid;

use crate::ui::sync_service::SyncService;
use crate::ui::VaultState;

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
    let is_saved = vault_state.update(cx, |vault_state, cx| {
        let Some(vault) = vault_state.manager_mut() else {
            return false;
        };
        let saved = match vault_id {
            Some(vault_id) => vault.share_entry(id, vault_id),
            None => vault.unshare_entry(id),
        };
        cx.notify();
        saved
            .inspect_err(|e| tracing::error!(?e, "failed to save the vault after sharing a host"))
            .is_ok()
    });
    // Nothing changed, so there is nothing to send.
    if !is_saved {
        return;
    }
    SyncService::global(cx).update(cx, |service, cx| service.sync_now(cx));
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
