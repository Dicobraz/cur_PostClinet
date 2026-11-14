//! «Настройки → Каналы» (CH-04, DESIGN «Каналы», макеты `ChSettings`, `ChLogin`): учётки каналов строками (состояние
//! точкой, включатель канала, «Выйти»), «Подключить» — карточкой входа на месте (`account_telegram.rs`,
//! `account_matrix.rs`); ниже «Политика организации» — строки с замком и меткой «задано организацией», без управления;
//! справа — какие умные папки показывать в дереве.

use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;
use pets_ui::theme::Metric;
use postman_core::channel::ChannelState;
use postman_core::channel::e2e::E2eStore;

use crate::chats::Smart;
use crate::chrome::{glyph, icon};
use crate::roles::Look;
use crate::settings_tab::{SettingsEvent, SettingsView, aside_card, group, toggle};

/// Какие умные папки показаны в дереве — настройкам (пишет экран почты по снимку бесед).
#[derive(Clone, Copy)]
pub(crate) struct SmartShown(pub(crate) [bool; 3]);

impl Global for SmartShown {}

/// Точка состояния учётки: в сети — зелёная, проблема — предупреждение, иначе серая.
pub(crate) fn state_dot(state: Option<&ChannelState>, l: &Look) -> Div {
    let color = match state {
        Some(ChannelState::Online) => l.success(),
        Some(ChannelState::Problem(_)) => l.warning(),
        _ => l.text3(),
    };
    div().size(px(8.)).flex_none().rounded_full().bg(color)
}

/// Строка учётки канала: точка состояния, имя и состояние словами, справа — действия.
pub(crate) fn account_row(state: Option<&ChannelState>, name: String, state_text: String, actions: impl IntoElement, l: &Look) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .py_3()
        .border_b_1()
        .border_color(l.border())
        .child(state_dot(state, l))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(div().truncate().child(name))
                .child(div().text_size(px(12.)).text_color(l.text2()).child(state_text)),
        )
        .child(actions)
}

/// Что задала организация о каналах: название ключа политики словами и его значение.
pub(crate) fn policy_lines(p: &crate::security::Policy) -> Vec<(String, String)> {
    let allowed = |on: bool| tr!(if on { "chats-policy-on" } else { "chats-policy-off" });
    let mut out = Vec::new();
    if let Some(on) = p.telegram {
        out.push((tr!("tg-channel"), allowed(on)));
    }
    if let Some(on) = p.matrix {
        out.push((tr!("mx-channel"), allowed(on)));
    }
    if let Some(mode) = p.encrypted {
        let value = tr!(match mode {
            E2eStore::Search => "mx-e2e-search",
            E2eStore::Store => "mx-e2e-store",
            E2eStore::Memory => "mx-e2e-memory",
        });
        out.push((tr!("mx-e2e"), value));
    }
    out
}

impl SettingsView {
    /// Раздел «Каналы»: слева учётки и политика, справа — умные папки.
    pub(crate) fn channels_page(&mut self, l: &Look, cx: &mut Context<Self>) -> (Option<Stateful<Div>>, AnyElement, Vec<Div>) {
        let policy = crate::security::policy();
        let telegram = crate::account_telegram::BUILT && !crate::account_telegram::forbidden();
        let matrix = crate::account_matrix::BUILT && !crate::account_matrix::forbidden();
        let mut body = div().flex().flex_col();
        let prefs = crate::prefs::get(cx);
        let off = !crate::account_telegram::enabled(&prefs) && !crate::account_matrix::enabled(&prefs);
        if telegram && off {
            // Пустое состояние (макет `ChEmpty`): каналы выключены — одна фраза и «Включить».
            body = body.child(div().py_4().child(
                crate::chat_pane::empty_state(glyph::CONVERSATION, tr!("chats-off-title"), tr!("chats-off-text"), l).child(
                    crate::buttons::button("channels-turn-on", tr!("chats-turn-on"), true, l).mt_2().on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SettingsEvent::Telegram(crate::account_telegram::PanelEvent::Toggled(true)));
                    })),
                ),
            ));
        }
        if telegram || matrix {
            body = body.child(self.channels.clone()).child(self.matrix.clone());
        } else {
            // Каналов нет: не собраны или запрещены организацией.
            body = body.child(div().h(px(220.)).child(crate::chat_pane::empty_state(
                glyph::CONVERSATION,
                tr!("chats-none-title"),
                tr!("chats-none-text"),
                l,
            )));
        }
        let lines = policy_lines(&policy);
        if !lines.is_empty() {
            body = body.child(group(tr!("chats-policy"), l));
            for (name, value) in lines {
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .py_3()
                        .border_b_1()
                        .border_color(l.border())
                        .child(icon(glyph::LOCK, 14.).text_color(l.text2()))
                        .child(div().flex_1().min_w_0().child(name).child(div().text_size(px(12.)).text_color(l.text2()).child(value)))
                        .child(
                            div()
                                .flex_none()
                                .h(px(20.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .rounded(px(4.))
                                .bg(l.fill())
                                .text_size(px(12.))
                                .text_color(l.text2())
                                .child(tr!("chats-policy-set")),
                        ),
                );
            }
        }
        // Справа — умные папки дерева: три постоянные, включателями.
        let shown = cx.try_global::<SmartShown>().map_or([true; 3], |s| s.0);
        let mut smart = aside_card(tr!("chats-smart"), vec![tr!("chats-smart-hint")], l);
        for (i, s) in Smart::FIXED.iter().enumerate() {
            let on = shown[i];
            smart = smart.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .h(l.px(Metric::HMenuItem))
                    .child(div().flex_1().min_w_0().truncate().child(s.title()))
                    .child(toggle(("smart-shown", i), on, l).on_click(cx.listener(move |_, _, _, cx| cx.emit(SettingsEvent::Smart(i, !on))))),
            );
        }
        (None, body.into_any_element(), vec![smart])
    }
}

#[cfg(test)]
mod tests {
    use postman_core::channel::e2e::E2eStore;

    use super::policy_lines;
    use crate::security::Policy;

    /// «Политика организации»: строка на каждый заданный ключ; ничего не задано — блока нет.
    #[test]
    fn policy_block_lists_only_set_keys() {
        // Тексты — ключами Fluent (переводов в тесте нет): сверяем состав.
        assert!(policy_lines(&Policy::default()).is_empty());
        let p = Policy { telegram: Some(false), encrypted: Some(E2eStore::Memory), ..Policy::default() };
        assert_eq!(policy_lines(&p).len(), 2);
        let all = Policy { telegram: Some(true), matrix: Some(true), encrypted: Some(E2eStore::Search), ..Policy::default() };
        assert_eq!(policy_lines(&all).len(), 3);
    }
}
