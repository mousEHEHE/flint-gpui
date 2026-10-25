use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window};

use crate::vault::VaultError;

use super::app_view::AppView;
use super::create_vault_view::CreateVaultView;
use super::onboarding_view::{OnboardingEvent, OnboardingView};
use super::phrase_view::PhraseView;
use super::sync_service::SyncService;
use super::unlock_view::UnlockView;
use super::update_service::UpdateService;
use super::VaultState;

fn is_vault_on_disk() -> bool {
    crate::vault::default_vault_path().is_ok_and(|path| path.exists())
}

/// Delete the vault of this device, open or locked. `RootView` sees the file
/// is gone and shows the create screen. On an error the vault file is still
/// there and the vault stays as it was, except that it no longer syncs.
pub fn delete_vault(vault_state: &Entity<VaultState>, cx: &mut App) -> Result<(), VaultError> {
    let path = crate::vault::default_vault_path()?;
    // Signing out saves the vault, so it has to come before the removal.
    SyncService::global(cx).update(cx, |service, cx| service.sign_out(cx));
    crate::vault::delete_vault_files(&path)
        .inspect_err(|e| tracing::error!(?e, "failed to delete the vault"))?;
    crate::touch_id::delete_key();
    crate::preferences::Preferences::update(cx, |prefs| prefs.is_touch_id_enabled = false);
    let was_open = vault_state.update(cx, |vault_state, cx| {
        let was_open = !matches!(vault_state, VaultState::Locked);
        // Dropping the manager wipes the key, and nothing is left that could
        // save the file back.
        *vault_state = VaultState::Locked;
        cx.notify();
        was_open
    });
    SyncService::global(cx).update(cx, |service, cx| service.forget_vault(cx));
    tracing::info!(path = %path.display(), was_open, "vault deleted");
    Ok(())
}

pub struct RootView {
    focus_handle: FocusHandle,
    vault_state: Entity<VaultState>,
    /// A vault file is on disk. Kept here so that rendering does not ask the
    /// file system; checked again whenever the lock screen is about to show.
    has_vault: bool,
    was_locked: bool,
    was_unlocked: bool,
    is_onboarding: bool,
    onboarding_view: Entity<OnboardingView>,
    create_view: Entity<CreateVaultView>,
    unlock_view: Entity<UnlockView>,
    phrase_view: Entity<PhraseView>,
    app_view: Entity<AppView>,
}

impl Focusable for RootView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl RootView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let vault_state = cx.new(|_| VaultState::Locked);
        SyncService::install(vault_state.clone(), cx);
        UpdateService::install(cx);

        let has_vault = is_vault_on_disk();
        let is_onboarding = !has_vault && OnboardingView::is_needed(cx);

        let onboarding_view = cx.new(OnboardingView::new);
        let create_view = cx.new(|cx| CreateVaultView::new(vault_state.clone(), window, cx));
        let unlock_view = cx.new(|cx| UnlockView::new(vault_state.clone(), window, cx));
        let phrase_view = cx.new(|cx| PhraseView::new(vault_state.clone(), window, cx));
        let app_view = cx.new(|cx| AppView::new(vault_state.clone(), window, cx));

        // Focus the field of whichever lock screen actually shows at startup, so
        // the user can type their password without clicking first. Done here (not
        // in each view's `new`) because both views are built eagerly above and the
        // later one would otherwise steal focus from the one on screen.
        if has_vault {
            unlock_view.update(cx, |view, cx| {
                view.focus_password(window, cx);
                view.offer_touch_id(window, cx);
            });
        } else if is_onboarding {
            onboarding_view.update(cx, |view, cx| view.focus(window, cx));
        } else {
            create_view.update(cx, |view, cx| view.focus_password(window, cx));
        }

        cx.subscribe_in(&onboarding_view, window, Self::on_onboarding_event)
            .detach();

        cx.observe_in(&vault_state, window, |this, vault_state, window, cx| {
            let is_locked = matches!(vault_state.read(cx), VaultState::Locked);
            let is_unlocked = matches!(vault_state.read(cx), VaultState::Unlocked { .. });
            let just_left_lock_screen = this.was_locked && !is_locked;
            let just_unlocked = is_unlocked && !this.was_unlocked;
            let just_locked = is_locked && this.was_unlocked;
            this.was_locked = is_locked;
            this.was_unlocked = is_unlocked;

            if just_left_lock_screen {
                // Leaving the lock screen takes a vault on disk, and from here
                // on a lock must land on the unlock form, not the create form.
                this.has_vault = true;
                this.unlock_view
                    .update(cx, |view, cx| view.clear(window, cx));
                this.create_view
                    .update(cx, |view, cx| view.clear(window, cx));
            }
            if just_unlocked {
                this.app_view.clone().update(cx, |view, cx| {
                    view.resume_after_unlock(window, cx);
                });
            }
            if just_locked {
                this.unlock_view
                    .update(cx, |view, cx| view.focus_password(window, cx));
            }
            if is_locked && this.has_vault && !is_vault_on_disk() {
                this.start_over(window, cx);
            }
            // Made by another flint, or left by a failed sign-in.
            if is_locked && !this.has_vault && is_vault_on_disk() {
                this.has_vault = true;
                this.is_onboarding = false;
                // Otherwise a create-screen sign-in error shows up later in Settings.
                SyncService::global(cx).update(cx, |service, cx| service.cancel_sign_in(cx));
                this.create_view
                    .update(cx, |view, cx| view.clear(window, cx));
                this.unlock_view
                    .update(cx, |view, cx| view.focus_password(window, cx));
            }
            cx.notify();
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            vault_state,
            has_vault,
            was_locked: true,
            was_unlocked: false,
            is_onboarding,
            onboarding_view,
            create_view,
            unlock_view,
            phrase_view,
            app_view,
        }
    }

    fn on_onboarding_event(
        &mut self,
        _: &Entity<OnboardingView>,
        event: &OnboardingEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let OnboardingEvent::Finished { wants_sign_in } = *event;
        self.is_onboarding = false;
        self.create_view.update(cx, |view, cx| {
            if wants_sign_in {
                view.start_account_sign_in(window, cx);
            } else {
                view.focus_password(window, cx);
            }
        });
        cx.notify();
    }

    /// The vault file is gone: show onboarding or the create screen, and keep nothing that
    /// belonged to the old vault.
    fn start_over(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.has_vault = false;
        crate::touch_id::delete_key();
        crate::preferences::Preferences::update(cx, |prefs| prefs.is_touch_id_enabled = false);
        self.app_view
            .update(cx, |view, cx| view.close_everything(window, cx));
        self.unlock_view
            .update(cx, |view, cx| view.clear(window, cx));
        self.create_view
            .update(cx, |view, cx| view.clear(window, cx));
        self.onboarding_view.update(cx, |view, cx| view.restart(cx));
        self.is_onboarding = OnboardingView::is_needed(cx);
        if self.is_onboarding {
            self.onboarding_view
                .update(cx, |view, cx| view.focus(window, cx));
        } else {
            self.create_view
                .update(cx, |view, cx| view.focus_password(window, cx));
        }
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.vault_state.read(cx);
        match state {
            VaultState::Unlocked { .. } => self.app_view.clone().into_any_element(),
            VaultState::AwaitingPhraseConfirmation { .. } => {
                self.phrase_view.clone().into_any_element()
            }
            VaultState::Locked => {
                if self.has_vault {
                    self.unlock_view.clone().into_any_element()
                } else if self.is_onboarding {
                    self.onboarding_view.clone().into_any_element()
                } else {
                    self.create_view.clone().into_any_element()
                }
            }
        }
    }
}
