//! Язык ленты беседы (DESIGN «Беседы» и «Каналы», макет `ThreadChat`): общие части ленты почтовой беседы
//! (`thread.rs`) и ленты чата (`chat_pane.rs`) — аватар с инициалами, подпись дня на линии времени, вложения плашками,
//! значок канала. Лента чата и лента писем собираются из них: вид один, данные — по природе канала (CH-04).

use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;

use crate::chrome::{glyph, icon};
use crate::icons;
use crate::roles::Look;

/// Круг с инициалами (нейтральный цвет, как в шапке письма).
pub(crate) fn avatar(name: &str, size: f32, text: f32, l: &Look) -> Div {
    let c = l.cat(postman_core::palette::NEUTRAL);
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(c.bg)
        .text_color(c.text)
        .text_size(px(text))
        .child(crate::list::initials(name))
}

/// Подпись дня: «Сегодня», «Вчера», иначе дата.
pub(crate) fn day_label(day: jiff::civil::Date, today: jiff::civil::Date) -> String {
    if day == today {
        tr!("thread-today")
    } else if today.yesterday().is_ok_and(|y| y == day) {
        tr!("thread-yesterday")
    } else {
        day.strftime("%d.%m.%Y").to_string()
    }
}

/// Разделитель дня на линии времени.
pub(crate) fn day_row(label: String, l: &Look) -> Div {
    div().flex().justify_center().pt_1().text_color(l.text2()).child(label)
}

/// Вложения сообщения плашками: скрепка, имя, размер (0 — размер неизвестен, без него).
pub(crate) fn att_chips(atts: &[(String, u64)], l: &Look) -> Div {
    div().flex().flex_wrap().gap(px(6.)).pt_1().children(atts.iter().map(|(name, size)| {
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(24.))
            .px(px(8.))
            .rounded(px(4.))
            .border_1()
            .border_color(l.border())
            .text_size(px(12.))
            .child(icon(glyph::ATTACH, 12.).text_color(l.text2()))
            .child(div().max_w(px(220.)).truncate().child(name.clone()))
            .when(*size > 0, |d| d.child(div().text_color(l.text2()).child(crate::attachments::size_text(*size))))
    }))
}

/// Значок канала (DESIGN «Каналы»): почта — `mail`, Telegram — `send`, Matrix и остальные — беседы; серый.
pub(crate) fn channel_icon(channel: &str, size: f32, l: &Look) -> AnyElement {
    match channel {
        postman_core::channel::MAIL => icons::icon(icons::MAIL, size, l.text2()).into_any_element(),
        crate::account_telegram::TELEGRAM => icons::icon(icons::SEND, size, l.text2()).into_any_element(),
        _ => icon(glyph::CONVERSATION, size - 2.).text_color(l.text2()).into_any_element(),
    }
}

/// Имя канала для подписей («Telegram · группа · 4 участника», заголовок раздела дерева).
pub(crate) fn channel_name(channel: &str) -> String {
    match channel {
        postman_core::channel::MAIL => tr!("chats-channel-mail"),
        crate::account_telegram::TELEGRAM => tr!("tg-channel"),
        crate::account_matrix::MATRIX => tr!("mx-channel"),
        postman_demo::channel::ID => tr!("chats-channel-demo"),
        other => other.to_owned(),
    }
}

/// Время сообщения (Unix, мс) — «ЧЧ:ММ» в поясе системы.
pub(crate) fn clock(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string()).unwrap_or_default()
}

/// День сообщения (Unix, мс) в поясе системы.
pub(crate) fn day_of(ms: i64) -> Option<jiff::civil::Date> {
    jiff::Timestamp::from_millisecond(ms).ok().map(|t| t.to_zoned(jiff::tz::TimeZone::system()).date())
}
