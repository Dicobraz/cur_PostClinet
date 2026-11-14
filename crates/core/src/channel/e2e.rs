//! Переписка со сквозным шифрованием в базе и поиске (ADR 0012, «Решения для владельца» п. 1, решено настройкой):
//! «Настройки → Каналы → Шифрованные беседы» — [`E2eStore`]; ключ политики `ChannelE2EStorePlaintext` (`2`/`1`/`0`)
//! задаёт выбор за пользователя.
//!
//! - **Хранить и искать** ([`E2eStore::Search`], по умолчанию) — как обычные сообщения.
//! - **Хранить, не искать** ([`E2eStore::Store`]) — в базе, документа поиска нет.
//! - **Только в памяти** ([`E2eStore::Memory`]) — в базе строка без текста, цитат и вложений (у вложения шифрованной
//!   комнаты в источнике — ключ файла); текст — в памяти процесса, пока Postman запущен. После перезапуска канал
//!   присылает сообщения заново — они снова в памяти.
//!
//! Смена выбора ([`switch`]): на «не искать» — документы шифрованных бесед уходят из индекса, на «только в памяти» —
//! и текст из базы (строки переписываются без текста, история правок и загруженные части уходят). Обратно — текст,
//! что в памяти, ложится в базу, и (для «искать») в индекс. S/MIME в письмах — своим правилом (ADR 0006 п. 7).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};

use postman_cache::{Cache, MsgKey};
use serde::{Deserialize, Serialize};

use super::{Body, ChannelError, ChannelStore, Message};

/// Где держать расшифрованный текст шифрованных бесед.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum E2eStore {
    /// «Хранить на этом компьютере и искать» (`2`).
    #[default]
    Search,
    /// «Хранить, не искать» (`1`).
    Store,
    /// «Только в памяти» (`0`).
    Memory,
}

impl E2eStore {
    /// Значение ключа политики `ChannelE2EStorePlaintext`: `2`, `1`, `0`; иначе — `None`.
    pub fn from_policy(v: i64) -> Option<Self> {
        match v {
            2 => Some(Self::Search),
            1 => Some(Self::Store),
            0 => Some(Self::Memory),
            _ => None,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Search => 2,
            Self::Store => 1,
            Self::Memory => 0,
        }
    }

    /// Текст шифрованных бесед — в индексе поиска.
    pub fn indexed(self) -> bool {
        self == Self::Search
    }

    /// Текст шифрованных бесед — в базе.
    pub fn stored(self) -> bool {
        self != Self::Memory
    }
}

static MODE: AtomicU8 = AtomicU8::new(2);

/// Действующий выбор процесса (ставит композиционный корень из настройки и политики).
pub fn mode() -> E2eStore {
    match MODE.load(Ordering::Relaxed) {
        1 => E2eStore::Store,
        0 => E2eStore::Memory,
        _ => E2eStore::Search,
    }
}

/// Поставить выбор; базу и индекс под него переписывает [`switch`].
pub fn set_mode(m: E2eStore) {
    MODE.store(m.code(), Ordering::Relaxed);
}

/// Текст сообщений шифрованных бесед в режиме «только в памяти»: ключ строки базы → сообщение целиком.
static MEMORY: LazyLock<Mutex<HashMap<MsgKey, Message>>> = LazyLock::new(Mutex::default);

fn memory() -> std::sync::MutexGuard<'static, HashMap<MsgKey, Message>> {
    MEMORY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Сообщение без текста — строка базы в режиме «только в памяти»: автор, время, отметки, реакции остаются.
pub fn strip(m: &Message) -> Message {
    Message { body: Body::Plain(String::new()), attachments: Vec::new(), quotes: Vec::new(), forwarded: None, ..m.clone() }
}

/// Запомнить текст строки `key` (запись в режиме «только в памяти»).
pub(super) fn keep(key: MsgKey, m: &Message) {
    memory().insert(key, m.clone());
}

/// Строка базы → сообщение с текстом из памяти, если он там есть (реакции и отметки — из базы: они новее).
pub(super) fn recall(key: MsgKey, m: &mut Message) {
    if let Some(full) = memory().get(&key) {
        m.body = full.body.clone();
        m.attachments = full.attachments.clone();
        m.quotes = full.quotes.clone();
        m.forwarded = full.forwarded.clone();
    }
}

/// Строки больше нет (удаление, «Выйти»): забыть текст.
pub(super) fn forget(key: MsgKey) {
    memory().remove(&key);
}

pub(super) fn take(key: MsgKey) -> Option<Message> {
    memory().remove(&key)
}

/// Сколько сообщений сейчас только в памяти (проверки, журнал).
pub fn in_memory() -> usize {
    memory().len()
}

/// Итог смены выбора: сколько сообщений переписано в базе и что сделать с индексом.
#[derive(Debug, Default)]
pub struct Switched {
    pub rewritten: usize,
    /// В индекс заново (выбор «искать»).
    pub reindex: Vec<MsgKey>,
    /// Из индекса.
    pub unindex: Vec<MsgKey>,
}

/// Переписать шифрованные беседы всех учёток каналов под выбор `to` (выбор процесса уже поставлен [`set_mode`]);
/// документы поиска — по [`Switched`] (`index::e2e_switched`). Зовётся в фоне, не в потоке интерфейса.
pub fn switch(cache: &Cache, to: E2eStore) -> Result<Switched, ChannelError> {
    let mut out = Switched::default();
    for store in ChannelStore::all(cache)? {
        let s = store.e2e_rewrite(to)?;
        out.rewritten += s.rewritten;
        out.reindex.extend(s.reindex);
        out.unindex.extend(s.unindex);
    }
    tracing::info!(mode = to.code(), rewritten = out.rewritten, "channel: encrypted conversations follow the setting");
    Ok(out)
}

/// Под какой выбор записана база (`local`): другой при открытии — привести ([`apply`]).
const APPLIED: &str = "channels/e2e-mode";

/// Поставить выбор процесса и привести базу и индекс, если они записаны под другой (смена в настройках, новая
/// политика между запусками). Зовётся в фоне: переписывает строки шифрованных бесед.
pub fn apply(cache: &Cache, index: Option<&postman_search::SearchIndex>, to: E2eStore) -> Result<Switched, ChannelError> {
    set_mode(to);
    let was = cache.local(APPLIED)?.and_then(|d| d.value.first().copied()).and_then(|c| E2eStore::from_policy(i64::from(c))).unwrap_or_default();
    if was == to {
        return Ok(Switched::default());
    }
    let s = switch(cache, to)?;
    if let Some(index) = index {
        super::index::e2e_switched(index, cache, &s)?;
    }
    cache.put_local(&postman_cache::LocalPut { key: APPLIED.into(), value: vec![to.code()], parts: Vec::new() })?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{AccountId, ChannelEvent, ChannelId, Conversation, ConversationId, MessageId, ParticipantId, index};
    use postman_search::{Scope, SearchIndex};

    fn msg(id: &str, sent: i64, text: &str) -> Message {
        Message {
            id: MessageId::new(id),
            author: ParticipantId::new("@anna:example.test"),
            sent,
            body: Body::Plain(text.into()),
            ..Message::default()
        }
    }

    /// Смена выбора: «искать» → «не искать» (документы ушли) → «только в памяти» (в базе без текста, текст — в
    /// памяти; новое — сразу так) → «искать» (текст из памяти — в базу и в индекс). Нешифрованные — как были.
    #[test]
    fn switching_follows_the_setting() {
        let cache = Cache::open_in_memory().unwrap();
        let store = ChannelStore::open(cache.clone(), &ChannelId::new("matrix"), &AccountId::new("@me:example.test"), "Я").unwrap();
        let index = SearchIndex::in_memory().unwrap();
        let conv = |id: &str, encrypted: bool| Conversation { id: ConversationId::new(id), title: id.into(), encrypted, ..Conversation::default() };
        let ev = |c: &str, m: Vec<Message>| ChannelEvent::Messages { conversation: ConversationId::new(c), messages: m, live: false };
        set_mode(E2eStore::Search);
        let a = store
            .apply(vec![
                ChannelEvent::Conversations(vec![conv("!secret", true), conv("!open", false)]),
                ev("!secret", vec![msg("$1", 1_000, "секретное слово")]),
                ev("!open", vec![msg("$2", 2_000, "открытое слово")]),
            ])
            .unwrap();
        index::applied(&index, &store, &a);
        let hits = |w: &str| index.hits_in(w, &Scope::default(), 10).unwrap().len();
        assert_eq!((hits("секретное"), hits("открытое")), (1, 1));
        let text = |c: &str, id: &str| {
            store.message(&ConversationId::new(c), &MessageId::new(id)).unwrap().and_then(|e| e.message().map(|m| m.body.plain()))
        };

        set_mode(E2eStore::Store);
        let s = switch(&cache, E2eStore::Store).unwrap();
        index::e2e_switched(&index, &cache, &s).unwrap();
        assert_eq!((hits("секретное"), hits("открытое")), (0, 1), "«не искать» — шифрованные ушли из индекса");
        assert_eq!(text("!secret", "$1").as_deref(), Some("секретное слово"));

        set_mode(E2eStore::Memory);
        let s = switch(&cache, E2eStore::Memory).unwrap();
        index::e2e_switched(&index, &cache, &s).unwrap();
        assert_eq!(s.rewritten, 1);
        assert_eq!(text("!secret", "$1").as_deref(), Some("секретное слово"), "пока запущен — из памяти");
        let a = store.apply(vec![ev("!secret", vec![msg("$3", 3_000, "второе тайное")])]).unwrap();
        index::applied(&index, &store, &a);
        assert_eq!(hits("тайное"), 0);
        assert_eq!(in_memory(), 2);
        // «Перезапуск»: памяти нет — в базе только строка без текста.
        let saved: Vec<(MsgKey, Message)> = memory().drain().collect();
        assert_eq!(text("!secret", "$1").as_deref(), Some(""), "в базе — без текста");
        assert_eq!(text("!secret", "$3").as_deref(), Some(""));
        assert_eq!(text("!open", "$2").as_deref(), Some("открытое слово"));
        memory().extend(saved);

        set_mode(E2eStore::Search);
        let s = switch(&cache, E2eStore::Search).unwrap();
        index::e2e_switched(&index, &cache, &s).unwrap();
        memory().clear();
        assert_eq!(text("!secret", "$3").as_deref(), Some("второе тайное"), "текст из памяти — снова в базе");
        assert_eq!((hits("секретное"), hits("тайное")), (1, 1), "и в индексе");
    }

    #[test]
    fn codes_and_policy() {
        for m in [E2eStore::Search, E2eStore::Store, E2eStore::Memory] {
            assert_eq!(E2eStore::from_policy(i64::from(m.code())), Some(m));
        }
        assert_eq!(E2eStore::from_policy(3), None);
        assert!(E2eStore::Search.indexed() && !E2eStore::Store.indexed() && E2eStore::Store.stored() && !E2eStore::Memory.stored());
        assert_eq!(E2eStore::default(), E2eStore::Search);
    }
}
