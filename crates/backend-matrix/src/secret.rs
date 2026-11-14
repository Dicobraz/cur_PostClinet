//! Секрет сессии Matrix (ADR 0012 п. 3, «Проверка договора»: токен доступа, id устройства, ключ шифрования хранилища
//! ключей). Его хранит ядро — в хранилище секретов ОС; канал отдаёт его `Prompt::Done` и получает в `ConnectCtx`.
//! `Debug` — без значений.

use base64::Engine as _;
use postman_core::channel::Secret;
use serde::{Deserialize, Serialize};

/// Версия формата секрета.
const VERSION: u8 = 1;

/// Длина ключа шифрования своих данных канала.
pub const STORE_KEY: usize = 32;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub v: u8,
    /// Адрес сервера Client-Server API (после well-known).
    pub homeserver: String,
    pub user_id: String,
    pub device_id: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Ключ шифрования хранилища канала (состояние и ключи сквозного шифрования в базе профиля), base64.
    pub store_key: String,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("homeserver", &self.homeserver).field("device_id", &self.device_id).finish_non_exhaustive()
    }
}

impl Session {
    pub fn new(homeserver: String, user_id: String, device_id: String, access_token: String, refresh_token: Option<String>) -> Result<Self, String> {
        let mut key = [0u8; STORE_KEY];
        getrandom::fill(&mut key).map_err(|e| format!("random: {e}"))?;
        let store_key = base64::engine::general_purpose::STANDARD.encode(key);
        Ok(Self { v: VERSION, homeserver, user_id, device_id, access_token, refresh_token, store_key })
    }

    pub fn to_secret(&self) -> Secret {
        Secret::new(serde_json::to_vec(self).unwrap_or_default())
    }

    /// Секрет этого канала; чужой или испорченный — `None`.
    pub fn from_secret(s: &Secret) -> Option<Self> {
        let v: Self = serde_json::from_slice(s.expose()).ok()?;
        (v.v == VERSION && !v.access_token.is_empty() && !v.user_id.is_empty() && v.key().is_some()).then_some(v)
    }

    /// Ключ шифрования хранилища.
    pub fn key(&self) -> Option<[u8; STORE_KEY]> {
        let b = base64::engine::general_purpose::STANDARD.decode(&self.store_key).ok()?;
        b.try_into().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_debug() {
        let s = Session::new("https://matrix.example.test".into(), "@anna:example.test".into(), "DEV1".into(), "tok-secret".into(), None).unwrap();
        let back = Session::from_secret(&s.to_secret()).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.key().unwrap().len(), STORE_KEY);
        assert!(!format!("{s:?}").contains("tok-secret") && !format!("{s:?}").contains(&s.store_key));
        assert!(Session::from_secret(&Secret::new(b"not-a-session".to_vec())).is_none());
    }
}
