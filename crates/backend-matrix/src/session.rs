//! Подключённая учётка Matrix (`ChannelSession`, ADR 0012 п. 3): sync — догон от курсора (`next_batch`) и долгий
//! опрос своей задачей; события комнат — в модель (`convert`); действия, история, поиск, файлы — запросами SDK.
//!
//! - **Курсор** — `next_batch` sync; ядро хранит его той же транзакцией, что и события до него. Догон от курсора
//!   перезапускает опрос: устаревший опрос своё не шлёт (поколение `epoch`), сервер отдаст то же от курсора ядра.
//! - **Не расшифровалось** — заглушка `Undecryptable` и сырое событие в своих данных (`mx/utd`, до [`UTD_MAX`]);
//!   после каждого sync и после «Восстановить ключи» — повтор: вышло — то же сообщение заново, тот же id.
//! - **Реакции** — по событию на сообщение: свод с сервера (`/relations`, `m.annotation`), запасной — что видели.
//! - **Правка** — сообщение целиком заново с `edited`; правку чужого сообщения (автор правки — не автор) не берём.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::{StoreConfig, SyncSettings};
use matrix_sdk::cross_process_lock::CrossProcessLockConfig;
use matrix_sdk::deserialized_responses::{RawAnySyncOrStrippedState, TimelineEvent, TimelineEventKind};
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::room::{IncludeRelations, MessagesOptions, RelationsOptions};
use matrix_sdk::ruma::api::client::filter::{FilterDefinition, LazyLoadOptions};
use matrix_sdk::ruma::api::client::receipt::create_receipt;
use matrix_sdk::ruma::api::client::search::search_events;
use matrix_sdk::ruma::api::client::sync::sync_events::v3::Filter;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};
use matrix_sdk::ruma::events::relation::{Annotation, InReplyTo, RelationType};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::message::{Relation, ReplacementMetadata, RoomMessageEventContent};
use matrix_sdk::ruma::events::{AnySyncEphemeralRoomEvent, AnyTimelineEvent, MessageLikeEventType, StateEventType};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, UInt};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::sync::{JoinedRoomUpdate, SyncResponse};
use matrix_sdk::{Client, Room, RoomMemberships, SessionMeta, SessionTokens};
use postman_core::backend::BoxFuture;
use postman_core::channel::{
    AccountId, Action, Address, AttachmentKind, AttachmentSource, ByteSink, ByteSource, ChannelError, ChannelEvent, ChannelFeatures, ChannelFolder,
    ChannelId, ChannelSession, ChannelState, ConnectCtx, Conversation, ConversationId, ConversationKind, Counts, Cursor, Done, FileMeta, Flow,
    FlowKind, FolderRef, HISTORY_PAGE, Incoming, Member, Message, MessageId, Page, Participant, ParticipantId, Reaction, Reader, Role, Uploaded,
};
use serde::{Deserialize, Serialize};

use crate::client::{err, http};
use crate::convert::{self, Item};
use crate::store::crypto::Crypto;
use crate::store::disk::{self, Disk};
use crate::store::state::State;

/// Долгий опрос sync.
pub const POLL: Duration = Duration::from_secs(30);
/// Сколько неразобранных сообщений помнить для повтора расшифровки.
pub const UTD_MAX: usize = 500;
const UTD_KEY: &str = "mx/utd";

fn storage(e: impl std::fmt::Display) -> ChannelError {
    ChannelError::Storage(e.to_string())
}

/// Неразобранное сообщение: комната и сырое событие.
#[derive(Clone, Serialize, Deserialize)]
struct Utd {
    id: String,
    room: String,
    raw: String,
}

/// Входящий запрос проверки с другого устройства: от кого и номер потока.
pub(crate) type Incoming2 = Arc<Mutex<Option<(OwnedUserId, String)>>>;

pub(crate) struct Inner {
    pub(crate) client: Client,
    me_id: OwnedUserId,
    me: Participant,
    incoming: Incoming,
    disk: Arc<Disk>,
    epoch: Cell<u64>,
    driver: RefCell<Option<tokio::task::JoinHandle<()>>>,
    lost: Cell<bool>,
    typing: RefCell<HashMap<OwnedRoomId, BTreeSet<OwnedUserId>>>,
    /// Реакция → (сообщение, ключ, кто).
    reactions: RefCell<HashMap<OwnedEventId, (OwnedEventId, String, OwnedUserId)>>,
    people: RefCell<HashSet<OwnedUserId>>,
    utd: RefCell<Vec<Utd>>,
    pub(crate) verify_requests: Incoming2,
}

pub struct MxSession(Rc<Inner>);

impl Drop for MxSession {
    fn drop(&mut self) {
        if let Some(h) = self.0.driver.borrow_mut().take() {
            h.abort();
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn event_id(m: &MessageId) -> Result<OwnedEventId, ChannelError> {
    OwnedEventId::try_from(m.as_str()).map_err(|_| ChannelError::NotFound)
}

/// Настройки sync: последние [`HISTORY_PAGE`] событий комнаты, участники — лениво.
fn settings(token: Option<String>, timeout: Duration) -> SyncSettings {
    let mut f = FilterDefinition::default();
    f.room.timeline.limit = Some(UInt::from(HISTORY_PAGE));
    f.room.state.lazy_load_options = LazyLoadOptions::Enabled { include_redundant_members: false };
    let s = SyncSettings::new().timeout(timeout).filter(Filter::FilterDefinition(f));
    match token {
        Some(t) => s.token(t),
        None => s.token(matrix_sdk::config::SyncToken::NoToken),
    }
}

/// Уровень прав числом: создатель комнаты (версия 12) — выше всех.
fn level_of(l: matrix_sdk::ruma::events::room::power_levels::UserPowerLevel) -> i64 {
    match l {
        matrix_sdk::ruma::events::room::power_levels::UserPowerLevel::Infinite => i64::MAX,
        matrix_sdk::ruma::events::room::power_levels::UserPowerLevel::Int(i) => i64::from(i),
        _ => 0,
    }
}

/// Роль по уровню прав: 100 — владелец, 50+ — администратор, писать нельзя — только чтение.
fn role(level: i64, can_write: bool) -> Role {
    match level {
        l if l >= 100 => Role::Owner,
        l if l >= 50 => Role::Admin,
        _ if !can_write => Role::ReadOnly,
        _ => Role::Member,
    }
}

impl MxSession {
    pub async fn open(s: crate::secret::Session, ctx: ConnectCtx) -> Result<Self, ChannelError> {
        let key = s.key().ok_or_else(|| ChannelError::SignedOut("session without store key".into()))?;
        let (disk, loaded, rx) = Disk::open(&*ctx.store, &key, &s.device_id)?;
        tokio::task::spawn_local(disk::drain(Rc::clone(&ctx.store), rx));
        let state = State::open(Arc::clone(&disk), &loaded).await.map_err(storage)?;
        let crypto = Crypto::open(Arc::clone(&disk), &loaded).await.map_err(storage)?;
        let cfg = StoreConfig::new(CrossProcessLockConfig::SingleProcess).state_store(state).crypto_store(crypto);
        let client = crate::client::build(&s.homeserver, false, Some(cfg)).await?;
        let user = OwnedUserId::try_from(s.user_id.as_str()).map_err(|_| ChannelError::SignedOut("bad user id".into()))?;
        let device = s.device_id.as_str().into();
        let session = MatrixSession {
            meta: SessionMeta { user_id: user.clone(), device_id: device },
            tokens: SessionTokens { access_token: s.access_token.clone(), refresh_token: s.refresh_token.clone() },
        };
        client.matrix_auth().restore_session(session, RoomLoadSettings::default()).await.map_err(err)?;
        // Токен жив? Проверка — первым запросом: отозванный вход — «вход истёк», не обрыв связи.
        let who = client.whoami().await.map_err(|e| http(&e))?;
        if who.user_id != user {
            return Err(ChannelError::SignedOut("token of another account".into()));
        }
        let name = client.account().get_display_name().await.ok().flatten().unwrap_or_else(|| user.localpart().to_owned());
        let me = Participant { id: ParticipantId::new(user.as_str()), name, handle: Some(user.to_string()), ..Participant::default() };
        let utd = loaded.get(UTD_KEY).and_then(|v| serde_json::from_slice(v).ok()).unwrap_or_default();
        let verify_requests: Incoming2 = Arc::default();
        let pending = Arc::clone(&verify_requests);
        let own = user.clone();
        client.add_event_handler(move |ev: matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent| {
            let pending = Arc::clone(&pending);
            let own = own.clone();
            async move {
                if ev.sender == own
                    && let Ok(mut p) = pending.lock()
                {
                    *p = Some((ev.sender, ev.content.transaction_id.to_string()));
                }
            }
        });
        let inner = Inner {
            client,
            me_id: user,
            me,
            incoming: ctx.incoming,
            disk,
            epoch: Cell::new(0),
            driver: RefCell::new(None),
            lost: Cell::new(false),
            typing: RefCell::default(),
            reactions: RefCell::default(),
            people: RefCell::default(),
            utd: RefCell::new(utd),
            verify_requests,
        };
        Ok(Self(Rc::new(inner)))
    }
}

impl Inner {
    fn emit(&self, ev: ChannelEvent) {
        let _ = self.incoming.send(ev);
    }

    fn room(&self, conv: &ConversationId) -> Result<Room, ChannelError> {
        let id = <&RoomId>::try_from(conv.as_str()).map_err(|_| ChannelError::NotFound)?;
        self.client.get_room(id).filter(|r| r.state() == matrix_sdk::RoomState::Joined).ok_or(ChannelError::NotFound)
    }

    /// Пространства: папки и в какие из них входит комната.
    async fn spaces(&self) -> (Vec<ChannelFolder>, HashMap<OwnedRoomId, Vec<FolderRef>>) {
        let mut folders = Vec::new();
        let mut of: HashMap<OwnedRoomId, Vec<FolderRef>> = HashMap::new();
        for space in self.client.joined_rooms().into_iter().filter(|r| r.is_space()) {
            let mut children = Vec::new();
            for raw in space.get_state_events(StateEventType::SpaceChild).await.unwrap_or_default() {
                let RawAnySyncOrStrippedState::Sync(raw) = raw else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.json().get()) else { continue };
                let live = v.pointer("/content/via").and_then(|x| x.as_array()).is_some_and(|a| !a.is_empty());
                if let (true, Some(child)) = (live, v.get("state_key").and_then(|x| x.as_str())) {
                    children.push(child.to_owned());
                }
            }
            let id = FolderRef::new(space.room_id().as_str());
            for c in &children {
                if let Ok(r) = OwnedRoomId::try_from(c.as_str()) {
                    of.entry(r).or_default().push(id.clone());
                }
            }
            let name = space.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| space.room_id().to_string());
            folders.push(ChannelFolder { id, name, parent: None, conversations: children.into_iter().map(ConversationId::new).collect() });
        }
        // Пространство внутри пространства — дерево.
        let parents: HashMap<String, FolderRef> =
            folders.iter().flat_map(|f| f.conversations.iter().map(move |c| (c.0.clone(), f.id.clone()))).collect();
        for f in &mut folders {
            f.parent = parents.get(&f.id.0).cloned();
        }
        (folders, of)
    }

    async fn conversation(&self, room: &Room, folders: &HashMap<OwnedRoomId, Vec<FolderRef>>) -> Conversation {
        let id = room.room_id();
        let direct = room.is_direct().await.unwrap_or(false);
        let title = room.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| id.to_string());
        let (level, can_write) = match room.power_levels().await {
            Ok(pl) => (level_of(pl.for_user(&self.me_id)), pl.user_can_send_message(&self.me_id, MessageLikeEventType::RoomMessage)),
            Err(_) => (0, true),
        };
        let counts = room.unread_notification_counts();
        let last_read = room
            .load_user_receipt(ReceiptType::Read, &ReceiptThread::Unthreaded, &self.me_id)
            .await
            .ok()
            .flatten()
            .map(|(e, _)| MessageId::new(e.as_str()));
        Conversation {
            id: ConversationId::new(id.as_str()),
            kind: if direct { ConversationKind::Direct } else { ConversationKind::Group },
            title,
            avatar: room.avatar_url().map(|u| convert::source(&MediaSource::Plain(u))),
            parent: None,
            folders: folders.get(id).cloned().unwrap_or_default(),
            my_role: role(level, can_write),
            members: u32::try_from(room.joined_members_count()).ok(),
            last_read,
            unread: Some(Counts {
                unread: u32::try_from(counts.notification_count).unwrap_or(u32::MAX),
                mentions: u32::try_from(counts.highlight_count).unwrap_or(u32::MAX),
            }),
            muted_until: None,
            pinned: room.is_favourite(),
            encrypted: room.encryption_state().is_encrypted(),
            extra: Default::default(),
        }
    }

    /// Участники, которых ещё не называли.
    async fn people(&self, room: &Room, ids: impl Iterator<Item = &ParticipantId>) {
        let mut out = Vec::new();
        for id in ids {
            let Ok(user) = OwnedUserId::try_from(id.as_str()) else { continue };
            if self.people.borrow().contains(&user) {
                continue;
            }
            let Ok(Some(m)) = room.get_member_no_sync(&user).await else { continue };
            self.people.borrow_mut().insert(user.clone());
            out.push(Participant {
                id: id.clone(),
                name: m.display_name().map_or_else(|| user.localpart().to_owned(), str::to_owned),
                handle: Some(user.to_string()),
                avatar: m.avatar_url().map(|u| convert::source(&MediaSource::Plain(u.to_owned()))),
                ..Participant::default()
            });
        }
        if !out.is_empty() {
            self.emit(ChannelEvent::People(out));
        }
    }

    /// Реакции сообщения: свод с сервера, запасной — что видели в этой сессии.
    async fn reactions_of(&self, room: &Room, target: &OwnedEventId) -> Vec<Reaction> {
        let opts = RelationsOptions {
            include_relations: IncludeRelations::RelationsOfType(RelationType::Annotation),
            limit: Some(UInt::from(100u32)),
            ..RelationsOptions::default()
        };
        if let Ok(r) = room.relations(target.clone(), opts).await {
            let mut m = self.reactions.borrow_mut();
            m.retain(|_, (t, _, _)| t != target);
            for ev in &r.chunk {
                if let Some(Item::React { id, target: t, key, sender }) = convert::item(ev, &self.me_id) {
                    m.insert(id, (t, key, sender));
                }
            }
        }
        let mut by_key: BTreeMap<String, Reaction> = BTreeMap::new();
        for (t, key, who) in self.reactions.borrow().values() {
            if t != target {
                continue;
            }
            let r = by_key.entry(key.clone()).or_insert_with(|| Reaction { key: key.clone(), count: 0, mine: false, who: Vec::new() });
            let p = ParticipantId::new(who.as_str());
            if !r.who.contains(&p) {
                r.count += 1;
                r.mine |= *who == self.me_id;
                r.who.push(p);
            }
        }
        by_key.into_values().collect()
    }

    /// События ленты комнаты → события канала.
    async fn emit_items(&self, room: &Room, items: Vec<Item>, live: bool) {
        let conv = ConversationId::new(room.room_id().as_str());
        let mut msgs: Vec<Message> = Vec::new();
        let mut reacted: BTreeSet<OwnedEventId> = BTreeSet::new();
        let mut deleted: Vec<MessageId> = Vec::new();
        let mut utd_changed = false;
        let upsert = |msgs: &mut Vec<Message>, m: Message| match msgs.iter_mut().find(|x| x.id == m.id) {
            Some(x) => *x = m,
            None => msgs.push(m),
        };
        for it in items {
            match it {
                Item::Message(m) => upsert(&mut msgs, m),
                Item::Undecryptable { message, raw } => {
                    let mut u = self.utd.borrow_mut();
                    if !u.iter().any(|x| x.id == message.id.0) {
                        u.push(Utd { id: message.id.0.clone(), room: room.room_id().to_string(), raw });
                        let over = u.len().saturating_sub(UTD_MAX);
                        u.drain(..over);
                        utd_changed = true;
                    }
                    upsert(&mut msgs, message);
                }
                Item::Edit { target, at, sender, content } => {
                    let id = MessageId::new(target.as_str());
                    let mut original = msgs.iter().find(|m| m.id == id).cloned();
                    if original.is_none()
                        && let Ok(ev) = room.event(&target, None).await
                        && let Some(Item::Message(m)) = convert::item(&ev, &self.me_id)
                    {
                        original = Some(m);
                    }
                    if let Some(mut m) = original.filter(|m| m.author.0 == sender.as_str()) {
                        convert::apply_edit(&mut m, &content, at);
                        upsert(&mut msgs, m);
                    }
                }
                Item::Redact { redacts } => {
                    let reaction = self.reactions.borrow_mut().remove(&redacts);
                    match reaction {
                        Some((target, _, _)) => {
                            reacted.insert(target);
                        }
                        None => {
                            let id = MessageId::new(redacts.as_str());
                            msgs.retain(|m| m.id != id);
                            deleted.push(id);
                        }
                    }
                }
                Item::React { id, target, key, sender } => {
                    self.reactions.borrow_mut().insert(id, (target.clone(), key, sender));
                    reacted.insert(target);
                }
            }
        }
        if utd_changed {
            self.disk.put_json(UTD_KEY.into(), &*self.utd.borrow());
        }
        let authors: Vec<ParticipantId> = msgs.iter().map(|m| m.author.clone()).collect();
        self.people(room, authors.iter()).await;
        if !msgs.is_empty() {
            self.emit(ChannelEvent::Messages { conversation: conv.clone(), messages: msgs, live });
        }
        if !deleted.is_empty() {
            self.emit(ChannelEvent::Deleted { conversation: Some(conv.clone()), ids: deleted, for_all: true });
        }
        for target in reacted {
            let reactions = self.reactions_of(room, &target).await;
            self.emit(ChannelEvent::Reactions { conversation: conv.clone(), message: MessageId::new(target.as_str()), reactions });
        }
    }

    /// «Прочитано» и «печатает» комнаты.
    fn ephemeral(&self, room: &RoomId, upd: &JoinedRoomUpdate) {
        let conv = ConversationId::new(room.as_str());
        for raw in &upd.ephemeral {
            match raw.deserialize() {
                Ok(AnySyncEphemeralRoomEvent::Receipt(e)) => {
                    let mut latest: BTreeMap<OwnedUserId, (u64, OwnedEventId)> = BTreeMap::new();
                    for (event, receipts) in &e.content.0 {
                        for (kind, users) in receipts {
                            if !matches!(kind, ReceiptType::Read | ReceiptType::ReadPrivate) {
                                continue;
                            }
                            for (user, r) in users {
                                let ts = r.ts.map_or(0, |t| u64::from(t.0));
                                let slot = latest.entry(user.clone()).or_insert((0, event.clone()));
                                if ts >= slot.0 {
                                    *slot = (ts, event.clone());
                                }
                            }
                        }
                    }
                    for (user, (_, event)) in latest {
                        let who = if user == self.me_id { Reader::Me } else { Reader::Them(ParticipantId::new(user.as_str())) };
                        self.emit(ChannelEvent::Read { conversation: conv.clone(), who, up_to: MessageId::new(event.as_str()) });
                    }
                }
                Ok(AnySyncEphemeralRoomEvent::Typing(e)) => {
                    let now: BTreeSet<OwnedUserId> = e.content.user_ids.into_iter().filter(|u| *u != self.me_id).collect();
                    let before = self.typing.borrow_mut().insert(room.to_owned(), now.clone()).unwrap_or_default();
                    for u in now.difference(&before) {
                        self.emit(ChannelEvent::Typing { conversation: conv.clone(), who: ParticipantId::new(u.as_str()), on: true });
                    }
                    for u in before.difference(&now) {
                        self.emit(ChannelEvent::Typing { conversation: conv.clone(), who: ParticipantId::new(u.as_str()), on: false });
                    }
                }
                _ => {}
            }
        }
    }

    /// Ответ sync → события канала. `first` — первый вход: все комнаты и папки.
    async fn on_sync(&self, r: &SyncResponse, first: bool, live: bool) {
        let (folders, of) = self.spaces().await;
        let spaces_changed = first || r.rooms.joined.keys().any(|id| self.client.get_room(id).is_some_and(|x| x.is_space()));
        let rooms: Vec<Room> =
            if first { self.client.joined_rooms() } else { r.rooms.joined.keys().filter_map(|id| self.client.get_room(id)).collect() };
        let mut convs = Vec::new();
        for room in rooms.iter().filter(|r| !r.is_space()) {
            convs.push(self.conversation(room, &of).await);
        }
        if !convs.is_empty() {
            self.emit(ChannelEvent::Conversations(convs));
        }
        if spaces_changed {
            self.emit(ChannelEvent::Folders(folders));
        }
        for (id, upd) in &r.rooms.joined {
            let Some(room) = self.client.get_room(id).filter(|x| !x.is_space()) else { continue };
            let items: Vec<Item> = upd.timeline.events.iter().filter_map(|e| convert::item(e, &self.me_id)).collect();
            self.emit_items(&room, items, live).await;
            if upd.timeline.limited && !first {
                self.emit(ChannelEvent::Gap { conversation: Some(ConversationId::new(id.as_str())) });
            }
            self.ephemeral(id, upd);
        }
        for id in r.rooms.left.keys() {
            self.emit(ChannelEvent::ConversationGone(ConversationId::new(id.as_str())));
        }
        self.retry_utd().await;
    }

    /// Повтор расшифровки: пришёл ключ — то же сообщение заново (тот же id).
    pub(crate) async fn retry_utd(&self) {
        let pending: Vec<Utd> = self.utd.borrow().clone();
        if pending.is_empty() {
            return;
        }
        let mut fixed: BTreeMap<String, Vec<Item>> = BTreeMap::new();
        let mut done: HashSet<String> = HashSet::new();
        for u in pending {
            let Ok(room_id) = <&RoomId>::try_from(u.room.as_str()) else {
                done.insert(u.id);
                continue;
            };
            let Some(room) = self.client.get_room(room_id) else { continue };
            let Ok(raw) = Raw::<OriginalSyncRoomEncryptedEvent>::from_json_string(u.raw.clone()) else {
                done.insert(u.id);
                continue;
            };
            let Ok(ev) = room.decrypt_event(&raw, None).await else { continue };
            if matches!(ev.kind, TimelineEventKind::UnableToDecrypt { .. }) {
                continue;
            }
            done.insert(u.id);
            if let Some(it) = convert::item(&ev, &self.me_id) {
                fixed.entry(u.room).or_default().push(it);
            }
        }
        if done.is_empty() {
            return;
        }
        self.utd.borrow_mut().retain(|u| !done.contains(&u.id));
        self.disk.put_json(UTD_KEY.into(), &*self.utd.borrow());
        for (room, items) in fixed {
            let Some(room) = <&RoomId>::try_from(room.as_str()).ok().and_then(|id| self.client.get_room(id)) else { continue };
            self.emit_items(&room, items, false).await;
        }
    }

    /// Комнаты, где ждут ключей (для загрузки из резервной копии).
    pub(crate) fn utd_rooms(&self) -> BTreeSet<OwnedRoomId> {
        self.utd.borrow().iter().filter_map(|u| OwnedRoomId::try_from(u.room.as_str()).ok()).collect()
    }

    fn lost_link(&self, e: ChannelError) {
        self.lost.set(true);
        match e {
            ChannelError::SignedOut(why) => self.emit(ChannelEvent::State(ChannelState::Problem(ChannelError::SignedOut(why)))),
            _ => self.emit(ChannelEvent::State(ChannelState::Offline { retry_in: 5 })),
        }
    }

    /// Долгий опрос: ответ — события и курсор; обрыв — `Offline`, вход отозван — `SignedOut`.
    fn start_driver(self: &Rc<Self>, token: String, generation: u64) {
        let weak: Weak<Inner> = Rc::downgrade(self);
        let client = self.client.clone();
        let task = tokio::task::spawn_local(async move {
            let mut token = token;
            loop {
                let r = client.sync_once(settings(Some(token.clone()), POLL)).await;
                let Some(me) = weak.upgrade() else { return };
                if me.epoch.get() != generation {
                    return;
                }
                match r {
                    Ok(r) => {
                        me.on_sync(&r, false, true).await;
                        if me.epoch.get() != generation {
                            return;
                        }
                        token = r.next_batch.clone();
                        me.emit(ChannelEvent::Cursor(Cursor(token.clone().into_bytes())));
                    }
                    Err(e) => {
                        let e = err(e);
                        tracing::info!("matrix: sync stopped: {e}");
                        match e {
                            ChannelError::Wait { secs } => tokio::time::sleep(Duration::from_secs(u64::from(secs))).await,
                            e => return me.lost_link(e),
                        }
                    }
                }
            }
        });
        if let Some(old) = self.driver.borrow_mut().replace(task) {
            old.abort();
        }
    }

    async fn catch_up(self: &Rc<Self>, cursor: Option<Cursor>) -> Result<(), ChannelError> {
        if self.lost.get() {
            return Err(ChannelError::Network("link lost".into()));
        }
        let generation = self.epoch.get() + 1;
        self.epoch.set(generation);
        if let Some(h) = self.driver.borrow_mut().take() {
            h.abort();
        }
        let first = cursor.is_none();
        let token = cursor.and_then(|c| String::from_utf8(c.0).ok()).filter(|t| !t.is_empty());
        let r = self.client.sync_once(settings(token, Duration::ZERO)).await.map_err(err)?;
        self.on_sync(&r, first, !first).await;
        self.emit(ChannelEvent::Cursor(Cursor(r.next_batch.clone().into_bytes())));
        self.start_driver(r.next_batch, generation);
        Ok(())
    }

    /// Сообщения страницы истории или поиска: правки — к сообщениям страницы, служебное без строк — мимо.
    async fn page(&self, room: &Room, events: Vec<TimelineEvent>) -> Vec<Message> {
        let mut msgs: Vec<Message> = Vec::new();
        let mut edits = Vec::new();
        for ev in &events {
            match convert::item(ev, &self.me_id) {
                Some(Item::Message(m)) => msgs.push(m),
                Some(Item::Undecryptable { message, raw }) => {
                    let mut u = self.utd.borrow_mut();
                    if !u.iter().any(|x| x.id == message.id.0) {
                        u.push(Utd { id: message.id.0.clone(), room: room.room_id().to_string(), raw });
                    }
                    msgs.push(message);
                }
                Some(Item::Edit { target, at, sender, content }) => edits.push((target, at, sender, content)),
                Some(Item::React { id, target, key, sender }) => {
                    self.reactions.borrow_mut().insert(id, (target, key, sender));
                }
                _ => {}
            }
        }
        edits.sort_by_key(|e| e.1);
        for (target, at, sender, content) in edits {
            if let Some(m) = msgs.iter_mut().find(|m| m.id.0 == target.as_str() && m.author.0 == sender.as_str()) {
                convert::apply_edit(m, &content, at);
            }
        }
        let mut seen = HashSet::new();
        msgs.retain(|m| seen.insert(m.id.clone()));
        msgs.sort_by_key(|m| m.sent);
        let authors: Vec<ParticipantId> = msgs.iter().map(|m| m.author.clone()).collect();
        self.people(room, authors.iter()).await;
        msgs
    }

    async fn history(&self, conv: &ConversationId, before: Option<&MessageId>, limit: u32) -> Result<Page, ChannelError> {
        let room = self.room(conv)?;
        let from = match before {
            Some(m) => {
                let id = event_id(m)?;
                let ctx = room.event_with_context(&id, false, UInt::from(0u32), None).await.map_err(err)?;
                match ctx.prev_batch_token {
                    Some(t) => Some(t),
                    None => return Ok(Page::default()),
                }
            }
            None => None,
        };
        let mut opts = MessagesOptions::backward();
        opts.from = from;
        opts.limit = UInt::from(limit.max(1));
        let r = room.messages(opts).await.map_err(err)?;
        let more = r.end.is_some() && !r.chunk.is_empty();
        let mut msgs = self.page(&room, r.chunk).await;
        if let Some(b) = before {
            msgs.retain(|m| m.id != *b);
        }
        Ok(Page { messages: msgs.into_iter().map(|m| (conv.clone(), m)).collect(), more })
    }

    async fn search(&self, query: &str, conv: Option<&ConversationId>, limit: u32) -> Result<Page, ChannelError> {
        let mut filter = matrix_sdk::ruma::api::client::filter::RoomEventFilter::default();
        filter.limit = Some(UInt::from(limit.max(1)));
        if let Some(c) = conv {
            let room = self.room(c)?;
            filter.rooms = Some(vec![room.room_id().to_owned()]);
        }
        let mut criteria = search_events::v3::Criteria::new(query.to_owned());
        criteria.filter = filter;
        let mut categories = search_events::v3::Categories::new();
        categories.room_events = Some(criteria);
        let resp = self.client.send(search_events::v3::Request::new(categories)).await.map_err(|e| http(&e))?;
        let results = resp.search_categories.room_events;
        let more = results.next_batch.is_some();
        let mut by_room: BTreeMap<OwnedRoomId, Vec<TimelineEvent>> = BTreeMap::new();
        for r in results.results {
            let Some(raw) = r.result else { continue };
            let Ok(room_id) = raw.get_field::<OwnedRoomId>("room_id") else { continue };
            let Some(room_id) = room_id else { continue };
            let raw: Raw<AnyTimelineEvent> = raw;
            by_room.entry(room_id).or_default().push(TimelineEvent::from_plaintext(raw.cast_unchecked()));
        }
        let mut out = Vec::new();
        for (room_id, events) in by_room {
            let Some(room) = self.client.get_room(&room_id) else { continue };
            let conv = ConversationId::new(room_id.as_str());
            out.extend(self.page(&room, events).await.into_iter().map(|m| (conv.clone(), m)));
        }
        out.truncate(limit as usize);
        Ok(Page { messages: out, more })
    }

    async fn act(&self, action: Action) -> Result<Done, ChannelError> {
        if !crate::features().allows(&action) {
            return Err(ChannelError::Unsupported);
        }
        match action {
            Action::Send(o) => {
                let room = self.room(&o.conversation)?;
                let txn = format!("pm{}", o.client_id);
                let reply = o.reply_to.as_ref().map(event_id).transpose()?;
                let with_reply = |mut c: RoomMessageEventContent| {
                    if let Some(r) = &reply {
                        c.relates_to = Some(Relation::Reply(matrix_sdk::ruma::events::relation::Reply::new(InReplyTo::new(r.clone()))));
                    }
                    c
                };
                let mut first: Option<OwnedEventId> = None;
                if o.attachments.is_empty() {
                    let c = with_reply(convert::content(&o.body, &o.quotes, &o.mentions));
                    let r = room.send(c).with_transaction_id(OwnedTransactionId::from(txn)).await.map_err(err)?;
                    first = Some(r.response.event_id);
                } else {
                    let caption = convert::text(&o.body, &o.quotes);
                    for (n, up) in o.attachments.iter().enumerate() {
                        let src = convert::media(&up.source).ok_or(ChannelError::NotFound)?;
                        let c = convert::file_content(up, src, (n == 0).then(|| caption.clone()));
                        let c = if n == 0 { with_reply(c) } else { c };
                        let t = if n == 0 { txn.clone() } else { format!("{txn}-{n}") };
                        let r = room.send(c).with_transaction_id(OwnedTransactionId::from(t)).await.map_err(err)?;
                        first.get_or_insert(r.response.event_id);
                    }
                }
                let id = first.ok_or_else(|| ChannelError::Protocol("nothing sent".into()))?;
                Ok(Done::Sent { client_id: o.client_id, id: MessageId::new(id.as_str()), at: now_ms() })
            }
            Action::Edit { conversation, message, body, mentions } => {
                let room = self.room(&conversation)?;
                let target = event_id(&message)?;
                let new = convert::edit_content(&body, &mentions);
                let c = RoomMessageEventContent::from(new).make_replacement(ReplacementMetadata::new(target, None));
                room.send(c).await.map_err(err)?;
                Ok(Done::Ok)
            }
            Action::Delete { conversation, ids, for_all } => {
                let room = self.room(&conversation)?;
                if !for_all {
                    // Удалить у себя Matrix не умеет: только в базе профиля.
                    self.emit(ChannelEvent::Deleted { conversation: Some(conversation), ids, for_all: false });
                    return Ok(Done::Ok);
                }
                for id in &ids {
                    room.redact(&event_id(id)?, None, None).await.map_err(|e| http(&e))?;
                }
                Ok(Done::Ok)
            }
            Action::React { conversation, message, key, on } => {
                let room = self.room(&conversation)?;
                let target = event_id(&message)?;
                if on {
                    room.send(ReactionEventContent::new(Annotation::new(target, key))).await.map_err(err)?;
                } else {
                    self.reactions_of(&room, &target).await;
                    let mine: Vec<OwnedEventId> = self
                        .reactions
                        .borrow()
                        .iter()
                        .filter(|(_, (t, k, who))| *t == target && *k == key && *who == self.me_id)
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in mine {
                        room.redact(&id, None, None).await.map_err(|e| http(&e))?;
                    }
                }
                Ok(Done::Ok)
            }
            Action::MarkRead { conversation, up_to } => {
                let room = self.room(&conversation)?;
                room.send_single_receipt(create_receipt::v3::ReceiptType::Read, ReceiptThread::Unthreaded, event_id(&up_to)?).await.map_err(err)?;
                Ok(Done::Ok)
            }
            Action::Typing { conversation, on } => {
                let room = self.room(&conversation)?;
                room.typing_notice(on).await.map_err(err)?;
                Ok(Done::Ok)
            }
            Action::Pin { conversation, on } => {
                let room = self.room(&conversation)?;
                room.set_is_favourite(on, None).await.map_err(err)?;
                Ok(Done::Ok)
            }
            Action::Start { with } => {
                let users: Vec<OwnedUserId> = with.iter().filter_map(|p| OwnedUserId::try_from(p.as_str()).ok()).collect();
                let room = match users.as_slice() {
                    [] => return Err(ChannelError::NotFound),
                    [one] => self.client.create_dm(one).await.map_err(err)?,
                    many => {
                        let mut req = matrix_sdk::ruma::api::client::room::create_room::v3::Request::new();
                        req.invite = many.to_vec();
                        req.preset = Some(matrix_sdk::ruma::api::client::room::create_room::v3::RoomPreset::PrivateChat);
                        self.client.create_room(req).await.map_err(err)?
                    }
                };
                Ok(Done::Conversation(ConversationId::new(room.room_id().as_str())))
            }
            Action::Members { conversation } => {
                let room = self.room(&conversation)?;
                let pl = room.power_levels().await.ok();
                let members = room.members(RoomMemberships::JOIN).await.map_err(err)?;
                Ok(Done::Members(
                    members
                        .iter()
                        .map(|m| {
                            let (level, write) = pl.as_ref().map_or((0, true), |pl| {
                                (level_of(pl.for_user(m.user_id())), pl.user_can_send_message(m.user_id(), MessageLikeEventType::RoomMessage))
                            });
                            Member { id: ParticipantId::new(m.user_id().as_str()), role: role(level, write) }
                        })
                        .collect(),
                ))
            }
            Action::Resolve { link } => {
                let l = crate::links::parse(&link).ok_or(ChannelError::NotFound)?;
                let room: OwnedRoomId = match &l.target {
                    crate::links::Target::Room(id) => OwnedRoomId::try_from(id.as_str()).map_err(|_| ChannelError::NotFound)?,
                    crate::links::Target::Alias(a) => {
                        let alias = matrix_sdk::ruma::OwnedRoomAliasId::try_from(a.as_str()).map_err(|_| ChannelError::NotFound)?;
                        self.client.resolve_room_alias(&alias).await.map_err(|e| http(&e))?.room_id
                    }
                    crate::links::Target::User(u) => {
                        let user = OwnedUserId::try_from(u.as_str()).map_err(|_| ChannelError::NotFound)?;
                        self.client.get_dm_room(&user).ok_or(ChannelError::NotFound)?.room_id().to_owned()
                    }
                };
                let (channel, account) = (ChannelId::new(crate::ID), AccountId::new(self.me_id.as_str()));
                let conv = ConversationId::new(room.as_str());
                Ok(Done::Address(match l.event {
                    Some(e) => Address::message(&channel, &account, &conv, &MessageId::new(e)),
                    None => Address::conversation(&channel, &account, &conv),
                }))
            }
            // Пересылки сервером, заглушить, черновики на сервере, довесок (`Action::Extra`) — не у Matrix.
            _ => Err(ChannelError::Unsupported),
        }
    }

    async fn download(&self, src: &AttachmentSource, out: &mut dyn ByteSink) -> Result<(), ChannelError> {
        let source = convert::media(src).ok_or(ChannelError::NotFound)?;
        let req = MediaRequestParameters { source, format: MediaFormat::File };
        let bytes = self.client.media().get_media_content(&req, false).await.map_err(err)?;
        for chunk in bytes.chunks(64 * 1024) {
            out.write(chunk)?;
        }
        Ok(())
    }

    async fn upload(&self, meta: &FileMeta, src: &mut dyn ByteSource) -> Result<Uploaded, ChannelError> {
        let mut bytes = Vec::with_capacity(usize::try_from(src.len()).unwrap_or(0));
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        // Всегда шифром (`EncryptedFile`): беседа станет известна только при отправке, а открытый файл на сервере
        // для шифрованной комнаты — утечка.
        let mut reader = std::io::Cursor::new(&bytes);
        let file = self.client.upload_encrypted_file(&mut reader).await.map_err(err)?;
        let kind = if meta.mime.starts_with("image/") { AttachmentKind::Image { w: 0, h: 0 } } else { AttachmentKind::File };
        Ok(Uploaded {
            name: meta.name.clone(),
            mime: meta.mime.clone(),
            size: bytes.len() as u64,
            kind,
            source: convert::source(&MediaSource::Encrypted(Box::new(file))),
        })
    }
}

impl ChannelSession for MxSession {
    fn me(&self) -> Participant {
        self.0.me.clone()
    }

    fn features(&self) -> ChannelFeatures {
        crate::features()
    }

    fn catch_up<'a>(&'a self, cursor: Option<Cursor>) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(self.0.catch_up(cursor))
    }

    fn history<'a>(&'a self, conv: &'a ConversationId, before: Option<&'a MessageId>, limit: u32) -> BoxFuture<'a, Result<Page, ChannelError>> {
        Box::pin(self.0.history(conv, before, limit))
    }

    fn search<'a>(&'a self, query: &'a str, conv: Option<&'a ConversationId>, limit: u32) -> BoxFuture<'a, Result<Page, ChannelError>> {
        Box::pin(self.0.search(query, conv, limit))
    }

    fn act<'a>(&'a self, action: Action) -> BoxFuture<'a, Result<Done, ChannelError>> {
        Box::pin(self.0.act(action))
    }

    fn download<'a>(&'a self, src: &'a AttachmentSource, out: &'a mut dyn ByteSink) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(self.0.download(src, out))
    }

    fn upload<'a>(&'a self, meta: &'a FileMeta, src: &'a mut dyn ByteSource) -> BoxFuture<'a, Result<Uploaded, ChannelError>> {
        Box::pin(self.0.upload(meta, src))
    }

    fn flow<'a>(&'a self, kind: FlowKind) -> BoxFuture<'a, Result<Box<dyn Flow + 'a>, ChannelError>> {
        Box::pin(async move {
            let flow: Box<dyn Flow + 'a> = match kind {
                FlowKind::Verify => Box::new(crate::verify::Verify::new(Rc::clone(&self.0))),
                FlowKind::Recovery => Box::new(crate::verify::Recovery::new(Rc::clone(&self.0))),
            };
            Ok(flow)
        })
    }

    fn sign_out<'a>(&'a self) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async move {
            self.0.epoch.set(self.0.epoch.get() + 1);
            if let Some(h) = self.0.driver.borrow_mut().take() {
                h.abort();
            }
            self.0.client.matrix_auth().logout().await.map_err(|e| http(&e))?;
            Ok(())
        })
    }
}
