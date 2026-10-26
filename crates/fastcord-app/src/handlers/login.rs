//! Sign-in (token, password, QR, MFA), saved accounts, switching and logging out.

use crate::*;

impl App {
    pub(crate) fn login_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::StartQr
                if self.acknowledged && matches!(self.phase, Phase::Login | Phase::Qr(_)) =>
            {
                self.notice = None;
                return self.start_qr();
            }
            Message::CancelQr if matches!(self.phase, Phase::Qr(_)) => {
                // Dropping the attempt aborts its worker: the socket closes and
                // the attempt's keys are dropped.
                self.phase = Phase::Login;
                self.notice = None;
            }
            Message::Qr(id, event) if matches!(&self.phase, Phase::Qr(attempt) if attempt.id == id) =>
            {
                let Phase::Qr(attempt) = &mut self.phase else {
                    return Task::none();
                };
                if let Applied::Authorized(token) = attempt.apply(event) {
                    // Same validation and storage path as pasted tokens.
                    self.phase = Phase::Authenticating {
                        saved_account: None,
                    };
                    self.notice = None;
                    return Task::perform(
                        login::token_login(
                            self.network.clone(),
                            token,
                            self.store.clone(),
                            !self.memory_only,
                        ),
                        Message::LoginFinished,
                    );
                }
            }
            Message::PasswordLoginEdited(value) if matches!(self.phase, Phase::Login) => {
                self.password_login = value;
            }
            Message::PasswordEdited(value) if matches!(self.phase, Phase::Login) => {
                self.password = value;
            }
            Message::PasswordSubmit
                if matches!(self.phase, Phase::Login) && self.can_password_login() =>
            {
                self.notice = None;
                self.captcha_solved = false;
                self.phase = Phase::Authenticating {
                    saved_account: None,
                };
                return self.password_attempt(None);
            }
            Message::PasswordStep(result) if matches!(self.phase, Phase::Authenticating { .. }) => {
                match result {
                    Ok(password::Step::Token(token)) => {
                        self.clear_password();
                        return Task::perform(
                            login::token_login(
                                self.network.clone(),
                                token,
                                self.store.clone(),
                                !self.memory_only,
                            ),
                            Message::LoginFinished,
                        );
                    }
                    Ok(password::Step::Mfa(challenge)) => {
                        self.clear_password();
                        self.clear_token();
                        self.phase = Phase::Mfa(challenge);
                    }
                    Ok(password::Step::Captcha(_)) if self.captcha_solved => {
                        // One challenge per login: a repeat means Discord did not accept ours.
                        self.clear_password();
                        self.phase = Phase::Login;
                        self.notice = Some("Discord asked for another CAPTCHA. Login stopped; try again later or use QR or token login.".to_owned());
                    }
                    Ok(password::Step::Captcha(challenge)) => {
                        self.notice =
                            Some("Complete the CAPTCHA in the window that opened.".to_owned());
                        return Task::perform(captcha::solve(challenge), Message::CaptchaDone);
                    }
                    Err(error) => {
                        self.clear_password();
                        self.phase = Phase::Login;
                        self.notice = Some(error.to_string());
                    }
                }
            }
            Message::CaptchaDone(result) if matches!(self.phase, Phase::Authenticating { .. }) => {
                match result {
                    Ok(solution) => {
                        self.captcha_solved = true;
                        self.notice = None;
                        return self.password_attempt(Some(solution));
                    }
                    Err(error) => {
                        self.clear_password();
                        self.phase = Phase::Login;
                        self.notice = Some(error.to_string());
                    }
                }
            }
            Message::MfaSubmit
                if matches!(self.phase, Phase::Mfa(_))
                    && !self.token.expose_secret().trim().is_empty() =>
            {
                let Phase::Mfa(challenge) = std::mem::replace(
                    &mut self.phase,
                    Phase::Authenticating {
                        saved_account: None,
                    },
                ) else {
                    return Task::none();
                };
                let code = self.take_input_token();
                return Task::perform(
                    password::second_factor(self.network.clone(), challenge, code),
                    Message::MfaResult,
                );
            }
            Message::CancelMfa if matches!(self.phase, Phase::Mfa(_)) => {
                self.clear_token();
                self.phase = Phase::Login;
            }
            Message::MfaResult(result) if matches!(self.phase, Phase::Authenticating { .. }) => {
                match result {
                    Ok(token) => {
                        return Task::perform(
                            login::token_login(
                                self.network.clone(),
                                token,
                                self.store.clone(),
                                !self.memory_only,
                            ),
                            Message::LoginFinished,
                        );
                    }
                    Err(error) => {
                        self.phase = Phase::Login;
                        self.notice = Some(error.to_string());
                    }
                }
            }
            Message::TokenEdited(token) if matches!(self.phase, Phase::Login | Phase::Mfa(_)) => {
                self.token = token;
            }
            Message::InputTooLong if matches!(self.phase, Phase::Login) => {
                self.notice = Some(LoginError::InvalidInput.to_string());
                self.clear_token();
            }
            Message::LoginUi(event) => return self.login_ui.update(event),
            Message::Acknowledge(value) if matches!(self.phase, Phase::Login | Phase::Qr(_)) => {
                self.acknowledged = value;
            }
            Message::MemoryOnly(value) if matches!(self.phase, Phase::Login | Phase::Qr(_)) => {
                self.memory_only = value;
            }
            Message::Login if matches!(self.phase, Phase::Login) && self.can_login() => {
                let token = self.take_input_token();
                self.notice = None;
                self.phase = Phase::Authenticating {
                    saved_account: None,
                };
                return Task::perform(
                    login::token_login(
                        self.network.clone(),
                        token,
                        self.store.clone(),
                        !self.memory_only,
                    ),
                    Message::LoginFinished,
                );
            }
            Message::RetrySaved if matches!(self.phase, Phase::Login) => {
                self.phase = Phase::Starting;
                self.clear_token();
                self.notice = None;
                return Task::perform(login::initialize_store(), Message::StoreReady);
            }
            Message::StoreReady(result) if matches!(self.phase, Phase::Starting) => {
                self.phase = Phase::Login;
                match result {
                    Ok(ready) => {
                        self.store = Some(ready.store);
                        self.saved_accounts = ready.accounts;
                        self.account_index = ready.index;
                        if self.saved_accounts.len() == 1 {
                            return self.start_restore(self.saved_accounts[0]);
                        }
                    }
                    Err(error) => {
                        self.store = None;
                        self.notice = Some(LoginError::Store(error).to_string());
                    }
                }
            }
            Message::Restore(account) if matches!(self.phase, Phase::Login) => {
                return self.start_restore(account);
            }
            Message::LoginFinished(result)
                if matches!(self.phase, Phase::Authenticating { .. }) =>
            {
                match result {
                    Ok(LoginOutcome::Authenticated(session, persistence)) => {
                        if persistence == Persistence::Saved
                            && !self.saved_accounts.contains(&session.user.id)
                        {
                            self.saved_accounts.push(session.user.id);
                        }
                        let mut index_task = Task::none();
                        if persistence == Persistence::Saved {
                            self.account_index.upsert(&session.user);
                            index_task = self.save_account_index();
                        }
                        self.notice = None;
                        return Task::batch([index_task, self.enter_account(session, persistence)]);
                    }
                    Ok(LoginOutcome::NeedsMemoryConsent(session, error)) => {
                        self.phase = Phase::MemoryConsent { session, error };
                    }
                    Err(error) => {
                        self.phase = Phase::Login;
                        self.notice = Some(error.notice(&self.network));
                    }
                }
            }
            Message::ContinueInMemory => {
                if let Phase::MemoryConsent { session, .. } = &self.phase {
                    let session = Arc::clone(session);
                    return self.enter_account(session, Persistence::MemoryOnly);
                }
            }
            Message::CancelLogin if matches!(self.phase, Phase::MemoryConsent { .. }) => {
                self.reset_login();
            }
            Message::Logout => {
                if let Some((account, cache)) = self.teardown_account() {
                    // Dropping the account phase aborts attachment workers before purge starts.
                    let delete = self.start_delete(account);
                    return Task::batch([delete, Self::purge_cache(cache)]);
                }
            }
            Message::SwitchAccount(target) => {
                let Phase::Account {
                    session,
                    persistence,
                    ..
                } = &self.phase
                else {
                    return Task::none();
                };
                // A memory-only login would be lost, and the target must be saved.
                if *persistence != Persistence::Saved
                    || session.user.id == target
                    || !self.saved_accounts.contains(&target)
                    || self.store.is_none()
                {
                    return Task::none();
                }
                // Everything of the current account is gone (as for logout, but
                // its saved login stays) before the other one starts signing in.
                let cache = self.teardown_account().and_then(|(_, cache)| cache);
                return Task::batch([self.start_restore(target), Self::purge_cache(cache)]);
            }
            Message::Account(event) => return self.account_event(event),
            Message::AccountRemoved(account, result) => {
                if result.is_ok() {
                    self.saved_accounts.retain(|id| *id != account);
                    self.account_index.remove(account);
                    return Task::batch([
                        Self::purge_voice_volumes(account),
                        self.save_account_index(),
                    ]);
                }
                self.ui
                    .account_panel
                    .fail("The native store could not remove that saved login.");
            }
            Message::AccountIndexSaved | Message::VolumeSettingsLoaded => {}
            Message::VoiceVolumesPurged { failed } => {
                if failed && matches!(self.phase, Phase::Login) && self.notice.is_some() {
                    self.notice = Some("Logged out locally. The saved credential was removed, but saved voice volumes could not be cleared. Other Discord sessions were not revoked.".to_owned());
                }
            }
            Message::Forget(account) if matches!(self.phase, Phase::Login) => {
                return self.start_delete(account);
            }
            Message::RetryDelete => {
                if let Phase::DeleteFailed { account, .. } = self.phase {
                    return self.start_delete(account);
                }
            }
            Message::Deleted(result) => {
                if let Phase::Deleting { account } = self.phase {
                    match result {
                        Ok(()) => {
                            self.saved_accounts.retain(|id| *id != account);
                            self.account_index.remove(account);
                            let index_task = self.save_account_index();
                            let purge_failed = self.attachment_purge_failed;
                            self.attachment_purge_failed = false;
                            self.reset_login();
                            self.notice = Some(if purge_failed {
                                "Logged out locally. The saved credential was removed, but some local attachment cache or saved voice volumes could not be cleared. Other Discord sessions were not revoked."
                            } else {
                                "Logged out locally. The saved credential was removed and account state was cleared. Other Discord sessions were not revoked."
                            }.to_owned());
                            return Task::batch([index_task, Self::purge_voice_volumes(account)]);
                        }
                        Err(error) => {
                            self.phase = Phase::DeleteFailed { account, error };
                        }
                    }
                }
            }
            _ => {}
        }
        Task::none()
    }
}
