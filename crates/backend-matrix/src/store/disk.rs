//! Свои данные канала в базе профиля (`PluginStore`, решение «всё в базе»): ключи `mx/…`, значения зашифрованы
//! ключом из секрета сессии (`StoreCipher`, XChaCha20-Poly1305). Хранилища SDK (`Send + Sync`) пишут сюда через
//! очередь: запись в redb делает задача потока учётки ([`drain`]), пачками — последнее значение ключа побеждает.
//!
//! `PluginStore` не перечисляет ключи — список ключей канала лежит отдельной записью ([`INDEX`]); пустое значение —
//! ключа больше нет.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use matrix_sdk_store_encryption::{EncryptedValue, StoreCipher};
use postman_core::channel::{ChannelError, PluginStore};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Все ключи канала начинаются с него (`LocalStore` добавит `chan/matrix/<учётка>/`).
pub const ROOT: &str = "mx/";
/// Список ключей.
const INDEX: &str = "mx/index";
/// Шифр хранилища, зашифрованный ключом секрета.
const CIPHER: &str = "mx/cipher";
/// Устройство, которому принадлежат данные: другое (вход заново) — данные прежнего удаляются.
const DEVICE: &str = "mx/device";

/// Запись в очередь: ключ и зашифрованное значение (пустое — удалить).
type Write = (String, Vec<u8>);

/// Хранилище своих данных канала: зеркало в памяти и очередь записи в базу.
pub struct Disk {
    cipher: StoreCipher,
    keys: Mutex<BTreeSet<String>>,
    tx: UnboundedSender<Write>,
}

impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disk").finish_non_exhaustive()
    }
}

/// Что прочитано из базы при открытии: значения по ключам (уже расшифрованные).
pub type Loaded = BTreeMap<String, Vec<u8>>;

fn storage(e: impl std::fmt::Display) -> ChannelError {
    ChannelError::Storage(e.to_string())
}

impl Disk {
    /// Открыть данные устройства `device`: прочитать всё, что было, и очередь записи. Данные другого устройства (или
    /// не расшифровать — ключ сменился) удаляются: начинаем с чистого листа.
    pub fn open(store: &dyn PluginStore, key: &[u8; 32], device: &str) -> Result<(Arc<Disk>, Loaded, UnboundedReceiver<Write>), ChannelError> {
        let same = store.get(DEVICE)?.is_some_and(|d| d == device.as_bytes());
        let cipher = match store.get(CIPHER)? {
            Some(blob) if same => StoreCipher::import_with_key(key, &blob).ok(),
            _ => None,
        };
        let (cipher, fresh) = match cipher {
            Some(c) => (c, false),
            None => {
                store.remove_prefix(ROOT)?;
                let c = StoreCipher::new().map_err(storage)?;
                store.put(CIPHER, &c.export_with_key(key).map_err(storage)?)?;
                store.put(DEVICE, device.as_bytes())?;
                (c, true)
            }
        };
        let (tx, rx) = unbounded_channel();
        let disk = Disk { cipher, keys: Mutex::new(BTreeSet::new()), tx };
        let mut loaded = Loaded::new();
        if !fresh && let Some(index) = store.get(INDEX)? {
            let keys: Vec<String> = disk.open_value(&index).and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_default();
            for k in keys {
                let Some(raw) = store.get(&k)? else { continue };
                if raw.is_empty() {
                    continue;
                }
                match disk.open_value(&raw) {
                    Some(v) => {
                        loaded.insert(k.clone(), v);
                    }
                    None => tracing::warn!("matrix store: a value is not readable, skipped"),
                }
            }
        }
        if let Ok(mut set) = disk.keys.lock() {
            set.extend(loaded.keys().cloned());
        }
        Ok((Arc::new(disk), loaded, rx))
    }

    fn seal(&self, plain: Vec<u8>) -> Option<Vec<u8>> {
        let v = self.cipher.encrypt_value_data(plain).ok()?;
        serde_json::to_vec(&v).ok()
    }

    fn open_value(&self, raw: &[u8]) -> Option<Vec<u8>> {
        let v: EncryptedValue = serde_json::from_slice(raw).ok()?;
        self.cipher.decrypt_value_data(v).ok()
    }

    /// Записать значение ключа (`mx/…`).
    pub fn put(&self, key: String, plain: Vec<u8>) {
        let Some(sealed) = self.seal(plain) else {
            return tracing::error!("matrix store: value not encrypted");
        };
        let added = self.keys.lock().map(|mut k| k.insert(key.clone())).unwrap_or(false);
        let _ = self.tx.send((key, sealed));
        if added {
            self.write_index();
        }
    }

    /// Значение в JSON.
    pub fn put_json<T: serde::Serialize>(&self, key: String, value: &T) {
        match serde_json::to_vec(value) {
            Ok(v) => self.put(key, v),
            Err(e) => tracing::error!("matrix store: value not serialized: {e}"),
        }
    }

    pub fn remove(&self, key: &str) {
        let removed = self.keys.lock().map(|mut k| k.remove(key)).unwrap_or(false);
        if removed {
            let _ = self.tx.send((key.to_owned(), Vec::new()));
            self.write_index();
        }
    }

    fn write_index(&self) {
        let keys: Vec<String> = self.keys.lock().map(|k| k.iter().cloned().collect()).unwrap_or_default();
        let Ok(plain) = serde_json::to_vec(&keys) else { return };
        if let Some(sealed) = self.seal(plain) {
            let _ = self.tx.send((INDEX.to_owned(), sealed));
        }
    }
}

/// Задача потока учётки: записи из очереди — в базу профиля. Пачка — всё, что пришло за 30 мс; ключ — последнее
/// значение. Очередь закрыта (сессии нет) — дописать и закончить.
pub async fn drain(store: Rc<dyn PluginStore>, mut rx: UnboundedReceiver<Write>) {
    while let Some(first) = rx.recv().await {
        let mut batch: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        batch.insert(first.0, first.1);
        tokio::time::sleep(Duration::from_millis(30)).await;
        while let Ok((k, v)) = rx.try_recv() {
            batch.insert(k, v);
        }
        for (k, v) in batch {
            if let Err(e) = store.put(&k, &v) {
                tracing::warn!("matrix store: not written: {e}");
            }
        }
    }
}

/// Всё, что в очереди, — сейчас (тесты, выход).
pub fn flush_now(store: &dyn PluginStore, rx: &mut UnboundedReceiver<Write>) {
    while let Ok((k, v)) = rx.try_recv() {
        if let Err(e) = store.put(&k, &v) {
            tracing::warn!("matrix store: not written: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    pub(crate) struct Mem(RefCell<HashMap<String, Vec<u8>>>);

    impl PluginStore for Mem {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, ChannelError> {
            Ok(self.0.borrow().get(key).cloned())
        }
        fn put(&self, key: &str, value: &[u8]) -> Result<(), ChannelError> {
            self.0.borrow_mut().insert(key.to_owned(), value.to_vec());
            Ok(())
        }
        fn remove_prefix(&self, prefix: &str) -> Result<(), ChannelError> {
            self.0.borrow_mut().retain(|k, _| !k.starts_with(prefix));
            Ok(())
        }
    }

    /// Значения — зашифрованы, переживают переоткрытие; другое устройство или ключ — с чистого листа.
    #[test]
    fn sealed_and_reopened() {
        let mem = Mem::default();
        let key = [7u8; 32];
        let (disk, loaded, mut rx) = Disk::open(&mem, &key, "DEV1").unwrap();
        assert!(loaded.is_empty());
        disk.put("mx/s/x".into(), b"room state".to_vec());
        disk.put("mx/s/y".into(), b"other".to_vec());
        disk.remove("mx/s/y");
        flush_now(&mem, &mut rx);
        assert!(!mem.0.borrow().values().any(|v| v.windows(10).any(|w| w == b"room state")), "на диске — шифр");
        let (_, loaded, _) = Disk::open(&mem, &key, "DEV1").unwrap();
        assert_eq!(loaded.get("mx/s/x").map(Vec::as_slice), Some(&b"room state"[..]));
        assert!(!loaded.contains_key("mx/s/y"));
        let (_, loaded, _) = Disk::open(&mem, &[8u8; 32], "DEV1").unwrap();
        assert!(loaded.is_empty(), "другой ключ — данные не читаются");
        let (_, loaded, _) = Disk::open(&mem, &[8u8; 32], "DEV2").unwrap();
        assert!(loaded.is_empty(), "другое устройство — с чистого листа");
    }
}
