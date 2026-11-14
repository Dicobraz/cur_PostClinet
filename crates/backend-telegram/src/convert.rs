//! Ответы Telegram (TL) → общая модель (ADR 0012 п. 4): люди, беседы диалогов, сообщения с вложениями, реакции,
//! альбомы, папки. Чего в модели нет (геометка, контакт) — строкой в теле: знак и данные, без слов интерфейса; кнопки
//! бота и опрос — структурой в `extra` ([`crate::extra`]), опрос ещё и строками тела.

use std::collections::HashMap;

use grammers_tl_types as tl;
use postman_core::channel::{
    Attachment, AttachmentKind, AttachmentSource, Body, ChannelFolder, Conversation, ConversationId, ConversationKind, Counts, FolderRef, Message,
    MessageId, MessageKind, Origin, Participant, ParticipantId, Quote, Reaction, Role, ServiceKind,
};
use postman_ir::{Align, Block, Inline};
use serde::{Deserialize, Serialize};

use crate::extra::{self, TgExtra};
use crate::ids::{Peer, msg_id, parse_msg};
use crate::text;

/// Откуда байты вложения (внутри `AttachmentSource`, JSON): по нему канал строит `InputFileLocation`; просроченный
/// `file_reference` обновляет, перечитав сообщение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Doc {
        conv: String,
        msg: i32,
        id: i64,
        hash: i64,
        fref: Vec<u8>,
        dc: i32,
        thumb: String,
    },
    Photo {
        conv: String,
        msg: i32,
        id: i64,
        hash: i64,
        fref: Vec<u8>,
        dc: i32,
        size: String,
    },
    /// Снимок профиля человека или беседы.
    PeerPhoto {
        conv: String,
        photo: i64,
        dc: i32,
    },
}

impl Source {
    pub fn to_bytes(&self) -> AttachmentSource {
        AttachmentSource(serde_json::to_vec(self).unwrap_or_default())
    }

    pub fn parse(s: &AttachmentSource) -> Option<Self> {
        serde_json::from_slice(&s.0).ok()
    }

    pub fn dc(&self) -> i32 {
        match self {
            Source::Doc { dc, .. } | Source::Photo { dc, .. } | Source::PeerPhoto { dc, .. } => *dc,
        }
    }

    pub fn message(&self) -> Option<(&str, i32)> {
        match self {
            Source::Doc { conv, msg, .. } | Source::Photo { conv, msg, .. } => Some((conv, *msg)),
            Source::PeerPhoto { .. } => None,
        }
    }
}

/// Выгруженный файл (внутри `Uploaded::source`, JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpFile {
    pub id: i64,
    pub parts: i32,
    pub name: String,
    pub big: bool,
}

impl UpFile {
    pub fn input(&self) -> tl::enums::InputFile {
        if self.big {
            tl::types::InputFileBig { id: self.id, parts: self.parts, name: self.name.clone() }.into()
        } else {
            tl::types::InputFile { id: self.id, parts: self.parts, name: self.name.clone(), md5_checksum: String::new() }.into()
        }
    }
}

/// Люди и беседы ответа: по ним — имена, роли, заголовки.
pub struct Ctx<'a> {
    pub me: i64,
    pub users: HashMap<i64, &'a tl::types::User>,
    pub chats: HashMap<Peer, &'a tl::enums::Chat>,
}

impl<'a> Ctx<'a> {
    pub fn new(me: i64, users: &'a [tl::enums::User], chats: &'a [tl::enums::Chat]) -> Self {
        let users = users.iter().filter_map(|u| if let tl::enums::User::User(u) = u { Some((u.id, u)) } else { None }).collect();
        let chats = chats.iter().filter_map(|c| chat_peer(c).map(|p| (p, c))).collect();
        Self { me, users, chats }
    }
}

pub fn chat_peer(c: &tl::enums::Chat) -> Option<Peer> {
    Some(match c {
        tl::enums::Chat::Chat(x) => Peer::Chat(x.id),
        tl::enums::Chat::Forbidden(x) => Peer::Chat(x.id),
        tl::enums::Chat::Empty(x) => Peer::Chat(x.id),
        tl::enums::Chat::Channel(x) => Peer::Channel(x.id),
        tl::enums::Chat::ChannelForbidden(x) => Peer::Channel(x.id),
        _ => return None,
    })
}

pub fn user_name(u: &tl::types::User) -> String {
    let name = [u.first_name.as_deref(), u.last_name.as_deref()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    if !name.is_empty() {
        return name;
    }
    u.username.clone().or_else(|| u.phone.as_ref().map(|p| format!("+{p}"))).unwrap_or_else(|| u.id.to_string())
}

fn username(u: &tl::types::User) -> Option<String> {
    u.username.clone().or_else(|| {
        u.usernames.as_ref()?.iter().find_map(|n| {
            let tl::enums::Username::Username(n) = n;
            n.active.then(|| n.username.clone())
        })
    })
}

pub fn participant(u: &tl::types::User) -> Participant {
    Participant {
        id: ParticipantId(u.id.to_string()),
        name: user_name(u),
        handle: username(u),
        phone: u.phone.as_ref().filter(|p| !p.is_empty()).map(|p| format!("+{}", p.trim_start_matches('+'))),
        emails: Vec::new(),
        avatar: match &u.photo {
            Some(tl::enums::UserProfilePhoto::Photo(p)) => {
                Some(Source::PeerPhoto { conv: Peer::User(u.id).conv().0, photo: p.photo_id, dc: p.dc_id }.to_bytes())
            }
            _ => None,
        },
        bot: u.bot,
    }
}

pub fn people(users: &[tl::enums::User]) -> Vec<Participant> {
    users.iter().filter_map(|u| if let tl::enums::User::User(u) = u { Some(participant(u)) } else { None }).collect()
}

/// Публичное имя беседы (`t.me/<имя>`).
pub fn chat_username(c: &tl::enums::Chat) -> Option<String> {
    let tl::enums::Chat::Channel(ch) = c else { return None };
    ch.username.clone().or_else(|| {
        ch.usernames.as_ref()?.iter().find_map(|n| {
            let tl::enums::Username::Username(n) = n;
            n.active.then(|| n.username.clone())
        })
    })
}

fn banned_send(r: &Option<tl::enums::ChatBannedRights>) -> bool {
    matches!(r, Some(tl::enums::ChatBannedRights::Rights(r)) if r.send_messages || r.view_messages)
}

/// Беседа по собеседнику — без диалога (сообщение пришло раньше беседы, или из истории).
pub fn conversation_of(p: Peer, cx: &Ctx) -> Conversation {
    let mut c = Conversation { id: p.conv(), ..Conversation::default() };
    match p {
        Peer::User(id) => {
            c.kind = ConversationKind::Direct;
            c.title = cx.users.get(&id).map(|u| user_name(u)).unwrap_or_else(|| id.to_string());
            c.members = Some(if id == cx.me { 1 } else { 2 });
            c.avatar = cx.users.get(&id).and_then(|u| participant(u).avatar);
        }
        Peer::Chat(_) | Peer::Channel(_) => {
            c.kind = ConversationKind::Group;
            c.title = c.id.0.clone();
            match cx.chats.get(&p) {
                Some(tl::enums::Chat::Chat(g)) => {
                    c.title = g.title.clone();
                    c.members = u32::try_from(g.participants_count).ok();
                    c.my_role = if g.creator {
                        Role::Owner
                    } else if g.admin_rights.is_some() {
                        Role::Admin
                    } else if g.left || g.deactivated || banned_send(&g.default_banned_rights) {
                        Role::ReadOnly
                    } else {
                        Role::Member
                    };
                    if let tl::enums::ChatPhoto::Photo(ph) = &g.photo {
                        c.avatar = Some(Source::PeerPhoto { conv: c.id.0.clone(), photo: ph.photo_id, dc: ph.dc_id }.to_bytes());
                    }
                }
                Some(tl::enums::Chat::Channel(ch)) => {
                    c.title = ch.title.clone();
                    c.members = ch.participants_count.and_then(|n| u32::try_from(n).ok());
                    let admin = ch.admin_rights.is_some();
                    if ch.broadcast {
                        c.kind = ConversationKind::Broadcast;
                        let can_post = matches!(&ch.admin_rights, Some(tl::enums::ChatAdminRights::Rights(r)) if r.post_messages);
                        c.my_role = if ch.creator {
                            Role::Owner
                        } else if can_post {
                            Role::Admin
                        } else {
                            Role::ReadOnly
                        };
                    } else {
                        c.my_role = if ch.creator {
                            Role::Owner
                        } else if admin {
                            Role::Admin
                        } else if ch.left {
                            Role::Guest
                        } else if banned_send(&ch.banned_rights) || banned_send(&ch.default_banned_rights) {
                            Role::ReadOnly
                        } else {
                            Role::Member
                        };
                    }
                    if let tl::enums::ChatPhoto::Photo(ph) = &ch.photo {
                        c.avatar = Some(Source::PeerPhoto { conv: c.id.0.clone(), photo: ph.photo_id, dc: ph.dc_id }.to_bytes());
                    }
                }
                Some(tl::enums::Chat::Forbidden(g)) => {
                    c.title = g.title.clone();
                    c.my_role = Role::ReadOnly;
                }
                Some(tl::enums::Chat::ChannelForbidden(ch)) => {
                    c.title = ch.title.clone();
                    c.kind = if ch.broadcast { ConversationKind::Broadcast } else { ConversationKind::Group };
                    c.my_role = Role::ReadOnly;
                }
                _ => {}
            }
        }
    }
    c
}

/// Заглушено до (Unix, мс); прошло — `None`.
pub fn muted_until(s: &tl::enums::PeerNotifySettings, now_s: i64) -> Option<i64> {
    let tl::enums::PeerNotifySettings::Settings(s) = s;
    let until = i64::from(s.mute_until?);
    (until > now_s).then_some(if until >= i64::from(i32::MAX) { i64::MAX } else { until * 1000 })
}

/// Беседа диалога: счётчики, «прочитано», заглушено, закреплено.
pub fn conversation(d: &tl::types::Dialog, cx: &Ctx, now_s: i64) -> Conversation {
    let mut c = conversation_of(Peer::from_tl(&d.peer), cx);
    c.last_read = (d.read_inbox_max_id > 0).then(|| msg_id(d.read_inbox_max_id));
    c.unread = Some(Counts { unread: u32::try_from(d.unread_count).unwrap_or(0), mentions: u32::try_from(d.unread_mentions_count).unwrap_or(0) });
    c.muted_until = muted_until(&d.notify_settings, now_s);
    c.pinned = d.pinned;
    if d.folder_id == Some(1) {
        c.folders.push(FolderRef::new(crate::ARCHIVE));
    }
    c
}

fn photo_size(s: &tl::enums::PhotoSize) -> Option<(String, i32, i32, u64)> {
    match s {
        tl::enums::PhotoSize::Size(x) => Some((x.r#type.clone(), x.w, x.h, u64::try_from(x.size).ok()?)),
        tl::enums::PhotoSize::Progressive(x) => Some((x.r#type.clone(), x.w, x.h, u64::try_from(*x.sizes.iter().max()?).ok()?)),
        tl::enums::PhotoSize::PhotoCachedSize(x) => Some((x.r#type.clone(), x.w, x.h, x.bytes.len() as u64)),
        _ => None,
    }
}

fn photo_attachment(p: &tl::types::Photo, conv: &ConversationId, msg: i32) -> Option<Attachment> {
    let sizes: Vec<_> = p.sizes.iter().filter_map(photo_size).collect();
    let big = sizes.iter().max_by_key(|s| i64::from(s.1) * i64::from(s.2))?;
    let small = sizes.iter().filter(|s| s.0 != big.0).min_by_key(|s| i64::from(s.1) * i64::from(s.2));
    let src = |size: &str| Source::Photo {
        conv: conv.0.clone(),
        msg,
        id: p.id,
        hash: p.access_hash,
        fref: p.file_reference.clone(),
        dc: p.dc_id,
        size: size.into(),
    };
    Some(Attachment {
        name: format!("photo_{}.jpg", p.id),
        mime: "image/jpeg".into(),
        size: Some(big.3),
        kind: AttachmentKind::Image { w: u32::try_from(big.1).unwrap_or(0), h: u32::try_from(big.2).unwrap_or(0) },
        source: src(&big.0).to_bytes(),
        thumb: small.map(|s| src(&s.0).to_bytes()),
    })
}

fn ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "application/pdf" => "pdf",
        "application/x-tgsticker" => "tgs",
        _ => "bin",
    }
}

fn doc_attachment(d: &tl::types::Document, conv: &ConversationId, msg: i32, voice: bool) -> Attachment {
    use tl::enums::DocumentAttribute as A;
    let mut name = None;
    let mut kind = AttachmentKind::File;
    for a in &d.attributes {
        match a {
            A::Filename(f) => name = Some(f.file_name.clone()),
            A::Sticker(_) => kind = AttachmentKind::Sticker,
            A::Video(v) if !matches!(kind, AttachmentKind::Sticker) => {
                kind =
                    AttachmentKind::Video { w: u32::try_from(v.w).unwrap_or(0), h: u32::try_from(v.h).unwrap_or(0), secs: v.duration.max(0.0) as u32 }
            }
            A::Audio(au) => {
                let secs = u32::try_from(au.duration).unwrap_or(0);
                kind = if au.voice || voice { AttachmentKind::Voice { secs } } else { AttachmentKind::Audio { secs } };
            }
            A::ImageSize(s) if matches!(kind, AttachmentKind::File) && d.mime_type.starts_with("image/") => {
                kind = AttachmentKind::Image { w: u32::try_from(s.w).unwrap_or(0), h: u32::try_from(s.h).unwrap_or(0) }
            }
            _ => {}
        }
    }
    let stem = match kind {
        AttachmentKind::Sticker => "sticker",
        AttachmentKind::Video { .. } => "video",
        AttachmentKind::Voice { .. } => "voice",
        AttachmentKind::Audio { .. } => "audio",
        AttachmentKind::Image { .. } => "image",
        _ => "file",
    };
    let thumb = d.thumbs.as_ref().and_then(|ts| ts.iter().filter_map(photo_size).max_by_key(|s| i64::from(s.1) * i64::from(s.2)));
    let src = |thumb: &str| Source::Doc {
        conv: conv.0.clone(),
        msg,
        id: d.id,
        hash: d.access_hash,
        fref: d.file_reference.clone(),
        dc: d.dc_id,
        thumb: thumb.into(),
    };
    Attachment {
        name: name.unwrap_or_else(|| format!("{stem}_{}.{}", d.id, ext(&d.mime_type))),
        mime: d.mime_type.clone(),
        size: u64::try_from(d.size).ok(),
        kind,
        source: src("").to_bytes(),
        thumb: thumb.map(|t| src(&t.0).to_bytes()),
    }
}

/// Что дало медиа сообщения: вложения, строки тела для того, чего в модели нет, опрос.
#[derive(Default)]
struct Media {
    attachments: Vec<Attachment>,
    lines: Vec<String>,
    poll: Option<extra::Poll>,
}

fn media(m: &tl::enums::MessageMedia, conv: &ConversationId, msg: i32) -> Media {
    use tl::enums::MessageMedia as M;
    let line = |l: Option<String>| Media { lines: l.into_iter().collect(), ..Media::default() };
    match m {
        M::Photo(p) => match &p.photo {
            Some(tl::enums::Photo::Photo(ph)) => Media { attachments: photo_attachment(ph, conv, msg).into_iter().collect(), ..Media::default() },
            _ => Media::default(),
        },
        M::Document(d) => match &d.document {
            Some(tl::enums::Document::Document(doc)) => Media { attachments: vec![doc_attachment(doc, conv, msg, d.voice)], ..Media::default() },
            _ => Media::default(),
        },
        M::Contact(c) => {
            let name = [c.first_name.as_str(), c.last_name.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
            line(Some(format!("👤 {name} +{}", c.phone_number.trim_start_matches('+'))))
        }
        M::Geo(g) => line(geo(&g.geo)),
        M::GeoLive(g) => line(geo(&g.geo)),
        M::Venue(v) => line(Some(format!("📍 {}, {}", v.title, v.address))),
        // Опрос — структурой в `extra` (вид — CH-04), в теле — вопрос и варианты текстом (поиск, уведомления).
        M::Poll(p) => {
            let poll = extra::poll(&p.poll, &p.results);
            let lines = std::iter::once(format!("📊 {}", poll.question)).chain(poll.answers.iter().map(|a| format!("◦ {}", a.text))).collect();
            Media { lines, poll: Some(poll), ..Media::default() }
        }
        M::Dice(d) => line(Some(d.emoticon.clone())),
        M::Game(g) => {
            let tl::enums::Game::Game(game) = &g.game;
            line(Some(format!("🎮 {}", game.title)))
        }
        M::Invoice(i) => line(Some(format!("🧾 {}", i.title))),
        _ => Media::default(),
    }
}

fn geo(g: &tl::enums::GeoPoint) -> Option<String> {
    match g {
        tl::enums::GeoPoint::Point(p) => Some(format!("📍 {:.5}, {:.5}", p.lat, p.long)),
        _ => None,
    }
}

/// Реакции сообщения: эмодзи и платные (⭐); свои эмодзи наборов — пропускаются (замены нет).
pub fn reactions(r: &tl::enums::MessageReactions) -> Vec<Reaction> {
    let tl::enums::MessageReactions::Reactions(r) = r;
    let key = |x: &tl::enums::Reaction| match x {
        tl::enums::Reaction::Emoji(e) => Some(e.emoticon.clone()),
        tl::enums::Reaction::Paid => Some("⭐".to_owned()),
        _ => None,
    };
    r.results
        .iter()
        .filter_map(|c| {
            let tl::enums::ReactionCount::Count(c) = c;
            let k = key(&c.reaction)?;
            let who = r
                .recent_reactions
                .iter()
                .flatten()
                .filter_map(|p| {
                    let tl::enums::MessagePeerReaction::Reaction(p) = p;
                    (key(&p.reaction).as_deref() == Some(k.as_str())).then(|| Peer::from_tl(&p.peer_id).participant())
                })
                .collect();
            Some(Reaction { key: k, count: u32::try_from(c.count).unwrap_or(0), mine: c.chosen_order.is_some(), who })
        })
        .collect()
}

fn service(a: &tl::enums::MessageAction, reply: Option<i32>) -> ServiceKind {
    use tl::enums::MessageAction as A;
    match a {
        A::ChatCreate(_) | A::ChannelCreate(_) => ServiceKind::Created,
        A::ChatAddUser(u) => {
            u.users.first().map(|id| ServiceKind::Joined(ParticipantId(id.to_string()))).unwrap_or(ServiceKind::Other(String::new()))
        }
        A::ChatJoinedByLink(_) | A::ChatJoinedByRequest => ServiceKind::Other("joined".into()),
        A::ChatDeleteUser(u) => ServiceKind::Left(ParticipantId(u.user_id.to_string())),
        A::ChatEditTitle(t) => ServiceKind::Renamed(t.title.clone()),
        A::PinMessage => reply.map(|id| ServiceKind::Pinned(msg_id(id))).unwrap_or(ServiceKind::Other("pinned".into())),
        other => ServiceKind::Other(tl::name_for_id(constructor(other)).to_owned()),
    }
}

fn constructor(a: &tl::enums::MessageAction) -> u32 {
    use grammers_tl_types::Serializable as _;
    let b = a.to_bytes();
    b.get(..4).and_then(|x| x.try_into().ok()).map(u32::from_le_bytes).unwrap_or(0)
}

/// Сообщение ответа → (беседа, сообщение); пустое — `None`.
pub fn message(m: &tl::enums::Message, cx: &Ctx) -> Option<(ConversationId, Message)> {
    match m {
        tl::enums::Message::Empty(_) => None,
        tl::enums::Message::Message(x) => {
            let peer = Peer::from_tl(&x.peer_id);
            let conv = peer.conv();
            let author = author(x.from_id.as_ref(), peer, x.out, cx.me);
            let entities = x.entities.as_deref().unwrap_or_default();
            let mut doc = text::to_ir(&x.message, entities);
            let Media { attachments, lines, poll } = x.media.as_ref().map(|m| media(m, &conv, x.id)).unwrap_or_default();
            for line in lines {
                doc.blocks.push(Block::Paragraph { align: Align::Start, content: vec![Inline::text(line)] });
            }
            let more = TgExtra { poll, keyboard: x.reply_markup.as_ref().and_then(extra::keyboard), ..TgExtra::default() };
            let mut mentions: Vec<ParticipantId> = entities
                .iter()
                .filter_map(|e| if let tl::enums::MessageEntity::MentionName(n) = e { Some(ParticipantId(n.user_id.to_string())) } else { None })
                .collect();
            if x.mentioned && !mentions.iter().any(|p| p.0 == cx.me.to_string()) {
                mentions.push(ParticipantId(cx.me.to_string()));
            }
            let (reply_to, quotes) = match &x.reply_to {
                Some(tl::enums::MessageReplyHeader::Header(h)) if h.reply_to_peer_id.is_none() => {
                    let to = h.reply_to_msg_id.map(msg_id);
                    let q = h.quote_text.iter().filter(|_| h.quote).map(|t| Quote { text: t.clone(), source: to.clone() }).collect();
                    (to, q)
                }
                _ => (None, Vec::new()),
            };
            let forwarded = x.fwd_from.as_ref().map(|f| {
                let tl::enums::MessageFwdHeader::Header(f) = f;
                let from = f.from_id.as_ref().map(Peer::from_tl);
                let name = f.from_name.clone().or_else(|| from.map(|p| conversation_of(p, cx).title)).unwrap_or_default();
                Origin { name, author: from.map(Peer::participant), sent: Some(i64::from(f.date) * 1000) }
            });
            Some((
                conv,
                Message {
                    id: msg_id(x.id),
                    author,
                    sent: i64::from(x.date) * 1000,
                    edited: x.edit_date.filter(|_| !x.edit_hide).map(|d| i64::from(d.max(x.date)) * 1000),
                    kind: MessageKind::Normal,
                    body: Body::Ir(doc),
                    attachments,
                    reply_to,
                    quotes,
                    thread: None,
                    forwarded,
                    reactions: x.reactions.as_ref().map(reactions).unwrap_or_default(),
                    mentions,
                    mine: x.out || peer == Peer::User(cx.me),
                    extra: more.to_extra(),
                },
            ))
        }
        tl::enums::Message::Service(x) => {
            let peer = Peer::from_tl(&x.peer_id);
            let reply = match &x.reply_to {
                Some(tl::enums::MessageReplyHeader::Header(h)) => h.reply_to_msg_id,
                _ => None,
            };
            Some((
                peer.conv(),
                Message {
                    id: msg_id(x.id),
                    author: author(x.from_id.as_ref(), peer, x.out, cx.me),
                    sent: i64::from(x.date) * 1000,
                    kind: MessageKind::Service(service(&x.action, reply)),
                    body: Body::Plain(String::new()),
                    reply_to: reply.map(msg_id),
                    reactions: x.reactions.as_ref().map(reactions).unwrap_or_default(),
                    mine: x.out,
                    ..Message::default()
                },
            ))
        }
    }
}

/// Автор: `from_id`; без него — в личной беседе собеседник или я, в канале — сам канал.
fn author(from: Option<&tl::enums::Peer>, peer: Peer, out: bool, me: i64) -> ParticipantId {
    if out {
        return ParticipantId(me.to_string());
    }
    match from {
        Some(p) => Peer::from_tl(p).participant(),
        None => match peer {
            Peer::User(id) => ParticipantId(id.to_string()),
            other => other.participant(),
        },
    }
}

/// Сообщения ответа по беседам, в порядке ответа.
pub fn messages(ms: &[tl::enums::Message], cx: &Ctx) -> Vec<(ConversationId, Message)> {
    ms.iter().filter_map(|m| message(m, cx)).collect()
}

/// Что известно о беседе для папок с правилами: тип (контакты, не контакты, группы, каналы, боты) и состояние
/// («без заглушённых», «без прочитанного», «без архива»).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kindly {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub contact: bool,
    pub bot: bool,
    pub muted: bool,
    /// Есть непрочитанное или беседа отмечена непрочитанной (`unread_mark`).
    pub unread: bool,
    /// Есть непрочитанное упоминание меня.
    pub mention: bool,
    pub archived: bool,
}

impl Kindly {
    /// По беседе модели: заглушено — на `now_ms`; `marked` — отметка «непрочитано» диалога.
    pub fn of(c: &Conversation, contact: bool, bot: bool, marked: bool, now_ms: i64) -> Self {
        let n = c.unread.unwrap_or_default();
        Kindly {
            id: c.id.clone(),
            kind: c.kind,
            contact,
            bot,
            muted: c.muted_until.is_some_and(|u| u > now_ms),
            unread: n.unread > 0 || n.mentions > 0 || marked,
            mention: n.mentions > 0,
            archived: c.folders.iter().any(|f| f.0 == crate::ARCHIVE),
        }
    }
}

fn input_peer_conv(p: &tl::enums::InputPeer, me: i64) -> Option<ConversationId> {
    Some(match p {
        tl::enums::InputPeer::PeerSelf => Peer::User(me).conv(),
        tl::enums::InputPeer::User(u) => Peer::User(u.user_id).conv(),
        tl::enums::InputPeer::Chat(c) => Peer::Chat(c.chat_id).conv(),
        tl::enums::InputPeer::Channel(c) => Peer::Channel(c.channel_id).conv(),
        _ => return None,
    })
}

/// Правила папки Telegram.
struct Rules {
    contacts: bool,
    non_contacts: bool,
    groups: bool,
    broadcasts: bool,
    bots: bool,
    exclude_muted: bool,
    exclude_read: bool,
    exclude_archived: bool,
}

impl Rules {
    /// Беседа подходит по правилам — как у официального клиента: тип отмечен; без заглушённых — кроме заглушённых
    /// с упоминанием меня вне архива; без прочитанного — только с непрочитанным или упоминанием; без архива.
    fn fits(&self, k: &Kindly) -> bool {
        let kind = match k.kind {
            ConversationKind::Direct if k.bot => self.bots,
            ConversationKind::Direct => (k.contact && self.contacts) || (!k.contact && self.non_contacts),
            ConversationKind::Group => self.groups,
            ConversationKind::Broadcast => self.broadcasts,
            ConversationKind::MailThread => false,
        };
        kind && (!self.exclude_muted || !k.muted || (k.mention && !k.archived))
            && (!self.exclude_read || k.unread || k.mention)
            && (!self.exclude_archived || !k.archived)
    }
}

/// Папки Telegram → папки учётки (ADR 0012, дерево — CH-04), состав — как у официального клиента: исключённые
/// беседы — никогда; закреплённые и добавленные явно — всегда (первыми, закреплённые — в своём порядке); прочие
/// известные — по правилам папки ([`Rules::fits`]). Правила «без заглушённых / прочитанного / архива» зависят от
/// состояния бесед — канал пересчитывает папки, когда оно меняется.
pub fn folders(filters: &[tl::enums::DialogFilter], known: &[Kindly], me: i64) -> Vec<ChannelFolder> {
    let mut out = Vec::new();
    for f in filters {
        let (id, title, include, pinned, exclude, rules) = match f {
            tl::enums::DialogFilter::Filter(f) => {
                let tl::enums::TextWithEntities::Entities(t) = &f.title;
                let rules = Rules {
                    contacts: f.contacts,
                    non_contacts: f.non_contacts,
                    groups: f.groups,
                    broadcasts: f.broadcasts,
                    bots: f.bots,
                    exclude_muted: f.exclude_muted,
                    exclude_read: f.exclude_read,
                    exclude_archived: f.exclude_archived,
                };
                (f.id, t.text.clone(), &f.include_peers, &f.pinned_peers, &f.exclude_peers[..], Some(rules))
            }
            tl::enums::DialogFilter::Chatlist(f) => {
                let tl::enums::TextWithEntities::Entities(t) = &f.title;
                (f.id, t.text.clone(), &f.include_peers, &f.pinned_peers, &[][..], None)
            }
            tl::enums::DialogFilter::Default => continue,
        };
        let never: Vec<ConversationId> = exclude.iter().filter_map(|p| input_peer_conv(p, me)).collect();
        let mut convs: Vec<ConversationId> = Vec::new();
        for c in pinned.iter().chain(include.iter()).filter_map(|p| input_peer_conv(p, me)) {
            if !never.contains(&c) && !convs.contains(&c) {
                convs.push(c);
            }
        }
        if let Some(rules) = rules {
            for k in known {
                if rules.fits(k) && !never.contains(&k.id) && !convs.contains(&k.id) {
                    convs.push(k.id.clone());
                }
            }
        }
        out.push(ChannelFolder { id: FolderRef(format!("f{id}")), name: title, parent: None, conversations: convs });
    }
    out
}

/// Номер альбома сообщения (`grouped_id`).
pub fn grouped(m: &tl::enums::Message) -> Option<i64> {
    match m {
        tl::enums::Message::Message(x) => x.grouped_id,
        _ => None,
    }
}

/// Части альбома → одно сообщение модели (как у официальных клиентов — одним «пузырём»): номер, автор, время,
/// ответ — первой части; вложения — всех по порядку; тело — подпись (первая непустая); правка — последняя;
/// реакции — первой части (на неё же уходит своя реакция); упоминания — всех. В `extra` — номера частей.
pub fn merge_album(group: i64, parts: &[Message]) -> Option<Message> {
    let mut sorted: Vec<&Message> = parts.iter().collect();
    sorted.sort_by_key(|m| parse_msg(&m.id).unwrap_or(i32::MAX));
    sorted.dedup_by_key(|m| m.id.clone());
    let head = *sorted.first()?;
    let caption = sorted.iter().find(|m| !m.body.plain().trim().is_empty()).copied();
    let mut out = head.clone();
    out.attachments = sorted.iter().flat_map(|m| m.attachments.iter().cloned()).collect();
    if let Some(c) = caption {
        out.body = c.body.clone();
    }
    out.edited = sorted.iter().filter_map(|m| m.edited).max();
    out.reply_to = sorted.iter().find_map(|m| m.reply_to.clone());
    out.quotes = sorted.iter().find(|m| !m.quotes.is_empty()).map(|m| m.quotes.clone()).unwrap_or_default();
    out.mentions = Vec::new();
    for m in &sorted {
        for p in &m.mentions {
            if !out.mentions.contains(p) {
                out.mentions.push(p.clone());
            }
        }
    }
    let mut more = TgExtra::of(&head.extra);
    more.album = Some(extra::Album {
        group,
        parts: sorted.iter().filter_map(|m| parse_msg(&m.id)).collect(),
        caption: caption.and_then(|c| parse_msg(&c.id)),
    });
    out.extra = more.to_extra();
    Some(out)
}

/// Сообщение, которое сервер вернул на отправку коротким ответом (`updateShortSentMessage`): тело — отправленное.
pub fn short_sent(me: i64, id: i32, date: i32, text: &str, entities: &[tl::enums::MessageEntity], reply_to: Option<MessageId>) -> Message {
    Message {
        id: msg_id(id),
        author: ParticipantId(me.to_string()),
        sent: i64::from(date) * 1000,
        body: Body::Ir(text::to_ir(text, entities)),
        reply_to,
        mine: true,
        ..Message::default()
    }
}
