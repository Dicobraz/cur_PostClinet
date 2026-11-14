//! Список бесед канала (CH-04, DESIGN «Каналы»): строка той же высоты, что строка письма (`list::row_height`) —
//! аватар 32 (в смешанных папках — со значком канала в углу), название, «закреплено» и «заглушено», время; ниже
//! «Автор: последнее сообщение» в две строки, справа «@» (упоминание меня) и счётчик непрочитанного (заглушённой
//! беседы — серый). Закреплённые — группой «Закреплённые» сверху.

use gpui_kit::component::scroll::ScrollbarMode;
use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;
use pets_ui::theme::Metric;
use postman_core::channel::Address;
use postman_core::channel::smart::COUNT_CAP;

use crate::chats::Row;
use crate::chrome::{glyph, icon};
use crate::roles::{Look, look};

pub(crate) enum ListEvent {
    /// Открыть беседу в области чтения.
    Open(Address),
}

impl EventEmitter<ListEvent> for ChatList {}

/// Что стоит в строке списка: заголовок группы или беседа (номер в `rows`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    Pinned,
    Others,
    Row(usize),
}

/// Строки списка по порядку: «Закреплённые» над закреплёнными, остальные — под своей подписью (только если
/// закреплённые есть).
pub(crate) fn entries(rows: &[Row]) -> Vec<Entry> {
    let pinned = rows.iter().filter(|r| r.item.pinned).count();
    let mut out = Vec::with_capacity(rows.len() + 2);
    if pinned > 0 {
        out.push(Entry::Pinned);
    }
    for (i, r) in rows.iter().enumerate() {
        if pinned > 0 && i == pinned && !r.item.pinned {
            out.push(Entry::Others);
        }
        out.push(Entry::Row(i));
    }
    out
}

/// Счётчик строкой: больше предела ядра — «999+».
pub(crate) fn count_text(n: u32) -> String {
    if n >= COUNT_CAP { format!("{COUNT_CAP}+") } else { n.to_string() }
}

pub(crate) struct ChatList {
    rows: Vec<Row>,
    entries: Vec<Entry>,
    pub(crate) selected: Option<Address>,
    /// Смешанная папка (беседы разных каналов): значок канала в углу аватара.
    pub(crate) mixed: bool,
    list: ListState,
    /// Экран почты в фокусе (выделенная строка — тоном акцента).
    pub(crate) focused: bool,
    /// Пустое состояние: ключи заголовка и фразы — по тому, что показано (папка канала, умная папка).
    pub(crate) empty: (&'static str, &'static str),
}

impl ChatList {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            entries: Vec::new(),
            selected: None,
            mixed: false,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            focused: true,
            empty: ("chats-empty-list-title", "chats-empty-list-text"),
        }
    }

    pub(crate) fn set_rows(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) {
        if self.rows == rows {
            return;
        }
        let entries = entries(&rows);
        // Тот же состав строк — без сброса прокрутки (пришло сообщение, сменился счётчик).
        if entries.len() == self.entries.len() {
            self.list.splice(0..entries.len(), entries.len());
        } else {
            self.list.reset(entries.len());
        }
        self.rows = rows;
        self.entries = entries;
        cx.notify();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Первая беседа списка (сцены).
    pub(crate) fn first(&self) -> Option<Address> {
        self.rows.first().map(|r| r.at().clone())
    }

    /// Шаг по беседам (клавиши списка): выбранная — в видимую область; беседа не выбрана — первая.
    pub(crate) fn step(&mut self, to: crate::chats::Step, cx: &mut Context<Self>) -> Option<Address> {
        use crate::chats::Step;
        let last = self.rows.len().checked_sub(1)?;
        let now = self.selected.as_ref().and_then(|at| self.rows.iter().position(|r| r.at() == at));
        let next = match (to, now) {
            (Step::First, _) | (Step::By(_), None) => 0,
            (Step::Last, _) => last,
            (Step::By(d), Some(i)) => i.saturating_add_signed(d).min(last),
        };
        if now == Some(next) {
            return None;
        }
        let at = self.rows[next].at().clone();
        self.selected = Some(at.clone());
        if let Some(ix) = self.entries.iter().position(|e| *e == Entry::Row(next)) {
            self.list.scroll_to_reveal_item(ix);
        }
        cx.notify();
        Some(at)
    }

    fn group(&self, key: &'static str, l: &Look) -> AnyElement {
        div()
            .h(px(28.))
            .w_full()
            .flex()
            .items_end()
            .px(px(14.))
            .pb(px(4.))
            .border_b_1()
            .border_color(l.border())
            .text_size(l.px(Metric::TypeCaption))
            .text_color(l.text2())
            .child(tr!(key))
            .into_any_element()
    }

    fn row(&self, i: usize, l: &Look, cx: &mut Context<Self>) -> AnyElement {
        let Some(r) = self.rows.get(i) else { return div().into_any_element() };
        let c = &r.item.summary.conversation;
        let lit = self.selected.as_ref() == Some(r.at());
        let unread = r.item.counts.unread;
        let roomy = !l.compact();
        let title = r.title();
        let face = crate::tape::avatar(&title, 32., 12., l);
        // Значок канала 16 px в углу аватара — только в смешанных папках.
        let face = div().relative().flex_none().size(px(32.)).child(face).when(self.mixed, |d| {
            d.child(
                div()
                    .absolute()
                    .right(px(-4.))
                    .bottom(px(-4.))
                    .size(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(l.panel())
                    .child(crate::tape::channel_icon(&r.at().channel.0, 12., l)),
            )
        });
        let time = if r.item.summary.last > 0 { crate::format_date(r.item.summary.last / 1000) } else { String::new() };
        let line1 = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(div().flex_1().min_w_0().truncate().when(unread > 0, |d| d.font_weight(FontWeight::SEMIBOLD)).child(title))
            .when(c.encrypted, |d| d.child(icon(glyph::LOCK, 12.).text_color(l.text3())))
            .when(r.item.pinned, |d| d.child(icon(glyph::PIN, 12.).text_color(l.text3())))
            .when(r.item.muted, |d| d.child(icon(glyph::MUTED, 12.).text_color(l.text3())))
            .child(div().flex_none().text_size(l.px(Metric::TypeCaption)).text_color(l.text2()).child(time));
        // Счётчик — заливка акцентом, у заглушённой беседы серый; «@» — упоминание меня.
        let badge = (unread > 0).then(|| {
            div()
                .flex_none()
                .h(px(18.))
                .min_w(px(18.))
                .px(px(5.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_size(px(11.))
                .when(r.item.muted, |d| d.bg(l.fill()).text_color(l.text2()))
                .when(!r.item.muted, |d| d.bg(l.accent()).text_color(l.on_accent()))
                .child(count_text(unread))
        });
        let mention = (r.item.counts.mentions > 0)
            .then(|| div().flex_none().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(l.accent_text()).child("@"));
        // Чат — «Автор: последнее сообщение» в две строки; письмо — тема и начало текста, по строке.
        let caption = || div().text_size(l.px(Metric::TypeCaption)).line_height(px(18.)).text_color(l.text2());
        let text = if r.is_mail() {
            let c = &r.item.summary;
            div()
                .flex_1()
                .min_w_0()
                .child(caption().truncate().text_color(l.text()).child(c.conversation.title.clone()))
                .when(roomy, |d| d.child(caption().truncate().child(c.preview.clone())))
        } else {
            div().flex_1().min_w_0().child(caption().when(roomy, |d| d.line_clamp(2)).when(!roomy, |d| d.truncate()).child(r.last_line()))
        };
        let last = div().flex().items_start().gap(px(6.)).child(text).children(mention).children(badge);
        let at = r.at().clone();
        let id = SharedString::from(format!("chat-row-{i}"));
        div()
            .id(id.clone())
            .children(crate::screens::spot(id))
            .relative()
            .w_full()
            .h(px(crate::list::row_height(l.density())))
            .flex()
            .gap(px(10.))
            .pr(px(12.))
            .border_b_1()
            .border_color(l.border())
            .when(lit, |d| d.bg(l.row_fill(self.focused)))
            .when(!lit, |d| d.hover(|d| d.bg(l.row_hover())))
            .cursor_pointer()
            .child(div().w(px(4.)).h_full().flex_none().rounded_r(px(2.)).when(lit, |d| d.bg(l.row_strip(self.focused))))
            .when(roomy, |d| d.child(div().flex_none().flex().items_center().child(face)))
            .child(div().flex_1().min_w_0().flex().flex_col().justify_center().gap(px(2.)).line_height(px(18.)).child(line1).child(last))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(ListEvent::Open(at.clone()))))
            .into_any_element()
    }

    fn item(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let l = look(cx);
        match self.entries.get(ix) {
            Some(Entry::Pinned) => self.group("chats-group-pinned", &l),
            Some(Entry::Others) => self.group("chats-group-others", &l),
            Some(Entry::Row(i)) => self.row(*i, &l, cx),
            None => div().into_any_element(),
        }
    }
}

impl Render for ChatList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let l = look(cx);
        if self.rows.is_empty() {
            return crate::chat_pane::empty_state(glyph::CONVERSATION, tr!(self.empty.0), tr!(self.empty.1), &l).into_any_element();
        }
        let rows = list(self.list.clone(), cx.processor(|this, ix: usize, _window, cx| this.item(ix, cx))).size_full();
        div()
            .id("chat-list")
            .relative()
            .size_full()
            .text_color(l.text())
            .child(rows)
            .child(crate::chrome::vbar(&self.list, ScrollbarMode::Hover))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use postman_core::channel::smart::Item;
    use postman_core::channel::{AccountId, Address, ChannelId, Conversation, ConversationId, Counts, Summary};

    use super::{Entry, count_text, entries};
    use crate::chats::Row;

    fn row(id: &str, pinned: bool) -> Row {
        let address = Address::conversation(&ChannelId::new("demo"), &AccountId::new("me@example.test"), &ConversationId::new(id));
        let summary = Summary { address, conversation: Conversation::default(), last: 0, preview: String::new(), author: None };
        Row { item: Item { summary, counts: Counts::default(), muted: false, pinned }, author: String::new(), mine: false, poll: String::new() }
    }

    /// Закреплённые — группой сверху; без закреплённых — без подписей групп.
    #[test]
    fn pinned_conversations_are_grouped_on_top() {
        let rows = [row("a", true), row("b", true), row("c", false)];
        assert_eq!(entries(&rows), [Entry::Pinned, Entry::Row(0), Entry::Row(1), Entry::Others, Entry::Row(2)]);
        let plain = [row("a", false), row("b", false)];
        assert_eq!(entries(&plain), [Entry::Row(0), Entry::Row(1)]);
        let only = [row("a", true)];
        assert_eq!(entries(&only), [Entry::Pinned, Entry::Row(0)]);
    }

    #[test]
    fn counter_is_capped() {
        assert_eq!(count_text(7), "7");
        assert_eq!(count_text(999), "999+");
        assert_eq!(count_text(5000), "999+");
    }
}
