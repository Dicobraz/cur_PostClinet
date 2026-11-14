//! «Медиа беседы» в области чтения (MED-02, DESIGN «Медиа беседы», макеты `MedEntry`, `MedFiles`): режим вместо
//! ленты — под шапкой каждой беседы (почта и чаты) две вкладки-режима «Беседа · N | Медиа беседы · M»; над лентой —
//! строка-сводка «В беседе … — Открыть «Медиа беседы» →». Команда `thread.media` переключает режим, Esc — назад.
//!
//! Внутри: поиск во всю ширину (счётчики — по найденному), вкладки «Файлы, Медиа, Ссылки, Контакты, Встречи» со
//! счётчиком (пустая — видна, с 0), справа вид «Список | Галерея | Таблица» (запоминается на вкладку; у «Медиа» по
//! умолчанию галерея), строка «Загружено N из M писем», выбранный элемент — рамкой акцента и полосой «Выбрано: …»
//! с кнопками. Канал виден во всех видах: значок перед автором, в таблице — столбец «Канал».
//!
//! Данные — `postman_core::thread_media` (MED-01): сбор в фоне с воркером-песочницей; докачка тел — командами
//! синхронизации; тело письма беседы пришло — собрать заново.

use std::sync::{Arc, Mutex, PoisonError};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollbarMode;
use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;
use pets_ui::Level as Toast;
use pets_ui::theme::Metric;
use postman_cache::Cache;
use postman_calendar::Partstat;
use postman_core::channel::Address;
use postman_core::sync::Command;
use postman_core::thread_media::{self, Counts, LinkKind, Origin, ThreadMedia};
use postman_worker::Client as Worker;

use crate::chrome::{glyph, icon};
use crate::roles::{Look, Pm, look};
use crate::tape;

/// Чья это беседа: письма (ключ беседы кэша и мой адрес) или беседа канала.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    Mail { thread: String, me: Option<String> },
    Chat(Address),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Files,
    Media,
    Links,
    Contacts,
    Meetings,
}

impl Tab {
    pub(crate) const ALL: [Tab; 5] = [Tab::Files, Tab::Media, Tab::Links, Tab::Contacts, Tab::Meetings];

    fn key(self) -> &'static str {
        match self {
            Tab::Files => "media-tab-files",
            Tab::Media => "media-tab-media",
            Tab::Links => "media-tab-links",
            Tab::Contacts => "media-tab-contacts",
            Tab::Meetings => "media-tab-meetings",
        }
    }

    fn count(self, c: &Counts) -> usize {
        match self {
            Tab::Files => c.files,
            Tab::Media => c.media,
            Tab::Links => c.links,
            Tab::Contacts => c.contacts,
            Tab::Meetings => c.meetings,
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Вид вкладки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    List,
    Gallery,
    Table,
}

impl Kind {
    const ALL: [(Kind, &'static str, char); 3] = [
        (Kind::List, "media-kind-list", glyph::BULLETS),
        (Kind::Gallery, "media-kind-gallery", glyph::PICTURE),
        (Kind::Table, "media-kind-table", glyph::TABLE),
    ];
}

/// Вид по умолчанию: у «Медиа» — галерея, у остальных — список.
fn default_kinds() -> [Kind; 5] {
    [Kind::List, Kind::Gallery, Kind::List, Kind::List, Kind::List]
}

pub(crate) enum MediaEvent {
    /// «К письму»: ключ письма кэша или адрес сообщения канала `postman://…`.
    Goto(String),
    /// Докачать тела писем беседы: ключи писем.
    Fetch(Vec<String>),
    /// «Написать» участнику.
    Write(String),
}

impl EventEmitter<MediaEvent> for MediaView {}

/// Метка у строки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chip {
    Plain,
    /// Подозрительная ссылка, отменённая встреча.
    Alert,
    /// «Принято».
    Ok,
}

/// Строка любой вкладки в общем виде: по ней рисуются список, галерея и таблица.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Entry {
    pub(crate) glyph: char,
    pub(crate) title: String,
    /// Тип и размер, текст ссылки, адрес, время встречи.
    pub(crate) sub: String,
    pub(crate) who: String,
    /// Unix, секунды; 0 — нет.
    pub(crate) when: i64,
    pub(crate) channel: String,
    pub(crate) chips: Vec<(String, Chip)>,
    /// Куда «К письму».
    pub(crate) letter: Option<String>,
    /// Ссылка и «подозрительная».
    pub(crate) url: Option<(String, bool)>,
    /// Адрес участника — «Написать».
    pub(crate) email: Option<String>,
    /// Остальные письма с этим же файлом («×N»): подпись и ключ письма.
    pub(crate) copies: Vec<(String, String)>,
    /// Отменённая встреча — название перечёркнуто.
    pub(crate) struck: bool,
}

fn who_of(o: &Origin) -> String {
    if o.from_name.is_empty() { o.from_addr.clone() } else { o.from_name.clone() }
}

fn from_origin(o: Option<&Origin>, e: &mut Entry) {
    if let Some(o) = o {
        e.who = who_of(o);
        e.when = o.at;
        e.channel = o.channel.clone();
        e.letter = Some(o.letter.clone());
    }
}

fn file_entry(f: &thread_media::File, g: char) -> Entry {
    let kind = f.name.rsplit_once('.').map(|(_, ext)| ext.to_uppercase()).filter(|e| e.len() <= 5).unwrap_or_else(|| f.mime.clone());
    let mut e = Entry { glyph: g, title: f.name.clone(), sub: format!("{kind} · {}", crate::attachments::size_text(f.size)), ..Entry::default() };
    from_origin(f.first().map(|c| &c.origin), &mut e);
    if f.count() > 1 {
        e.chips.push((format!("×{}", f.count()), Chip::Plain));
        e.copies =
            f.copies.iter().map(|c| (format!("{} · {}", who_of(&c.origin), crate::format_date(c.origin.at)), c.origin.letter.clone())).collect();
    }
    e
}

/// Строки вкладки `tab` (тексты меток — Fluent; время встречи — в поясе `tz`).
pub(crate) fn entries(m: &ThreadMedia, tab: Tab, tz: &jiff::tz::TimeZone) -> Vec<Entry> {
    match tab {
        Tab::Files => m.files.iter().map(|f| file_entry(f, glyph::ATTACH)).collect(),
        Tab::Media => m
            .media
            .iter()
            .map(|x| {
                let mut e = file_entry(&x.file, if x.video { glyph::PREVIEW } else { glyph::PICTURE });
                if x.video {
                    e.chips.insert(0, (tr!("media-chip-video"), Chip::Plain));
                }
                e
            })
            .collect(),
        Tab::Links => m
            .links
            .iter()
            .map(|x| {
                let mut e = Entry {
                    glyph: glyph::LINK,
                    title: x.domain.clone(),
                    sub: x.text.clone(),
                    url: Some((x.url.clone(), x.suspicious)),
                    ..Entry::default()
                };
                from_origin(x.seen.last(), &mut e);
                match x.kind {
                    LinkKind::Unsubscribe => e.chips.push((tr!("media-chip-unsubscribe"), Chip::Plain)),
                    LinkKind::Tracking => e.chips.push((tr!("media-chip-tracking"), Chip::Plain)),
                    LinkKind::Plain => {}
                }
                if x.suspicious {
                    e.glyph = glyph::PHISHING;
                    e.chips.push((tr!("media-chip-suspicious"), Chip::Alert));
                }
                e
            })
            .collect(),
        Tab::Contacts => {
            let people = m.contacts.people.iter().map(|p| {
                let mut e = Entry {
                    glyph: glyph::CONTACTS,
                    title: if p.name.is_empty() { p.email.clone() } else { p.name.clone() },
                    sub: p.email.clone(),
                    who: tr!("media-letters", n = p.letters as i64),
                    email: Some(p.email.clone()),
                    ..Entry::default()
                };
                if p.me {
                    e.chips.push((tr!("media-chip-me"), Chip::Plain));
                }
                e
            });
            // Визитки vCard: имя из визитки; не разобрана — имя файла и серая метка.
            let cards = m.contacts.cards.iter().map(|c| {
                let mut e = file_entry(&c.file, glyph::ADD_CONTACT);
                match c.cards.first() {
                    Some(card) => {
                        e.title = if card.name.is_empty() { c.file.name.clone() } else { card.name.clone() };
                        e.sub = [card.emails.first().cloned().unwrap_or_default(), card.organization.clone()]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" · ");
                        e.email = card.emails.first().cloned();
                    }
                    None => e.chips.push((tr!("media-chip-unparsed"), Chip::Plain)),
                }
                e.chips.insert(0, (tr!("media-chip-vcard"), Chip::Plain));
                e
            });
            people.chain(cards).collect()
        }
        Tab::Meetings => m
            .meetings
            .iter()
            .map(|x| {
                let when = x.span.map(|s| crate::invite_card::when_line(s, x.all_day, tz)).unwrap_or_default();
                let sub = [when, x.location.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                let mut e = Entry { glyph: crate::calendar::g::CALENDAR, title: x.title.clone(), sub, struck: x.cancelled, ..Entry::default() };
                from_origin(x.file.first().map(|c| &c.origin), &mut e);
                if x.cancelled {
                    e.chips.push((tr!("media-chip-cancelled"), Chip::Alert));
                }
                // Мой ответ меткой; ответа нет — «нет ответа».
                let (key, chip) = match x.status {
                    Some(Partstat::Accepted) => ("media-answer-accepted", Chip::Ok),
                    Some(Partstat::Tentative) => ("media-answer-tentative", Chip::Plain),
                    Some(Partstat::Declined) => ("media-answer-declined", Chip::Plain),
                    _ => ("media-answer-none", Chip::Plain),
                };
                e.chips.push((tr!(key), chip));
                e
            })
            .collect(),
    }
}

/// Режим «Медиа беседы» одной области чтения.
pub(crate) struct MediaView {
    cache: Cache,
    worker: Arc<Mutex<Worker>>,
    source: Option<Source>,
    /// Режим включён: вид — вместо ленты.
    pub(crate) on: bool,
    all: Option<ThreadMedia>,
    loading: bool,
    again: bool,
    /// Докачку тел для этой беседы уже просили.
    fetched: bool,
    query: Option<(Entity<InputState>, Subscription)>,
    text: String,
    clear_query: bool,
    pub(crate) tab: Tab,
    kinds: [Kind; 5],
    picked: Option<usize>,
    /// Развёрнут список писем файла «×N».
    copies: Option<usize>,
    /// Подозрительная ссылка ждёт «Открыть всё равно».
    confirm: Option<String>,
    scroll: ScrollHandle,
}

impl MediaView {
    pub(crate) fn new(cache: Cache, worker: Arc<Mutex<Worker>>) -> Self {
        Self {
            cache,
            worker,
            source: None,
            on: false,
            all: None,
            loading: false,
            again: false,
            fetched: false,
            query: None,
            text: String::new(),
            clear_query: false,
            tab: Tab::Files,
            kinds: default_kinds(),
            picked: None,
            copies: None,
            confirm: None,
            scroll: ScrollHandle::new(),
        }
    }

    /// Открыта другая беседа (или никакая): режим — «Беседа», данные — заново.
    pub(crate) fn set_source(&mut self, source: Option<Source>, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        self.source = source;
        self.on = false;
        self.all = None;
        self.fetched = false;
        self.text.clear();
        self.clear_query = true;
        self.tab = Tab::Files;
        self.picked = None;
        self.copies = None;
        self.confirm = None;
        self.reload(cx);
        cx.notify();
    }

    /// Беседа, для которой открыт режим.
    pub(crate) fn source(&self) -> Option<&Source> {
        self.source.as_ref()
    }

    /// Собрать заново (пришло тело письма, сообщение канала): в фоне, с воркером.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.source.clone() else { return };
        if self.loading {
            self.again = true;
            return;
        }
        self.loading = true;
        let (cache, worker) = (self.cache.clone(), self.worker.clone());
        let task = cx.background_spawn({
            let source = source.clone();
            async move {
                let mut w = worker.lock().unwrap_or_else(PoisonError::into_inner);
                match &source {
                    Source::Mail { thread, me } => thread_media::load(&cache, &mut w, thread, me.as_deref()).map_err(|e| e.to_string()),
                    Source::Chat(at) => thread_media::load_group(&cache, &mut w, None, at, None).map_err(|e| e.to_string()),
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let loaded = task.await;
            let _ = this.update(cx, |v, cx| {
                v.loading = false;
                if v.source.as_ref() == Some(&source) {
                    match loaded {
                        Ok((media, fetch)) => {
                            v.all = Some(media);
                            let ids: Vec<String> = fetch
                                .into_iter()
                                .filter_map(|c| match c {
                                    Command::Body(id) => Some(id),
                                    _ => None,
                                })
                                .collect();
                            if !ids.is_empty() && !std::mem::replace(&mut v.fetched, true) {
                                cx.emit(MediaEvent::Fetch(ids));
                            }
                        }
                        Err(e) => tracing::warn!("thread media: not collected: {e}"),
                    }
                }
                if std::mem::take(&mut v.again) {
                    v.reload(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Данные собраны.
    pub(crate) fn ready(&self) -> bool {
        self.all.is_some()
    }

    /// Собрано, и тела всех писем есть.
    pub(crate) fn complete(&self) -> bool {
        self.all.as_ref().is_some_and(ThreadMedia::complete)
    }

    /// Собрано по части писем: остальные тела ещё докачиваются.
    pub(crate) fn partial(&self) -> bool {
        self.all.as_ref().is_some_and(|m| !m.complete() && m.loaded > 0)
    }

    /// Попросить недостающие тела ещё раз и собрать заново.
    pub(crate) fn refetch(&mut self, cx: &mut Context<Self>) {
        self.fetched = false;
        self.reload(cx);
    }

    fn counts(&self) -> Counts {
        self.all.as_ref().map(ThreadMedia::counts).unwrap_or_default()
    }

    /// Всего в «Медиа беседы» — число у вкладки режима.
    pub(crate) fn total(&self) -> usize {
        let c = self.counts();
        c.files + c.media + c.links + c.contacts + c.meetings
    }

    pub(crate) fn set_on(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.source.is_some() && self.on != on {
            self.on = on;
            self.picked = None;
            self.confirm = None;
            cx.notify();
        }
    }

    pub(crate) fn toggle(&mut self, cx: &mut Context<Self>) {
        self.set_on(!self.on, cx);
    }

    /// Вкладка (сцены, щелчок).
    pub(crate) fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.picked = None;
        self.copies = None;
        cx.notify();
    }

    /// Вид вкладки (сцены, щелчок).
    pub(crate) fn set_kind(&mut self, kind: Kind, cx: &mut Context<Self>) {
        self.kinds[self.tab.index()] = kind;
        cx.notify();
    }

    /// Esc: строка подтверждения ссылки, затем режим — назад к «Беседе».
    pub(crate) fn escape(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.on {
            return false;
        }
        if self.confirm.take().is_none() {
            self.on = false;
        }
        cx.notify();
        true
    }

    fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.query.is_none() {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("media-search")));
            let sub = cx.subscribe_in(&state, window, |this: &mut MediaView, state, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.text = state.read(cx).value().to_string();
                    this.picked = None;
                    this.copies = None;
                    cx.notify();
                }
            });
            self.query = Some((state, sub));
        }
        if std::mem::take(&mut self.clear_query)
            && let Some((state, _)) = &self.query
        {
            state.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    fn open_link(&mut self, url: &str, suspicious: bool, cx: &mut Context<Self>) {
        // Подозрительная — строкой подтверждения в окне (без модальных окон).
        if suspicious && crate::prefs::get(cx).reading.warn_link_mismatch {
            self.confirm = Some(url.to_owned());
            cx.notify();
        } else {
            cx.open_url(url);
        }
    }

    // --- отрисовка ---

    fn chip(&self, text: String, kind: Chip, l: &Look) -> Div {
        let (bg, fg) = match kind {
            Chip::Plain => (l.fill(), l.text2()),
            Chip::Alert => (l.pm(Pm::ThreatBg), l.pm(Pm::ThreatFg)),
            Chip::Ok => (l.pm(Pm::OkBg), l.pm(Pm::OkFg)),
        };
        div().flex_none().h(px(18.)).px(px(6.)).flex().items_center().rounded(px(4.)).bg(bg).text_color(fg).text_size(px(11.)).child(text)
    }

    /// Кто и когда, со значком канала перед автором.
    fn by(&self, e: &Entry, l: &Look) -> Div {
        let when = if e.when > 0 { crate::format_date(e.when) } else { String::new() };
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(px(12.))
            .text_color(l.text2())
            .when(!e.channel.is_empty(), |d| d.child(tape::channel_icon(&e.channel, 12., l)))
            .child(div().truncate().child(e.who.clone()))
            .when(!when.is_empty(), |d| d.child(div().flex_none().child(when)))
    }

    fn title(&self, e: &Entry, l: &Look) -> Div {
        let g = if e.url.as_ref().is_some_and(|(_, bad)| *bad) { l.pm(Pm::ThreatDot) } else { l.text2() };
        // Название — в первую очередь; метки обрезаются справа, когда места мало.
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .min_w_0()
            .child(icon(e.glyph, 14.).text_color(g))
            .child(
                div()
                    .flex_initial()
                    .min_w(px(90.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .when(e.struck, |d| d.line_through())
                    .child(e.title.clone()),
            )
            .when(!e.chips.is_empty(), |d| {
                d.child(
                    div().flex_1().min_w_0().flex().gap(px(4.)).overflow_hidden().children(e.chips.iter().map(|(t, k)| self.chip(t.clone(), *k, l))),
                )
            })
    }

    fn pick(&mut self, i: usize, cx: &mut Context<Self>) {
        self.picked = (self.picked != Some(i)).then_some(i);
        cx.notify();
    }

    fn list(&self, items: &[Entry], l: &Look, cx: &mut Context<Self>) -> Div {
        let mut col = div().flex().flex_col();
        for (i, e) in items.iter().enumerate() {
            let on = self.picked == Some(i);
            let many = !e.copies.is_empty();
            let row = div()
                .id(("media-row", i))
                .children(crate::screens::spot(("media-row", i)))
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(8.))
                .py(px(8.))
                .rounded(l.px(Metric::RadiusControl))
                .border_1()
                .border_color(if on { l.accent() } else { l.none() })
                .when(on, |d| d.bg(l.row_fill(true)))
                .when(!on, |d| d.hover(|d| d.bg(l.row_hover())))
                .cursor_pointer()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(self.title(e, l))
                        .when(!e.sub.is_empty(), |d| d.child(div().truncate().text_size(px(12.)).text_color(l.text2()).child(e.sub.clone()))),
                )
                .child(div().flex_none().max_w(px(260.)).child(self.by(e, l)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pick(i, cx);
                    // «×N» разворачивает список писем с этим файлом.
                    this.copies = (many && this.picked == Some(i)).then_some(i);
                }));
            col = col.child(row);
            if self.copies == Some(i) {
                col = col.child(div().pl(px(30.)).pb(px(6.)).flex().flex_col().gap(px(2.)).children(e.copies.iter().enumerate().map(
                    |(k, (text, letter))| {
                        let letter = letter.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .text_size(px(12.))
                            .text_color(l.text2())
                            .child(div().flex_1().min_w_0().truncate().child(text.clone()))
                            .child(
                                div()
                                    .id(SharedString::from(format!("media-copy-{i}-{k}")))
                                    .flex_none()
                                    .cursor_pointer()
                                    .text_color(l.accent_text())
                                    .child(tr!("media-goto"))
                                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(MediaEvent::Goto(letter.clone())))),
                            )
                    },
                )));
            }
        }
        col
    }

    /// Галерея: плитки; у «Медиа» — пять в ряд.
    fn gallery(&self, items: &[Entry], l: &Look, cx: &mut Context<Self>) -> Div {
        div().flex().flex_wrap().gap(px(8.)).children(items.iter().enumerate().map(|(i, e)| {
            let on = self.picked == Some(i);
            div()
                .id(("media-tile", i))
                .children(crate::screens::spot(("media-tile", i)))
                .w(px(150.))
                .h(px(112.))
                .p(px(8.))
                .flex()
                .flex_col()
                .justify_between()
                .rounded(px(6.))
                .border_1()
                .border_color(if on { l.accent() } else { l.border() })
                .bg(l.fill())
                .cursor_pointer()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(icon(e.glyph, 22.).text_color(l.text2()))
                        .children(e.chips.first().map(|(t, k)| self.chip(t.clone(), *k, l))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .when(e.struck, |d| d.line_through())
                                .child(e.title.clone()),
                        )
                        .child(self.by(e, l)),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.pick(i, cx)))
        }))
    }

    /// Таблица: название со сведениями, кто, когда, канал.
    fn table(&self, items: &[Entry], l: &Look, cx: &mut Context<Self>) -> Div {
        let cell = |w: f32| div().w(px(w)).flex_none().truncate();
        let mut col = div().flex().flex_col().child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(8.))
                .h(px(28.))
                .border_b_1()
                .border_color(l.border())
                .text_size(px(12.))
                .text_color(l.text2())
                .child(div().flex_1().min_w_0().truncate().child(tr!("media-col-name")))
                .child(cell(120.).child(tr!("media-col-who")))
                .child(cell(84.).child(tr!("media-col-when")))
                .child(cell(92.).child(tr!("media-col-channel"))),
        );
        for (i, e) in items.iter().enumerate() {
            let on = self.picked == Some(i);
            let when = if e.when > 0 { crate::format_date(e.when) } else { String::new() };
            col =
                col.child(
                    div()
                        .id(("media-line", i))
                        .children(crate::screens::spot(("media-line", i)))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(8.))
                        .h(px(32.))
                        .border_b_1()
                        .border_color(l.border())
                        .when(on, |d| d.bg(l.row_fill(true)))
                        .when(!on, |d| d.hover(|d| d.bg(l.row_hover())))
                        .cursor_pointer()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(div().flex_1().min_w_0().child(self.title(e, l)))
                                .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(l.text2()).child(e.sub.clone())),
                        )
                        .child(cell(120.).child(e.who.clone()))
                        .child(cell(84.).text_size(px(12.)).text_color(l.text2()).child(when))
                        .child(
                            cell(92.).flex().items_center().gap(px(6.)).text_size(px(12.)).when(!e.channel.is_empty(), |d| {
                                d.child(tape::channel_icon(&e.channel, 12., l)).child(tape::channel_name(&e.channel))
                            }),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.pick(i, cx))),
                );
        }
        col
    }

    /// Полоса «Выбрано: …» с подписанными кнопками.
    fn picked_bar(&self, e: &Entry, l: &Look, cx: &mut Context<Self>) -> Stateful<Div> {
        let button = |id: &'static str, key: &'static str, l: &Look| {
            crate::buttons::secondary(id, l).h(px(26.)).px(px(10.)).text_size(px(12.)).child(tr!(key))
        };
        let mut bar = div()
            .id("media-picked")
            .children(crate::screens::spot("media-picked"))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .h(px(38.))
            .rounded(l.px(Metric::RadiusControl))
            .bg(l.pm(Pm::Secondary))
            .text_color(l.pm(Pm::SecondaryFg))
            .child(div().flex_1().min_w_0().truncate().child(tr!("media-picked", name = e.title.clone())));
        if let Some((url, bad)) = e.url.clone() {
            bar = bar.child(button("media-open-link", "media-open", l).on_click(cx.listener(move |this, _, _, cx| this.open_link(&url, bad, cx))));
        }
        if let Some(email) = e.email.clone().filter(|a| !a.is_empty()) {
            bar = bar
                .child(button("media-write", "media-write", l).on_click(cx.listener(move |_, _, _, cx| cx.emit(MediaEvent::Write(email.clone())))));
        }
        if let Some(letter) = e.letter.clone() {
            bar =
                bar.child(button("media-goto", "media-goto", l).on_click(cx.listener(move |_, _, _, cx| cx.emit(MediaEvent::Goto(letter.clone())))));
        }
        bar
    }
}

impl Render for MediaView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.prepare(window, cx);
        let l = look(cx);
        let Some(all) = &self.all else {
            return div().size_full().flex().items_center().justify_center().text_color(l.text2()).child(tr!("media-loading")).into_any_element();
        };
        let shown = thread_media::filter(all, &self.text);
        let counts = shown.counts();
        let tz = crate::prefs::get(cx).calendar.zone();
        let items = entries(&shown, self.tab, &tz);
        let kind = self.kinds[self.tab.index()];
        // Вкладки со счётчиком (пустая — видна, с 0) и вид справа.
        let tabs = div().flex().flex_wrap().items_center().gap(px(2.)).children(Tab::ALL.into_iter().map(|t| {
            let on = self.tab == t;
            div()
                .id(t.key())
                .children(crate::screens::spot(t.key()))
                .h(px(30.))
                .px(px(8.))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(5.))
                .rounded(l.px(Metric::RadiusControl))
                .cursor_pointer()
                .when(on, |d| d.bg(l.pm(Pm::Secondary)).text_color(l.pm(Pm::SecondaryFg)).font_weight(FontWeight::SEMIBOLD))
                .when(!on, |d| d.text_color(l.text2()).hover(|d| d.bg(l.hover())))
                .child(tr!(t.key()))
                .child(div().text_size(px(12.)).child(t.count(&counts).to_string()))
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(t, cx)))
        }));
        let kinds = div().flex().items_center().p(px(2.)).gap(px(2.)).rounded(l.px(Metric::RadiusControl)).bg(l.fill()).children(
            Kind::ALL.into_iter().map(|(k, key, g)| {
                let on = kind == k;
                div()
                    .id(key)
                    .children(crate::screens::spot(key))
                    .w(px(30.))
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(l.panel()).text_color(l.text()))
                    .when(!on, |d| d.text_color(l.text2()))
                    .child(icon(g, 13.))
                    .tooltip(move |window, cx| crate::screens::tip(tr!(key), window, cx))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_kind(k, cx)))
            }),
        );
        // «Загружено N из M писем» с полосой хода, пока тела докачиваются.
        let progress = (!all.complete()).then(|| {
            let share = if all.total == 0 { 0. } else { all.loaded as f32 / all.total as f32 };
            div()
                .id("media-progress")
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(4.))
                .text_size(px(12.))
                .text_color(l.text2())
                .child(tr!("media-progress", n = all.loaded as i64, m = all.total as i64))
                .child(div().h(px(3.)).w_full().rounded_full().bg(l.fill()).child(div().h_full().w(relative(share)).rounded_full().bg(l.accent())))
        });
        // Подозрительная ссылка: строка подтверждения на красной подложке.
        let confirm = self.confirm.clone().map(|url| {
            let go = url.clone();
            div()
                .id("media-confirm")
                .children(crate::screens::spot("media-confirm"))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .py(px(8.))
                .rounded(l.px(Metric::RadiusControl))
                .bg(l.pm(Pm::ThreatBg))
                .text_color(l.pm(Pm::ThreatFg))
                .child(icon(glyph::PHISHING, 14.))
                .child(div().flex_1().min_w_0().child(tr!("media-confirm")).child(div().truncate().text_size(px(12.)).child(url)))
                .child(crate::buttons::button("media-confirm-open", tr!("media-confirm-open"), false, &l).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.confirm = None;
                        cx.open_url(&go);
                        cx.notify();
                    },
                )))
                .child(crate::buttons::button("media-confirm-cancel", tr!("chats-cancel"), true, &l).on_click(cx.listener(|this, _, _, cx| {
                    this.confirm = None;
                    cx.notify();
                })))
        });
        let picked = self.picked.and_then(|i| items.get(i)).map(|e| self.picked_bar(e, &l, cx));
        // Пустые состояния: в беседе ничего нет; письма ещё не скачаны; поиск ничего не нашёл; вкладка пуста.
        let empty = |title: String, text: String| crate::chat_pane::empty_state(glyph::ATTACH, title, text, &l).into_any_element();
        let body: AnyElement = if all.is_empty() && !all.complete() {
            empty(tr!("media-empty-pending-title"), tr!("media-empty-pending-text"))
        } else if all.is_empty() {
            empty(tr!("media-empty-title"), tr!("media-empty-text"))
        } else if items.is_empty() && !self.text.trim().is_empty() {
            crate::chat_pane::empty_state(
                glyph::SEARCH,
                tr!("media-empty-found-title", query = self.text.trim().to_owned()),
                tr!("media-empty-found-text"),
                &l,
            )
            .child(crate::buttons::button("media-reset", tr!("media-reset"), false, &l).mt_2().on_click(cx.listener(|this, _, _, cx| {
                this.text.clear();
                this.clear_query = true;
                cx.notify();
            })))
            .into_any_element()
        } else if items.is_empty() {
            empty(tr!("media-empty-tab-title"), tr!("media-empty-tab-text"))
        } else {
            match kind {
                Kind::List => self.list(&items, &l, cx).into_any_element(),
                Kind::Gallery => self.gallery(&items, &l, cx).into_any_element(),
                Kind::Table => self.table(&items, &l, cx).into_any_element(),
            }
        };
        let search = self.query.as_ref().map(|(state, _)| Input::new(state).prefix(crate::icons::icon(crate::icons::SEARCH, 18., l.text3())));
        div()
            .id("thread-media")
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .text_color(l.text())
            .children(search)
            .child(div().flex_none().flex().flex_wrap().items_center().justify_between().gap(px(8.)).child(tabs).child(kinds))
            .children(progress)
            .children(confirm)
            .children(picked)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(div().id("media-items").size_full().overflow_y_scroll().track_scroll(&self.scroll).child(body))
                    .child(crate::chrome::vbar(&self.scroll, ScrollbarMode::Hover)),
            )
            .into_any_element()
    }
}

/// Вкладки-режимы под шапкой беседы: «Беседа · N» и «Медиа беседы · M» с подчёркиванием; справа — подсказка Esc.
pub(crate) fn mode_tabs(media: &Entity<MediaView>, talk: String, l: &Look, cx: &App) -> Div {
    let v = media.read(cx);
    let (on, total, ready) = (v.on, v.total(), v.ready());
    let tab = |id: &'static str, label: String, active: bool, to: bool| {
        let media = media.clone();
        div()
            .id(id)
            .children(crate::screens::spot(id))
            .h(px(32.))
            .flex()
            .items_center()
            .cursor_pointer()
            .border_b_2()
            .border_color(if active { l.accent() } else { l.none() })
            .when(active, |d| d.font_weight(FontWeight::SEMIBOLD))
            .when(!active, |d| d.text_color(l.text2()).hover(|d| d.text_color(l.text())))
            .child(label)
            .on_click(move |_, _, cx| media.update(cx, |m, cx| m.set_on(to, cx)))
    };
    let media_label = if ready { tr!("media-mode-media-n", n = total as i64) } else { tr!("media-mode-media") };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(20.))
        .border_b_1()
        .border_color(l.border())
        .child(tab("mode-talk", talk, !on, false))
        .child(tab("mode-media", media_label, on, true))
        .child(div().flex_1())
        .when(on, |d| d.child(div().text_size(px(12.)).text_color(l.text3()).child(tr!("media-mode-esc"))))
}

/// Строка-сводка над лентой: «В беседе 8 файлов, 13 картинок и видео, 9 ссылок — Открыть «Медиа беседы» →».
pub(crate) fn summary(media: &Entity<MediaView>, l: &Look, cx: &App) -> Option<Stateful<Div>> {
    let v = media.read(cx);
    let c = v.counts();
    if v.on || c.files + c.media + c.links == 0 {
        return None;
    }
    let parts: Vec<String> = [("media-sum-files", c.files), ("media-sum-media", c.media), ("media-sum-links", c.links)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(key, n)| tr!(key, n = n as i64))
        .collect();
    let media = media.clone();
    Some(
        div()
            .id("media-summary")
            .children(crate::screens::spot("media-summary"))
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .text_size(px(12.))
            .text_color(l.text2())
            .child(tr!("media-sum", what = parts.join(", ")))
            .child(div().text_color(l.accent_text()).child(tr!("media-sum-open")))
            .cursor_pointer()
            .on_click(move |_, _, cx| media.update(cx, |m, cx| m.set_on(true, cx))),
    )
}

// --- экран почты: команда, события режима, сцены ---

/// Опись части (ADR 0005, `parts.rs`): тексты, команда `thread.media`, сцены.
pub(crate) const PART: crate::part::Part =
    crate::part::Part { name: "thread-media", texts: TEXTS, commands: COMMANDS, handlers, scenes: scenes::SCENES, reset: Some(scenes::reset) };

const TEXTS: &[pets_i18n::Resource] = &[("ru", include_str!("../i18n/ru/media.ftl")), ("en", include_str!("../i18n/en/media.ftl"))];

/// «Беседа: файлы, ссылки, медиа» — переключить режим открытой беседы.
// Имя — область действий (`thread::…`), как в id команд: так обработчики читаются и их находит `docs/PARITY.md`.
pub(crate) mod thread {
    gpui_kit::actions!(thread, [Media]);
}

const COMMANDS: &[pets_commands::Command] = {
    use crate::commands::{LIST, cmd};
    // Ctrl+L — вне полей ввода; в схеме Outlook сочетание свободно.
    &[cmd("thread.media", &["ctrl-l"], LIST)]
};

fn handlers(root: Div, cx: &mut Context<crate::Mailer>) -> Div {
    root.on_action(cx.listener(|this, _: &thread::Media, _, cx| this.media_toggle(cx)))
}

/// Прогон снимков: демо отдаёт тела только первой половины писем беседы (строка «Загружено N из M»).
static HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl crate::Mailer {
    /// Режим «Медиа беседы» области чтения, которая сейчас показана: лента чата или письмо.
    pub(crate) fn media_now(&self) -> &Entity<MediaView> {
        if self.chat_active() && !self.chats.mail_open { &self.chats.media } else { &self.media }
    }

    /// `thread.media`: переключить режим открытой беседы; беседа не открыта — подсказка.
    fn media_toggle(&mut self, cx: &mut Context<Self>) {
        let media = self.media_now().clone();
        if media.read(cx).source().is_none() {
            return self.toast(Toast::Info, tr!("media-pick-first"), cx);
        }
        media.update(cx, |m, cx| m.toggle(cx));
    }

    /// Esc: строка подтверждения ссылки, затем назад к «Беседе».
    pub(crate) fn media_escape(&mut self, cx: &mut Context<Self>) -> bool {
        let media = self.media_now().clone();
        media.update(cx, |m, cx| m.escape(cx))
    }

    pub(crate) fn on_media(&mut self, ev: &MediaEvent, cx: &mut Context<Self>) {
        match ev {
            MediaEvent::Fetch(ids) => self.media_fetch(ids.clone(), cx),
            MediaEvent::Write(email) => {
                let to = vec![postman_compose::Address::new("", email)];
                self.open_compose(postman_compose::Draft { to, ..Default::default() }, cx);
            }
            MediaEvent::Goto(letter) => self.media_goto(letter.clone(), cx),
        }
    }

    /// Докачать тела писем беседы: серверу — командами; в демо тела — из демо-ящика в базу (в фоне), затем собрать
    /// заново.
    fn media_fetch(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        if self.backend.is_live() {
            for id in ids {
                if !self.backend.send(Command::Body(id)) {
                    break;
                }
            }
            return;
        }
        let take = if HOLD.load(std::sync::atomic::Ordering::Relaxed) { ids.len() / 2 } else { ids.len() };
        let cache = self.cache.clone();
        let task = cx.background_spawn(async move {
            for id in ids.iter().take(take) {
                let body = postman_demo::body(id);
                // Байты вложений — тоже: по ним воркер разбирает визитки и приглашения, считаются одинаковые файлы.
                for a in &body.attachments {
                    if let Some(bytes) = postman_demo::blob(&a.blob_id)
                        && let Err(e) = cache.put_blob(&a.blob_id, &bytes)
                    {
                        tracing::warn!("thread media: demo attachment not stored: {e}");
                    }
                }
                if let Err(e) = cache.put_body(id, &body) {
                    tracing::warn!("thread media: demo body not stored: {e}");
                }
            }
        });
        cx.spawn(async move |this, cx| {
            task.await;
            let _ = this.update(cx, |m, cx| m.media.update(cx, |v, cx| v.reload(cx)));
        })
        .detach();
    }

    /// «К письму»: сообщение канала — его беседа в ленту; письмо — в область чтения. Режим — назад к «Беседе».
    fn media_goto(&mut self, letter: String, cx: &mut Context<Self>) {
        let media = self.media_now().clone();
        media.update(cx, |m, cx| m.set_on(false, cx));
        if let Some(at) = Address::parse(&letter) {
            if self.chat_active() {
                self.open_chat(at.of_conversation(), cx);
            }
            return;
        }
        let cache = self.cache.clone();
        let task = cx.background_spawn(async move { cache.rows_by_ids(&[letter]) });
        cx.spawn(async move |this, cx| {
            let rows = task.await;
            let _ = this.update(cx, |m, cx| match rows {
                Ok(rows) => {
                    if let Some(row) = rows.into_iter().next() {
                        m.open_letter(row, cx);
                    }
                }
                Err(e) => tracing::warn!("thread media: letter not read: {e}"),
            });
        })
        .detach();
    }
}

/// Сцены `--screens` «Медиа беседы» (макеты «✓ Медиа беседы») — на демо-беседе `postman_demo::media::THREAD`.
pub(crate) mod scenes {
    use std::sync::atomic::Ordering;

    use gpui_kit::*;

    use super::{HOLD, Kind, Tab};
    use crate::screens::Scene;
    use crate::{Mailer, Section};

    pub(crate) const SCENES: &[Scene] = &[
        // `MedStates`: тела писем докачиваются — «Загружено N из M писем» с полосой хода.
        Scene::new("thread-media-partial", partial, partial_ready).wait(wait),
        // `MedFiles`: вкладка «Файлы» списком, пересланный дважды файл — «×2».
        Scene::new("thread-media-files", full, tab_ready).continues().wait(wait),
        // «Медиа» — галереей.
        Scene::new("thread-media-media", tab, tab_ready).continues().wait(wait),
        // `MedLinksTable`: «Ссылки» — таблицей со столбцом «Канал», метки отписки и счётчика переходов.
        Scene::new("thread-media-links", tab, tab_ready).continues().wait(wait),
        // `MedContactsGrid`: «Контакты» — участники и визитка.
        Scene::new("thread-media-contacts", tab, tab_ready).continues().wait(wait),
        // «Встречи»: приглашение беседы и мой ответ меткой.
        Scene::new("thread-media-meetings", tab, tab_ready).continues().wait(wait),
        // `MedEntry`: режим «Беседа» — вкладки под шапкой и строка-сводка над лентой.
        Scene::new("thread-media-entry", entry, entry_ready).continues(),
    ];

    /// Вкладка и вид сцены.
    fn want(s: &Scene) -> (Tab, Kind) {
        match s.name {
            "thread-media-media" => (Tab::Media, Kind::Gallery),
            "thread-media-links" => (Tab::Links, Kind::Table),
            "thread-media-contacts" => (Tab::Contacts, Kind::List),
            "thread-media-meetings" => (Tab::Meetings, Kind::List),
            _ => (Tab::Files, Kind::List),
        }
    }

    /// Вне сцен «Медиа беседы» — режим «Беседа», демо отдаёт все тела.
    pub(crate) fn reset(s: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) {
        if !s.name.starts_with("thread-media-") {
            HOLD.store(false, Ordering::Relaxed);
            m.media.update(cx, |v, cx| v.set_on(false, cx));
        }
    }

    fn open(m: &mut Mailer, cx: &mut Context<Mailer>) -> Result<(), String> {
        m.set_section(Section::Mail, cx);
        let last = postman_demo::media::LETTERS[postman_demo::media::LETTERS.len() - 1].to_string();
        let row = m.cache.rows_by_ids(&[last]).map_err(|e| e.to_string())?.into_iter().next();
        let Some(row) = row else { return Err("no media thread in the demo mailbox".into()) };
        m.open_letter(row, cx);
        Ok(())
    }

    fn partial(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        HOLD.store(true, Ordering::Relaxed);
        open(m, cx)
    }

    fn partial_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        let v = m.media.read(cx);
        // Тела уже в базе (беседу открывали раньше) — собрана целиком: сцена не ждёт докачки, которой не будет.
        v.on && (v.partial() || v.complete())
    }

    /// Остальные тела — из демо-ящика: беседа собрана целиком.
    fn full(s: &Scene, m: &mut Mailer, window: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        HOLD.store(false, Ordering::Relaxed);
        m.media.update(cx, |v, cx| v.refetch(cx));
        tab(s, m, window, cx)
    }

    fn tab(s: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        let (tab, kind) = want(s);
        m.media.update(cx, |v, cx| {
            v.set_on(true, cx);
            v.set_tab(tab, cx);
            v.set_kind(kind, cx);
        });
        Ok(())
    }

    fn tab_ready(s: &Scene, m: &Mailer, cx: &App) -> bool {
        let v = m.media.read(cx);
        v.on && v.tab == want(s).0 && v.complete()
    }

    fn entry(_: &Scene, m: &mut Mailer, _: &mut Window, cx: &mut Context<Mailer>) -> Result<(), String> {
        m.media.update(cx, |v, cx| v.set_on(false, cx));
        Ok(())
    }

    fn entry_ready(_: &Scene, m: &Mailer, cx: &App) -> bool {
        let v = m.media.read(cx);
        !v.on && v.complete() && v.total() > 0
    }

    /// Режим включается, когда беседа собрана первый раз.
    fn wait(s: &Scene, m: &mut Mailer, cx: &mut Context<Mailer>) {
        let (ready, on) = {
            let v = m.media.read(cx);
            (v.ready(), v.on)
        };
        if ready && !on {
            let (tab, kind) = want(s);
            m.media.update(cx, |v, cx| {
                v.set_on(true, cx);
                v.set_tab(tab, cx);
                v.set_kind(kind, cx);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use postman_core::samefile::FileKey;
    use postman_core::thread_media::{Contacts, File, Link, LinkKind, Media, Occurrence, Origin, Person, ThreadMedia};

    use super::{Chip, Kind, Tab, default_kinds, entries};

    fn origin(letter: &str, who: &str, at: i64, channel: &str) -> Origin {
        Origin { letter: letter.into(), from_name: who.into(), from_addr: format!("{letter}@example.test"), at, channel: channel.into() }
    }

    fn file(name: &str, copies: &[Origin]) -> File {
        File {
            key: FileKey::Blob(name.into()),
            name: name.into(),
            mime: "application/pdf".into(),
            size: 2048,
            copies: copies.iter().map(|o| Occurrence { origin: o.clone(), blob_id: name.into(), name: name.into() }).collect(),
        }
    }

    /// Строки вкладок: файл с пересылками — «×N» и список писем; видео — метка; ссылка — домен, метки отписки и
    /// подозрительной; участник — адрес и «я»; канал и письмо — от первой копии.
    #[test]
    fn tabs_become_rows() {
        let tz = jiff::tz::TimeZone::UTC;
        let a = origin("m1", "Мария", 100, "mail");
        let b = origin("m2", "Анна", 200, "telegram");
        let media = ThreadMedia {
            files: vec![file("plan.pdf", &[a.clone(), b.clone()]), file("one.txt", std::slice::from_ref(&a))],
            media: vec![Media { file: file("clip.mp4", std::slice::from_ref(&b)), video: true, inline: false, frame: None }],
            links: vec![
                Link {
                    url: "https://lists.example.test/u".into(),
                    domain: "lists.example.test".into(),
                    text: "Отписаться".into(),
                    kind: LinkKind::Unsubscribe,
                    suspicious: false,
                    seen: vec![a.clone()],
                },
                Link {
                    url: "https://evil.example.test/".into(),
                    domain: "evil.example.test".into(),
                    text: "bank.example.test".into(),
                    kind: LinkKind::Plain,
                    suspicious: true,
                    seen: vec![b.clone()],
                },
            ],
            contacts: Contacts {
                people: vec![Person { name: "Я".into(), email: "me@example.test".into(), letters: 3, sent: 1, me: true }],
                cards: Vec::new(),
            },
            ..ThreadMedia::default()
        };
        let files = entries(&media, Tab::Files, &tz);
        assert_eq!(files[0].chips[0].0, "×2");
        assert_eq!(files[0].copies.len(), 2);
        assert_eq!((files[0].channel.as_str(), files[0].letter.as_deref()), ("mail", Some("m1")));
        assert!(files[1].chips.is_empty() && files[1].copies.is_empty());
        assert!(files[0].sub.starts_with("PDF"));
        let clips = entries(&media, Tab::Media, &tz);
        assert_eq!((clips.len(), clips[0].chips.len(), clips[0].channel.as_str()), (1, 1, "telegram"));
        let links = entries(&media, Tab::Links, &tz);
        assert_eq!(links[0].title, "lists.example.test");
        assert_eq!(links[0].url, Some(("https://lists.example.test/u".into(), false)));
        assert_eq!(links[1].chips.last().map(|c| c.1), Some(Chip::Alert), "подозрительная — красной меткой");
        assert_eq!(links[1].url.as_ref().map(|u| u.1), Some(true));
        let people = entries(&media, Tab::Contacts, &tz);
        assert_eq!((people[0].email.as_deref(), people[0].chips.len()), (Some("me@example.test"), 1));
        assert!(entries(&media, Tab::Meetings, &tz).is_empty());
    }

    /// Вид по умолчанию: у «Медиа» — галерея, у остальных — список.
    #[test]
    fn media_tab_opens_as_gallery() {
        let kinds = default_kinds();
        assert_eq!(kinds[Tab::Media.index()], Kind::Gallery);
        assert!(Tab::ALL.iter().filter(|t| **t != Tab::Media).all(|t| kinds[t.index()] == Kind::List));
    }
}
