//! Хранилище состояния SDK (`StateStore`) поверх базы профиля: чтение — из `MemoryStore` SDK, запись — туда же и в
//! [`Disk`]. Сохраняется то, без чего SDK после перезапуска не продолжит sync от курсора: сведения о комнатах
//! (`RoomInfo`: шифрование, счётчики, имя), события состояния и приглашения, данные учётки и комнат (`m.direct`, теги),
//! профили участников, токен sync и фильтр, свои значения SDK. Квитанции, присутствие, черновики, очередь отправки SDK
//! (её не используем: очередь — у ядра) — только в памяти.
//!
//! Ключи: `mx/s/i/<комната>` — `RoomInfo`; `mx/s/r/<комната>` — состояние, приглашение, данные комнаты, профили;
//! `mx/s/g` — данные учётки, токен, фильтры, свои значения.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use matrix_sdk::deserialized_responses::{DisplayName, RawAnySyncOrStrippedState};
use matrix_sdk::ruma::events::presence::PresenceEvent;
use matrix_sdk::ruma::events::receipt::{Receipt, ReceiptThread, ReceiptType};
use matrix_sdk::ruma::events::{
    AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType,
    RoomAccountDataEventType, StateEventType,
};
use matrix_sdk::ruma::profile::UserProfile;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UserId,
};
use matrix_sdk::store::{
    ChildTransactionId, DependentQueuedRequest, DependentQueuedRequestKind, MemoryStore, QueuedRequest, QueuedRequestKind, RoomLoadSettings,
    SentRequestKey, StateStoreDataKey, StateStoreDataValue, StoredThreadSubscription,
};
use matrix_sdk::{QueueWedgeError, RoomInfo, RoomMemberships, StateChanges, StateStore, StoreError};
use matrix_sdk_base::MinimalRoomMemberEvent;
use serde::{Deserialize, Serialize};

use super::disk::{Disk, Loaded};

type Result<T> = std::result::Result<T, StoreError>;

const GLOBAL: &str = "mx/s/g";

fn info_key(room: &RoomId) -> String {
    format!("mx/s/i/{room}")
}

fn room_key(room: &RoomId) -> String {
    format!("mx/s/r/{room}")
}

/// Состояние комнаты в базе.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct RoomBlob {
    #[serde(default)]
    state: BTreeMap<StateEventType, BTreeMap<String, Raw<AnySyncStateEvent>>>,
    #[serde(default)]
    stripped: BTreeMap<StateEventType, BTreeMap<String, Raw<AnyStrippedStateEvent>>>,
    #[serde(default)]
    account_data: BTreeMap<RoomAccountDataEventType, Raw<AnyRoomAccountDataEvent>>,
    #[serde(default)]
    profiles: BTreeMap<OwnedUserId, MinimalRoomMemberEvent>,
}

/// Общее для учётки.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct GlobalBlob {
    #[serde(default)]
    account_data: BTreeMap<GlobalAccountDataEventType, Raw<AnyGlobalAccountDataEvent>>,
    #[serde(default)]
    sync_token: Option<String>,
    #[serde(default)]
    filters: BTreeMap<String, String>,
    #[serde(default)]
    one_time_key_uploaded: bool,
    /// Свои значения SDK: ключ и значение в base64.
    #[serde(default)]
    custom: BTreeMap<String, String>,
}

#[derive(Default)]
struct Mirror {
    rooms: BTreeMap<OwnedRoomId, RoomBlob>,
    global: GlobalBlob,
}

/// `StateStore` SDK над базой профиля.
pub struct State {
    inner: MemoryStore,
    disk: Arc<Disk>,
    mirror: Mutex<Mirror>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").finish_non_exhaustive()
    }
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

impl State {
    /// Из прочитанного при открытии: зеркало и `MemoryStore` с тем же содержимым.
    pub async fn open(disk: Arc<Disk>, loaded: &Loaded) -> Result<Self> {
        let mut mirror = Mirror::default();
        let mut infos: BTreeMap<OwnedRoomId, RoomInfo> = BTreeMap::new();
        for (k, v) in loaded {
            if k == GLOBAL {
                mirror.global = serde_json::from_slice(v).unwrap_or_default();
            } else if let Some(room) = k.strip_prefix("mx/s/r/") {
                let (Ok(id), Ok(blob)) = (OwnedRoomId::try_from(room), serde_json::from_slice::<RoomBlob>(v)) else { continue };
                mirror.rooms.insert(id, blob);
            } else if let Some(room) = k.strip_prefix("mx/s/i/") {
                let (Ok(id), Ok(info)) = (OwnedRoomId::try_from(room), serde_json::from_slice::<RoomInfo>(v)) else { continue };
                infos.insert(id, info);
            }
        }
        let inner = MemoryStore::new();
        let mut changes = StateChanges { sync_token: mirror.global.sync_token.clone(), ..StateChanges::default() };
        changes.account_data = mirror.global.account_data.clone();
        for (room, blob) in &mirror.rooms {
            if !blob.state.is_empty() {
                changes.state.insert(room.clone(), blob.state.clone());
            }
            if !blob.stripped.is_empty() && blob.state.is_empty() {
                changes.stripped_state.insert(room.clone(), blob.stripped.clone());
            }
            if !blob.account_data.is_empty() {
                changes.room_account_data.insert(room.clone(), blob.account_data.clone());
            }
            if !blob.profiles.is_empty() {
                changes.profiles.insert(room.clone(), blob.profiles.clone());
            }
        }
        changes.room_infos = infos;
        inner.save_changes(&changes).await?;
        for (name, filter) in &mirror.global.filters {
            inner.set_kv_data(StateStoreDataKey::Filter(name), StateStoreDataValue::Filter(filter.clone())).await?;
        }
        if mirror.global.one_time_key_uploaded {
            inner.set_kv_data(StateStoreDataKey::OneTimeKeyAlreadyUploaded, StateStoreDataValue::OneTimeKeyAlreadyUploaded).await?;
        }
        for (k, v) in &mirror.global.custom {
            let engine = base64::engine::general_purpose::STANDARD;
            if let (Ok(k), Ok(v)) = (engine.decode(k), engine.decode(v)) {
                inner.set_custom_value(&k, v).await?;
            }
        }
        Ok(Self { inner, disk, mirror: Mutex::new(mirror) })
    }

    fn write_global(&self, m: &Mirror) {
        self.disk.put_json(GLOBAL.into(), &m.global);
    }

    fn with_mirror<T>(&self, f: impl FnOnce(&mut Mirror) -> T) -> Option<T> {
        self.mirror.lock().ok().map(|mut m| f(&mut m))
    }
}

#[async_trait]
impl StateStore for State {
    type Error = StoreError;

    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        self.inner.get_kv_data(key).await
    }

    async fn set_kv_data(&self, key: StateStoreDataKey<'_>, value: StateStoreDataValue) -> Result<()> {
        let persist = match (&key, &value) {
            (StateStoreDataKey::SyncToken, StateStoreDataValue::SyncToken(t)) => {
                let t = t.clone();
                self.with_mirror(|m| m.global.sync_token = Some(t));
                true
            }
            (StateStoreDataKey::Filter(name), StateStoreDataValue::Filter(f)) => {
                let (name, f) = ((*name).to_owned(), f.clone());
                self.with_mirror(|m| m.global.filters.insert(name, f));
                true
            }
            (StateStoreDataKey::OneTimeKeyAlreadyUploaded, _) => {
                self.with_mirror(|m| m.global.one_time_key_uploaded = true);
                true
            }
            _ => false,
        };
        self.inner.set_kv_data(key, value).await?;
        if persist {
            self.with_mirror(|m| self.write_global(m));
        }
        Ok(())
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<()> {
        let persist = match &key {
            StateStoreDataKey::SyncToken => self.with_mirror(|m| m.global.sync_token = None).is_some(),
            StateStoreDataKey::Filter(name) => {
                let name = (*name).to_owned();
                self.with_mirror(|m| m.global.filters.remove(&name)).is_some()
            }
            StateStoreDataKey::OneTimeKeyAlreadyUploaded => self.with_mirror(|m| m.global.one_time_key_uploaded = false).is_some(),
            _ => false,
        };
        self.inner.remove_kv_data(key).await?;
        if persist {
            self.with_mirror(|m| self.write_global(m));
        }
        Ok(())
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        self.inner.save_changes(changes).await?;
        let Ok(mut m) = self.mirror.lock() else { return Ok(()) };
        let mut global = false;
        if let Some(t) = &changes.sync_token
            && m.global.sync_token.as_ref() != Some(t)
        {
            m.global.sync_token = Some(t.clone());
            global = true;
        }
        for (t, e) in &changes.account_data {
            m.global.account_data.insert(t.clone(), e.clone());
            global = true;
        }
        let mut touched: BTreeSet<OwnedRoomId> = BTreeSet::new();
        for (room, users) in &changes.profiles_to_delete {
            if let Some(blob) = m.rooms.get_mut(room) {
                for u in users {
                    blob.profiles.remove(u);
                }
                touched.insert(room.clone());
            }
        }
        for (room, users) in &changes.profiles {
            let blob = m.rooms.entry(room.clone()).or_default();
            blob.profiles.extend(users.iter().map(|(u, p)| (u.clone(), p.clone())));
            touched.insert(room.clone());
        }
        for (room, events) in &changes.room_account_data {
            let blob = m.rooms.entry(room.clone()).or_default();
            blob.account_data.extend(events.iter().map(|(t, e)| (t.clone(), e.clone())));
            touched.insert(room.clone());
        }
        for (room, types) in &changes.state {
            let blob = m.rooms.entry(room.clone()).or_default();
            for (t, events) in types {
                blob.state.entry(t.clone()).or_default().extend(events.iter().map(|(k, e)| (k.clone(), e.clone())));
            }
            blob.stripped.clear();
            touched.insert(room.clone());
        }
        for (room, types) in &changes.stripped_state {
            let blob = m.rooms.entry(room.clone()).or_default();
            for (t, events) in types {
                blob.stripped.entry(t.clone()).or_default().extend(events.iter().map(|(k, e)| (k.clone(), e.clone())));
            }
            touched.insert(room.clone());
        }
        for room in &touched {
            if let Some(blob) = m.rooms.get(room) {
                self.disk.put_json(room_key(room), blob);
            }
        }
        for (room, info) in &changes.room_infos {
            self.disk.put_json(info_key(room), info);
        }
        if global {
            self.write_global(&m);
        }
        Ok(())
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.inner.get_presence_event(user_id).await
    }

    async fn get_presence_events(&self, user_ids: &[OwnedUserId]) -> Result<Vec<Raw<PresenceEvent>>> {
        self.inner.get_presence_events(user_ids).await
    }

    async fn get_state_event(&self, room_id: &RoomId, event_type: StateEventType, state_key: &str) -> Result<Option<RawAnySyncOrStrippedState>> {
        self.inner.get_state_event(room_id, event_type, state_key).await
    }

    async fn get_state_events(&self, room_id: &RoomId, event_type: StateEventType) -> Result<Vec<RawAnySyncOrStrippedState>> {
        self.inner.get_state_events(room_id, event_type).await
    }

    async fn get_state_events_for_keys(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        self.inner.get_state_events_for_keys(room_id, event_type, state_keys).await
    }

    async fn get_profile(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<MinimalRoomMemberEvent>> {
        self.inner.get_profile(room_id, user_id).await
    }

    async fn get_profiles<'a>(&self, room_id: &RoomId, user_ids: &'a [OwnedUserId]) -> Result<BTreeMap<&'a UserId, MinimalRoomMemberEvent>> {
        self.inner.get_profiles(room_id, user_ids).await
    }

    async fn get_user_ids(&self, room_id: &RoomId, memberships: RoomMemberships) -> Result<Vec<OwnedUserId>> {
        self.inner.get_user_ids(room_id, memberships).await
    }

    async fn get_room_infos(&self, room_load_settings: &RoomLoadSettings) -> Result<Vec<RoomInfo>> {
        self.inner.get_room_infos(room_load_settings).await
    }

    async fn get_users_with_display_name(&self, room_id: &RoomId, display_name: &DisplayName) -> Result<BTreeSet<OwnedUserId>> {
        self.inner.get_users_with_display_name(room_id, display_name).await
    }

    async fn get_users_with_display_names<'a>(
        &self,
        room_id: &RoomId,
        display_names: &'a [DisplayName],
    ) -> Result<HashMap<&'a DisplayName, BTreeSet<OwnedUserId>>> {
        self.inner.get_users_with_display_names(room_id, display_names).await
    }

    async fn get_account_data_event(&self, event_type: GlobalAccountDataEventType) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.inner.get_account_data_event(event_type).await
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.inner.get_room_account_data_event(room_id, event_type).await
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        receipt_thread: &ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        self.inner.get_user_room_receipt_event(room_id, receipt_type, receipt_thread, user_id).await
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        receipt_thread: &ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        self.inner.get_event_room_receipt_events(room_id, receipt_type, receipt_thread, event_id).await
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner.get_custom_value(key).await
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let (k, v) = (b64(key), b64(&value));
        let old = self.inner.set_custom_value(key, value).await?;
        self.with_mirror(|m| {
            m.global.custom.insert(k, v);
            self.write_global(m);
        });
        Ok(old)
    }

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let k = b64(key);
        let old = self.inner.remove_custom_value(key).await?;
        self.with_mirror(|m| {
            if m.global.custom.remove(&k).is_some() {
                self.write_global(m);
            }
        });
        Ok(old)
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        self.inner.remove_room(room_id).await?;
        self.with_mirror(|m| m.rooms.remove(room_id));
        self.disk.remove(&room_key(room_id));
        self.disk.remove(&info_key(room_id));
        Ok(())
    }

    async fn save_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: OwnedTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        request: QueuedRequestKind,
        priority: usize,
    ) -> Result<()> {
        self.inner.save_send_queue_request(room_id, transaction_id, created_at, request, priority).await
    }

    async fn update_send_queue_request(&self, room_id: &RoomId, transaction_id: &TransactionId, content: QueuedRequestKind) -> Result<bool> {
        self.inner.update_send_queue_request(room_id, transaction_id, content).await
    }

    async fn remove_send_queue_request(&self, room_id: &RoomId, transaction_id: &TransactionId) -> Result<bool> {
        self.inner.remove_send_queue_request(room_id, transaction_id).await
    }

    async fn load_send_queue_requests(&self, room_id: &RoomId) -> Result<Vec<QueuedRequest>> {
        self.inner.load_send_queue_requests(room_id).await
    }

    async fn update_send_queue_request_status(&self, room_id: &RoomId, transaction_id: &TransactionId, error: Option<QueueWedgeError>) -> Result<()> {
        self.inner.update_send_queue_request_status(room_id, transaction_id, error).await
    }

    async fn load_rooms_with_unsent_requests(&self) -> Result<Vec<OwnedRoomId>> {
        self.inner.load_rooms_with_unsent_requests().await
    }

    async fn save_dependent_queued_request(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        own_txn_id: ChildTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        content: DependentQueuedRequestKind,
    ) -> Result<()> {
        self.inner.save_dependent_queued_request(room_id, parent_txn_id, own_txn_id, created_at, content).await
    }

    async fn mark_dependent_queued_requests_as_ready(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        sent_parent_key: SentRequestKey,
    ) -> Result<usize> {
        self.inner.mark_dependent_queued_requests_as_ready(room_id, parent_txn_id, sent_parent_key).await
    }

    async fn update_dependent_queued_request(
        &self,
        room_id: &RoomId,
        own_transaction_id: &ChildTransactionId,
        new_content: DependentQueuedRequestKind,
    ) -> Result<bool> {
        self.inner.update_dependent_queued_request(room_id, own_transaction_id, new_content).await
    }

    async fn remove_dependent_queued_request(&self, room: &RoomId, own_txn_id: &ChildTransactionId) -> Result<bool> {
        self.inner.remove_dependent_queued_request(room, own_txn_id).await
    }

    async fn load_dependent_queued_requests(&self, room: &RoomId) -> Result<Vec<DependentQueuedRequest>> {
        self.inner.load_dependent_queued_requests(room).await
    }

    async fn upsert_thread_subscriptions(&self, updates: Vec<(&RoomId, &EventId, StoredThreadSubscription)>) -> Result<()> {
        self.inner.upsert_thread_subscriptions(updates).await
    }

    async fn remove_thread_subscription(&self, room: &RoomId, thread_id: &EventId) -> Result<()> {
        self.inner.remove_thread_subscription(room, thread_id).await
    }

    async fn load_thread_subscription(&self, room: &RoomId, thread_id: &EventId) -> Result<Option<StoredThreadSubscription>> {
        self.inner.load_thread_subscription(room, thread_id).await
    }

    async fn get_global_profile(&self, user_id: &UserId) -> Result<Option<UserProfile>> {
        self.inner.get_global_profile(user_id).await
    }

    async fn get_global_profiles<'a>(&self, user_ids: &'a [OwnedUserId]) -> Result<BTreeMap<&'a UserId, UserProfile>> {
        self.inner.get_global_profiles(user_ids).await
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    async fn reopen(&self) -> Result<()> {
        Ok(())
    }

    async fn optimize(&self) -> Result<()> {
        Ok(())
    }

    async fn get_size(&self) -> Result<Option<usize>> {
        Ok(None)
    }
}
