//! Хранилище ключей сквозного шифрования SDK (`CryptoStore`) поверх базы профиля: чтение — из `MemoryStore` SDK,
//! запись — туда же и в [`Disk`] (зашифровано ключом секрета сессии из хранилища ОС, ADR 0012 п. 9). Сохраняется всё,
//! без чего шифрование после перезапуска не работает: учётка Olm, свои ключи кросс-подписи, сеансы Olm и Megolm (входящие
//! — с отметкой резервной копии, исходящие), устройства и личности, отслеживаемые люди, запросы и полученные секреты,
//! ключ и версия резервной копии, отказы в ключах, настройки комнат, токен sync, свои значения. Блокировки между
//! процессами — только в памяти (процесс один).
//!
//! Ключи: `mx/c/account`, `mx/c/meta`, `mx/c/olm/<ключ отправителя>`, `mx/c/igs/<комната>`, `mx/c/ogs/<комната>`,
//! `mx/c/dev/<человек>`, `mx/c/id/<человек>`, `mx/c/wh/<комната>`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use matrix_sdk::cross_process_lock::CrossProcessLockGeneration;
use matrix_sdk::ruma::events::secret::request::SecretName;
use matrix_sdk::ruma::{DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, TransactionId, UserId};
use matrix_sdk_base::crypto::olm::{
    InboundGroupSession, OlmMessageHash, OutboundGroupSession, PickledAccount, PickledCrossSigningIdentity, PickledInboundGroupSession,
    PickledOutboundGroupSession, PickledSession, PrivateCrossSigningIdentity, SenderDataType,
};
use matrix_sdk_base::crypto::store::types::{
    BackupDecryptionKey, BackupKeys, Changes, DehydratedDeviceKey, DeviceChanges, IdentityChanges, PendingChanges, RoomKeyCounts,
    RoomKeyWithheldEntry, RoomPendingKeyBundleDetails, RoomSettings, SecretsInboxItem, StoredRoomKeyBundleData, TrackedUser,
};
use matrix_sdk_base::crypto::store::{CryptoStore, CryptoStoreError, MemoryStore};
use matrix_sdk_base::crypto::vodozemac::Curve25519PublicKey;
use matrix_sdk_base::crypto::{Account, DeviceData, GossipRequest, SecretInfo, Session, UserIdentityData};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::disk::{Disk, Loaded};

type Result<T> = std::result::Result<T, CryptoStoreError>;

const ACCOUNT: &str = "mx/c/account";
const META: &str = "mx/c/meta";

fn key(kind: &str, id: &str) -> String {
    format!("mx/c/{kind}/{id}")
}

/// Мелкое общее — одной записью.
#[derive(Default, Serialize, Deserialize)]
struct Meta {
    #[serde(default)]
    identity: Option<serde_json::Value>,
    #[serde(default)]
    backup_version: Option<String>,
    #[serde(default)]
    backup_key: Option<BackupDecryptionKey>,
    #[serde(default)]
    next_batch: Option<String>,
    #[serde(default)]
    tracked: BTreeMap<OwnedUserId, bool>,
    #[serde(default)]
    room_settings: BTreeMap<OwnedRoomId, RoomSettings>,
    #[serde(default)]
    requests: BTreeMap<String, GossipRequest>,
    /// Полученные секреты по имени (`m.secret.send`).
    #[serde(default)]
    secrets: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    hashes: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    custom: BTreeMap<String, String>,
}

/// Входящий сеанс Megolm и версия резервной копии, в которой он есть.
#[derive(Serialize, Deserialize)]
struct Igs {
    pickle: serde_json::Value,
    #[serde(default)]
    backed_up_to: Option<String>,
}

#[derive(Default)]
struct Mirror {
    meta: Meta,
    olm: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    igs: BTreeMap<OwnedRoomId, BTreeMap<String, Igs>>,
    devices: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, DeviceData>>,
    withheld: BTreeMap<OwnedRoomId, BTreeMap<String, RoomKeyWithheldEntry>>,
}

/// `CryptoStore` SDK над базой профиля.
pub struct Crypto {
    inner: MemoryStore,
    disk: Arc<Disk>,
    mirror: Mutex<Mirror>,
}

impl std::fmt::Debug for Crypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Crypto").finish_non_exhaustive()
    }
}

fn json<T: serde::de::DeserializeOwned>(v: &[u8]) -> Option<T> {
    serde_json::from_slice(v).inspect_err(|e| tracing::warn!("matrix crypto store: value skipped: {e}")).ok()
}

impl Crypto {
    /// Из прочитанного при открытии: зеркало и `MemoryStore` с теми же ключами.
    pub async fn open(disk: Arc<Disk>, loaded: &Loaded) -> Result<Self> {
        let inner = MemoryStore::new();
        let mut m = Mirror::default();
        let mut account = None;
        let mut outbound: Vec<PickledOutboundGroupSession> = Vec::new();
        let mut identities: Vec<UserIdentityData> = Vec::new();
        for (k, v) in loaded {
            let Some(rest) = k.strip_prefix("mx/c/") else { continue };
            match rest.split_once('/') {
                None if k == ACCOUNT => account = json::<PickledAccount>(v),
                None if k == META => m.meta = json(v).unwrap_or_default(),
                Some(("olm", id)) => {
                    if let Some(s) = json(v) {
                        m.olm.insert(id.to_owned(), s);
                    }
                }
                Some(("igs", room)) => {
                    if let (Ok(room), Some(s)) = (OwnedRoomId::try_from(room), json(v)) {
                        m.igs.insert(room, s);
                    }
                }
                Some(("ogs", _)) => outbound.extend(json(v)),
                Some(("dev", user)) => {
                    if let (Ok(user), Some(d)) = (OwnedUserId::try_from(user), json(v)) {
                        m.devices.insert(user, d);
                    }
                }
                Some(("id", _)) => identities.extend(json(v)),
                Some(("wh", room)) => {
                    if let (Ok(room), Some(w)) = (OwnedRoomId::try_from(room), json(v)) {
                        m.withheld.insert(room, w);
                    }
                }
                _ => {}
            }
        }
        let Some(pickle) = account else {
            // Учётки ещё нет: OlmMachine заведёт её сама и сохранит.
            return Ok(Self { inner, disk, mirror: Mutex::new(m) });
        };
        let account = Account::from_pickle(pickle).map_err(|e| CryptoStoreError::Backend(Box::new(e)))?;
        let device_keys = account.device_keys();
        let statics = account.static_data().clone();
        inner.save_pending_changes(PendingChanges { account: Some(account) }).await?;

        let mut changes = Changes::default();
        if let Some(p) = m.meta.identity.clone() {
            changes.private_identity =
                serde_json::from_value::<PickledCrossSigningIdentity>(p).ok().and_then(|p| PrivateCrossSigningIdentity::from_pickle(p).ok());
        }
        changes.backup_version = m.meta.backup_version.clone();
        changes.backup_decryption_key = m.meta.backup_key.clone();
        changes.next_batch_token = m.meta.next_batch.clone();
        for sessions in m.olm.values() {
            for p in sessions.values() {
                match serde_json::from_value::<PickledSession>(p.clone())
                    .map_err(|e| e.to_string())
                    .and_then(|p| Session::from_pickle(device_keys.clone(), p).map_err(|e| e.to_string()))
                {
                    Ok(s) => changes.sessions.push(s),
                    Err(e) => tracing::warn!("matrix crypto store: olm session skipped: {e}"),
                }
            }
        }
        for p in outbound {
            if let Ok(s) = OutboundGroupSession::from_pickle(statics.device_id.clone(), statics.identity_keys.clone(), p) {
                changes.outbound_group_sessions.push(s);
            }
        }
        changes.devices = DeviceChanges { new: m.devices.values().flat_map(|d| d.values().cloned()).collect(), ..DeviceChanges::default() };
        changes.identities = IdentityChanges { new: identities, ..IdentityChanges::default() };
        changes.key_requests = m.meta.requests.values().cloned().collect();
        changes.room_settings = m.meta.room_settings.iter().map(|(r, s)| (r.clone(), s.clone())).collect::<HashMap<_, _>>();
        for (name, values) in &m.meta.secrets {
            for s in values {
                changes.secrets.push(SecretsInboxItem { secret_name: SecretName::from(name.as_str()), secret: Zeroizing::new(s.clone()) });
            }
        }
        for (sender_key, hashes) in &m.meta.hashes {
            changes.message_hashes.extend(hashes.iter().map(|h| OlmMessageHash { sender_key: sender_key.clone(), hash: h.clone() }));
        }
        for (room, entries) in &m.withheld {
            changes.withheld_session_info.insert(room.clone(), entries.clone().into_iter().collect());
        }
        inner.save_changes(changes).await?;
        // Входящие сеансы — по версии резервной копии, в которой они есть.
        let mut by_version: BTreeMap<Option<String>, Vec<InboundGroupSession>> = BTreeMap::new();
        for sessions in m.igs.values() {
            for s in sessions.values() {
                match serde_json::from_value::<PickledInboundGroupSession>(s.pickle.clone())
                    .map_err(|e| e.to_string())
                    .and_then(|p| InboundGroupSession::from_pickle(p).map_err(|e| e.to_string()))
                {
                    Ok(g) => by_version.entry(s.backed_up_to.clone()).or_default().push(g),
                    Err(e) => tracing::warn!("matrix crypto store: room key skipped: {e}"),
                }
            }
        }
        for (version, sessions) in by_version {
            inner.save_inbound_group_sessions(sessions, version.as_deref()).await?;
        }
        let tracked: Vec<(OwnedUserId, bool)> = m.meta.tracked.iter().map(|(u, d)| (u.clone(), *d)).collect();
        let refs: Vec<(&UserId, bool)> = tracked.iter().map(|(u, d)| (u.as_ref(), *d)).collect();
        inner.save_tracked_users(&refs).await?;
        let engine = base64::engine::general_purpose::STANDARD;
        for (k, v) in &m.meta.custom {
            if let Ok(v) = engine.decode(v) {
                inner.set_custom_value(k, v).await?;
            }
        }
        Ok(Self { inner, disk, mirror: Mutex::new(m) })
    }

    fn write_meta(&self, m: &Mirror) {
        self.disk.put_json(META.into(), &m.meta);
    }

    fn with<T>(&self, f: impl FnOnce(&mut Mirror) -> T) -> Option<T> {
        self.mirror.lock().ok().map(|mut m| f(&mut m))
    }

    /// Входящие сеансы — в зеркало и базу (комнатами).
    async fn persist_inbound(&self, sessions: &[InboundGroupSession], version: Option<&str>) {
        let mut pickles = Vec::with_capacity(sessions.len());
        for s in sessions {
            pickles.push((s.room_id().to_owned(), s.session_id().to_owned(), s.pickle().await));
        }
        self.with(|m| {
            let mut rooms = BTreeSet::new();
            for (room, id, pickle) in pickles {
                let backed_up_to = if pickle.backed_up { version.map(str::to_owned) } else { None };
                let Ok(pickle) = serde_json::to_value(&pickle) else { continue };
                m.igs.entry(room.clone()).or_default().insert(id, Igs { pickle, backed_up_to });
                rooms.insert(room);
            }
            for room in rooms {
                if let Some(v) = m.igs.get(&room) {
                    self.disk.put_json(key("igs", room.as_str()), v);
                }
            }
        });
    }
}

#[async_trait]
impl CryptoStore for Crypto {
    type Error = CryptoStoreError;

    async fn load_account(&self) -> Result<Option<Account>> {
        self.inner.load_account().await.map_err(Into::into)
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        self.inner.load_identity().await.map_err(Into::into)
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        // Пиклы — до передачи `changes` в память: сеансы `Changes` владеющие.
        let identity = match &changes.private_identity {
            Some(i) => Some(i.pickle().await),
            None => None,
        };
        let mut olm = Vec::with_capacity(changes.sessions.len());
        for s in &changes.sessions {
            olm.push((s.sender_key().to_base64(), s.session_id().to_owned(), s.pickle().await));
        }
        let mut ogs = Vec::with_capacity(changes.outbound_group_sessions.len());
        for s in &changes.outbound_group_sessions {
            ogs.push((s.room_id().to_owned(), s.pickle().await));
        }
        let inbound = changes.inbound_group_sessions.clone();
        let devices = changes.devices.clone();
        let identities: Vec<UserIdentityData> = changes.identities.new.iter().chain(&changes.identities.changed).cloned().collect();
        let requests = changes.key_requests.clone();
        let secrets: Vec<(String, String)> = changes.secrets.iter().map(|s| (s.secret_name.to_string(), s.secret.to_string())).collect();
        let hashes: Vec<(String, String)> = changes.message_hashes.iter().map(|h| (h.sender_key.clone(), h.hash.clone())).collect();
        let withheld = changes.withheld_session_info.clone();
        let room_settings = changes.room_settings.clone();
        let (backup_key, backup_version, next_batch) =
            (changes.backup_decryption_key.clone(), changes.backup_version.clone(), changes.next_batch_token.clone());
        self.inner.save_changes(changes).await?;

        self.persist_inbound(&inbound, None).await;
        self.with(|m| {
            let mut meta = false;
            if let Some(i) = identity {
                m.meta.identity = serde_json::to_value(&i).ok();
                meta = true;
            }
            let mut senders = BTreeSet::new();
            for (sender, id, pickle) in olm {
                if let Ok(v) = serde_json::to_value(&pickle) {
                    m.olm.entry(sender.clone()).or_default().insert(id, v);
                }
                senders.insert(sender);
            }
            for sender in senders {
                if let Some(v) = m.olm.get(&sender) {
                    self.disk.put_json(key("olm", &sender), v);
                }
            }
            for (room, pickle) in ogs {
                self.disk.put_json(key("ogs", room.as_str()), &pickle);
            }
            let mut users = BTreeSet::new();
            for d in devices.new.iter().chain(&devices.changed) {
                m.devices.entry(d.user_id().to_owned()).or_default().insert(d.device_id().to_owned(), d.clone());
                users.insert(d.user_id().to_owned());
            }
            for d in &devices.deleted {
                if let Some(u) = m.devices.get_mut(d.user_id()) {
                    u.remove(d.device_id());
                }
                users.insert(d.user_id().to_owned());
            }
            for u in users {
                match m.devices.get(&u) {
                    Some(v) => self.disk.put_json(key("dev", u.as_str()), v),
                    None => self.disk.remove(&key("dev", u.as_str())),
                }
            }
            for i in identities {
                self.disk.put_json(key("id", i.user_id().as_str()), &i);
            }
            for r in requests {
                m.meta.requests.retain(|_, old| old.info != r.info);
                m.meta.requests.insert(r.request_id.to_string(), r);
                meta = true;
            }
            for (name, s) in secrets {
                m.meta.secrets.entry(name).or_default().push(s);
                meta = true;
            }
            for (sender, h) in hashes {
                m.meta.hashes.entry(sender).or_default().insert(h);
                meta = true;
            }
            for (room, entries) in withheld {
                m.withheld.entry(room.clone()).or_default().extend(entries);
                if let Some(v) = m.withheld.get(&room) {
                    self.disk.put_json(key("wh", room.as_str()), v);
                }
            }
            if !room_settings.is_empty() {
                m.meta.room_settings.extend(room_settings);
                meta = true;
            }
            if let Some(k) = backup_key {
                m.meta.backup_key = Some(k);
                meta = true;
            }
            if let Some(v) = backup_version {
                m.meta.backup_version = Some(v);
                meta = true;
            }
            if let Some(t) = next_batch {
                m.meta.next_batch = Some(t);
                meta = true;
            }
            if meta {
                self.write_meta(m);
            }
        });
        Ok(())
    }

    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<()> {
        let pickle = changes.account.as_ref().map(Account::pickle);
        self.inner.save_pending_changes(changes).await?;
        if let Some(p) = pickle {
            self.disk.put_json(ACCOUNT.into(), &p);
        }
        Ok(())
    }

    async fn save_inbound_group_sessions(&self, sessions: Vec<InboundGroupSession>, backed_up_to_version: Option<&str>) -> Result<()> {
        self.persist_inbound(&sessions, backed_up_to_version).await;
        self.inner.save_inbound_group_sessions(sessions, backed_up_to_version).await.map_err(Into::into)
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Vec<Session>>> {
        self.inner.get_sessions(sender_key).await.map_err(Into::into)
    }

    async fn get_inbound_group_session(&self, room_id: &RoomId, session_id: &str) -> Result<Option<InboundGroupSession>> {
        self.inner.get_inbound_group_session(room_id, session_id).await.map_err(Into::into)
    }

    async fn get_withheld_info(&self, room_id: &RoomId, session_id: &str) -> Result<Option<RoomKeyWithheldEntry>> {
        self.inner.get_withheld_info(room_id, session_id).await.map_err(Into::into)
    }

    async fn get_withheld_sessions_by_room_id(&self, room_id: &RoomId) -> Result<Vec<RoomKeyWithheldEntry>> {
        self.inner.get_withheld_sessions_by_room_id(room_id).await.map_err(Into::into)
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        self.inner.get_inbound_group_sessions().await.map_err(Into::into)
    }

    async fn inbound_group_session_counts(&self, backup_version: Option<&str>) -> Result<RoomKeyCounts> {
        self.inner.inbound_group_session_counts(backup_version).await.map_err(Into::into)
    }

    async fn get_inbound_group_sessions_by_room_id(&self, room_id: &RoomId) -> Result<Vec<InboundGroupSession>> {
        self.inner.get_inbound_group_sessions_by_room_id(room_id).await.map_err(Into::into)
    }

    async fn get_inbound_group_sessions_for_device_batch(
        &self,
        curve_key: Curve25519PublicKey,
        sender_data_type: SenderDataType,
        after_session_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        self.inner.get_inbound_group_sessions_for_device_batch(curve_key, sender_data_type, after_session_id, limit).await.map_err(Into::into)
    }

    async fn inbound_group_sessions_for_backup(&self, backup_version: &str, limit: usize) -> Result<Vec<InboundGroupSession>> {
        self.inner.inbound_group_sessions_for_backup(backup_version, limit).await.map_err(Into::into)
    }

    async fn mark_inbound_group_sessions_as_backed_up(&self, backup_version: &str, room_and_session_ids: &[(&RoomId, &str)]) -> Result<()> {
        self.inner.mark_inbound_group_sessions_as_backed_up(backup_version, room_and_session_ids).await?;
        let mut marked = Vec::new();
        for (room, id) in room_and_session_ids {
            if let Some(s) = self.inner.get_inbound_group_session(room, id).await? {
                marked.push(s);
            }
        }
        self.persist_inbound(&marked, Some(backup_version)).await;
        Ok(())
    }

    async fn reset_backup_state(&self) -> Result<()> {
        self.inner.reset_backup_state().await?;
        let all = self.inner.get_inbound_group_sessions().await?;
        self.persist_inbound(&all, None).await;
        Ok(())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        self.inner.load_backup_keys().await.map_err(Into::into)
    }

    async fn load_dehydrated_device_pickle_key(&self) -> Result<Option<DehydratedDeviceKey>> {
        self.inner.load_dehydrated_device_pickle_key().await.map_err(Into::into)
    }

    async fn delete_dehydrated_device_pickle_key(&self) -> Result<()> {
        self.inner.delete_dehydrated_device_pickle_key().await.map_err(Into::into)
    }

    async fn get_outbound_group_session(&self, room_id: &RoomId) -> Result<Option<OutboundGroupSession>> {
        self.inner.get_outbound_group_session(room_id).await.map_err(Into::into)
    }

    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>> {
        self.inner.load_tracked_users().await.map_err(Into::into)
    }

    async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<()> {
        self.inner.save_tracked_users(users).await?;
        self.with(|m| {
            for (u, dirty) in users {
                m.meta.tracked.insert((*u).to_owned(), *dirty);
            }
            self.write_meta(m);
        });
        Ok(())
    }

    async fn get_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<Option<DeviceData>> {
        self.inner.get_device(user_id, device_id).await.map_err(Into::into)
    }

    async fn get_user_devices(&self, user_id: &UserId) -> Result<HashMap<OwnedDeviceId, DeviceData>> {
        self.inner.get_user_devices(user_id).await.map_err(Into::into)
    }

    async fn get_own_device(&self) -> Result<DeviceData> {
        self.inner.get_own_device().await.map_err(Into::into)
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentityData>> {
        self.inner.get_user_identity(user_id).await.map_err(Into::into)
    }

    async fn is_message_known(&self, message_hash: &OlmMessageHash) -> Result<bool> {
        self.inner.is_message_known(message_hash).await.map_err(Into::into)
    }

    async fn get_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<Option<GossipRequest>> {
        self.inner.get_outgoing_secret_requests(request_id).await.map_err(Into::into)
    }

    async fn get_secret_request_by_info(&self, secret_info: &SecretInfo) -> Result<Option<GossipRequest>> {
        self.inner.get_secret_request_by_info(secret_info).await.map_err(Into::into)
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        self.inner.get_unsent_secret_requests().await.map_err(Into::into)
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        self.inner.delete_outgoing_secret_requests(request_id).await?;
        let id = request_id.to_string();
        self.with(|m| {
            if m.meta.requests.remove(&id).is_some() {
                self.write_meta(m);
            }
        });
        Ok(())
    }

    async fn get_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<Vec<Zeroizing<String>>> {
        self.inner.get_secrets_from_inbox(secret_name).await.map_err(Into::into)
    }

    async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<()> {
        self.inner.delete_secrets_from_inbox(secret_name).await?;
        let name = secret_name.to_string();
        self.with(|m| {
            if m.meta.secrets.remove(&name).is_some() {
                self.write_meta(m);
            }
        });
        Ok(())
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        self.inner.get_room_settings(room_id).await.map_err(Into::into)
    }

    async fn get_received_room_key_bundle_data(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<StoredRoomKeyBundleData>> {
        self.inner.get_received_room_key_bundle_data(room_id, user_id).await.map_err(Into::into)
    }

    async fn get_pending_key_bundle_details_for_room(&self, room_id: &RoomId) -> Result<Option<RoomPendingKeyBundleDetails>> {
        self.inner.get_pending_key_bundle_details_for_room(room_id).await.map_err(Into::into)
    }

    async fn get_all_rooms_pending_key_bundles(&self) -> Result<Vec<RoomPendingKeyBundleDetails>> {
        self.inner.get_all_rooms_pending_key_bundles().await.map_err(Into::into)
    }

    async fn has_downloaded_all_room_keys(&self, room_id: &RoomId) -> Result<bool> {
        self.inner.has_downloaded_all_room_keys(room_id).await.map_err(Into::into)
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get_custom_value(key).await.map_err(Into::into)
    }

    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<()> {
        let v = base64::engine::general_purpose::STANDARD.encode(&value);
        self.inner.set_custom_value(key, value).await?;
        let k = key.to_owned();
        self.with(|m| {
            m.meta.custom.insert(k, v);
            self.write_meta(m);
        });
        Ok(())
    }

    async fn remove_custom_value(&self, key: &str) -> Result<()> {
        self.inner.remove_custom_value(key).await?;
        self.with(|m| {
            if m.meta.custom.remove(key).is_some() {
                self.write_meta(m);
            }
        });
        Ok(())
    }

    async fn try_take_leased_lock(&self, lease_duration_ms: u32, key: &str, holder: &str) -> Result<Option<CrossProcessLockGeneration>> {
        self.inner.try_take_leased_lock(lease_duration_ms, key, holder).await.map_err(Into::into)
    }

    async fn next_batch_token(&self) -> Result<Option<String>> {
        self.inner.next_batch_token().await.map_err(Into::into)
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    async fn reopen(&self) -> Result<()> {
        Ok(())
    }

    async fn get_size(&self) -> Result<Option<usize>> {
        Ok(None)
    }
}
