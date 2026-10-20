//! Keeps the open vault in step with the sync account: runs a round when
//! something changed, on a timer, and right after unlock. Also owns the
//! sign-in, so closing Settings while the browser is open does not lose it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, Entity, Global};
use secrecy::SecretString;
use uuid::Uuid;

use crate::account::api::JoinRequest;
use crate::account::team::{self, Roster, TeamError, TeamKeys};
use crate::account::{self, AccountError, Api, ApiError, GitHubStep, OAuthError};
use crate::preferences::Preferences;
use crate::ui::VaultState;
use crate::vault::sync::{
    AdoptedAccount, InviteSecret, PendingJoin, Registration, SyncJob, SyncResult, SyncState,
    TeamContext, TeamUpdate, TeamsUpdate,
};
use crate::vault::{NewVaultKeys, RecoveryPhrase, VaultError, VaultManager};

/// How often the vault is checked for edits to send.
const TICK: Duration = Duration::from_secs(20);
/// How often the server is asked for changes when nothing was edited here.
const PULL_INTERVAL: Duration = Duration::from_secs(120);
/// How long to leave the server alone after a failed round.
const RETRY_DELAY: Duration = Duration::from_secs(60);
/// A round that leaves work behind is followed by another right away, but
/// only this many times: two devices editing one host must not spin forever.
const MAX_ROUNDS_IN_A_ROW: u8 = 3;
/// Coming back to the window checks the teams again, but not more often
/// than this: switching apps back and forth must not hit the server.
const REFRESH_AFTER: Duration = Duration::from_secs(30);

pub enum SignIn {
    Idle,
    /// Waiting for the user to finish with GitHub in the browser.
    Browser,
    /// GitHub is done; the password is needed to open or create the account.
    Password(Arc<GitHubStep>),
    Working(Arc<GitHubStep>),
}

pub struct SyncService {
    vault_state: Entity<VaultState>,
    pub sign_in: SignIn,
    pub sign_in_error: Option<String>,
    is_cancelled: Arc<AtomicBool>,
    /// Counts sign-in attempts, so an answer to a cancelled one is ignored.
    attempt: u64,
    /// Counts sign-outs and deleted vaults, so an answer meant for one
    /// account or vault is not given to the next.
    generation: u64,
    /// An account made while the vault got locked. The server already has
    /// it, with a recovery phrase the user has not seen, so it is installed
    /// and the phrase shown as soon as the vault opens again.
    owed_registration: Option<(Registration, RecoveryPhrase)>,
    /// A new account whose vault write failed. The server has its recovery
    /// phrase, so a retry must use these same keys.
    unsaved_vault: Option<(Arc<GitHubStep>, Box<UnsavedVault>)>,
    pub is_syncing: bool,
    pub last_synced: Option<Instant>,
    /// The rounds chained after unlock or sign-in still run. A team vault
    /// often comes only in the third.
    is_first_series: bool,
    pub sync_error: Option<String>,
    retry_after: Option<Instant>,
    rounds_left: u8,
    was_unlocked: bool,
    teams_checked: Option<Instant>,
    /// When members who left may next be signed out automatically. Without
    /// it a request that keeps failing would be sent in a tight loop.
    left_retry: Option<Instant>,
    /// A team action is running, or it is done and the teams are being
    /// fetched again. The pane shows a spinner for the whole stretch, so a
    /// row does not sit there looking alive after its button was pressed.
    pub is_team_busy: bool,
    is_settling: bool,
    pub team_error: Option<String>,
    /// Shown once after a team action that has something to say.
    pub team_notice: Option<String>,
    /// People waiting to be let into a team, by team. Each was checked
    /// against the secret of an invite link made on this device.
    pub join_requests: BTreeMap<Uuid, Vec<(Uuid, JoinRequest)>>,
}

/// Something the user asked to do with a team.
pub enum TeamAction {
    Create {
        name: String,
    },
    Invite {
        team_id: Uuid,
    },
    Revoke {
        team_id: Uuid,
        invite_id: Uuid,
    },
    Join {
        link: String,
    },
    Accept {
        team_id: Uuid,
        invite_id: Uuid,
        request: JoinRequest,
    },
    Remove {
        team_id: Uuid,
        user_ids: Vec<Uuid>,
    },
    SetRole {
        team_id: Uuid,
        user_id: Uuid,
        role: &'static str,
    },
    HandOver {
        team_id: Uuid,
        user_id: Uuid,
    },
    Leave {
        team_id: Uuid,
    },
    Delete {
        team_id: Uuid,
    },
    AddVault {
        team_id: Uuid,
        name: String,
    },
    RenameVault {
        team_id: Uuid,
        vault_id: Uuid,
        name: String,
    },
    DeleteVault {
        team_id: Uuid,
        vault_id: Uuid,
    },
    /// The plain members who see the vault from now on.
    SetAccess {
        team_id: Uuid,
        vault_id: Uuid,
        user_ids: Vec<Uuid>,
    },
}

enum TeamDone {
    Created(TeamUpdate),
    Invited(Uuid, InviteSecret),
    Joined(String, PendingJoin),
    Changed,
}

struct TeamsChecked {
    update: TeamsUpdate,
    join_requests: BTreeMap<Uuid, Vec<(Uuid, JoinRequest)>>,
}

struct GlobalSyncService(Entity<SyncService>);

impl Global for GlobalSyncService {}

enum SignInEvent {
    OpenBrowser(String),
    Done(Result<GitHubStep, AccountError>),
}

enum Opened {
    Registered(Registration, RecoveryPhrase),
    Adopted(AdoptedAccount),
}

/// A vault made on this device from a flint account.
enum Created {
    Opened(VaultManager),
    /// A new account. The user has not seen this phrase yet.
    WithPhrase(VaultManager, RecoveryPhrase),
}

enum PreVaultError {
    /// Nothing was written.
    Account(AccountError),
    /// Nothing was sent to the server.
    VaultExists,
    /// Signed in, but the vault file was not written.
    NotSaved(VaultError),
    /// Registered, but not written. The keys come back for a retry.
    RegisteredNotSaved(VaultError, Box<UnsavedVault>),
}

/// Accepted by the server, not yet written to disk.
struct UnsavedVault {
    keys: NewVaultKeys,
    state: SyncState,
    phrase: RecoveryPhrase,
}

impl From<AccountError> for PreVaultError {
    fn from(error: AccountError) -> Self {
        Self::Account(error)
    }
}

enum Round {
    /// With the session token the round used.
    Synced(String, Vec<Result<SyncResult, ApiError>>),
    RecoveryTold(Result<Option<String>, ApiError>),
    /// With the session token the round used. `None` when the account has
    /// no team keys, so it is in no team.
    Teams(String, Result<Option<TeamsChecked>, TeamError>),
}

/// Run blocking work on its own thread and hand the result to async code.
fn in_background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> async_channel::Receiver<T> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.try_send(work());
    });
    rx
}

impl SyncService {
    pub fn install(vault_state: Entity<VaultState>, cx: &mut App) {
        let service = cx.new(|cx| Self::new(vault_state, cx));
        cx.set_global(GlobalSyncService(service));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalSyncService>().0.clone()
    }

    /// While true, an empty host list may only mean they have not arrived yet.
    pub fn is_first_sync_running(&self) -> bool {
        self.is_syncing && self.is_first_series
    }

    fn new(vault_state: Entity<VaultState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&vault_state, |this, vault_state, cx| {
            let is_unlocked = vault_state.read(cx).manager().is_some();
            let just_unlocked = is_unlocked && !this.was_unlocked;
            this.was_unlocked = is_unlocked;
            if !just_unlocked {
                return;
            }
            // A new session pulls its personal vault first again. See `is_first_pull`.
            this.last_synced = None;
            this.is_first_series = true;
            // For users who signed in before the flag existed.
            if vault_state
                .read(cx)
                .manager()
                .is_some_and(|manager| manager.sync_state().is_some())
            {
                set_has_account(cx);
            }
            match this.owed_registration.take() {
                Some((registration, phrase)) => {
                    let installed = vault_state.update(cx, |vault_state, cx| {
                        cx.notify();
                        install(vault_state, Opened::Registered(registration, phrase))
                    });
                    if let Err(e) = installed {
                        tracing::error!(
                            ?e,
                            "could not attach the sync account made before the lock"
                        );
                        this.sign_in_error = Some(format!(
                            "The sync account was created, but this vault could not be attached \
                             to it: {e}. Sign in with GitHub again."
                        ));
                    }
                }
                None => this.refresh_all(cx),
            }
        })
        .detach();

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                break;
            }
        })
        .detach();

        Self {
            vault_state,
            sign_in: SignIn::Idle,
            sign_in_error: None,
            is_cancelled: Arc::new(AtomicBool::new(false)),
            attempt: 0,
            generation: 0,
            owed_registration: None,
            unsaved_vault: None,
            is_syncing: false,
            last_synced: None,
            is_first_series: false,
            sync_error: None,
            retry_after: None,
            rounds_left: 0,
            was_unlocked: false,
            teams_checked: None,
            left_retry: None,
            is_team_busy: false,
            is_settling: false,
            team_error: None,
            team_notice: None,
            join_requests: BTreeMap::new(),
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.retry_after.is_some_and(|at| Instant::now() < at) {
            return;
        }
        if self.is_settling {
            // Nothing to wait for when the vault got locked or signed out.
            let can_check = self
                .vault_state
                .read(cx)
                .manager()
                .is_some_and(|v| v.team_context().is_some());
            if !can_check {
                self.settle(cx);
                return;
            }
            self.rounds_left = MAX_ROUNDS_IN_A_ROW;
            self.run_round(cx);
            return;
        }
        let is_pull_due = self
            .last_synced
            .is_none_or(|at| at.elapsed() >= PULL_INTERVAL);
        let has_edits = self
            .vault_state
            .read(cx)
            .manager()
            .is_some_and(VaultManager::has_unsynced_changes);
        // A refresh asked for while a round was running is picked up here.
        let is_refresh_owed = self.teams_checked.is_none();
        if is_pull_due || has_edits || is_refresh_owed {
            self.rounds_left = MAX_ROUNDS_IN_A_ROW;
            self.run_round(cx);
        }
    }

    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.rounds_left = MAX_ROUNDS_IN_A_ROW;
        self.run_round(cx);
    }

    /// Fetch the teams as well as the items. A plain round checks the teams
    /// only every couple of minutes, so a host shared a moment ago would not
    /// show up yet.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.teams_checked = None;
        self.sync_now(cx);
    }

    /// `refresh_all` on the next tick. Right after a wake the network may
    /// not be back yet.
    pub fn refresh_soon(&mut self) {
        self.teams_checked = None;
    }

    /// `refresh_all` for things that happen on their own, such as the window
    /// coming to the front.
    pub fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.retry_after.is_some_and(|at| Instant::now() < at) {
            return;
        }
        if self
            .teams_checked
            .is_some_and(|at| at.elapsed() < REFRESH_AFTER)
        {
            return;
        }
        self.refresh_all(cx);
    }

    fn run_round(&mut self, cx: &mut Context<Self>) {
        if self.is_syncing || self.rounds_left == 0 {
            return;
        }
        let Some(vault) = self.vault_state.read(cx).manager() else {
            return;
        };
        // The account must hear about a recovery first: until then it still
        // checks the old password.
        let recovery = vault.recovery_job();
        let jobs = vault.begin_sync();
        if recovery.is_none() && jobs.is_empty() {
            return;
        }
        // Teams before items: a vault whose key changed cannot be read until
        // the new key is here. The first pull of a session is the exception.
        let is_teams_due = vault.is_behind_on_teams()
            || self
                .teams_checked
                .is_none_or(|at| at.elapsed() >= PULL_INTERVAL);
        let is_personal_first = is_first_pull(self.last_synced.is_some(), &jobs);
        let teams = vault
            .team_context()
            .filter(|_| is_teams_due && recovery.is_none() && !is_personal_first);
        if teams.is_some() {
            self.teams_checked = Some(Instant::now());
        }

        self.rounds_left -= 1;
        self.is_syncing = true;
        cx.notify();

        let round = in_background(move || {
            let api = Api::shared();
            if let Some(recovery) = recovery {
                return Round::RecoveryTold(account::tell_recovery(&api, &recovery));
            }
            if let Some(context) = teams {
                let token = context.session_token.clone();
                return Round::Teams(token, check_teams(&api, &context));
            }
            let token = jobs[0].session_token.clone();
            let results = jobs
                .into_iter()
                .map(|job| account::run_sync(&api, job))
                .collect();
            Round::Synced(token, results)
        });
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let Ok(round) = round.recv().await else {
                // Without this the round would look like it runs forever.
                tracing::error!("sync round stopped without an answer");
                this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.is_syncing = false;
                        this.is_first_series = false;
                        this.sync_error = Some("Sync stopped unexpectedly.".into());
                        this.retry_after = Some(Instant::now() + RETRY_DELAY);
                        if this.is_settling {
                            this.settle(cx);
                        }
                        cx.notify();
                    }
                })
                .ok();
                return;
            };
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.finish_round(round, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn settle(&mut self, cx: &mut Context<Self>) {
        self.is_settling = false;
        self.is_team_busy = false;
        cx.notify();
    }

    fn finish_round(&mut self, round: Round, cx: &mut Context<Self>) {
        self.is_syncing = false;
        let was_team_check = matches!(round, Round::Teams(..));
        let was_team_check_ok = matches!(round, Round::Teams(_, Ok(Some(_))));
        let outcome = self.vault_state.update(cx, |vault_state, cx| {
            // Locked while the round ran. Nothing is lost: the next round
            // after unlock fetches the same changes again.
            let Some(vault) = vault_state.manager_mut() else {
                return Ok(false);
            };
            if let Round::Teams(_, Ok(Some(checked))) = &round {
                self.join_requests = checked.join_requests.clone();
            }
            let (has_more, changed) = take_in(vault, round)?;
            if changed {
                cx.notify();
            }
            Ok(has_more)
        });

        match outcome {
            Ok(has_more) => {
                self.sync_error = None;
                self.retry_after = None;
                self.last_synced = Some(Instant::now());
                if was_team_check && self.is_settling {
                    self.settle(cx);
                }
                self.remove_those_who_left(cx);
                // A round that was already running when a team action ended
                // did not look at the teams. Do that now, even with no rounds
                // left. Work left behind gets only the rounds that are left:
                // a vault that stays behind must not be asked in a tight loop.
                if self.is_settling {
                    self.rounds_left = self.rounds_left.max(1);
                }
                // A team pin from the last pull may bring a team vault to pull.
                // Only a check that worked chains a round, so this cannot loop.
                if was_team_check_ok {
                    self.rounds_left = self.rounds_left.max(1);
                }
                // A refresh asked for while this round ran comes right after it.
                let is_refresh_owed = self.teams_checked.is_none();
                if has_more || self.is_settling || is_refresh_owed {
                    self.run_round(cx);
                }
            }
            Err(message) => {
                tracing::warn!(error = %message, "sync round failed");
                self.sync_error = Some(message);
                self.retry_after = Some(Instant::now() + RETRY_DELAY);
                if self.is_settling {
                    self.settle(cx);
                }
            }
        }
        // No round chained: the series is over.
        if !self.is_syncing {
            self.is_first_series = false;
        }
        cx.notify();
    }

    pub fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.sign_in, SignIn::Idle) {
            return;
        }
        self.sign_in = SignIn::Browser;
        self.sign_in_error = None;
        self.is_cancelled = Arc::new(AtomicBool::new(false));
        self.attempt += 1;
        let attempt = self.attempt;
        cx.notify();

        let (tx, rx) = async_channel::unbounded();
        let is_cancelled = self.is_cancelled.clone();
        std::thread::spawn(move || {
            let open_browser = |url| {
                let _ = tx.try_send(SignInEvent::OpenBrowser(url));
            };
            let step = account::github_sign_in(&Api::shared(), open_browser, &is_cancelled);
            let _ = tx.try_send(SignInEvent::Done(step));
        });

        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let handled = this.update(cx, |this, cx| {
                    // Cancelled since: this belongs to an attempt nobody waits for.
                    if this.attempt != attempt {
                        return;
                    }
                    match event {
                        SignInEvent::OpenBrowser(url) => cx.open_url(&url),
                        SignInEvent::Done(step) => this.finish_github(step, cx),
                    }
                });
                if handled.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn finish_github(&mut self, step: Result<GitHubStep, AccountError>, cx: &mut Context<Self>) {
        match step {
            Ok(step) => self.sign_in = SignIn::Password(Arc::new(step)),
            Err(AccountError::OAuth(OAuthError::Cancelled)) => self.sign_in = SignIn::Idle,
            Err(e) => {
                tracing::warn!(error = %e, "GitHub sign-in failed");
                self.sign_in = SignIn::Idle;
                self.sign_in_error = Some(e.to_string());
            }
        }
        cx.notify();
    }

    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some((_, unsaved)) = self.unsaved_vault.take() {
            tracing::warn!("dropping a new sync account whose vault was never saved");
            log_out_quietly(unsaved.state.session_token.clone());
        }
        self.is_cancelled.store(true, Ordering::Relaxed);
        self.attempt += 1;
        self.sign_in = SignIn::Idle;
        self.sign_in_error = None;
        cx.notify();
    }

    /// Open the account with `password`, or create it when GitHub said the
    /// user has none.
    pub fn submit_password(&mut self, password: SecretString, cx: &mut Context<Self>) {
        let SignIn::Password(step) = &self.sign_in else {
            return;
        };
        let step = step.clone();
        let Some(slot) = self
            .vault_state
            .read(cx)
            .manager()
            .map(VaultManager::password_slot)
        else {
            return;
        };

        self.sign_in = SignIn::Working(step.clone());
        self.sign_in_error = None;
        cx.notify();

        // Argon2id and two requests: a second or two.
        let opened = in_background({
            let step = step.clone();
            move || {
                let api = Api::shared();
                if step.is_new {
                    account::register(&api, &step, &slot, &password)
                        .map(|(registration, phrase)| Opened::Registered(registration, phrase))
                } else {
                    account::sign_in(&api, &step, &password).map(Opened::Adopted)
                }
            }
        });
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let Ok(opened) = opened.recv().await else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.finish_password(step, opened, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn finish_password(
        &mut self,
        step: Arc<GitHubStep>,
        opened: Result<Opened, AccountError>,
        cx: &mut Context<Self>,
    ) {
        if opened.is_ok() {
            set_has_account(cx);
        }
        let is_locked = self.vault_state.read(cx).manager().is_none();
        let opened = match opened {
            Ok(Opened::Registered(registration, phrase)) if is_locked => {
                self.owed_registration = Some((registration, phrase));
                self.sign_in = SignIn::Idle;
                cx.notify();
                return;
            }
            other => other,
        };
        let installed = opened.and_then(|opened| {
            self.vault_state.update(cx, |vault_state, cx| {
                cx.notify();
                install(vault_state, opened).map_err(AccountError::Vault)
            })
        });

        match installed {
            Ok(()) => {
                tracing::info!(login = %step.github_login, is_new = step.is_new, "signed in to sync");
                self.sign_in = SignIn::Idle;
                self.sync_error = None;
                self.retry_after = None;
                self.sync_now(cx);
            }
            Err(e) => {
                tracing::warn!(error = %e, "sync sign-in failed");
                self.show_sign_in_error(&step, &e);
            }
        }
        cx.notify();
    }

    fn show_sign_in_error(&mut self, step: &Arc<GitHubStep>, error: &AccountError) {
        // Another password will not help these: GitHub has to be asked again.
        let is_stale = matches!(
            error,
            AccountError::Api(api)
                if matches!(api.code(), Some("ticket_invalid" | "account_exists"))
        );
        self.sign_in = if is_stale {
            SignIn::Idle
        } else {
            SignIn::Password(step.clone())
        };
        self.sign_in_error = Some(sign_in_problem(error, step.is_new));
    }

    /// Only before a vault exists; `submit_password` is for an open vault.
    pub fn create_vault_with_account(&mut self, password: SecretString, cx: &mut Context<Self>) {
        let SignIn::Password(step) = &self.sign_in else {
            return;
        };
        let step = step.clone();
        if !matches!(self.vault_state.read(cx), VaultState::Locked) {
            return;
        }

        self.sign_in = SignIn::Working(step.clone());
        self.sign_in_error = None;
        cx.notify();

        let created = in_background({
            let step = step.clone();
            move || {
                let path = crate::vault::default_vault_path().map_err(AccountError::Vault)?;
                create_vault_for(&path, &step, &password)
            }
        });
        self.await_created_vault(step, created, cx);
    }

    pub fn has_unsaved_vault(&self) -> bool {
        self.unsaved_vault.is_some()
    }

    pub fn save_unsaved_vault(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.sign_in, SignIn::Idle)
            || !matches!(self.vault_state.read(cx), VaultState::Locked)
        {
            return;
        }
        let Some((step, unsaved)) = self.unsaved_vault.take() else {
            return;
        };

        self.sign_in = SignIn::Working(step.clone());
        self.sign_in_error = None;
        cx.notify();

        let created = in_background(move || retry_registered(*unsaved));
        self.await_created_vault(step, created, cx);
    }

    fn await_created_vault(
        &self,
        step: Arc<GitHubStep>,
        created: async_channel::Receiver<Result<Created, PreVaultError>>,
        cx: &mut Context<Self>,
    ) {
        // No attempt check: a vault that comes back is already on disk and
        // must be shown.
        cx.spawn(async move |this, cx| {
            let Ok(created) = created.recv().await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.finish_vault_with_account(step, created, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_vault_with_account(
        &mut self,
        step: Arc<GitHubStep>,
        created: Result<Created, PreVaultError>,
        cx: &mut Context<Self>,
    ) {
        match created {
            Ok(created) => self.show_created_vault(&step, created, cx),
            Err(PreVaultError::Account(e)) => {
                tracing::warn!(error = %e, "sign-in before a vault failed");
                self.show_sign_in_error(&step, &e);
            }
            Err(PreVaultError::VaultExists) => {
                tracing::warn!("a vault appeared before the sign-in could make one");
                // No error: the unlock screen shows the vault that is there.
                self.sign_in = SignIn::Idle;
                self.sign_in_error = None;
                // RootView checks the disk again.
                self.vault_state.update(cx, |_, cx| cx.notify());
            }
            Err(PreVaultError::NotSaved(e)) => {
                tracing::error!(?e, "signed in, but the vault was not saved");
                self.sign_in = SignIn::Idle;
                self.sign_in_error =
                    Some(format!("Signed in, but the vault could not be saved: {e}."));
                // A failed write may leave a file. RootView shows it, so nothing overwrites it.
                self.vault_state.update(cx, |_, cx| cx.notify());
            }
            Err(PreVaultError::RegisteredNotSaved(e, unsaved)) => {
                tracing::error!(?e, "account created, but the vault was not saved");
                set_has_account(cx);
                self.unsaved_vault = Some((step, unsaved));
                self.sign_in = SignIn::Idle;
                self.sign_in_error = Some(format!(
                    "Your flint account was created, but the vault could not be saved: {e}. \
                     Fix the problem, then try again."
                ));
                self.vault_state.update(cx, |_, cx| cx.notify());
            }
        }
        cx.notify();
    }

    fn show_created_vault(&mut self, step: &GitHubStep, created: Created, cx: &mut Context<Self>) {
        self.sign_in = SignIn::Idle;
        if !matches!(self.vault_state.read(cx), VaultState::Locked) {
            // RootView finds the file on disk.
            tracing::error!("a vault was made from the account, but another one is open");
            return;
        }
        tracing::info!(login = %step.github_login, is_new = step.is_new, "vault made from the sync account");
        set_has_account(cx);
        self.sync_error = None;
        self.retry_after = None;
        // The vault state observer runs the first round.
        self.vault_state.update(cx, |vault_state, cx| {
            *vault_state = match created {
                Created::Opened(manager) => VaultState::Unlocked { manager },
                Created::WithPhrase(manager, phrase) => VaultState::AwaitingPhraseConfirmation {
                    manager,
                    phrase,
                    is_rotation: false,
                },
            };
            cx.notify();
        });
    }

    /// Do something with a team. `password` is the master password: needed
    /// the first time the account uses teams, and to leave or delete one.
    pub fn team_action(
        &mut self,
        action: TeamAction,
        password: Option<SecretString>,
        cx: &mut Context<Self>,
    ) {
        if self.is_team_busy {
            return;
        }
        let Some(vault) = self.vault_state.read(cx).manager() else {
            return;
        };
        let Some(context) = vault.team_context() else {
            return;
        };
        let slot = vault.password_slot();
        self.is_team_busy = true;
        self.team_error = None;
        self.team_notice = None;
        cx.notify();

        let done = in_background(move || {
            let auth_key = match password.map(|password| slot.open(&password)) {
                Some(Ok(opened)) => Some(opened.auth_key),
                Some(Err(_)) => return Err("Wrong master password.".to_string()),
                None => None,
            };
            run_team_action(&Api::shared(), &context, auth_key, action)
                .map_err(|e| team_problem(&e))
        });
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let Ok(done) = done.recv().await else { return };
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.finish_team_action(done, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn finish_team_action(
        &mut self,
        done: Result<(TeamKeys, TeamDone), String>,
        cx: &mut Context<Self>,
    ) {
        let stored = done.and_then(|(keys, done)| {
            self.vault_state.update(cx, |vault_state, cx| {
                cx.notify();
                let Some(vault) = vault_state.manager_mut() else {
                    return Err("The vault was locked first.".to_string());
                };
                let mut notice = None;
                let saved = vault.set_key_seed(keys.seed()).and_then(|()| match done {
                    TeamDone::Created(team) => vault.add_team(team),
                    TeamDone::Invited(id, invite) => vault.add_invite(id, invite),
                    TeamDone::Joined(name, join) => {
                        notice = Some(format!(
                            "Asked to join {name}. You are in once the person who sent the \
                             link opens flint and accepts."
                        ));
                        vault.add_join(join)
                    }
                    TeamDone::Changed => Ok(()),
                });
                saved
                    .map(|()| notice)
                    .map_err(|e| format!("Could not save the vault: {e}"))
            })
        });
        match stored {
            Ok(notice) => {
                self.team_notice = notice;
                // Stay busy until the teams were fetched again: only then
                // does the pane show what the action changed.
                self.is_settling = true;
            }
            Err(message) => {
                self.team_error = Some(message);
                self.is_team_busy = false;
            }
        }
        self.teams_checked = None;
        self.sync_now(cx);
        cx.notify();
    }

    /// A member who left is still in the roster, and still holds the vault
    /// keys. The server takes no other change to the team until an admin's
    /// device signs them out and changes the keys, so do it without asking.
    fn remove_those_who_left(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|v| v.team_context())
        else {
            return;
        };
        let owed = context.teams.iter().find(|(_, team)| {
            let is_manager =
                Roster::parse(&team.roster).is_some_and(|r| r.is_manager(context.user_id));
            is_manager && !team.left.is_empty()
        });
        if self.left_retry.is_some_and(|at| Instant::now() < at) {
            return;
        }
        if let Some((team_id, team)) = owed {
            self.left_retry = Some(Instant::now() + RETRY_DELAY);
            tracing::info!(team = %team_id, "signing out members who left the team");
            let action = TeamAction::Remove {
                team_id: *team_id,
                user_ids: team.left.clone(),
            };
            self.team_action(action, None, cx);
        }
    }

    /// Forget the account on this device. Records stay in the vault and on
    /// the server.
    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        let signed_out = self.vault_state.update(cx, |vault_state, cx| {
            let vault = vault_state.manager_mut()?;
            let token = vault.sync_state()?.session_token.clone();
            if let Err(e) = vault.sign_out() {
                tracing::error!(?e, "failed to save the vault after signing out of sync");
            }
            cx.notify();
            Some(token)
        });
        // No account here (or a locked vault): a sign-in that is under way
        // is not this account's, and its answer must still land.
        let Some(token) = signed_out else { return };
        log_out_quietly(token);
        self.forget_account(cx);
    }

    /// The vault was deleted. Whatever was going on belonged to it: a
    /// sign-in half done, an account owed to it, a team action, its errors.
    /// The next vault starts with none of that.
    pub fn forget_vault(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in(cx);
        self.owed_registration = None;
        self.forget_account(cx);
    }

    /// Drop what belonged to the account: a round or a team action still
    /// running, the teams, the errors. Their answers are ignored from here on.
    fn forget_account(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        // The answer to a password that is being checked is ignored as well.
        if matches!(self.sign_in, SignIn::Working(_)) {
            self.sign_in = SignIn::Idle;
        }
        self.is_syncing = false;
        self.last_synced = None;
        self.is_first_series = true;
        self.sync_error = None;
        self.retry_after = None;
        self.rounds_left = 0;
        self.teams_checked = None;
        self.left_retry = None;
        self.is_team_busy = false;
        self.is_settling = false;
        self.team_error = None;
        self.team_notice = None;
        self.join_requests.clear();
        cx.notify();
    }
}

/// Personal vault first: it holds the team pins a teams check needs. Only
/// once per session, as an empty vault never moves its cursor.
fn is_first_pull(has_synced_this_session: bool, jobs: &[SyncJob]) -> bool {
    !has_synced_this_session && jobs.first().is_some_and(|job| job.cursor == 0)
}

/// Best effort: a session left open holds nothing this device still uses.
fn log_out_quietly(token: Option<String>) {
    let Some(token) = token else { return };
    std::thread::spawn(move || {
        if let Err(e) = Api::shared().logout(&token) {
            tracing::warn!(error = %e, "could not close the sync session on the server");
        }
    });
}

/// Blocks on Argon2id and the network.
fn create_vault_for(
    path: &Path,
    step: &GitHubStep,
    password: &SecretString,
) -> Result<Created, PreVaultError> {
    // Before any request, so an existing vault is never replaced.
    if path.exists() {
        return Err(PreVaultError::VaultExists);
    }
    let api = Api::shared();
    if step.is_new {
        let (keys, phrase) = NewVaultKeys::generate(password).map_err(AccountError::Vault)?;
        let state = account::register_new_vault(&api, step, &keys)?;
        return save_registered(
            path,
            UnsavedVault {
                keys,
                state,
                phrase,
            },
        );
    }
    // A wrong password fails here, before anything is written.
    let account = account::sign_in(&api, step, password)?;
    let token = account.state.session_token.clone();
    VaultManager::create_from_account(path, account)
        .map(Created::Opened)
        .map_err(|e| {
            log_out_quietly(token);
            PreVaultError::NotSaved(e)
        })
}

/// On failure the keys come back and the session stays open for a retry.
fn save_registered(path: &Path, unsaved: UnsavedVault) -> Result<Created, PreVaultError> {
    match VaultManager::create_registered(path, &unsaved.keys, &unsaved.state) {
        Ok(manager) => Ok(Created::WithPhrase(manager, unsaved.phrase)),
        Err(e) => Err(PreVaultError::RegisteredNotSaved(e, Box::new(unsaved))),
    }
}

fn retry_registered(unsaved: UnsavedVault) -> Result<Created, PreVaultError> {
    let path = match crate::vault::default_vault_path() {
        Ok(path) => path,
        Err(e) => return Err(PreVaultError::RegisteredNotSaved(e, Box::new(unsaved))),
    };
    retry_registered_at(&path, unsaved)
}

fn retry_registered_at(path: &Path, unsaved: UnsavedVault) -> Result<Created, PreVaultError> {
    // Checked again: a vault made since then is never replaced.
    if path.exists() {
        log_out_quietly(unsaved.state.session_token.clone());
        return Err(PreVaultError::VaultExists);
    }
    save_registered(path, unsaved)
}

/// Fetch and check the account's teams, and who waits to join them.
fn check_teams(api: &Api, context: &TeamContext) -> Result<Option<TeamsChecked>, TeamError> {
    let keys = match team::ensure_keys(api, context, None) {
        Ok(keys) => keys,
        Err(TeamError::PasswordNeeded) => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut update = team::refresh(api, context, &keys)?;
    let mut join_requests = BTreeMap::new();
    for (invite_id, invite) in &context.invites {
        let team_id = invite.team_id;
        // An invite of a team that failed its check is kept as it is.
        if !update.checked.contains(&team_id) {
            continue;
        }
        let (requests, open) = team::requests(api, context, team_id)?;
        if !open.contains(invite_id) {
            update.closed_invites.push(*invite_id);
        }
        join_requests.insert(team_id, requests);
    }
    Ok(Some(TeamsChecked {
        update,
        join_requests,
    }))
}

fn run_team_action(
    api: &Api,
    context: &TeamContext,
    auth_key: Option<[u8; 32]>,
    action: TeamAction,
) -> Result<(TeamKeys, TeamDone), TeamError> {
    let keys = team::ensure_keys(api, context, auth_key.as_ref())?;
    let needs_password = || auth_key.ok_or(TeamError::PasswordNeeded);
    let done = match action {
        TeamAction::Create { name } => TeamDone::Created(team::create(api, context, &keys, &name)?),
        TeamAction::Invite { team_id } => {
            let (id, invite) = team::invite(api, context, &keys, team_id)?;
            TeamDone::Invited(id, invite)
        }
        TeamAction::Revoke { team_id, invite_id } => {
            team::revoke(api, context, team_id, invite_id)?;
            TeamDone::Changed
        }
        TeamAction::Join { link } => {
            let (name, join) = team::join(api, context, &keys, &link)?;
            TeamDone::Joined(name, join)
        }
        TeamAction::Accept {
            team_id,
            invite_id,
            request,
        } => {
            team::accept(api, context, &keys, team_id, invite_id, &request)?;
            TeamDone::Changed
        }
        TeamAction::Remove { team_id, user_ids } => {
            team::remove(api, context, &keys, team_id, &user_ids)?;
            TeamDone::Changed
        }
        TeamAction::SetRole {
            team_id,
            user_id,
            role,
        } => {
            team::set_role(api, context, &keys, team_id, user_id, role)?;
            TeamDone::Changed
        }
        TeamAction::HandOver { team_id, user_id } => {
            team::hand_over(api, context, &keys, team_id, user_id)?;
            TeamDone::Changed
        }
        TeamAction::Leave { team_id } => {
            team::leave(api, context, team_id, &needs_password()?)?;
            TeamDone::Changed
        }
        TeamAction::Delete { team_id } => {
            team::delete(api, context, team_id, &needs_password()?)?;
            TeamDone::Changed
        }
        TeamAction::AddVault { team_id, name } => {
            team::add_vault(api, context, &keys, team_id, &name)?;
            TeamDone::Changed
        }
        TeamAction::RenameVault {
            team_id,
            vault_id,
            name,
        } => {
            team::rename_vault(api, context, &keys, team_id, vault_id, &name)?;
            TeamDone::Changed
        }
        TeamAction::DeleteVault { team_id, vault_id } => {
            // The server asks for no password here. It was checked on this
            // device before the action started; this only insists it was given.
            needs_password()?;
            team::delete_vault(api, context, &keys, team_id, vault_id)?;
            TeamDone::Changed
        }
        TeamAction::SetAccess {
            team_id,
            vault_id,
            user_ids,
        } => {
            team::set_access(api, context, &keys, team_id, vault_id, &user_ids)?;
            TeamDone::Changed
        }
    };
    Ok((keys, done))
}

fn team_problem(error: &TeamError) -> String {
    match error {
        TeamError::Api(api) => match api.code() {
            Some("wrong_password") => "Wrong master password.".into(),
            Some("invite_not_found") => "This invite does not exist or has expired.".into(),
            Some("invite_used") => "This invite was already used.".into(),
            Some("already_a_member") => "You are already in this team.".into(),
            Some("too_many_teams") => "One account can own three teams.".into(),
            Some("owner_cannot_leave") => "The owner cannot leave. Delete the team instead.".into(),
            Some("roster_out_of_date" | "vault_changed") => {
                "The team changed in the meantime. Try again in a moment.".into()
            }
            _ => api.to_string(),
        },
        TeamError::Refused(reason) => sentence(reason),
        other => other.to_string(),
    }
}

/// "a team keeps at least one vault" as a line of the pane.
fn sentence(reason: &str) -> String {
    let mut text = reason.to_string();
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    text + "."
}

/// Apply a finished round to the vault. Returns whether another round
/// should follow at once, and whether any record changed.
fn take_in(vault: &mut VaultManager, round: Round) -> Result<(bool, bool), String> {
    let saved = |result: Result<(), VaultError>| {
        result.map_err(|e| format!("Could not save the vault: {e}"))
    };
    match round {
        Round::Synced(token, results) => {
            // The personal vault comes first. When it failed, the team vaults
            // wait too: a host moving into a team must leave here before it
            // arrives there.
            let is_personal_ok = results.first().is_some_and(Result::is_ok);
            let mut answers = Vec::new();
            let mut problem = None;
            for (index, result) in results.into_iter().enumerate() {
                match result {
                    Ok(result) if is_personal_ok => answers.push(result),
                    Ok(_) => {}
                    Err(e) if e.is_session_ended() => {
                        saved(vault.end_session(&token))?;
                        return Ok((false, false));
                    }
                    // A team vault got a new key, or it was deleted, or this
                    // account no longer sees it. The team check sorts that out.
                    Err(e) if index > 0 && is_team_news(&e) => vault.note_behind_on_teams(),
                    // One full or broken team vault must not stop the others.
                    Err(e) => problem = problem.or(Some(e.to_string())),
                }
            }
            let changed = vault
                .finish_round(answers)
                .map_err(|e| format!("Could not save the vault: {e}"))?;
            match problem {
                Some(problem) if changed == 0 => Err(problem),
                problem => {
                    // Not shown: the round did bring changes in.
                    if let Some(problem) = problem {
                        tracing::warn!(error = %problem, "a vault failed to sync in a round that changed others");
                    }
                    Ok((
                        vault.has_unsynced_changes() || vault.is_behind_on_teams(),
                        changed > 0,
                    ))
                }
            }
        }
        Round::RecoveryTold(Ok(session_token)) => {
            let is_signed_in = session_token.is_some();
            saved(vault.finish_recovery(session_token))?;
            Ok((is_signed_in, false))
        }
        Round::Teams(token, Err(TeamError::Api(e))) if e.is_session_ended() => {
            saved(vault.end_session(&token))?;
            Ok((false, false))
        }
        Round::Teams(_, Ok(Some(checked))) => {
            saved(vault.apply_teams(checked.update))?;
            // The items come next, in a round of their own.
            Ok((true, true))
        }
        Round::Teams(_, Ok(None)) => {
            vault.forget_behind_on_teams();
            Ok((true, false))
        }
        Round::Teams(_, Err(e)) => {
            // Otherwise every round retries the team check and items never sync.
            vault.forget_behind_on_teams();
            Err(e.to_string())
        }
        Round::RecoveryTold(Err(e)) => Err(e.to_string()),
    }
}

fn is_team_news(error: &ApiError) -> bool {
    matches!(error.code(), Some("key_rotated" | "vault_not_found"))
}

fn set_has_account(cx: &mut App) {
    if !Preferences::has_account(cx) {
        Preferences::update(cx, |prefs| prefs.has_account = true);
    }
}

fn install(vault_state: &mut VaultState, opened: Opened) -> Result<(), VaultError> {
    let Some(vault) = vault_state.manager_mut() else {
        return Err(VaultError::Io(std::io::Error::other(
            "the vault was locked first",
        )));
    };
    match opened {
        Opened::Adopted(account) => vault.adopt_account(account),
        Opened::Registered(registration, phrase) => {
            vault.install_registration(registration)?;
            // The new phrase takes over the window until the user confirms
            // they saved it, exactly like after a recovery.
            if let VaultState::Unlocked { manager } =
                std::mem::replace(vault_state, VaultState::Locked)
            {
                *vault_state = VaultState::AwaitingPhraseConfirmation {
                    manager,
                    phrase,
                    is_rotation: true,
                };
            }
            Ok(())
        }
    }
}

fn sign_in_problem(error: &AccountError, is_new: bool) -> String {
    match error {
        AccountError::Vault(VaultError::InvalidPassword) => "Wrong master password.".into(),
        AccountError::Api(api) => match api.code() {
            Some("wrong_password") if is_new => "Wrong master password.".into(),
            Some("wrong_password") => "Wrong password for this account.".into(),
            Some("too_many_attempts") => "Too many wrong passwords. Try again in an hour.".into(),
            Some("ticket_invalid") => "The sign-in expired. Sign in with GitHub again.".into(),
            Some("account_exists") => {
                "This GitHub account already has a flint account. Sign in with GitHub again.".into()
            }
            _ => api.to_string(),
        },
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::format::KdfParams;
    use crate::vault::sync::{SyncState, TeamState, VaultKey};

    fn refused(status: u16, code: &str) -> ApiError {
        ApiError::Refused {
            status,
            code: code.into(),
            message: format!("refused: {code}"),
        }
    }

    /// A signed-in vault in a team with one vault. Returns the team vault's id.
    fn vault_in_a_team(dir: &tempfile::TempDir) -> (VaultManager, Uuid) {
        let password = SecretString::new("pw".into());
        let kdf = KdfParams {
            m_cost: 256,
            t_cost: 1,
            p_cost: 1,
        };
        let (mut vault, _) =
            VaultManager::create_with_params(&dir.path().join("vault.bin"), &password, kdf)
                .unwrap();
        let slot = vault.password_slot().enc_dek_by_password().clone();
        vault
            .install_registration(Registration {
                replaces: slot.clone(),
                enc_dek_by_recovery: slot,
                state: SyncState::new(Uuid::nil(), "alice".into(), "token".into(), Uuid::nil()),
            })
            .unwrap();
        let (team_id, vault_id) = (Uuid::new_v4(), Uuid::new_v4());
        let state = TeamState {
            name: "ops".into(),
            roster: Vec::new(),
            logins: BTreeMap::new(),
            left: Vec::new(),
            vault_names: vec![(vault_id, None)],
        };
        let vaults = vec![VaultKey {
            vault_id,
            key: [7; 32],
            key_version: 1,
        }];
        vault
            .add_team(TeamUpdate {
                team_id,
                state,
                vaults,
                pin: None,
            })
            .unwrap();
        (vault, vault_id)
    }

    fn nothing_new(vault_id: Uuid) -> Result<SyncResult, ApiError> {
        Ok(SyncResult {
            vault_id,
            started_at: 0,
            pulled: Vec::new(),
            cursor: 0,
            pushed: Vec::new(),
        })
    }

    #[test]
    fn a_team_vault_that_is_gone_asks_for_a_team_check_and_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vault, _) = vault_in_a_team(&dir);
        let answers = vec![
            nothing_new(Uuid::nil()),
            Err(refused(404, "vault_not_found")),
        ];

        let outcome = take_in(&mut vault, Round::Synced("token".into(), answers));

        assert_eq!(outcome, Ok((true, false)));
        assert!(vault.is_behind_on_teams());
    }

    #[test]
    fn a_full_team_vault_is_still_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vault, _) = vault_in_a_team(&dir);
        let answers = vec![nothing_new(Uuid::nil()), Err(refused(413, "vault_full"))];

        let outcome = take_in(&mut vault, Round::Synced("token".into(), answers));

        assert_eq!(outcome, Err("refused: vault_full".to_string()));
        assert!(!vault.is_behind_on_teams());
    }

    #[test]
    fn a_missing_personal_vault_is_an_error_and_not_team_news() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vault, vault_id) = vault_in_a_team(&dir);
        let answers = vec![Err(refused(404, "vault_not_found")), nothing_new(vault_id)];

        let outcome = take_in(&mut vault, Round::Synced("token".into(), answers));

        assert_eq!(outcome, Err("refused: vault_not_found".to_string()));
        assert!(!vault.is_behind_on_teams());
    }

    #[test]
    fn signing_in_before_a_vault_stops_when_a_vault_file_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        std::fs::write(&path, b"not a vault").unwrap();
        // A ticket the server would refuse: the test fails if it is sent.
        let step = GitHubStep {
            ticket: "no-such-ticket".into(),
            is_new: false,
            github_login: "alice".into(),
            kdf: None,
        };

        let created = create_vault_for(&path, &step, &SecretString::new("pw".into()));

        assert!(matches!(created, Err(PreVaultError::VaultExists)));
        assert_eq!(std::fs::read(&path).unwrap(), b"not a vault");
    }

    #[test]
    fn a_new_account_vault_that_failed_to_save_is_written_with_the_same_phrase() {
        let dir = tempfile::tempdir().unwrap();
        let kdf = KdfParams {
            m_cost: 256,
            t_cost: 1,
            p_cost: 1,
        };
        let password = SecretString::new("pw".into());
        let (keys, phrase) = NewVaultKeys::generate_with_params(&password, kdf.clone()).unwrap();
        let words = phrase.expose_words().to_string();
        let state = SyncState::new(Uuid::new_v4(), "alice".into(), "token".into(), Uuid::nil());
        // A file where the vault's folder should be, so the write fails.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"").unwrap();

        let failed = save_registered(
            &blocker.join("vault.bin"),
            UnsavedVault {
                keys,
                state,
                phrase,
            },
        );
        let Err(PreVaultError::RegisteredNotSaved(_, unsaved)) = failed else {
            panic!("the write should fail and hand the keys back");
        };
        let path = dir.path().join("vault.bin");
        let Ok(Created::WithPhrase(vault, phrase)) = retry_registered_at(&path, *unsaved) else {
            panic!("the second write should succeed");
        };

        assert_eq!(phrase.expose_words(), words);
        vault.lock();
        VaultManager::unlock_with_recovery_with_params(
            &path,
            &phrase,
            &SecretString::new("new".into()),
            kdf,
        )
        .unwrap();
    }

    #[test]
    fn a_team_check_refused_for_the_session_ends_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vault, _) = vault_in_a_team(&dir);
        let refusal = Err(TeamError::Api(refused(401, "unauthorized")));

        let outcome = take_in(&mut vault, Round::Teams("token".into(), refusal));

        assert_eq!(outcome, Ok((false, false)));
        let state = vault.sync_state().unwrap();
        assert_eq!(state.session_token, None);
    }

    #[test]
    fn an_account_without_team_keys_stops_asking_for_team_checks() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vault, _) = vault_in_a_team(&dir);
        vault.note_behind_on_teams();

        let outcome = take_in(&mut vault, Round::Teams("token".into(), Ok(None)));

        assert_eq!(outcome, Ok((true, false)));
        assert!(!vault.is_behind_on_teams());
    }

    #[test]
    fn a_failed_team_check_lets_the_next_round_sync_items() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vault, _) = vault_in_a_team(&dir);
        vault.note_behind_on_teams();
        let failure = Err(TeamError::Untrusted(
            "the team keys on the server do not open",
        ));

        let outcome = take_in(&mut vault, Round::Teams("token".into(), failure));

        assert!(outcome.is_err());
        assert!(!vault.is_behind_on_teams());
    }

    fn personal_job_at(cursor: u64) -> SyncJob {
        SyncJob {
            session_token: "token".into(),
            vault_id: Uuid::nil(),
            key_version: 1,
            cursor,
            changes: Vec::new(),
        }
    }

    #[test]
    fn a_vault_that_never_pulled_pulls_its_personal_vault_before_the_teams() {
        let dir = tempfile::tempdir().unwrap();
        let (vault, _) = vault_in_a_team(&dir);

        assert!(is_first_pull(false, &vault.begin_sync()));
    }

    #[test]
    fn an_empty_personal_vault_goes_first_only_once_per_session() {
        let jobs = vec![personal_job_at(0)];

        assert!(is_first_pull(false, &jobs));
        assert!(!is_first_pull(true, &jobs));
    }

    #[test]
    fn a_vault_that_pulled_before_checks_the_teams_first() {
        assert!(!is_first_pull(false, &[personal_job_at(66)]));
    }

    #[test]
    fn no_jobs_is_not_a_first_pull() {
        assert!(!is_first_pull(false, &[]));
    }
}
