//! Лента чата в области чтения (CH-04, DESIGN «Каналы»): шапка беседы, лента языком ленты беседы (`tape.rs`, макет
//! `ThreadChat`) и поле ввода внизу.
//!
//! - Шапка: аватар 40, название, «Канал · вид · участники», справа — заглушить, закрепить.
//! - Лента: линия времени по дням, аватар 24, имя и время, сообщения подряд одного автора — без шапки; ответ — цитата
//!   с чертой и автором; «изменено» после времени; «прочитано» — ✓✓ акцентом; реакции плашками (свои — с контуром
//!   акцента, «+» — добавить); «Новые сообщения»; «… печатает» под лентой; упоминание меня — акцентом на голубой
//!   подложке. Действия при наведении — плашка справа: реакция, ответить, изменить (свои), ещё.
//! - Поле ввода: Enter — отправить, Shift+Enter — строка; над ним «Ответ …» или «Изменение сообщения» с ✕ (Esc).
//!
//! Всё, что зависит от канала, — по `ChannelFeatures` (`chats::shown`): нет у сервера — нет на экране. Чтение базы —
//! в фоне; запросы учётке уходят событиями экрану почты (`PaneEvent`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollbarMode;
use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;
use pets_ui::Level as Toast;
use pets_ui::theme::Metric;
use postman_cache::Cache;
use postman_core::channel::smart;
use postman_core::channel::{
    Action, Address, AttachmentKind, Body, ChanAccount, ChannelError, ChannelFeatures, ChannelStore, Chats, ConversationId, ConversationKind,
    ConversationSource, Line, Message, MessageId, MessageKind, Outgoing, Participant, ParticipantId, Reactions, Request, ServiceKind, Summary,
    TYPING_SECS, outbox,
};
use postman_ir::{Block, Inline, Marks};

use crate::chats::{self, Shown};
use crate::chrome::{glyph, icon};
use crate::ir_view;
use crate::roles::{Look, Pm, look};
use crate::tape;

/// Сообщений ленты за одно чтение базы; «Показать более ранние» добавляет столько же.
const PAGE: usize = 120;
/// Сообщения одного автора подряд — без шапки, если между ними не больше.
const GROUP_MS: i64 = 5 * 60_000;
/// «Печатает» учётке — не чаще.
const TYPING_EVERY: Duration = Duration::from_secs(4);
/// Реакции по умолчанию, когда канал принимает любые.
const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];
/// Эмодзи для вставки в текст.
const EMOJI: [&str; 16] = ["🙂", "😀", "😂", "😉", "😍", "🤔", "😢", "😮", "👍", "👎", "👏", "🙏", "🔥", "🎉", "❤️", "✅"];
/// Ширина колонки текста сообщения для таблиц и картинок IR.
const COLUMN: f32 = 560.;

pub(crate) enum PaneEvent {
    /// Запрос учётке канала.
    Request(ChanAccount, Request),
    /// Действие учётке (номер запроса ставит экран почты).
    Act(ChanAccount, Action),
    Toast(Toast, String),
    /// Заглушить беседу или вернуть звук.
    Mute(Address, bool),
    /// Закрепить или открепить.
    Pin(Address, bool),
    /// Текст — в буфер обмена.
    Copy(String),
    /// Действие довеска (кнопка бота, голос): ответ канала вернётся в [`ChatPane::extra_reply`].
    Extra(ChanAccount, Action),
    /// Выгрузить файл в канал перед отправкой: итог вернётся в [`ChatPane::uploaded`].
    Upload(ChanAccount, postman_core::channel::FileMeta, Vec<u8>),
}

impl EventEmitter<PaneEvent> for ChatPane {}

/// Беседа из базы профиля.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Loaded {
    pub(crate) summary: Summary,
    pub(crate) muted: bool,
    pub(crate) pinned: bool,
    pub(crate) lines: Vec<Line>,
    /// В базе есть сообщения раньше показанных.
    pub(crate) more: bool,
    pub(crate) people: HashMap<ParticipantId, Participant>,
    pub(crate) members: Option<u32>,
    pub(crate) features: ChannelFeatures,
    pub(crate) me: Option<ParticipantId>,
    /// До какого сообщения прочитали другие (по каждому).
    pub(crate) read_by: Vec<(ParticipantId, MessageId)>,
    /// Счётчик непрочитанного на момент чтения.
    pub(crate) unread: u32,
}

/// Прочитать беседу (I/O — только из фона): последние `limit` строк ленты, люди, возможности учётки.
pub(crate) fn load(cache: &Cache, at: &Address, limit: usize, now: i64) -> Result<Option<Loaded>, ChannelError> {
    let Some(store) = ChannelStore::find(cache, &at.channel, &at.account)? else { return Ok(None) };
    let Some(stored) = store.conversation(&at.conversation)? else { return Ok(None) };
    let Some(summary) = Chats::new(cache.clone()).conversation(at)? else { return Ok(None) };
    let mut entries = store.timeline(&at.conversation, None, limit + 1)?;
    let more = entries.len() > limit;
    if more {
        entries.remove(0);
    }
    let features = store.features()?;
    let me = store.me()?.map(|p| p.id);
    let unread = smart::chat_counts(&store, &summary, &features, me.as_ref())?.unread;
    let members = match summary.conversation.members {
        Some(n) => Some(n),
        None => Some(store.members(&at.conversation)?.len() as u32).filter(|n| *n > 0),
    };
    Ok(Some(Loaded {
        muted: smart::is_muted(cache, &summary, now)?,
        pinned: smart::is_pinned(cache, &summary)?,
        lines: entries.into_iter().map(Line::from).collect(),
        more,
        people: store.participants()?.into_iter().map(|p| (p.id.clone(), p)).collect(),
        members,
        features,
        me,
        read_by: stored.read_by,
        unread,
        summary,
    }))
}

/// Что стоит в ленте по порядку.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Piece {
    Day(jiff::civil::Date),
    /// Разделитель «Новые сообщения».
    New,
    /// Строка ленты `index`; `head` — с шапкой (имя и время).
    Msg {
        index: usize,
        head: bool,
    },
}

/// Лента по порядку: разделители дней, «Новые сообщения» перед `new_from`, сообщения подряд одного автора (не дальше
/// [`GROUP_MS`] друг от друга) — без шапки.
pub(crate) fn pieces(lines: &[Line], new_from: Option<&MessageId>, day: &dyn Fn(i64) -> Option<jiff::civil::Date>) -> Vec<Piece> {
    let mut out = Vec::with_capacity(lines.len() + 4);
    let mut last_day = None;
    // Автор и время прошлого обычного сообщения: продолжение группы — без шапки.
    let mut prev: Option<(&ParticipantId, i64)> = None;
    for (index, line) in lines.iter().enumerate() {
        let sent = match line {
            Line::Message { message, .. } => message.sent,
            Line::Deleted { sent, .. } => *sent,
        };
        let d = day(sent);
        if d.is_some() && d != last_day {
            if let Some(d) = d {
                out.push(Piece::Day(d));
            }
            last_day = d;
            prev = None;
        }
        if new_from == Some(line.id()) {
            out.push(Piece::New);
            prev = None;
        }
        let normal = line.message().filter(|m| m.kind == MessageKind::Normal);
        let head = match (normal, prev) {
            (Some(m), Some((author, at))) => *author != m.author || m.sent - at > GROUP_MS,
            _ => true,
        };
        out.push(Piece::Msg { index, head });
        prev = normal.map(|m| (&m.author, m.sent));
    }
    out
}

/// С какого сообщения «Новые сообщения»: первое чужое после моего «прочитано»; сервер его не дал — `unread` последних
/// чужих.
pub(crate) fn new_from(lines: &[Line], last_read: Option<&MessageId>, unread: u32, me: Option<&ParticipantId>) -> Option<MessageId> {
    let theirs = |l: &&Line| l.message().is_some_and(|m| m.kind == MessageKind::Normal && !chats::is_mine(m, me));
    if let Some(at) = last_read.and_then(|id| lines.iter().position(|l| l.id() == id)) {
        return lines[at + 1..].iter().find(theirs).map(|l| l.id().clone());
    }
    if unread == 0 {
        return None;
    }
    lines.iter().rev().filter(theirs).take(unread as usize).last().map(|l| l.id().clone())
}

/// До какой строки ленты дочитал кто-то, кроме меня.
pub(crate) fn read_index(lines: &[Line], read_by: &[(ParticipantId, MessageId)], me: Option<&ParticipantId>) -> Option<usize> {
    read_by.iter().filter(|(who, _)| Some(who) != me).filter_map(|(_, id)| lines.iter().position(|l| l.id() == id)).max()
}

/// Где в тексте упомянут я: `@слово`, с которого начинается моё имя или ник; не нашлось — первое `@слово`.
pub(crate) fn mention_ranges(text: &str, names: &[String]) -> Vec<std::ops::Range<usize>> {
    let mut tokens: Vec<std::ops::Range<usize>> = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match start {
            None if c == '@' => start = Some(i),
            Some(s) if !(c.is_alphanumeric() || c == '_' || c == '.' || c == '-') => {
                if i > s + 1 {
                    tokens.push(s..i);
                }
                start = (c == '@').then_some(i);
            }
            _ => {}
        }
    }
    if let Some(s) = start.filter(|s| text.len() > s + 1) {
        tokens.push(s..text.len());
    }
    let own: Vec<std::ops::Range<usize>> = tokens
        .iter()
        .filter(|r| {
            let word = text[r.start + 1..r.end].to_lowercase();
            names.iter().any(|n| !n.is_empty() && n.to_lowercase().starts_with(&word))
        })
        .cloned()
        .collect();
    if own.is_empty() { tokens.into_iter().take(1).collect() } else { own }
}

/// Текст документа из одних простых абзацев (без ссылок и начертаний) — для подсветки упоминания.
fn simple_text(blocks: &[Block]) -> Option<String> {
    let mut out = Vec::new();
    for b in blocks {
        let Block::Paragraph { content, .. } = b else { return None };
        let mut line = String::new();
        for i in content {
            match i {
                Inline::Text { text, marks } if *marks == Marks::default() => line.push_str(text),
                _ => return None,
            }
        }
        out.push(line);
    }
    Some(out.join("\n"))
}

/// Вид беседы для подписи шапки.
fn kind_text(kind: ConversationKind) -> String {
    tr!(match kind {
        ConversationKind::Direct => "chats-kind-direct",
        ConversationKind::Group => "chats-kind-group",
        ConversationKind::Broadcast => "chats-kind-broadcast",
        ConversationKind::MailThread => "chats-kind-mail",
    })
}

/// Пустое состояние (DESIGN «Каналы»): значок 40 px серым, заголовок, одна фраза.
pub(crate) fn empty_state(g: char, title: String, text: String, l: &Look) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .px(px(24.))
        .child(icon(g, 40.).text_color(l.text3()))
        .child(div().text_size(l.px(Metric::TypeSubtitle)).font_weight(FontWeight::SEMIBOLD).text_color(l.text()).child(title))
        .child(div().max_w(px(360.)).text_center().text_color(l.text2()).child(text))
}

/// Открытая беседа.
struct Open {
    at: Address,
    data: Option<Loaded>,
    /// Прочитать не больше строк ленты.
    limit: usize,
    /// «Новые сообщения» — перед этим сообщением (считается при открытии, дальше не двигается).
    new_from: Option<MessageId>,
    /// «Прочитано» уже отправлено до этого сообщения.
    marked: Option<MessageId>,
    loading: bool,
    again: bool,
}

pub(crate) struct ChatPane {
    cache: Cache,
    /// «Медиа беседы» открытой беседы (MED-02): режим вместо ленты.
    media: Entity<crate::thread_media::MediaView>,
    open: Option<Open>,
    input: Option<(Entity<TextareaState>, Subscription)>,
    focus_input: bool,
    reply_to: Option<MessageId>,
    editing: Option<MessageId>,
    /// Кто печатает и когда это пришло.
    typing: Vec<(ParticipantId, Instant)>,
    typing_sent: Option<Instant>,
    /// Строка выбора реакции и строка «ещё» — под этим сообщением.
    reacting: Option<MessageId>,
    more: Option<MessageId>,
    /// «Удалить сообщение?» — строка над полем ввода: сообщение и «у всех».
    deleting: Option<(MessageId, bool)>,
    /// Эмодзи над полем ввода.
    emoji: bool,
    /// Вложения следующего сообщения: уже выгруженные в канал и сколько ещё выгружается.
    files: Vec<postman_core::channel::Uploaded>,
    uploading: usize,
    /// Выбор в опросе с несколькими ответами, пока не нажато «Проголосовать»: сообщение и номера вариантов.
    picks: Option<(MessageId, Vec<usize>)>,
    /// Ответ бота предупреждением — строкой над полем ввода, до «ОК».
    alert: Option<String>,
    /// Ссылка, которую предложил канал: показана строкой, открывает её только нажатие.
    offered: Option<String>,
    scroll: ScrollHandle,
    to_bottom: bool,
    /// Текст поля при следующей отрисовке: очистить или подставить (правка, эмодзи).
    clear_text: bool,
    set_text: Option<String>,
    client_seq: u64,
}

impl ChatPane {
    pub(crate) fn new(cache: Cache, media: Entity<crate::thread_media::MediaView>) -> Self {
        Self {
            cache,
            media,
            open: None,
            input: None,
            focus_input: false,
            reply_to: None,
            editing: None,
            typing: Vec::new(),
            typing_sent: None,
            reacting: None,
            more: None,
            deleting: None,
            emoji: false,
            files: Vec::new(),
            uploading: 0,
            picks: None,
            alert: None,
            offered: None,
            scroll: ScrollHandle::new(),
            to_bottom: false,
            clear_text: false,
            set_text: None,
            client_seq: 0,
        }
    }

    pub(crate) fn address(&self) -> Option<&Address> {
        self.open.as_ref().map(|o| &o.at)
    }

    /// Беседа прочитана из базы.
    pub(crate) fn ready(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.data.is_some())
    }

    pub(crate) fn replying(&self) -> bool {
        self.reply_to.is_some()
    }

    /// Открытая беседа: адрес, заглушена ли, закреплена ли (команды беседы).
    pub(crate) fn state(&self) -> Option<(Address, bool, bool)> {
        self.data().map(|d| (d.summary.address.clone(), d.muted, d.pinned))
    }

    /// «Прочитано»: до последнего сообщения сервера.
    pub(crate) fn mark_read(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &mut self.open else { return };
        let last = o.data.iter().flat_map(|d| d.lines.iter().rev()).map(Line::id).find(|id| !outbox::is_pending(id)).cloned();
        if let Some(up_to) = last {
            o.marked = Some(up_to.clone());
            cx.emit(PaneEvent::Act(o.at.chan(), Action::MarkRead { conversation: o.at.conversation.clone(), up_to }));
        }
    }

    /// «Ответить»: фокус — в поле ввода; писать нельзя (рассылка) — подсказка.
    pub(crate) fn focus_reply(&mut self, cx: &mut Context<Self>) {
        let write = self.data().is_some_and(|d| d.summary.conversation.my_role.can_write());
        if write {
            self.focus_input = true;
            cx.notify();
        } else {
            cx.emit(PaneEvent::Toast(Toast::Info, tr!("chats-read-only")));
        }
    }

    fn reset_input_state(&mut self) {
        self.reply_to = None;
        self.editing = None;
        self.reacting = None;
        self.more = None;
        self.deleting = None;
        self.emoji = false;
        self.files.clear();
        self.uploading = 0;
        self.picks = None;
        self.alert = None;
        self.offered = None;
        self.typing.clear();
        self.typing_sent = None;
    }

    pub(crate) fn open(&mut self, at: Address, cx: &mut Context<Self>) {
        if self.address() == Some(&at) {
            return;
        }
        self.reset_input_state();
        self.clear_text = true;
        self.media.update(cx, |m, cx| m.set_source(Some(crate::thread_media::Source::Chat(at.clone())), cx));
        self.open = Some(Open { at, data: None, limit: PAGE, new_from: None, marked: None, loading: false, again: false });
        self.reload(cx);
        cx.notify();
    }

    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        self.reset_input_state();
        self.open = None;
        self.media.update(cx, |m, cx| m.set_source(None, cx));
        cx.notify();
    }

    /// Перечитать открытую беседу из базы (в фоне).
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &mut self.open else { return };
        if o.loading {
            o.again = true;
            return;
        }
        o.loading = true;
        let (cache, at, limit) = (self.cache.clone(), o.at.clone(), o.limit);
        let task = cx.background_spawn({
            let at = at.clone();
            async move { load(&cache, &at, limit, chats::now_ms()) }
        });
        cx.spawn(async move |this, cx| {
            let loaded = task.await;
            let _ = this.update(cx, |p, cx| p.loaded(at, loaded, cx));
        })
        .detach();
    }

    fn loaded(&mut self, at: Address, loaded: Result<Option<Loaded>, ChannelError>, cx: &mut Context<Self>) {
        let Some(o) = self.open.as_mut().filter(|o| o.at == at) else { return };
        o.loading = false;
        let again = std::mem::take(&mut o.again);
        match loaded {
            Ok(Some(data)) => {
                let first = o.data.is_none();
                let grew = o.data.as_ref().is_some_and(|d| d.lines.last().map(Line::id) != data.lines.last().map(Line::id));
                if first {
                    o.new_from = new_from(&data.lines, data.summary.conversation.last_read.as_ref(), data.unread, data.me.as_ref());
                }
                // «Прочитано» — до последнего сообщения сервера, один раз на сообщение.
                let last = data.lines.iter().rev().map(Line::id).find(|id| !outbox::is_pending(id)).cloned();
                let unread = data.unread > 0 || data.summary.conversation.last_read != last;
                if let Some(up_to) = last.filter(|id| unread && o.marked.as_ref() != Some(id)) {
                    o.marked = Some(up_to.clone());
                    cx.emit(PaneEvent::Act(at.chan(), Action::MarkRead { conversation: at.conversation.clone(), up_to }));
                }
                o.data = Some(data);
                if first || grew {
                    self.to_bottom = true;
                }
                // Новые сообщения — «Медиа беседы» собрать заново.
                if grew {
                    self.media.update(cx, |m, cx| m.reload(cx));
                }
            }
            // Беседы больше нет (вышли, удалили).
            Ok(None) => {
                self.close(cx);
                return;
            }
            Err(e) => tracing::warn!("chat: conversation not read: {e}"),
        }
        if again {
            self.reload(cx);
        }
        cx.notify();
    }

    /// Беседы учётки изменились: открытая — перечитать, пропавшая — закрыть.
    pub(crate) fn changed(&mut self, account: &ChanAccount, conversations: &[ConversationId], gone: &[ConversationId], cx: &mut Context<Self>) {
        let Some(at) = self.address().filter(|at| at.chan() == *account) else { return };
        if gone.contains(&at.conversation) {
            self.close(cx);
        } else if conversations.contains(&at.conversation) {
            self.reload(cx);
        }
    }

    /// «Печатает» от канала: держится [`TYPING_SECS`] секунд.
    pub(crate) fn typing(&mut self, account: &ChanAccount, conversation: &ConversationId, who: &ParticipantId, on: bool, cx: &mut Context<Self>) {
        if !self.address().is_some_and(|at| at.chan() == *account && at.conversation == *conversation) {
            return;
        }
        self.typing.retain(|(p, _)| p != who);
        if on {
            self.typing.push((who.clone(), Instant::now()));
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(TYPING_SECS)).await;
                let _ = this.update(cx, |p, cx| {
                    p.typing.retain(|(_, at)| at.elapsed() < Duration::from_secs(TYPING_SECS));
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Esc: строка удаления, выбор реакции и «ещё», эмодзи, затем ответ и правка.
    pub(crate) fn escape(&mut self, cx: &mut Context<Self>) -> bool {
        let had = self.alert.take().is_some() || self.offered.take().is_some();
        let had =
            had || self.deleting.take().is_some() || self.reacting.take().is_some() || self.more.take().is_some() || std::mem::take(&mut self.emoji);
        let had = had || self.cancel_draft();
        if had {
            cx.notify();
        }
        had
    }

    fn cancel_draft(&mut self) -> bool {
        let edit = self.editing.take().is_some();
        if edit {
            self.clear_text = true;
        }
        self.reply_to.take().is_some() || edit
    }

    /// Сцена снимков: «Ответить» на первое чужое сообщение.
    pub(crate) fn screen_reply_first(&mut self, cx: &mut Context<Self>) {
        let first =
            self.data().and_then(|d| d.lines.iter().filter_map(Line::message).find(|m| !chats::is_mine(m, d.me.as_ref())).map(|m| m.id.clone()));
        if let Some(id) = first {
            self.reply_to = Some(id);
            cx.notify();
        }
    }

    fn data(&self) -> Option<&Loaded> {
        self.open.as_ref().and_then(|o| o.data.as_ref())
    }

    fn name_of(&self, d: &Loaded, id: &ParticipantId) -> String {
        if d.me.as_ref() == Some(id) {
            return tr!("thread-me");
        }
        d.people.get(id).map(|p| p.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| id.0.clone())
    }

    /// Поле ввода — один раз; сброс текста при смене беседы; фокус по запросу.
    fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_none() {
            let state = cx.new(|cx| TextareaState::new(window, cx).submit_on_enter(true).auto_grow(1, 6).placeholder(tr!("chats-input-placeholder")));
            let sub = cx.subscribe_in(&state, window, |this: &mut ChatPane, state, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                InputEvent::Change => {
                    let empty = state.read(cx).value().trim().is_empty();
                    this.tell_typing(!empty, cx);
                }
                _ => {}
            });
            self.input = Some((state, sub));
        }
        let Some((state, _)) = &self.input else { return };
        if std::mem::take(&mut self.clear_text) {
            state.update(cx, |s, cx| s.set_value("", window, cx));
        }
        if let Some(text) = self.set_text.take() {
            state.update(cx, |s, cx| s.set_value(text, window, cx));
        }
        if std::mem::take(&mut self.focus_input) {
            state.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    /// «Печатает» учётке — только где канал это умеет, не чаще [`TYPING_EVERY`].
    fn tell_typing(&mut self, on: bool, cx: &mut Context<Self>) {
        let Some(o) = &self.open else { return };
        if !o.data.as_ref().is_some_and(|d| d.features.typing) || self.editing.is_some() {
            return;
        }
        let due = self.typing_sent.is_none_or(|t| t.elapsed() >= TYPING_EVERY);
        if on && due {
            self.typing_sent = Some(Instant::now());
            cx.emit(PaneEvent::Act(o.at.chan(), Action::Typing { conversation: o.at.conversation.clone(), on: true }));
        } else if !on && self.typing_sent.take().is_some() {
            cx.emit(PaneEvent::Act(o.at.chan(), Action::Typing { conversation: o.at.conversation.clone(), on: false }));
        }
    }

    /// Enter и «Отправить»: правка — действием, новое сообщение — через очередь отправки (окно «Отменить» — из
    /// настроек ответов).
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((state, _)) = &self.input else { return };
        let text = state.read(cx).value().trim().to_string();
        let Some(o) = &self.open else { return };
        let Some(d) = &o.data else { return };
        // Пустое сообщение не уходит; вложения без текста — уходят.
        if text.is_empty() && (self.files.is_empty() || self.editing.is_some()) {
            return;
        }
        if self.uploading > 0 {
            return cx.emit(PaneEvent::Toast(Toast::Info, tr!("chats-wait-upload")));
        }
        if let Some(max) = d.features.max_text.filter(|max| text.chars().count() > *max as usize) {
            return cx.emit(PaneEvent::Toast(Toast::Warning, tr!("chats-too-long", max = i64::from(max))));
        }
        let (chan, conversation) = (o.at.chan(), o.at.conversation.clone());
        // Упомянутые: участники, чьё `@имя` или `@ник` есть в тексте.
        let lower = text.to_lowercase();
        let mentions: Vec<ParticipantId> = if d.features.mentions {
            d.people
                .values()
                .filter(|p| d.me.as_ref() != Some(&p.id))
                .filter(|p| {
                    let by = |s: &str| !s.is_empty() && lower.contains(&format!("@{}", s.to_lowercase()));
                    by(&p.name) || p.handle.as_deref().is_some_and(by)
                })
                .map(|p| p.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        let body = postman_core::channel::text_doc(&text);
        if let Some(message) = self.editing.take() {
            cx.emit(PaneEvent::Act(chan, Action::Edit { conversation, message, body, mentions }));
        } else {
            self.client_seq += 1;
            let client_id = ((chats::now_ms() as u64) << 12) | (self.client_seq & 0xfff);
            let outgoing = Outgoing {
                conversation,
                client_id,
                body,
                attachments: std::mem::take(&mut self.files),
                reply_to: self.reply_to.take(),
                quotes: Vec::new(),
                mentions,
                at: None,
                silent: false,
            };
            let release = postman_core::send_later::undo_release(crate::prefs::get(cx).replies.undo());
            cx.emit(PaneEvent::Request(chan, Request::Send { outgoing, release }));
        }
        self.typing_sent = None;
        self.emoji = false;
        state.update(cx, |s, cx| s.set_value("", window, cx));
        self.to_bottom = true;
        cx.notify();
    }

    /// «Вложить»: выбор файлов; читаются в фоне и выгружаются в канал, затем — плашками над полем.
    fn pick_files(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |p, cx| p.attach(paths, cx));
            }
        })
        .detach();
    }

    fn attach(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let Some(o) = &self.open else { return };
        let (chan, max) = (o.at.chan(), o.data.as_ref().and_then(|d| d.features.max_attachment));
        let task = cx.background_spawn(async move {
            paths
                .into_iter()
                .map(|p| {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    (postman_core::files::safe_name(&name), std::fs::read(&p))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let files = task.await;
            let _ = this.update(cx, |p, cx| {
                // Беседу сменили, пока файлы читались, — им некуда.
                if p.address().map(Address::chan) != Some(chan.clone()) {
                    return;
                }
                for (name, data) in files {
                    match data {
                        Ok(bytes) if max.is_some_and(|m| bytes.len() as u64 > m) => {
                            cx.emit(PaneEvent::Toast(Toast::Warning, tr!("chats-file-too-large", name = name)));
                        }
                        Ok(bytes) => {
                            let meta =
                                postman_core::channel::FileMeta { mime: postman_core::files::mime_for(&name).into(), size: bytes.len() as u64, name };
                            p.uploading += 1;
                            cx.emit(PaneEvent::Upload(chan.clone(), meta, bytes));
                        }
                        Err(e) => {
                            tracing::warn!("chat: file not read: {e}");
                            cx.emit(PaneEvent::Toast(Toast::Error, tr!("chats-file-unread", name = name)));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Файл выгружен в канал (или канал отказал): плашкой над полем, уйдёт со следующим сообщением.
    pub(crate) fn uploaded(&mut self, result: Result<postman_core::channel::Uploaded, ChannelError>, cx: &mut Context<Self>) {
        self.uploading = self.uploading.saturating_sub(1);
        match result {
            Ok(file) => self.files.push(file),
            Err(e) => cx.emit(PaneEvent::Toast(Toast::Error, tr!("chats-upload-failed", why = chats::error_text(&e)))),
        }
        cx.notify();
    }

    /// Плашки вложений над полем: имя, размер, ✕; пока файлы выгружаются — подпись.
    fn files_row(&self, l: &Look, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if self.files.is_empty() && self.uploading == 0 {
            return None;
        }
        let chips = self.files.iter().enumerate().map(|(i, f)| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(24.))
                .pl(px(8.))
                .pr(px(2.))
                .rounded(px(4.))
                .border_1()
                .border_color(l.border())
                .text_size(px(12.))
                .child(icon(glyph::ATTACH, 12.).text_color(l.text2()))
                .child(div().max_w(px(200.)).truncate().child(f.name.clone()))
                .child(div().text_color(l.text2()).child(crate::attachments::size_text(f.size)))
                .child(self.tool(("chat-file-remove", i), glyph::CLOSE, tr!("chats-file-remove"), l).size(px(20.)).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if i < this.files.len() {
                            this.files.remove(i);
                        }
                        cx.notify();
                    },
                )))
        });
        Some(div().id("chat-files").flex().flex_wrap().items_center().gap(px(6.)).children(chips).when(self.uploading > 0, |d| {
            d.child(div().text_size(px(12.)).text_color(l.text2()).child(tr!("chats-uploading", n = self.uploading as i64)))
        }))
    }

    fn act(&self, action: impl FnOnce(ConversationId) -> Action, cx: &mut Context<Self>) {
        if let Some(o) = &self.open {
            cx.emit(PaneEvent::Act(o.at.chan(), action(o.at.conversation.clone())));
        }
    }

    fn react(&mut self, message: MessageId, key: String, on: bool, cx: &mut Context<Self>) {
        self.reacting = None;
        self.act(|conversation| Action::React { conversation, message, key, on }, cx);
        cx.notify();
    }

    fn start_edit(&mut self, m: &Message, cx: &mut Context<Self>) {
        self.reply_to = None;
        self.more = None;
        self.editing = Some(m.id.clone());
        self.set_text = Some(m.body.plain());
        self.focus_input = true;
        cx.notify();
    }

    fn start_reply(&mut self, id: MessageId, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.clear_text = true;
        }
        self.reply_to = Some(id);
        self.focus_input = true;
        cx.notify();
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        if let Some((id, for_all)) = self.deleting.take() {
            self.act(|conversation| Action::Delete { conversation, ids: vec![id], for_all }, cx);
        }
        cx.notify();
    }

    fn load_earlier(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &mut self.open else { return };
        let Some(d) = &o.data else { return };
        if d.more {
            // В базе есть ещё — показать больше.
            o.limit += PAGE;
        } else if let Some(first) = d.lines.first() {
            // В базе больше нет — попросить историю у сервера (найденное придёт событием `Changed`).
            o.limit += PAGE;
            let request = Request::History {
                conversation: o.at.conversation.clone(),
                before: Some(first.id().clone()),
                limit: postman_core::channel::HISTORY_PAGE,
            };
            cx.emit(PaneEvent::Request(o.at.chan(), request));
        }
        self.reload(cx);
    }

    // --- отрисовка ---

    fn tool(&self, id: impl Into<ElementId>, g: char, tip: String, l: &Look) -> Stateful<Div> {
        self.tool_with(id, icon(g, 13.), tip, l)
    }

    /// Кнопка-значок 26 px с подсказкой; содержимое — значок или знак эмодзи.
    fn tool_with(&self, id: impl Into<ElementId>, content: impl IntoElement, tip: String, l: &Look) -> Stateful<Div> {
        div()
            .id(id)
            .size(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(l.px(Metric::RadiusControl))
            .cursor_pointer()
            .text_color(l.text2())
            .hover(|d| d.bg(l.hover()))
            .child(content)
            .tooltip(move |window, cx| crate::screens::tip(tip.clone(), window, cx))
    }

    fn header(&self, d: &Loaded, l: &Look, cx: &mut Context<Self>) -> Div {
        let c = &d.summary.conversation;
        let mut sub = vec![tape::channel_name(&d.summary.address.channel.0), kind_text(c.kind)];
        if let Some(n) = d.members.filter(|_| c.kind != ConversationKind::Direct) {
            sub.push(tr!("chats-members", n = i64::from(n)));
        }
        if c.encrypted {
            sub.push(tr!("chats-encrypted"));
        }
        let at = d.summary.address.clone();
        let (muted, pinned) = (d.muted, d.pinned);
        let toggle = |id: &'static str, g: char, on: bool, tip: String, l: &Look| {
            self.tool(id, g, tip, l)
                .children(crate::screens::spot(id))
                .size(px(30.))
                .when(on, |d| d.bg(l.pm(Pm::Secondary)).text_color(l.pm(Pm::SecondaryFg)))
        };
        let mute = toggle("chat-mute", glyph::MUTED, muted, tr!(if muted { "chats-unmute" } else { "chats-mute" }), l).on_click({
            let at = at.clone();
            cx.listener(move |_, _, _, cx| cx.emit(PaneEvent::Mute(at.clone(), !muted)))
        });
        let pin = toggle("chat-pin", glyph::PIN, pinned, tr!(if pinned { "chats-unpin" } else { "chats-pin" }), l)
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(PaneEvent::Pin(at.clone(), !pinned))));
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(24.))
            .pt(px(16.))
            .pb(px(12.))
            .border_b_1()
            .border_color(l.border())
            .child(tape::avatar(&c.title, 40., 15., l))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_size(l.px(Metric::TypeSubtitle)).font_weight(FontWeight::SEMIBOLD).child(c.title.clone()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(l.px(Metric::TypeCaption))
                            .text_color(l.text2())
                            .child(tape::channel_icon(&d.summary.address.channel.0, 12., l))
                            .child(div().truncate().child(sub.join(" · "))),
                    ),
            )
            .child(mute)
            .child(pin)
    }

    /// Цитата ответа: черта 2 px, автор и начало текста.
    fn quote(&self, author: Option<String>, text: String, l: &Look) -> Div {
        div()
            .pl(px(8.))
            .border_l_2()
            .border_color(l.stroke())
            .text_size(px(12.))
            .children(author.map(|a| div().font_weight(FontWeight::SEMIBOLD).child(a)))
            .child(div().text_color(l.text2()).truncate().child(text))
    }

    fn body(&self, n: usize, m: &Message, mentioned: bool, d: &Loaded, l: &Look, cx: &mut Context<Self>) -> AnyElement {
        let doc;
        let blocks: &[Block] = match &m.body {
            Body::Ir(d) => &d.blocks,
            Body::Plain(s) | Body::Html { plain: s, .. } => {
                doc = postman_core::channel::text_doc(s);
                &doc.blocks
            }
        };
        if blocks.is_empty() {
            return div().into_any_element();
        }
        // Упоминание меня — акцентом на голубой подложке (в простом тексте; в тексте с разметкой — как есть).
        if mentioned && let Some(text) = simple_text(blocks) {
            let me = d.me.as_ref().and_then(|id| d.people.get(id));
            let names: Vec<String> = me.iter().flat_map(|p| [Some(p.name.clone()), p.handle.clone()]).flatten().collect();
            let style = HighlightStyle { color: Some(l.pm(Pm::SecondaryFg)), background_color: Some(l.pm(Pm::Secondary)), ..Default::default() };
            let marks = mention_ranges(&text, &names).into_iter().map(|r| (r, style));
            return div().child(StyledText::new(text.clone()).with_highlights(marks)).into_any_element();
        }
        let images = ir_view::Images::new();
        div().flex().flex_col().gap_1().children(ir_view::blocks(blocks, &format!("chat{n}"), &images, px(COLUMN), cx)).into_any_element()
    }

    /// Вложения: картинки и видео — плитками альбома, остальное — плашками (скрепка, имя, размер).
    fn attachments(&self, m: &Message, l: &Look) -> Option<Div> {
        if m.attachments.is_empty() {
            return None;
        }
        let media = |k: &AttachmentKind| matches!(k, AttachmentKind::Image { .. } | AttachmentKind::Video { .. } | AttachmentKind::Sticker);
        let tiles: Vec<Div> = m
            .attachments
            .iter()
            .filter(|a| media(&a.kind))
            .map(|a| {
                let video = matches!(a.kind, AttachmentKind::Video { .. });
                div()
                    .w(px(120.))
                    .h(px(90.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(4.))
                    .rounded(px(6.))
                    .bg(l.fill())
                    .text_size(px(11.))
                    .text_color(l.text2())
                    .child(icon(if video { glyph::PREVIEW } else { glyph::PICTURE }, 20.))
                    .child(div().max_w(px(108.)).truncate().child(a.name.clone()))
            })
            .collect();
        let files: Vec<(String, u64)> = m
            .attachments
            .iter()
            .filter(|a| !media(&a.kind))
            .map(|a| {
                let name = match a.kind {
                    AttachmentKind::Voice { .. } if a.name.is_empty() => tr!("chats-att-voice"),
                    AttachmentKind::Contact if a.name.is_empty() => tr!("chats-att-contact"),
                    _ => a.name.clone(),
                };
                (name, a.size.unwrap_or(0))
            })
            .collect();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .when(!tiles.is_empty(), |d| d.child(div().flex().flex_wrap().gap(px(4.)).pt_1().children(tiles)))
                .when(!files.is_empty(), |d| d.child(tape::att_chips(&files, l))),
        )
    }

    fn reactions(&self, n: usize, m: &Message, can: bool, l: &Look, cx: &mut Context<Self>) -> Option<Div> {
        if m.reactions.is_empty() {
            return None;
        }
        let chip = |l: &Look| {
            div().h(px(22.)).px(px(7.)).flex().items_center().gap(px(4.)).rounded(px(11.)).border_1().border_color(l.border()).text_size(px(12.))
        };
        let mut row = div().flex().flex_wrap().gap(px(4.)).pt_1();
        for (k, r) in m.reactions.iter().enumerate() {
            let (id, key, mine) = (m.id.clone(), r.key.clone(), r.mine);
            let el = chip(l)
                .id(SharedString::from(format!("chat-reaction-{n}-{k}")))
                .when(mine, |d| d.border_color(l.accent()).bg(l.pm(Pm::Secondary)))
                .child(r.key.clone())
                .child(div().text_color(l.text2()).child(r.count.to_string()))
                .when(can, |d| d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.react(id.clone(), key.clone(), !mine, cx))));
            row = row.child(el);
        }
        if can {
            let id = m.id.clone();
            row = row.child(
                chip(l)
                    .id(("chat-reaction-add", n))
                    .cursor_pointer()
                    .text_color(l.text2())
                    .hover(|d| d.bg(l.hover()))
                    .child("+")
                    .tooltip(|window, cx| crate::screens::tip(tr!("chats-react"), window, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.reacting = (this.reacting.as_ref() != Some(&id)).then(|| id.clone());
                        this.more = None;
                        cx.notify();
                    })),
            );
        }
        Some(row)
    }

    /// Строка выбора реакции под сообщением: что принимает канал.
    fn reaction_picker(&self, n: usize, m: &Message, d: &Loaded, l: &Look, cx: &mut Context<Self>) -> Div {
        let keys: Vec<String> = match &d.features.reactions {
            Reactions::Fixed(keys) => keys.clone(),
            _ => QUICK_REACTIONS.iter().map(|k| (*k).to_owned()).collect(),
        };
        div().flex().flex_wrap().gap(px(2.)).pt_1().children(keys.into_iter().enumerate().map(|(k, key)| {
            let id = m.id.clone();
            let mine = m.reactions.iter().any(|r| r.mine && r.key == key);
            div()
                .id(SharedString::from(format!("chat-pick-{n}-{k}")))
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(l.px(Metric::RadiusControl))
                .cursor_pointer()
                .hover(|d| d.bg(l.hover()))
                .child(key.clone())
                .on_click(cx.listener(move |this, _, _, cx| this.react(id.clone(), key.clone(), !mine, cx)))
        }))
    }

    /// Строка «ещё» под сообщением: копировать, удалить у себя, удалить у всех (если канал умеет и срок не прошёл).
    fn more_strip(&self, n: usize, m: &Message, s: Shown, l: &Look, cx: &mut Context<Self>) -> Div {
        let item = |k: &'static str, l: &Look| crate::buttons::secondary((k, n), l).h(px(26.)).px(px(8.)).text_size(px(12.)).child(tr!(k));
        let text = m.body.plain();
        let (own, all) = (m.id.clone(), m.id.clone());
        div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .pt_1()
            .child(item("chats-copy", l).on_click(cx.listener(move |this, _, _, cx| {
                this.more = None;
                cx.emit(PaneEvent::Copy(text.clone()));
                cx.notify();
            })))
            .child(item("chats-delete-own", l).on_click(cx.listener(move |this, _, _, cx| {
                this.more = None;
                this.deleting = Some((own.clone(), false));
                cx.notify();
            })))
            .when(s.delete_for_all, |d| {
                d.child(item("chats-delete-all", l).on_click(cx.listener(move |this, _, _, cx| {
                    this.more = None;
                    this.deleting = Some((all.clone(), true));
                    cx.notify();
                })))
            })
    }

    /// Плашка действий при наведении: реакция, ответить, изменить (свои), ещё.
    fn plate(&self, n: usize, m: &Message, s: Shown, group: SharedString, l: &Look, cx: &mut Context<Self>) -> Div {
        let (react, reply, more) = (m.id.clone(), m.id.clone(), m.id.clone());
        let edit = m.clone();
        div()
            .absolute()
            .top(px(-10.))
            .right(px(8.))
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded(l.px(Metric::RadiusControl))
            .border_1()
            .border_color(l.border())
            .bg(l.panel())
            .invisible()
            .group_hover(group, |s| s.visible())
            .when(s.react, |d| {
                d.child(self.tool_with(("chat-do-react", n), div().text_size(px(13.)).child("🙂"), tr!("chats-react"), l).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.reacting = (this.reacting.as_ref() != Some(&react)).then(|| react.clone());
                        this.more = None;
                        cx.notify();
                    },
                )))
            })
            .when(s.reply, |d| {
                d.child(
                    self.tool(("chat-do-reply", n), glyph::REPLY, tr!("chats-reply"), l)
                        .on_click(cx.listener(move |this, _, _, cx| this.start_reply(reply.clone(), cx))),
                )
            })
            .when(s.edit, |d| {
                d.child(
                    self.tool(("chat-do-edit", n), glyph::EDIT, tr!("chats-edit"), l)
                        .on_click(cx.listener(move |this, _, _, cx| this.start_edit(&edit, cx))),
                )
            })
            .child(self.tool(("chat-do-more", n), glyph::MORE, tr!("chats-more"), l).on_click(cx.listener(move |this, _, _, cx| {
                this.more = (this.more.as_ref() != Some(&more)).then(|| more.clone());
                this.reacting = None;
                cx.notify();
            })))
    }

    fn service(&self, text: String, l: &Look) -> AnyElement {
        div().flex().justify_center().py(px(2.)).text_size(px(12.)).text_color(l.text2()).child(text).into_any_element()
    }

    fn line(&self, n: usize, head: bool, read_up_to: Option<usize>, d: &Loaded, l: &Look, cx: &mut Context<Self>) -> AnyElement {
        let Some(line) = d.lines.get(n) else { return div().into_any_element() };
        let m = match line {
            Line::Deleted { .. } => return self.service(tr!("chats-deleted"), l),
            Line::Message { message, .. } => message,
        };
        match &m.kind {
            MessageKind::Service(kind) => {
                let who = |p: &ParticipantId| self.name_of(d, p);
                let text = match kind {
                    ServiceKind::Created => tr!("chats-service-created"),
                    ServiceKind::Joined(p) => tr!("chats-service-joined", who = who(p)),
                    ServiceKind::Left(p) => tr!("chats-service-left", who = who(p)),
                    ServiceKind::Renamed(title) => tr!("chats-service-renamed", title = title.clone()),
                    ServiceKind::Pinned(_) => tr!("chats-service-pinned"),
                    ServiceKind::Other(text) => text.clone(),
                };
                return self.service(text, l);
            }
            MessageKind::Undecryptable => return self.service(tr!("chats-undecryptable"), l),
            MessageKind::Normal => {}
        }
        let mine = chats::is_mine(m, d.me.as_ref());
        let s = chats::shown(&d.features, d.summary.conversation.my_role, Some(m), chats::now_ms());
        let pending = chats::pending_client(&m.id);
        let name = self.name_of(d, &m.author);
        let mentioned = !mine && d.me.as_ref().is_some_and(|me| m.mentions.contains(me));
        // Отметки после времени: «изменено», у моих — «отправляется» с «Отменить», ✓ отправлено, ✓✓ прочитано.
        let read = mine && s.receipts && read_up_to.is_some_and(|i| n <= i);
        let mut meta = div().flex().items_center().gap(px(6.)).text_size(px(12.)).text_color(l.text2()).child(tape::clock(m.sent));
        if m.edited.is_some() {
            meta = meta.child(tr!("chats-edited"));
        }
        if let Some(client_id) = pending {
            meta = meta.child(tr!("chats-sending")).child(
                div().id(("chat-unsend", n)).cursor_pointer().text_color(l.accent_text()).child(tr!("chats-unsend")).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(o) = &this.open {
                            cx.emit(PaneEvent::Request(o.at.chan(), Request::Unsend { client_id }));
                        }
                    },
                )),
            );
        } else if mine && s.receipts {
            meta = meta.child(div().text_color(if read { l.accent_text() } else { l.text3() }).child(if read { "✓✓" } else { "✓" }));
        }
        let head_row = head.then(|| div().flex().items_center().gap(px(8.)).child(div().font_weight(FontWeight::SEMIBOLD).child(name.clone())));
        // Без шапки отметки видны только там, где есть что сказать: правка, очередь, моё «прочитано».
        let meta_row = (head || m.edited.is_some() || pending.is_some() || (mine && s.receipts)).then_some(meta);
        let forwarded =
            m.forwarded.as_ref().map(|o| div().text_size(px(12.)).text_color(l.text2()).child(tr!("chats-forwarded", who = o.name.clone())));
        // Ответ: цитата сообщения-источника; цитаты фрагментов — своим текстом.
        let source = |id: &MessageId| d.lines.iter().filter_map(Line::message).find(|x| x.id == *id);
        let mut quotes: Vec<Div> = Vec::new();
        if m.quotes.is_empty()
            && let Some(id) = &m.reply_to
        {
            match source(id) {
                Some(src) => quotes.push(self.quote(Some(self.name_of(d, &src.author)), chats::preview(&src.body.plain()), l)),
                None => quotes.push(self.quote(None, tr!("chats-reply-gone"), l)),
            }
        }
        for q in &m.quotes {
            let author = q.source.as_ref().or(m.reply_to.as_ref()).and_then(source).map(|src| self.name_of(d, &src.author));
            quotes.push(self.quote(author, q.text.clone(), l));
        }
        let group = SharedString::from(format!("chat-msg-{n}"));
        let card = div()
            .flex()
            .flex_col()
            .gap_1()
            .max_w(relative(0.85))
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(l.border())
            .bg(if mine { l.picked() } else { l.panel() })
            .child(div().flex().items_center().gap(px(8.)).children(head_row).children(meta_row))
            .children(forwarded)
            .children(quotes)
            .child(self.body(n, m, mentioned, d, l, cx))
            .children(self.attachments(m, l))
            .children(self.extra(n, m, pending.is_none(), d, l, cx))
            .children(self.reactions(n, m, s.react, l, cx))
            .when(self.reacting.as_ref() == Some(&m.id) && s.react, |c| c.child(self.reaction_picker(n, m, d, l, cx)))
            .when(self.more.as_ref() == Some(&m.id), |c| c.child(self.more_strip(n, m, s, l, cx)));
        // Аватар 24 — у первого сообщения группы чужого автора; дальше — отступ на его ширину.
        let face = (!mine).then(|| if head { tape::avatar(&name, 24., 10., l).mt_1() } else { div().w(px(24.)).flex_none() });
        div()
            .id(("chat-msg", n))
            .children(crate::screens::spot(("chat-msg", n)))
            .group(group.clone())
            .relative()
            .flex()
            .gap_2()
            .items_start()
            .w_full()
            .when(mine, |d| d.justify_end())
            .when(pending.is_none(), |row| row.child(self.plate(n, m, s, group, l, cx)))
            .children(face)
            .child(card)
            .into_any_element()
    }

    /// «Анна печатает…», «Анна и Борис печатают…».
    fn typing_line(&self, d: &Loaded, l: &Look) -> Option<Div> {
        if !d.features.typing || self.typing.is_empty() {
            return None;
        }
        let names: Vec<String> = self.typing.iter().map(|(p, _)| self.name_of(d, p)).collect();
        let text = match names.as_slice() {
            [one] => tr!("chats-typing-one", who = one.clone()),
            many => tr!("chats-typing-many", who = many.join(", ")),
        };
        Some(div().flex_none().px(px(24.)).pb(px(4.)).text_size(px(12.)).text_color(l.text2()).child(text))
    }

    /// Строка над полем ввода: «Ответ …» или «Изменение сообщения», черта акцента, ✕.
    fn draft_bar(&self, d: &Loaded, l: &Look, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let (title, text) = if let Some(id) = &self.editing {
            let m = d.lines.iter().filter_map(Line::message).find(|m| m.id == *id)?;
            (tr!("chats-editing"), m.body.plain())
        } else {
            let id = self.reply_to.as_ref()?;
            let m = d.lines.iter().filter_map(Line::message).find(|m| m.id == *id)?;
            (tr!("chats-reply-to", who = self.name_of(d, &m.author)), m.body.plain())
        };
        Some(
            div()
                .id("chat-draft-bar")
                .children(crate::screens::spot("chat-draft-bar"))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pl(px(8.))
                        .border_l_2()
                        .border_color(l.accent())
                        .text_size(px(12.))
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(l.accent_text()).child(title))
                        .child(div().truncate().text_color(l.text2()).child(chats::preview(&text))),
                )
                .child(self.tool("chat-draft-cancel", glyph::CLOSE, tr!("chats-cancel"), l).on_click(cx.listener(|this, _, _, cx| {
                    this.cancel_draft();
                    cx.notify();
                }))),
        )
    }

    fn emoji_strip(&self, l: &Look, cx: &mut Context<Self>) -> Div {
        div().flex().flex_wrap().gap(px(2.)).children(EMOJI.iter().enumerate().map(|(k, e)| {
            div()
                .id(("chat-emoji", k))
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(l.px(Metric::RadiusControl))
                .cursor_pointer()
                .hover(|d| d.bg(l.hover()))
                .child(*e)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some((state, _)) = &this.input {
                        let text = format!("{}{e}", state.read(cx).value());
                        this.set_text = Some(text);
                        this.focus_input = true;
                    }
                    cx.notify();
                }))
        }))
    }

    /// Поле ввода: рамка 1 px, текст, эмодзи, «Отправить» акцентом.
    fn composer(&self, d: &Loaded, s: Shown, window: &Window, l: &Look, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !s.write {
            let note = div().flex_none().px(px(24.)).py(px(12.)).text_center().text_size(px(12.)).text_color(l.text2()).child(tr!("chats-read-only"));
            return Some(note.into_any_element());
        }
        let (state, _) = self.input.as_ref()?;
        let focused = state.focus_handle(cx).contains_focused(window, cx);
        let send_tip = tr!(if self.editing.is_some() { "chats-save" } else { "chats-send" });
        let emoji_on = self.emoji;
        let field = div()
            .flex()
            .items_end()
            .gap(px(6.))
            // «Вложить» — у нового сообщения; у правки вложения не меняются.
            .when(self.editing.is_none(), |d| {
                d.child(
                    self.tool("chat-attach", glyph::ATTACH, tr!("chats-attach"), l)
                        .children(crate::screens::spot("chat-attach"))
                        .size(px(30.))
                        .on_click(cx.listener(|this, _, _, cx| this.pick_files(cx))),
                )
            })
            .child(div().flex_1().min_w_0().child(Textarea::new(state).appearance(false)))
            .child(
                self.tool_with("chat-emoji-toggle", div().text_size(px(15.)).child("🙂"), tr!("chats-emoji"), l)
                    .size(px(30.))
                    .when(emoji_on, |d| d.bg(l.pressed()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.emoji = !this.emoji;
                        cx.notify();
                    })),
            )
            .child(
                crate::buttons::primary("chat-send", l)
                    .children(crate::screens::spot("chat-send"))
                    .size(px(30.))
                    .px_0()
                    .justify_center()
                    .child(icon(glyph::SEND, 14.))
                    .tooltip(move |window, cx| crate::screens::tip(send_tip.clone(), window, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            );
        Some(
            div()
                .id("chat-composer")
                .children(crate::screens::spot("chat-composer"))
                .flex_none()
                .mx(px(24.))
                .mb(px(16.))
                .mt(px(4.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(12.))
                .py(px(8.))
                .rounded(l.px(Metric::RadiusControl))
                .border_1()
                .border_color(if focused { l.accent() } else { l.stroke() })
                .bg(l.panel())
                .children(self.draft_bar(d, l, cx))
                .children(self.files_row(l, cx))
                .when(emoji_on, |c| c.child(self.emoji_strip(l, cx)))
                .child(field)
                .into_any_element(),
        )
    }

    /// Ответ канала на действие довеска (ADR 0012): текст — подсказкой (тост) или предупреждением (строка с «ОК»),
    /// ссылка — показать; открыть её — только нажатием.
    pub(crate) fn extra_reply(&mut self, reply: postman_core::channel::ExtraReply, cx: &mut Context<Self>) {
        match reply.text.filter(|t| !t.trim().is_empty()) {
            Some(text) if reply.alert => self.alert = Some(text),
            Some(text) => cx.emit(PaneEvent::Toast(Toast::Info, text)),
            None => {}
        }
        if let Some(url) = reply.url.filter(|u| !u.trim().is_empty()) {
            self.offered = Some(url);
        }
        cx.notify();
    }

    fn press(&mut self, message: MessageId, payload: postman_core::channel::Extra, cx: &mut Context<Self>) {
        if let Some(o) = &self.open {
            let action = Action::Extra { conversation: o.at.conversation.clone(), message, payload };
            cx.emit(PaneEvent::Extra(o.at.chan(), action));
        }
    }

    /// Голос в опросе: `picks` — номера вариантов, пусто — отозвать.
    fn vote(&mut self, m: &Message, picks: &[usize], cx: &mut Context<Self>) {
        let Some(channel) = self.address().map(|at| at.channel.0.clone()) else { return };
        if let Some(payload) = crate::chat_extra::vote(&channel, &m.extra, picks) {
            self.picks = None;
            self.press(m.id.clone(), payload, cx);
            cx.notify();
        }
    }

    /// Довесок сообщения (кнопки бота, опрос): нажать можно только при `ChannelFeatures::extra`.
    fn extra(&self, n: usize, m: &Message, sent: bool, d: &Loaded, l: &Look, cx: &mut Context<Self>) -> Option<Div> {
        let v = crate::chat_extra::view(&d.summary.address.channel.0, &m.extra)?;
        let can = d.features.extra && sent;
        let mut col = div().flex().flex_col().gap(px(6.)).pt_1().min_w(px(260.));
        if let Some(p) = &v.poll {
            col = col.child(self.poll(n, m, p, can, l, cx));
        }
        for (r, row) in v.rows.iter().enumerate() {
            col = col.child(div().flex().gap(px(6.)).children(row.iter().enumerate().map(|(k, b)| {
                let (id, payload) = (m.id.clone(), b.payload.clone());
                crate::buttons::secondary(SharedString::from(format!("chat-bot-{n}-{r}-{k}")), l)
                    .flex_1()
                    .h(px(28.))
                    .px(px(10.))
                    .justify_center()
                    .text_size(px(12.))
                    .when(b.link, |d| d.child(icon(glyph::LINK, 11.)))
                    .child(div().truncate().child(b.text.clone()))
                    .when(can, |d| d.on_click(cx.listener(move |this, _, _, cx| this.press(id.clone(), payload.clone(), cx))))
                    .when(!can, |d| d.opacity(0.6).cursor_default())
            })));
        }
        Some(col)
    }

    /// Опрос: вопрос, варианты с долей голосов и моим выбором; один ответ — голос щелчком, несколько — отметить и
    /// «Проголосовать»; «Отозвать голос» — пока опрос открыт.
    fn poll(&self, n: usize, m: &Message, p: &crate::chat_extra::Poll, can: bool, l: &Look, cx: &mut Context<Self>) -> Div {
        let open = can && !p.closed;
        let total = p.total();
        let voted = p.voted();
        let picks: Vec<usize> = self.picks.as_ref().filter(|(id, _)| *id == m.id).map(|(_, v)| v.clone()).unwrap_or_default();
        let mut col = div().flex().flex_col().gap(px(4.)).child(div().font_weight(FontWeight::SEMIBOLD).child(p.question.clone()));
        for (i, c) in p.choices.iter().enumerate() {
            let on = c.chosen || picks.contains(&i);
            let share = match (c.votes, total) {
                (Some(v), t) if t > 0 => v as f32 / t as f32,
                _ => 0.,
            };
            let mark = if p.multiple { crate::list::checkbox(on, l).size(px(16.)) } else { crate::settings_tab::radio(on, l) };
            let message = m.clone();
            let (multiple, chosen) = (p.multiple, c.chosen);
            let row = div()
                .id(SharedString::from(format!("chat-poll-{n}-{i}")))
                .children(crate::screens::spot(SharedString::from(format!("chat-poll-{n}-{i}"))))
                .relative()
                .h(px(30.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(l.px(Metric::RadiusControl))
                .border_1()
                .border_color(if on { l.accent() } else { l.border() })
                .overflow_hidden()
                // Доля голосов — заливкой от левого края.
                .child(div().absolute().left_0().top_0().bottom_0().w(relative(share)).bg(l.pm(Pm::Secondary)))
                .child(div().relative().child(mark))
                .child(div().relative().flex_1().min_w_0().truncate().child(c.text.clone()))
                .children(c.votes.map(|v| div().relative().flex_none().text_size(px(12.)).text_color(l.text2()).child(v.to_string())))
                .when(open, |d| {
                    d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                        if multiple {
                            let mut list = this.picks.take().filter(|(id, _)| *id == message.id).map(|(_, v)| v).unwrap_or_default();
                            match list.iter().position(|x| *x == i) {
                                Some(at) => {
                                    list.remove(at);
                                }
                                None => list.push(i),
                            }
                            this.picks = Some((message.id.clone(), list));
                            cx.notify();
                        } else if chosen {
                            this.vote(&message, &[], cx);
                        } else {
                            this.vote(&message, &[i], cx);
                        }
                    }))
                });
            col = col.child(row);
        }
        let mut foot = div().flex().items_center().gap(px(10.)).text_size(px(12.)).text_color(l.text2());
        foot = foot.child(if p.closed { tr!("chats-poll-closed") } else { tr!("chats-poll-votes", n = i64::from(total)) });
        if open && p.multiple && !picks.is_empty() {
            let message = m.clone();
            foot = foot.child(
                crate::buttons::primary(("chat-poll-vote", n), l)
                    .h(px(24.))
                    .px(px(10.))
                    .child(tr!("chats-poll-vote"))
                    .on_click(cx.listener(move |this, _, _, cx| this.vote(&message, &picks, cx))),
            );
        }
        if open && voted {
            let message = m.clone();
            foot = foot.child(
                div()
                    .id(("chat-poll-retract", n))
                    .cursor_pointer()
                    .text_color(l.accent_text())
                    .child(tr!("chats-poll-retract"))
                    .on_click(cx.listener(move |this, _, _, cx| this.vote(&message, &[], cx))),
            );
        }
        col.child(foot)
    }

    /// Строки ответа канала над полем ввода: предупреждение бота с «ОК» и предложенная ссылка с «Открыть».
    fn reply_lines(&self, l: &Look, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let line = |id: &'static str, l: &Look| {
            div()
                .id(id)
                .children(crate::screens::spot(id))
                .flex_none()
                .mx(px(24.))
                .mb(px(4.))
                .px(px(12.))
                .py(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(l.px(Metric::RadiusControl))
        };
        let mut out = Vec::new();
        if let Some(text) = &self.alert {
            out.push(
                line("chat-bot-alert", l)
                    .bg(l.warning_bg())
                    .child(icon(glyph::WARNING, 14.).text_color(l.warning()))
                    .child(div().flex_1().min_w_0().child(text.clone()))
                    .child(crate::buttons::button("chat-bot-alert-ok", tr!("chats-ok"), true, l).on_click(cx.listener(|this, _, _, cx| {
                        this.alert = None;
                        cx.notify();
                    }))),
            );
        }
        if let Some(url) = &self.offered {
            let href = url.clone();
            out.push(
                line("chat-bot-link", l)
                    .bg(l.fill())
                    .child(icon(glyph::LINK, 14.).text_color(l.text2()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(px(12.)).text_color(l.text2()).child(tr!("chats-link-offered")))
                            .child(div().truncate().child(url.clone())),
                    )
                    .child(crate::buttons::button("chat-bot-link-open", tr!("chats-link-open"), true, l).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.offered = None;
                            ir_view::open_link(&href, false, &href, cx);
                            cx.notify();
                        },
                    )))
                    .child(crate::buttons::button("chat-bot-link-close", tr!("chats-cancel"), false, l).on_click(cx.listener(|this, _, _, cx| {
                        this.offered = None;
                        cx.notify();
                    }))),
            );
        }
        out
    }

    /// Сцена снимков: ответ бота предупреждением и ссылкой — как после нажатия кнопки.
    pub(crate) fn screen_bot_reply(&mut self, cx: &mut Context<Self>) {
        self.extra_reply(
            postman_core::channel::ExtraReply {
                text: Some(tr!("chats-demo-alert")),
                alert: true,
                url: Some(postman_demo::channel::BUILD_LOG.into()),
            },
            cx,
        );
    }

    pub(crate) fn bot_reply_shown(&self) -> bool {
        self.alert.is_some() && self.offered.is_some()
    }

    /// «Удалить сообщение?» — строкой над полем ввода (без модальных окон).
    fn delete_line(&self, l: &Look, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let (_, for_all) = self.deleting.as_ref()?;
        Some(
            div()
                .id("chat-delete-line")
                .flex_none()
                .mx(px(24.))
                .mb(px(4.))
                .px(px(12.))
                .py(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(l.px(Metric::RadiusControl))
                .bg(l.fill())
                .child(div().flex_1().child(tr!(if *for_all { "chats-delete-all-ask" } else { "chats-delete-own-ask" })))
                .child(
                    crate::buttons::button("chat-delete-yes", tr!("chats-delete-yes"), true, l)
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_delete(cx))),
                )
                .child(crate::buttons::button("chat-delete-no", tr!("chats-cancel"), false, l).on_click(cx.listener(|this, _, _, cx| {
                    this.deleting = None;
                    cx.notify();
                }))),
        )
    }
}

impl Render for ChatPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.prepare(window, cx);
        let l = look(cx);
        let Some(o) = &self.open else {
            return empty_state(glyph::CONVERSATION, tr!("chats-empty-pane-title"), tr!("chats-empty-pane-text"), &l).into_any_element();
        };
        let Some(d) = &o.data else {
            return div().size_full().flex().items_center().justify_center().text_color(l.text2()).child(tr!("chats-loading")).into_any_element();
        };
        if std::mem::take(&mut self.to_bottom) {
            self.scroll.scroll_to_bottom();
        }
        let s = chats::shown(&d.features, d.summary.conversation.my_role, None, chats::now_ms());
        let today = jiff::Zoned::now().date();
        let read_up_to = read_index(&d.lines, &d.read_by, d.me.as_ref());
        let mut tape_col = div().flex().flex_col().gap_2().px(px(24.)).py(px(12.));
        // Раньше показанного: из базы или, если в ней больше нет, с сервера.
        if !d.lines.is_empty() {
            tape_col = tape_col.child(
                div().flex().justify_center().child(
                    crate::buttons::secondary("chat-earlier", &l)
                        .h(px(26.))
                        .px(px(10.))
                        .text_size(px(12.))
                        .child(tr!("chats-earlier"))
                        .on_click(cx.listener(|this, _, _, cx| this.load_earlier(cx))),
                ),
            );
        }
        if d.lines.is_empty() {
            tape_col = tape_col.child(div().flex().justify_center().py(px(24.)).text_color(l.text2()).child(tr!("chats-no-messages")));
        }
        for piece in pieces(&d.lines, o.new_from.as_ref(), &tape::day_of) {
            tape_col = match piece {
                Piece::Day(day) => tape_col.child(tape::day_row(tape::day_label(day, today), &l)),
                Piece::New => tape_col.child(
                    div()
                        .id("chat-new")
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(12.))
                        .text_color(l.accent_text())
                        .child(div().flex_1().h(px(1.)).bg(l.accent()))
                        .child(tr!("chats-new-messages"))
                        .child(div().flex_1().h(px(1.)).bg(l.accent())),
                ),
                Piece::Msg { index, head } => tape_col.child(self.line(index, head, read_up_to, d, &l, cx)),
            };
        }
        let tape_view = div().id("chat-tape").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.scroll).child(tape_col);
        div()
            .id("chat-pane")
            .size_full()
            .flex()
            .flex_col()
            .text_color(l.text())
            .child(self.header(d, &l, cx))
            // Режимы «Беседа | Медиа беседы» под шапкой (MED-02).
            .child(div().flex_none().px(px(24.)).child(crate::thread_media::mode_tabs(&self.media, tr!("media-mode-talk"), &l, cx)))
            .map(|pane| {
                if self.media.read(cx).on {
                    return pane.child(div().flex_1().min_h_0().px(px(24.)).py(px(12.)).child(self.media.clone()));
                }
                pane.children(crate::thread_media::summary(&self.media, &l, cx).map(|s| s.px(px(24.)).pt(px(8.)).pb(px(6.))))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(tape_view)
                            .child(crate::chrome::vbar(&self.scroll, ScrollbarMode::Hover)),
                    )
                    .children(self.typing_line(d, &l))
                    .children(self.delete_line(&l, cx))
                    .children(self.reply_lines(&l, cx))
                    .children(self.composer(d, s, window, &l, cx))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use postman_cache::Cache;
    use postman_core::channel::{
        AccountId, Address, Body, ChannelEvent, ChannelId, ChannelStore, Conversation, ConversationId, Line, Message, MessageId, MessageKind,
        Participant, ParticipantId, ServiceKind,
    };

    use super::{Piece, load, mention_ranges, new_from, pieces, read_index};

    fn msg(id: &str, author: &str, sent: i64) -> Message {
        Message {
            id: MessageId::new(id), author: ParticipantId::new(author), sent, body: Body::Plain(format!("текст {id}")), ..Message::default()
        }
    }

    fn line(m: Message) -> Line {
        Line::Message { message: m, loaded: Vec::new() }
    }

    fn day(ms: i64) -> Option<jiff::civil::Date> {
        jiff::Timestamp::from_millisecond(ms).ok().map(|t| t.to_zoned(jiff::tz::TimeZone::UTC).date())
    }

    const DAY: i64 = 86_400_000;

    /// Лента: разделитель дня, сообщения подряд одного автора — без шапки, другой автор, пауза, служебное и новый
    /// день — с шапкой; «Новые сообщения» — перед своим сообщением.
    #[test]
    fn tape_groups_by_day_and_author() {
        let service = Message { kind: MessageKind::Service(ServiceKind::Created), ..msg("s", "a", 3 * 60_000) };
        let lines = vec![
            line(msg("1", "a", 0)),
            line(msg("2", "a", 60_000)),
            line(msg("3", "b", 2 * 60_000)),
            line(service),
            line(msg("4", "b", 4 * 60_000)),
            line(msg("5", "b", 20 * 60_000)),
            line(msg("6", "b", DAY + 60_000)),
            line(msg("7", "b", DAY + 2 * 60_000)),
        ];
        let got = pieces(&lines, Some(&MessageId::new("7")), &day);
        let m = |index, head| Piece::Msg { index, head };
        assert_eq!(
            got,
            [
                Piece::Day(day(0).unwrap()),
                m(0, true),
                m(1, false),
                m(2, true),
                m(3, true),
                m(4, true),
                m(5, true),
                Piece::Day(day(DAY).unwrap()),
                m(6, true),
                Piece::New,
                m(7, true),
            ]
        );
    }

    /// «Новые сообщения»: после моего «прочитано» — первое чужое; сервер «прочитано» не дал — по счётчику с конца.
    #[test]
    fn new_messages_start_after_my_read_mark() {
        let me = ParticipantId::new("me");
        let lines = vec![line(msg("1", "a", 1)), line(msg("2", "me", 2)), line(msg("3", "a", 3)), line(msg("4", "a", 4))];
        let id = |s: &str| Some(MessageId::new(s));
        assert_eq!(new_from(&lines, Some(&MessageId::new("1")), 0, Some(&me)), id("3"), "своё не считается новым");
        assert_eq!(new_from(&lines, Some(&MessageId::new("4")), 0, Some(&me)), None);
        assert_eq!(new_from(&lines, None, 2, Some(&me)), id("3"));
        assert_eq!(new_from(&lines, None, 1, Some(&me)), id("4"));
        assert_eq!(new_from(&lines, None, 0, Some(&me)), None);
    }

    /// «Прочитано»: до самого дальнего сообщения, которое прочитал кто-то, кроме меня.
    #[test]
    fn read_mark_is_the_farthest_one_by_others() {
        let me = ParticipantId::new("me");
        let lines = vec![line(msg("1", "me", 1)), line(msg("2", "me", 2)), line(msg("3", "me", 3))];
        let by = |who: &str, id: &str| (ParticipantId::new(who), MessageId::new(id));
        assert_eq!(read_index(&lines, &[by("a", "1"), by("b", "2"), by("me", "3")], Some(&me)), Some(1));
        assert_eq!(read_index(&lines, &[by("me", "3")], Some(&me)), None);
        assert_eq!(read_index(&lines, &[by("a", "нет")], Some(&me)), None);
    }

    #[test]
    fn my_mention_is_found_in_the_text() {
        let names = vec!["Анна Смирнова".to_owned(), "anna".to_owned()];
        let text = "@boris и @anna, гляньте";
        let got = mention_ranges(text, &names);
        assert_eq!(got.iter().map(|r| &text[r.clone()]).collect::<Vec<_>>(), ["@anna"]);
        let text = "Привет, @Анна!";
        assert_eq!(mention_ranges(text, &names).iter().map(|r| &text[r.clone()]).collect::<Vec<_>>(), ["@Анна"]);
        // Имени нет среди слов — первое упоминание.
        let text = "@Я посмотри, почта a@example.test";
        assert_eq!(mention_ranges(text, &names).iter().map(|r| &text[r.clone()]).collect::<Vec<_>>(), ["@Я"]);
        assert!(mention_ranges("без упоминаний @", &names).is_empty());
    }

    /// Беседа из базы: последние строки ленты и «есть раньше», люди, моё имя, счётчик; чужой адрес — `None`.
    #[test]
    fn conversation_loads_from_the_store() {
        let cache = Cache::open_in_memory().unwrap();
        let (channel, account) = (ChannelId::new("demo"), AccountId::new("me@example.test"));
        let store = ChannelStore::open(cache.clone(), &channel, &account, "Я").unwrap();
        store.set_me(&Participant { id: ParticipantId::new("me"), name: "Я".into(), ..Participant::default() }).unwrap();
        let conv = ConversationId::new("g");
        store
            .apply(vec![
                ChannelEvent::Conversations(vec![Conversation { id: conv.clone(), title: "Команда".into(), ..Conversation::default() }]),
                ChannelEvent::People(vec![Participant { id: ParticipantId::new("a"), name: "Анна".into(), ..Participant::default() }]),
                ChannelEvent::Messages {
                    conversation: conv.clone(),
                    messages: vec![msg("1", "a", 1), msg("2", "a", 2), msg("3", "a", 3)],
                    live: false,
                },
            ])
            .unwrap();
        let at = Address::conversation(&channel, &account, &conv);
        let d = load(&cache, &at, 2, 10).unwrap().unwrap();
        assert_eq!(d.lines.iter().map(|l| l.id().0.as_str()).collect::<Vec<_>>(), ["2", "3"]);
        assert!(d.more, "в базе есть сообщения раньше");
        assert_eq!(d.people[&ParticipantId::new("a")].name, "Анна");
        assert_eq!(d.me, Some(ParticipantId::new("me")));
        assert_eq!(d.unread, 3);
        assert!(!load(&cache, &at, 10, 10).unwrap().unwrap().more);
        let other = Address::conversation(&channel, &account, &ConversationId::new("нет"));
        assert!(load(&cache, &other, 10, 10).unwrap().is_none());
    }
}
