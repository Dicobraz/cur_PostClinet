//! Клиент SDK и ошибки: сборка `matrix_sdk::Client` (свои хранилища, без повторов внутри SDK — повтор и паузу держит
//! ядро, `link::delay`) и перевод ошибок сервера в `ChannelError` (ADR 0012 «Проверка договора»: `M_LIMIT_EXCEEDED` →
//! `Wait`, `M_UNKNOWN_TOKEN` → `SignedOut`, `M_FORBIDDEN` → `Denied`).

use std::time::Duration;

use matrix_sdk::config::{RequestConfig, StoreConfig};
use matrix_sdk::cross_process_lock::CrossProcessLockConfig;
use matrix_sdk::encryption::{BackupDownloadStrategy, EncryptionSettings};
use matrix_sdk::ruma::api::error::{ErrorKind, RetryAfter};
use matrix_sdk::{Client, HttpError};
use postman_core::channel::ChannelError;

/// Запрос к серверу — не дольше (кроме долгого опроса sync: его срок — свой).
const REQUEST: Duration = Duration::from_secs(60);

/// Настройки шифрования: кросс-подпись и резервную копию не включать самим (это делает пользователь в своём
/// основном приложении или «Восстановить ключи»); ключи из копии — когда сообщение не расшифровалось.
fn encryption() -> EncryptionSettings {
    EncryptionSettings {
        auto_enable_cross_signing: false,
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        auto_enable_backups: false,
    }
}

/// Клиент сервера: `server` — имя сервера (well-known) или адрес; `stores` — свои хранилища, `None` — в памяти (вход).
pub async fn build(server: &str, discover: bool, stores: Option<StoreConfig>) -> Result<Client, ChannelError> {
    let mut b = Client::builder()
        .request_config(RequestConfig::new().retry_limit(1).timeout(REQUEST))
        .with_encryption_settings(encryption())
        .cross_process_store_config(CrossProcessLockConfig::SingleProcess)
        .user_agent(concat!("Postman/", env!("CARGO_PKG_VERSION")));
    b = if discover { b.server_name_or_homeserver_url(server) } else { b.homeserver_url(server) };
    if let Some(s) = stores {
        b = b.store_config(s);
    }
    b.build().await.map_err(|e| {
        tracing::info!("matrix: client not built: {e}");
        match e {
            matrix_sdk::ClientBuildError::Http(h) => http(&h),
            _ => ChannelError::Denied(crate::BAD_SERVER.into()),
        }
    })
}

/// Ошибка сервера словами договора.
fn kind(k: &ErrorKind) -> Option<ChannelError> {
    Some(match k {
        ErrorKind::LimitExceeded(d) => {
            let secs = match &d.retry_after {
                Some(RetryAfter::Delay(d)) => d.as_secs_f64().ceil() as u32,
                Some(RetryAfter::DateTime(t)) => t.duration_since(std::time::SystemTime::now()).map_or(1, |d| d.as_secs_f64().ceil() as u32),
                None => 5,
            };
            ChannelError::Wait { secs: secs.max(1) }
        }
        ErrorKind::UnknownToken(_) | ErrorKind::MissingToken | ErrorKind::UserDeactivated => ChannelError::SignedOut(k.errcode().to_string()),
        ErrorKind::Forbidden => ChannelError::Denied(k.errcode().to_string()),
        ErrorKind::NotFound => ChannelError::NotFound,
        ErrorKind::TooLarge => ChannelError::TooLarge { max: 0 },
        _ => return None,
    })
}

pub fn http(e: &HttpError) -> ChannelError {
    if let Some(k) = e.client_api_error_kind()
        && let Some(c) = kind(k)
    {
        return c;
    }
    match e {
        HttpError::Reqwest(r) => ChannelError::Network(if r.is_timeout() { "timeout".into() } else { "connection failed".into() }),
        HttpError::Cached(inner) => http(inner),
        HttpError::Api(a) => {
            // Ответ не по договору (502 прокси, обрыв) — как связь.
            let status = a.to_string();
            if status.contains("50") { ChannelError::Network(status) } else { ChannelError::Protocol(status) }
        }
        other => ChannelError::Protocol(other.to_string()),
    }
}

pub fn err(e: matrix_sdk::Error) -> ChannelError {
    if let Some(k) = e.client_api_error_kind()
        && let Some(c) = kind(k)
    {
        return c;
    }
    match e {
        matrix_sdk::Error::Http(h) => http(&h),
        matrix_sdk::Error::AuthenticationRequired => ChannelError::SignedOut("authentication required".into()),
        matrix_sdk::Error::StateStore(s) => ChannelError::Storage(s.to_string()),
        matrix_sdk::Error::CryptoStoreError(s) => ChannelError::Storage(s.to_string()),
        other => ChannelError::Protocol(other.to_string()),
    }
}
