//! Учётки каналов в дереве папок (CH-04, DESIGN «Каналы»): разделами под почтовыми — заголовок со значком канала
//! 12 px, внутри «Все беседы» и папки канала (папки Telegram, пространства Matrix) со счётчиками непрочитанного.
//! Данные — снимок бесед (`chats::Snapshot`), выбор — `FolderEvent::Chat`.

use gpui_kit::prelude::*;
use gpui_kit::*;
use pets_i18n::tr;
use pets_ui::theme::Metric;
use postman_core::channel::{ChanAccount, ChannelFolder, ChannelState, FolderRef};

use crate::chats::{Account, Scope, Smart};
use crate::chrome::{glyph, icon};
use crate::folders::{FolderEvent, Folders, chevron, row_h, strip, unread};
use crate::icons;
use crate::roles::Look;

/// Ключ раздела учётки канала в свёрнутых группах дерева.
fn group_key(chan: &ChanAccount) -> String {
    format!("chan:{chan}")
}

fn walk<'a>(folders: &'a [ChannelFolder], parent: Option<&FolderRef>, depth: usize, out: &mut Vec<(&'a ChannelFolder, usize)>) {
    if depth > folders.len() {
        return;
    }
    for f in folders {
        let up = f.parent.as_ref().filter(|p| folders.iter().any(|x| x.id == **p));
        if up == parent && !out.iter().any(|(x, _)| x.id == f.id) {
            out.push((f, depth));
            walk(folders, Some(&f.id), depth + 1, out);
        }
    }
}

/// Папки канала деревом: (папка, глубина); родитель, которого нет в списке, — как корень. Цикл не зацикливает.
pub(crate) fn folder_tree(folders: &[ChannelFolder]) -> Vec<(&ChannelFolder, usize)> {
    let mut out = Vec::with_capacity(folders.len());
    walk(folders, None, 0, &mut out);
    out
}

impl Folders {
    /// Блок «Умные папки»: «Непрочитанное везде», «Упоминания меня», «Закреплённые» и папки людей — беседы всех
    /// каналов и почты.
    pub(crate) fn smart_rows(&self, l: &Look, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let snapshot = self.chats.clone();
        if snapshot.smart.is_empty() {
            return Vec::new();
        }
        let mut rows = vec![
            div()
                .h(px(24.))
                .flex_none()
                .flex()
                .items_end()
                .pl(px(8.))
                .pb(px(2.))
                .text_size(px(12.))
                .text_color(l.text2())
                .child(tr!("chats-smart"))
                .into_any_element(),
        ];
        for (n, (s, _)) in snapshot.smart.iter().enumerate() {
            rows.push(self.smart_row(n, s, l, cx));
        }
        rows.push(div().h(px(1.)).mx(px(8.)).my(px(6.)).flex_none().bg(l.border()).into_any_element());
        rows
    }

    fn smart_row(&self, n: usize, s: &Smart, l: &Look, cx: &mut Context<Self>) -> AnyElement {
        let scope = Scope::Smart(s.clone());
        let selected = self.results.is_none() && self.chat_scope.as_ref() == Some(&scope);
        let counts = self.chats.counts(&scope);
        let mark = match s {
            Smart::Unread => icon(glyph::MAIL, 16.).into_any_element(),
            Smart::Mentions => div().w(px(16.)).text_center().font_weight(FontWeight::SEMIBOLD).child("@").into_any_element(),
            Smart::Pinned => icon(glyph::PIN, 16.).into_any_element(),
            Smart::Person(_) => icon(glyph::CONTACTS, 16.).into_any_element(),
        };
        let group = SharedString::from(format!("smart-{n}"));
        // Папку человека можно убрать: ✕ при наведении.
        let forget = match s {
            Smart::Person(p) => {
                let p = p.clone();
                Some(
                    div()
                        .id(("smart-forget", n))
                        .flex_none()
                        .size(px(20.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .text_color(l.text2())
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .hover(|d| d.bg(l.hover()))
                        .child(icon(glyph::CLOSE, 10.))
                        .tooltip(|window, cx| crate::screens::tip(tr!("chats-smart-forget"), window, cx))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.stop_propagation();
                            cx.emit(FolderEvent::ChatForget(p.clone()));
                        })),
                )
            }
            _ => None,
        };
        // Счётчик — у непрочитанного и упоминаний; у закреплённых и людей он не про папку.
        let count = match s {
            Smart::Unread => counts.unread,
            Smart::Mentions => counts.mentions,
            _ => 0,
        };
        let id = SharedString::from(format!("chat-smart-{n}"));
        div()
            .id(id.clone())
            .children(crate::screens::spot(id))
            .group(group)
            .h(px(row_h(l)))
            .flex_none()
            .pl(px(4.))
            .pr(px(8.))
            .rounded(l.px(Metric::RadiusControl))
            .when(selected, |d| d.relative().bg(l.row_fill(self.focused)).child(strip(l.row_strip(self.focused))))
            .when(!selected, |d| d.hover(|d| d.bg(l.row_hover())))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(div().w(px(12.)).flex_none())
            .child(div().ml(px(-4.)).mr(px(2.)).child(mark))
            .child(div().flex_1().min_w_0().truncate().child(s.title()))
            .children(forget)
            .when(count > 0, |d| unread(d, count, l))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(FolderEvent::Chat(scope.clone()))))
            .into_any_element()
    }

    /// Разделы учёток каналов — после почтовых.
    pub(crate) fn chat_rows(&self, l: &Look, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let snapshot = self.chats.clone();
        let mut rows = Vec::new();
        // Имя учётки в заголовке — только когда учёток канала несколько.
        let many = |a: &Account| snapshot.accounts.iter().filter(|x| x.chan.channel == a.chan.channel).count() > 1;
        for a in &snapshot.accounts {
            let key = group_key(&a.chan);
            let open = !self.collapsed.contains(&key);
            rows.push(self.chat_header(a, many(a), open, l, cx));
            if !open {
                continue;
            }
            rows.push(self.chat_row(Scope::All(a.chan.clone()), tr!("chats-all"), 0, false, l, cx));
            if a.features.folders {
                for (f, depth) in folder_tree(&a.folders) {
                    rows.push(self.chat_row(Scope::Folder(a.chan.clone(), f.id.clone()), f.name.clone(), depth, true, l, cx));
                }
            }
        }
        rows
    }

    fn chat_header(&self, a: &Account, with_name: bool, open: bool, l: &Look, cx: &mut Context<Self>) -> AnyElement {
        let channel = crate::tape::channel_name(&a.chan.channel.0);
        let label = if with_name && !a.name.is_empty() { format!("{channel} · {}", a.name) } else { channel };
        let total = if open { 0 } else { self.chats.counts(&Scope::All(a.chan.clone())).unread };
        let selected = !open && self.chat_scope.as_ref().is_some_and(|s| s.account() == Some(&a.chan));
        // Нет связи с каналом — метка, как у почтовой учётки.
        let lines = cx.try_global::<crate::account_telegram::Lines>();
        let state = lines.and_then(|g| g.0.iter().find(|x| x.account == a.chan)).and_then(|x| x.state.clone());
        let down = matches!(state, Some(ChannelState::Offline { .. } | ChannelState::Problem(_)));
        let mark = down.then(|| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.))
                .h(px(18.))
                .px(px(6.))
                .rounded(px(4.))
                .border_1()
                .border_color(l.stroke())
                .text_size(px(11.))
                .child(div().size(px(6.)).rounded_full().bg(l.text3()))
                .child(tr!("states-account-offline"))
        });
        let key = group_key(&a.chan);
        let id = SharedString::from(format!("chat-acc-{}", a.chan));
        div()
            .id(id.clone())
            .children(crate::screens::spot(id))
            .mt(px(10.))
            .h(px(28.))
            .flex_none()
            .px(px(8.))
            .rounded(l.px(Metric::RadiusControl))
            .when(selected, |d| d.bg(l.c(pets_ui::theme::Role::FillCursor)).font_weight(FontWeight::SEMIBOLD))
            .when(!selected, |d| d.hover(|d| d.bg(l.hover())))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap_1()
            .text_size(l.px(Metric::TypeCaption))
            .text_color(l.text2())
            .child(chevron(open))
            .child(crate::tape::channel_icon(&a.chan.channel.0, 12., l))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .children(mark)
            .when(total > 0, |d| unread(d, total, l))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&key) {
                    this.collapsed.insert(key.clone());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn chat_row(&self, scope: Scope, name: String, depth: usize, folder: bool, l: &Look, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.results.is_none() && self.chat_scope.as_ref() == Some(&scope);
        let counts = self.chats.counts(&scope);
        let id = SharedString::from(match &scope {
            Scope::All(a) => format!("chat-all-{a}"),
            Scope::Folder(a, f) => format!("chat-folder-{a}-{f}"),
            Scope::Smart(_) => String::from("chat-smart"),
        });
        let mark = if folder {
            icons::icon(icons::FOLDER, 20., l.text()).ml(px(-6.)).into_any_element()
        } else {
            icon(glyph::CONVERSATION, 16.).ml(px(-4.)).mr(px(2.)).into_any_element()
        };
        div()
            .id(id.clone())
            .children(crate::screens::spot(id))
            .h(px(row_h(l)))
            .flex_none()
            .pl(px(22. + 16. * depth as f32))
            .pr(px(8.))
            .rounded(l.px(Metric::RadiusControl))
            .when(selected, |d| d.relative().bg(l.row_fill(self.focused)).child(strip(l.row_strip(self.focused))))
            .when(!selected, |d| d.hover(|d| d.bg(l.row_hover())))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(div().w(px(12.)).flex_none())
            .child(mark)
            .child(div().flex_1().min_w_0().truncate().child(name))
            .when(counts.mentions > 0, |d| d.child(div().flex_none().font_weight(FontWeight::SEMIBOLD).text_color(l.accent_text()).child("@")))
            .when(counts.unread > 0, |d| unread(d, counts.unread, l))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(FolderEvent::Chat(scope.clone()))))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use postman_core::channel::{ChannelFolder, FolderRef};

    use super::folder_tree;

    fn folder(id: &str, parent: Option<&str>) -> ChannelFolder {
        ChannelFolder { id: FolderRef::new(id), name: id.into(), parent: parent.map(FolderRef::new), conversations: Vec::new() }
    }

    /// Пространства Matrix — деревом по родителю; родителя нет в списке — корень; цикл не зацикливает.
    #[test]
    fn channel_folders_are_a_tree() {
        let folders = [folder("work", None), folder("team", Some("work")), folder("lost", Some("нет")), folder("deep", Some("team"))];
        let got: Vec<(&str, usize)> = folder_tree(&folders).into_iter().map(|(f, d)| (f.id.0.as_str(), d)).collect();
        assert_eq!(got, [("work", 0), ("team", 1), ("deep", 2), ("lost", 0)]);
        let cycle = [folder("a", Some("b")), folder("b", Some("a"))];
        assert!(folder_tree(&cycle).is_empty(), "цикл без корня — папок в дереве нет");
    }
}
