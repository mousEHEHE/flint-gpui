use gpui::prelude::*;
use gpui::{
    px, AnyElement, App, Context, Div, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    switch::Switch,
    v_flex, ActiveTheme as _,
};

use crate::preferences::{Preferences, UpdateChecks};
use crate::ui::lock_screen;
use crate::ui::update_service::UpdateService;

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Updates,
    Account,
}

pub enum OnboardingEvent {
    Finished { wants_sign_in: bool },
}

/// Asked once, before the first vault.
pub struct OnboardingView {
    focus_handle: FocusHandle,
    screen: Screen,
    is_update_check_on: bool,
}

impl EventEmitter<OnboardingEvent> for OnboardingView {}

impl Focusable for OnboardingView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn shows_update_screen(cx: &App) -> bool {
    crate::update::platform().is_some()
        && Preferences::update_checks(cx) != Some(UpdateChecks::OnStart)
}

fn shows_account_screen(cx: &App) -> bool {
    !Preferences::has_account(cx)
}

fn first_screen(cx: &App) -> Screen {
    if shows_update_screen(cx) {
        Screen::Updates
    } else {
        Screen::Account
    }
}

fn is_update_check_stored_on(cx: &App) -> bool {
    Preferences::update_checks(cx) == Some(UpdateChecks::OnStart)
}

impl OnboardingView {
    /// Takes no focus: `RootView` focuses the lock screen that shows.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            screen: first_screen(cx),
            is_update_check_on: is_update_check_stored_on(cx),
        }
    }

    /// Called when the vault was deleted.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.screen = first_screen(cx);
        self.is_update_check_on = is_update_check_stored_on(cx);
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
    }

    pub fn is_needed(cx: &App) -> bool {
        shows_update_screen(cx) || shows_account_screen(cx)
    }

    fn continue_from_updates(&mut self, cx: &mut Context<Self>) {
        let answer = if self.is_update_check_on {
            UpdateChecks::OnStart
        } else {
            UpdateChecks::Manual
        };
        UpdateService::global(cx).update(cx, |service, cx| service.set_update_checks(answer, cx));
        if shows_account_screen(cx) {
            self.screen = Screen::Account;
            cx.notify();
        } else {
            cx.emit(OnboardingEvent::Finished {
                wants_sign_in: false,
            });
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen != Screen::Updates || event.keystroke.key != "enter" || event.is_held {
            return;
        }
        // Enter toggles a focused switch, so only the root may continue.
        if !self.focus_handle.is_focused(window) {
            return;
        }
        self.continue_from_updates(cx);
    }

    fn render_updates(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        column()
            .child(lock_screen::header(
                "Updates",
                Some(
                    "flint can ask its server for a newer version when it starts and every \
                     six hours while it runs. The request sends only your OS name.",
                ),
                cx,
            ))
            .child(
                Switch::new("onboarding-update-checks")
                    .checked(self.is_update_check_on)
                    .label("Check for updates automatically")
                    .on_click(cx.listener(|this, is_checked: &bool, _, cx| {
                        this.is_update_check_on = *is_checked;
                        cx.notify();
                    })),
            )
            .child(lock_screen::note(
                "You can change this later in Settings, About.",
                muted,
            ))
            .child(
                Button::new("onboarding-continue")
                    .primary()
                    .w_full()
                    .child("Continue")
                    .on_click(cx.listener(|this, _, _, cx| this.continue_from_updates(cx))),
            )
            .into_any_element()
    }

    fn render_account(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        column()
            .child(lock_screen::header(
                "Sync with a flint account",
                Some(
                    "Keep hosts, keys and snippets the same on all your devices. Everything is \
                     encrypted on this device before it is sent, so the server cannot read it.",
                ),
                cx,
            ))
            .child(lock_screen::note(
                "Already have an account? Sign in, and this device's vault is made from it.",
                muted,
            ))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("onboarding-skip")
                            .secondary()
                            .flex_1()
                            .child("Skip")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(OnboardingEvent::Finished {
                                    wants_sign_in: false,
                                });
                            })),
                    )
                    .child(
                        Button::new("onboarding-sign-in")
                            .primary()
                            .flex_1()
                            .child("Sign in with GitHub")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(OnboardingEvent::Finished {
                                    wants_sign_in: true,
                                });
                            })),
                    ),
            )
            .into_any_element()
    }
}

/// Wider than the form column, for the longer text.
fn column() -> Div {
    v_flex().w(px(400.)).gap(px(12.))
}

impl Render for OnboardingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.screen {
            Screen::Updates => self.render_updates(cx),
            Screen::Account => self.render_account(cx),
        };
        lock_screen::screen(content, window, cx)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
    }
}
