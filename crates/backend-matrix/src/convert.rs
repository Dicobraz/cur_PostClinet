//! События Matrix ↔ общая модель (ADR 0012 «Проверка договора», Matrix): `m.room.message` → сообщение (тело —
//! `formatted_body` как `Body::Html`, IR делает ядро в воркере; иначе `Body::Plain`; запасные цитаты ответа
//! `mx-reply` отрезаются), `m.replace` → правка того же сообщения, `m.redaction` → удаление у всех, `m.annotation` →
//! реакции, `m.room.encrypted` без ключа → `Undecryptable`, участники и название комнаты → служебные строки.
//! Обратно: IR → `body` и `formatted_body` (`to_markdown` → `markdown_to_html`), упоминания — ссылками `matrix.to`
//! и `m.mentions`; цитата фрагмента (её в протоколе нет) — блоком цитаты в теле.

use matrix_sdk::deserialized_responses::{TimelineEvent, TimelineEventKind};
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::message::{
    FileInfo, FileMessageEventContent, FormattedBody, MessageFormat, MessageType, Relation, RoomMessageEventContent,
    RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::events::room::{ImageInfo, MediaSource};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, Mentions, SyncMessageLikeEvent, SyncStateEvent};
use matrix_sdk::ruma::{OwnedEventId, OwnedUserId, UInt, UserId};
use postman_core::channel::{
    Attachment, AttachmentKind, AttachmentSource, Body, Message, MessageId, MessageKind, ParticipantId, Quote, ServiceKind, Uploaded, doc_text,
};
use postman_ir::{Align, Block, Document, Inline};

/// Что значит событие ленты для канала.
#[derive(Debug, Clone)]
pub enum Item {
    /// Сообщение целиком (новое или уже с правкой из `unsigned`).
    Message(Message),
    /// Правка сообщения `target` в `at` (мс): новое содержимое.
    Edit { target: OwnedEventId, at: i64, sender: OwnedUserId, content: Box<RoomMessageEventContentWithoutRelation> },
    /// Удалено у всех (`m.redaction`): что удалено.
    Redact { redacts: OwnedEventId },
    /// Реакция `key` на `target` от `sender`; `id` — событие реакции (его удалят, когда реакцию снимут).
    React { id: OwnedEventId, target: OwnedEventId, key: String, sender: OwnedUserId },
    /// Не расшифровалось: сообщение-заглушка и сырое событие для повтора, когда придёт ключ.
    Undecryptable { message: Message, raw: String },
}

fn ms(ts: matrix_sdk::ruma::MilliSecondsSinceUnixEpoch) -> i64 {
    i64::from(ts.0).max(1)
}

fn uint(v: Option<UInt>) -> u32 {
    v.map_or(0, |x| u32::try_from(u64::from(x)).unwrap_or(u32::MAX))
}

/// Источник байтов для ядра: `MediaSource` (адрес `mxc://` и, в шифрованной комнате, ключ файла) в JSON.
pub fn source(s: &MediaSource) -> AttachmentSource {
    AttachmentSource(serde_json::to_vec(s).unwrap_or_default())
}

/// Обратно к [`source`]; чужое — `None`.
pub fn media(s: &AttachmentSource) -> Option<MediaSource> {
    serde_json::from_slice(&s.0).ok()
}

/// `formatted_body` без запасной цитаты ответа (`<mx-reply>…</mx-reply>`).
pub fn strip_reply_html(html: &str) -> String {
    match (html.find("<mx-reply>"), html.find("</mx-reply>")) {
        (Some(a), Some(b)) if b > a => format!("{}{}", &html[..a], &html[b + "</mx-reply>".len()..]),
        _ => html.to_owned(),
    }
}

/// `body` без запасной цитаты ответа: строки `> …` в начале и пустая строка после них.
pub fn strip_reply_plain(body: &str) -> String {
    if !body.starts_with("> ") {
        return body.to_owned();
    }
    let mut lines = body.lines().skip_while(|l| l.starts_with('>'));
    let rest: Vec<&str> = lines.by_ref().collect();
    let rest = rest.join("\n");
    rest.trim_start_matches('\n').to_owned()
}

/// Тело текста: разметка — HTML (IR сделает ядро), без неё — простой текст.
fn text_body(body: &str, formatted: Option<&FormattedBody>, reply: bool) -> Body {
    let plain = if reply { strip_reply_plain(body) } else { body.to_owned() };
    match formatted {
        Some(f) if f.format == MessageFormat::Html => {
            let html = if reply { strip_reply_html(&f.body) } else { f.body.clone() };
            Body::Html { html, plain }
        }
        _ => Body::Plain(plain),
    }
}

/// Подпись к файлу (Matrix 1.10): `body` — подпись, если задано `filename` и оно другое.
fn caption(body: &str, filename: Option<&str>, formatted: Option<&FormattedBody>) -> (String, Body) {
    match filename {
        Some(f) if f != body => (f.to_owned(), text_body(body, formatted, false)),
        _ => (body.to_owned(), Body::default()),
    }
}

/// Тело и вложения сообщения.
pub fn body_of(t: &MessageType, reply: bool) -> (Body, Vec<Attachment>) {
    match t {
        MessageType::Text(c) => (text_body(&c.body, c.formatted.as_ref(), reply), Vec::new()),
        MessageType::Notice(c) => (text_body(&c.body, c.formatted.as_ref(), reply), Vec::new()),
        MessageType::Emote(c) => (text_body(&format!("* {}", c.body), None, reply), Vec::new()),
        MessageType::Image(c) => {
            let (name, body) = caption(&c.body, c.filename.as_deref(), c.formatted.as_ref());
            let info = c.info.as_deref();
            let a = Attachment {
                name,
                mime: info.and_then(|i| i.mimetype.clone()).unwrap_or_else(|| "image/*".into()),
                size: info.and_then(|i| i.size).map(u64::from),
                kind: AttachmentKind::Image { w: uint(info.and_then(|i| i.width)), h: uint(info.and_then(|i| i.height)) },
                source: source(&c.source),
                thumb: info.and_then(|i| i.thumbnail_source.as_ref()).map(source),
            };
            (body, vec![a])
        }
        MessageType::File(c) => {
            let (name, body) = caption(&c.body, c.filename.as_deref(), c.formatted.as_ref());
            let info = c.info.as_deref();
            let a = Attachment {
                name,
                mime: info.and_then(|i| i.mimetype.clone()).unwrap_or_else(|| "application/octet-stream".into()),
                size: info.and_then(|i| i.size).map(u64::from),
                kind: AttachmentKind::File,
                source: source(&c.source),
                thumb: info.and_then(|i| i.thumbnail_source.as_ref()).map(source),
            };
            (body, vec![a])
        }
        MessageType::Video(c) => {
            let (name, body) = caption(&c.body, c.filename.as_deref(), c.formatted.as_ref());
            let info = c.info.as_deref();
            let a = Attachment {
                name,
                mime: info.and_then(|i| i.mimetype.clone()).unwrap_or_else(|| "video/*".into()),
                size: info.and_then(|i| i.size).map(u64::from),
                kind: AttachmentKind::Video {
                    w: uint(info.and_then(|i| i.width)),
                    h: uint(info.and_then(|i| i.height)),
                    secs: info.and_then(|i| i.duration).map_or(0, |d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX)),
                },
                source: source(&c.source),
                thumb: info.and_then(|i| i.thumbnail_source.as_ref()).map(source),
            };
            (body, vec![a])
        }
        MessageType::Audio(c) => {
            let (name, body) = caption(&c.body, c.filename.as_deref(), c.formatted.as_ref());
            let info = c.info.as_deref();
            let secs = info.and_then(|i| i.duration).map_or(0, |d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX));
            let voice = c.voice.is_some();
            let a = Attachment {
                name,
                mime: info.and_then(|i| i.mimetype.clone()).unwrap_or_else(|| "audio/*".into()),
                size: info.and_then(|i| i.size).map(u64::from),
                kind: if voice { AttachmentKind::Voice { secs } } else { AttachmentKind::Audio { secs } },
                source: source(&c.source),
                thumb: None,
            };
            (body, vec![a])
        }
        other => (Body::Plain(other.body().to_owned()), Vec::new()),
    }
}

/// Содержимое сообщения (новое или из правки) — в сообщение.
fn fill(m: &mut Message, t: &MessageType, mentions: Option<&Mentions>, reply: bool) {
    let (body, attachments) = body_of(t, reply);
    m.body = body;
    m.attachments = attachments;
    m.mentions = mentions.map(|x| x.user_ids.iter().map(|u| ParticipantId::new(u.as_str())).collect()).unwrap_or_default();
}

/// Правка — в сообщение: новое содержимое и время правки.
pub fn apply_edit(m: &mut Message, content: &RoomMessageEventContentWithoutRelation, at: i64) {
    fill(m, &content.msgtype, content.mentions.as_ref(), false);
    m.edited = Some(at.max(m.sent));
}

fn base(id: &str, sender: &UserId, ts: i64, me: &UserId) -> Message {
    Message { id: MessageId::new(id), author: ParticipantId::new(sender.as_str()), sent: ts, mine: sender == me, ..Message::default() }
}

/// Сообщение `m.room.message` (не правка).
fn message(e: &matrix_sdk::ruma::events::room::message::OriginalSyncRoomMessageEvent, me: &UserId) -> Message {
    let mut m = base(e.event_id.as_str(), &e.sender, ms(e.origin_server_ts), me);
    let mut reply = false;
    match &e.content.relates_to {
        Some(Relation::Reply(r)) => {
            m.reply_to = Some(MessageId::new(r.in_reply_to.event_id.as_str()));
            reply = true;
        }
        Some(Relation::Thread(t)) => {
            m.thread = Some(MessageId::new(t.event_id.as_str()));
            if let Some(r) = t.in_reply_to.as_ref().filter(|_| !t.is_falling_back) {
                m.reply_to = Some(MessageId::new(r.event_id.as_str()));
                reply = true;
            }
        }
        _ => {}
    }
    fill(&mut m, &e.content.msgtype, e.content.mentions.as_ref(), reply);
    // Правку сервер может приложить к самому сообщению (`unsigned.m.relations.m.replace`).
    if let Some(r) = &e.unsigned.relations.replace
        && let Some(Relation::Replacement(rep)) = &r.content.relates_to
    {
        apply_edit(&mut m, &rep.new_content, ms(r.origin_server_ts));
    }
    m
}

fn service(id: &str, sender: &UserId, ts: i64, me: &UserId, kind: ServiceKind) -> Item {
    let mut m = base(id, sender, ts, me);
    m.kind = MessageKind::Service(kind);
    Item::Message(m)
}

/// Событие ленты → [`Item`]; служебное без смысла для ленты (состояние прав, ключи) — `None`.
pub fn item(ev: &TimelineEvent, me: &UserId) -> Option<Item> {
    let utd = matches!(ev.kind, TimelineEventKind::UnableToDecrypt { .. });
    let parsed = ev.raw().deserialize().ok()?;
    if utd {
        let mut m = base(parsed.event_id().as_str(), parsed.sender(), ms(parsed.origin_server_ts()), me);
        m.kind = MessageKind::Undecryptable;
        return Some(Item::Undecryptable { message: m, raw: ev.raw().json().get().to_owned() });
    }
    match parsed {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(e))) => match &e.content.relates_to {
            // Правку чужого сообщения сессия не принимает: автор правки — автор сообщения.
            Some(Relation::Replacement(r)) => Some(Item::Edit {
                target: r.event_id.clone(),
                at: ms(e.origin_server_ts),
                sender: e.sender.clone(),
                content: Box::new(r.new_content.clone()),
            }),
            _ => Some(Item::Message(message(&e, me))),
        },
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Sticker(SyncMessageLikeEvent::Original(e))) => {
            let mut m = base(e.event_id.as_str(), &e.sender, ms(e.origin_server_ts), me);
            let src = match &e.content.source {
                matrix_sdk::ruma::events::sticker::StickerMediaSource::Plain(u) => MediaSource::Plain(u.clone()),
                matrix_sdk::ruma::events::sticker::StickerMediaSource::Encrypted(f) => MediaSource::Encrypted(f.clone()),
                _ => return None,
            };
            m.attachments = vec![Attachment {
                name: e.content.body.clone(),
                mime: e.content.info.mimetype.clone().unwrap_or_else(|| "image/*".into()),
                size: e.content.info.size.map(u64::from),
                kind: AttachmentKind::Sticker,
                source: source(&src),
                thumb: None,
            }];
            Some(Item::Message(m))
        }
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(
            matrix_sdk::ruma::events::room::redaction::SyncRoomRedactionEvent::Original(e),
        )) => {
            let redacts = e.content.redacts.clone().or(e.redacts.clone())?;
            Some(Item::Redact { redacts })
        }
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Reaction(SyncMessageLikeEvent::Original(e))) => Some(Item::React {
            id: e.event_id.clone(),
            target: e.content.relates_to.event_id.clone(),
            key: e.content.relates_to.key.clone(),
            sender: e.sender.clone(),
        }),
        AnySyncTimelineEvent::State(s) => state_item(&s, me),
        _ => None,
    }
}

/// Строки ленты о составе и названии комнаты.
fn state_item(s: &AnySyncStateEvent, me: &UserId) -> Option<Item> {
    let (id, sender, ts) = (s.event_id().as_str(), s.sender(), ms(s.origin_server_ts()));
    match s {
        AnySyncStateEvent::RoomCreate(_) => Some(service(id, sender, ts, me, ServiceKind::Created)),
        AnySyncStateEvent::RoomName(SyncStateEvent::Original(e)) => Some(service(id, sender, ts, me, ServiceKind::Renamed(e.content.name.clone()))),
        AnySyncStateEvent::RoomMember(SyncStateEvent::Original(e)) => {
            let who = ParticipantId::new(e.state_key.as_str());
            let before = e.unsigned.prev_content.as_ref().map(|p| p.membership.clone());
            match (&e.content.membership, before) {
                (MembershipState::Join, Some(MembershipState::Join)) => None,
                (MembershipState::Join, _) => Some(service(id, sender, ts, me, ServiceKind::Joined(who))),
                (MembershipState::Leave | MembershipState::Ban, Some(MembershipState::Join)) => {
                    Some(service(id, sender, ts, me, ServiceKind::Left(who)))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Документ с цитатами фрагментов впереди — блоками цитаты (в протоколе цитаты фрагмента нет).
fn with_quotes(doc: &Document, quotes: &[Quote]) -> Document {
    if quotes.is_empty() {
        return doc.clone();
    }
    let mut d = doc.clone();
    let mut blocks: Vec<Block> = quotes
        .iter()
        .map(|q| Block::Quote(q.text.lines().map(|l| Block::Paragraph { align: Align::Start, content: vec![Inline::text(l)] }).collect()))
        .collect();
    blocks.append(&mut d.blocks);
    d.blocks = blocks;
    d
}

/// Упоминания — ссылками `matrix.to` (так их показывают клиенты Matrix).
fn pills(blocks: &mut [Block]) {
    fn inlines(v: &mut [Inline]) {
        for i in v {
            match i {
                Inline::Mention { id, text } => {
                    *i = Inline::Link {
                        href: crate::links::room(id, None),
                        host: Some("matrix.to".into()),
                        suspicious: false,
                        content: vec![Inline::text(text.clone())],
                    };
                }
                Inline::Link { content, .. } => inlines(content),
                _ => {}
            }
        }
    }
    for b in blocks {
        match b {
            Block::Paragraph { content, .. } | Block::Heading { content, .. } => inlines(content),
            Block::Quote(inner) => pills(inner),
            Block::List { items, .. } => items.iter_mut().for_each(|i| pills(i)),
            Block::Table(rows) => rows.iter_mut().flatten().for_each(|c| inlines(c)),
            Block::Code(_) | Block::Rule | Block::Figure => {}
        }
    }
}

/// Есть ли в документе разметка (иначе — только `body`).
fn rich(d: &Document) -> bool {
    fn inl(v: &[Inline]) -> bool {
        v.iter().any(|i| match i {
            Inline::Text { marks, .. } => *marks != postman_ir::Marks::default(),
            Inline::LineBreak => false,
            _ => true,
        })
    }
    d.blocks.iter().any(|b| match b {
        Block::Paragraph { content, .. } => inl(content),
        _ => true,
    })
}

/// Текст и разметка сообщения из IR.
pub fn text(doc: &Document, quotes: &[Quote]) -> (String, Option<String>) {
    let doc = with_quotes(doc, quotes);
    let plain = doc_text(&doc);
    if !rich(&doc) {
        return (plain, None);
    }
    let mut linked = doc.clone();
    pills(&mut linked.blocks);
    let html = postman_ir::markdown_to_html(&postman_ir::to_markdown(&linked));
    (plain, Some(html))
}

fn mentions(ids: &[ParticipantId]) -> Mentions {
    Mentions::with_user_ids(ids.iter().filter_map(|p| OwnedUserId::try_from(p.as_str()).ok()))
}

/// Новое текстовое сообщение.
pub fn content(doc: &Document, quotes: &[Quote], ids: &[ParticipantId]) -> RoomMessageEventContent {
    let (plain, html) = text(doc, quotes);
    let c = match html {
        Some(h) => RoomMessageEventContent::text_html(plain, h),
        None => RoomMessageEventContent::text_plain(plain),
    };
    c.add_mentions(mentions(ids))
}

/// Сообщение с файлом; `caption` — текст к нему (Matrix 1.10: `filename` + `body`).
pub fn file_content(up: &Uploaded, src: MediaSource, caption: Option<(String, Option<String>)>) -> RoomMessageEventContent {
    let (body, formatted) = match caption {
        Some((t, h)) if !t.trim().is_empty() => (t, h.map(FormattedBody::html)),
        _ => (up.name.clone(), None),
    };
    let size = UInt::try_from(up.size).ok();
    let t = match up.kind {
        AttachmentKind::Image { w, h } => {
            let mut c = matrix_sdk::ruma::events::room::message::ImageMessageEventContent::new(body, src);
            let mut info = ImageInfo::new();
            info.mimetype = Some(up.mime.clone());
            info.size = size;
            info.width = (w > 0).then(|| UInt::from(w));
            info.height = (h > 0).then(|| UInt::from(h));
            c.info = Some(Box::new(info));
            c.filename = Some(up.name.clone());
            c.formatted = formatted;
            MessageType::Image(c)
        }
        _ => {
            let mut c = FileMessageEventContent::new(body, src);
            let mut info = FileInfo::new();
            info.mimetype = Some(up.mime.clone());
            info.size = size;
            c.info = Some(Box::new(info));
            c.filename = Some(up.name.clone());
            c.formatted = formatted;
            MessageType::File(c)
        }
    };
    RoomMessageEventContent::new(t)
}

/// Содержимое правки: новый текст, `m.new_content`.
pub fn edit_content(doc: &Document, ids: &[ParticipantId]) -> RoomMessageEventContentWithoutRelation {
    let (plain, html) = text(doc, &[]);
    let c = match html {
        Some(h) => RoomMessageEventContentWithoutRelation::text_html(plain, h),
        None => RoomMessageEventContentWithoutRelation::text_plain(plain),
    };
    c.add_mentions(mentions(ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use postman_core::channel::text_doc;

    #[test]
    fn reply_fallbacks_are_cut() {
        assert_eq!(strip_reply_html("<mx-reply><blockquote>old</blockquote></mx-reply><b>new</b>"), "<b>new</b>");
        assert_eq!(strip_reply_plain("> <@anna:example.test> старое\n> ещё\n\nновое"), "новое");
        assert_eq!(strip_reply_plain("обычный текст"), "обычный текст");
    }

    #[test]
    fn outgoing_text_and_html() {
        let (plain, html) = text(&text_doc("привет"), &[]);
        assert_eq!((plain.as_str(), html), ("привет", None));
        let mut d = text_doc("x");
        if let Block::Paragraph { content, .. } = &mut d.blocks[0] {
            content.push(Inline::Mention { id: "@anna:example.test".into(), text: "@Анна".into() });
        }
        let (plain, html) = text(&d, &[Quote { text: "фрагмент".into(), source: None }]);
        assert!(plain.starts_with("фрагмент\n") && plain.contains("@Анна"), "{plain}");
        let html = html.unwrap();
        assert!(html.contains("https://matrix.to/#/@anna:example.test") && html.contains("<blockquote>"), "{html}");
        let src = MediaSource::Plain("mxc://example.test/abc".into());
        assert!(matches!(media(&source(&src)), Some(MediaSource::Plain(u)) if u.as_str() == "mxc://example.test/abc"));
        assert!(media(&AttachmentSource(b"no-such-file".to_vec())).is_none());
    }
}
