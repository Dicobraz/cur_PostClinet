//! Потоки после входа (ADR 0012 «Проверка договора»: `flow(Verify)` и `flow(Recovery)`).
//!
//! - **Проверка устройства** (SAS, эмодзи): запрос с другого своего устройства — принимается; иначе Postman сам
//!   просит своё другое устройство. Семь символов — `Prompt::Compare`, «совпадают» — `Confirm(true)` (подтверждение),
//!   «нет» — отмена. Без кросс-подписи учётки проверять нечем — отказ [`crate::NO_CROSS_SIGNING`] (её включают в
//!   основном приложении Matrix).
//! - **Резервная копия ключей**: ключ восстановления или фраза → секреты из хранилища сервера (4S), ключ копии;
//!   затем ключи комнат, где сообщения не расшифровались, — из копии, и повтор расшифровки.
//!
//! Ключ и фраза нигде не сохраняются; в журнал — только итог.

use std::rc::Rc;
use std::time::Duration;

use futures_util::StreamExt as _;
use matrix_sdk::encryption::verification::{SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState};
use postman_core::backend::BoxFuture;
use postman_core::channel::{Answer, ChannelError, Flow, Prompt};

use crate::client::err;
use crate::session::Inner;
use crate::{BAD_RECOVERY_KEY, NO_CROSS_SIGNING, VERIFY_CANCELLED};

/// Сколько ждать другое устройство.
pub const WAIT: Duration = Duration::from_secs(5 * 60);

fn cancelled() -> ChannelError {
    ChannelError::Denied(VERIFY_CANCELLED.into())
}

async fn timed<T>(f: impl std::future::Future<Output = Option<T>>) -> Result<T, ChannelError> {
    tokio::time::timeout(WAIT, f).await.map_err(|_| cancelled())?.ok_or_else(cancelled)
}

pub struct Verify {
    inner: Rc<Inner>,
    sas: Option<SasVerification>,
    over: bool,
}

impl Verify {
    pub(crate) fn new(inner: Rc<Inner>) -> Self {
        Self { inner, sas: None, over: false }
    }

    /// Запрос: входящий с другого устройства или свой.
    async fn request(&self) -> Result<VerificationRequest, ChannelError> {
        let enc = self.inner.client.encryption();
        let pending = self.inner.verify_requests.lock().ok().and_then(|mut p| p.take());
        if let Some((user, flow)) = pending
            && let Some(r) = enc.get_verification_request(&user, &flow).await
            && !r.is_done()
            && !r.is_cancelled()
        {
            r.accept().await.map_err(err)?;
            return Ok(r);
        }
        let me = self.inner.client.user_id().ok_or_else(|| ChannelError::SignedOut("no user".into()))?.to_owned();
        let identity = enc
            .get_user_identity(&me)
            .await
            .map_err(|e| ChannelError::Storage(e.to_string()))?
            .ok_or_else(|| ChannelError::Denied(NO_CROSS_SIGNING.into()))?;
        identity.request_verification().await.map_err(|e| ChannelError::Protocol(e.to_string()))
    }

    /// Дождаться SAS: другое устройство готово — начинаем сами; начало с той стороны — принять.
    async fn sas(&self, req: &VerificationRequest) -> Result<SasVerification, ChannelError> {
        let mut changes = req.changes();
        let mut state = req.state();
        loop {
            match state {
                VerificationRequestState::Ready { .. } => return req.start_sas().await.map_err(err)?.ok_or_else(cancelled),
                VerificationRequestState::Transitioned { verification: Verification::SasV1(s) } => {
                    s.accept().await.map_err(err)?;
                    return Ok(s);
                }
                VerificationRequestState::Done | VerificationRequestState::Cancelled(_) => return Err(cancelled()),
                _ => {}
            }
            state = timed(changes.next()).await?;
        }
    }

    async fn emoji(sas: &SasVerification) -> Result<Vec<(String, String)>, ChannelError> {
        let mut changes = sas.changes();
        loop {
            if let Some(e) = sas.emoji() {
                return Ok(e.iter().map(|x| (x.symbol.to_owned(), x.description.to_owned())).collect());
            }
            if sas.is_cancelled() || sas.is_done() {
                return Err(cancelled());
            }
            match timed(changes.next()).await? {
                SasState::Cancelled(_) => return Err(cancelled()),
                _ => continue,
            }
        }
    }

    async fn next(&mut self, answer: Option<Answer>) -> Result<Prompt, ChannelError> {
        if self.over {
            return Ok(Prompt::Finished);
        }
        match (self.sas.take(), answer) {
            (None, _) => {
                let req = self.request().await?;
                let sas = self.sas(&req).await?;
                let emoji = Self::emoji(&sas).await?;
                self.sas = Some(sas);
                Ok(Prompt::Compare { emoji })
            }
            (Some(sas), Some(Answer::Confirm(true))) => {
                sas.confirm().await.map_err(err)?;
                let mut changes = sas.changes();
                while !sas.is_done() {
                    match timed(changes.next()).await? {
                        SasState::Cancelled(_) => return Err(cancelled()),
                        SasState::Done { .. } => break,
                        _ => {}
                    }
                }
                self.over = true;
                tracing::info!("matrix: device verified");
                // Проверенному устройству другое отдаёт ключи: повтор расшифровки.
                self.inner.retry_utd().await;
                Ok(Prompt::Finished)
            }
            (Some(sas), _) => {
                let _ = sas.mismatch().await;
                self.over = true;
                tracing::info!("matrix: device verification refused");
                Err(cancelled())
            }
        }
    }
}

impl Flow for Verify {
    fn step<'a>(&'a mut self, answer: Option<Answer>) -> BoxFuture<'a, Result<Prompt, ChannelError>> {
        Box::pin(self.next(answer))
    }
}

pub struct Recovery {
    inner: Rc<Inner>,
    asked: bool,
    over: bool,
}

impl Recovery {
    pub(crate) fn new(inner: Rc<Inner>) -> Self {
        Self { inner, asked: false, over: false }
    }

    async fn next(&mut self, answer: Option<Answer>) -> Result<Prompt, ChannelError> {
        if self.over {
            return Ok(Prompt::Finished);
        }
        let key = match answer {
            Some(Answer::Secret(s)) if self.asked => String::from_utf8_lossy(s.expose()).trim().to_owned(),
            Some(Answer::Text(t)) if self.asked => t.trim().to_owned(),
            _ => {
                self.asked = true;
                return Ok(Prompt::RecoveryKey);
            }
        };
        let enc = self.inner.client.encryption();
        if let Err(e) = enc.recovery().recover(&key).await {
            tracing::info!("matrix: recovery refused: {e}");
            return Err(ChannelError::Denied(BAD_RECOVERY_KEY.into()));
        }
        tracing::info!("matrix: secrets recovered");
        for room in self.inner.utd_rooms() {
            if let Err(e) = enc.backups().download_room_keys_for_room(&room).await {
                tracing::info!("matrix: room keys not downloaded: {e}");
            }
        }
        self.inner.retry_utd().await;
        self.over = true;
        Ok(Prompt::Finished)
    }
}

impl Flow for Recovery {
    fn step<'a>(&'a mut self, answer: Option<Answer>) -> BoxFuture<'a, Result<Prompt, ChannelError>> {
        Box::pin(self.next(answer))
    }
}
