//! Довесок сообщения Telegram в `Message::extra` (ADR 0012 п. 4): чего нет в общей модели, но нужно виду беседы
//! (CH-04) и ответу серверу — альбом (номера частей), кнопки бота (подпись и действие), опрос (вопрос, варианты,
//! голоса, итоги). Ядро хранит байты, не разбирая; читает их канал и вид Telegram ([`TgExtra::parse`]).
//!
//! Формат — JSON с номером версии `v`; новые поля — с `serde(default)`, старое читается как было. Нечего
//! сказать — `extra` пуст (как до CH-03-хвостов).

use grammers_tl_types as tl;
use postman_core::channel::Extra;
use serde::{Deserialize, Serialize};

/// Версия формата довеска.
pub const VERSION: u32 = 1;

/// Довесок сообщения Telegram.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TgExtra {
    pub v: u32,
    /// Сообщение — альбом: части на сервере.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<Album>,
    /// Кнопки бота под сообщением или вместо клавиатуры.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<Keyboard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<Poll>,
}

/// Альбом (`grouped_id`): у Telegram каждая часть — своё сообщение, в модели — одно (номер — первой части).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Album {
    pub group: i64,
    /// Номера частей по возрастанию (первая — номер сообщения модели); вложения — в том же порядке.
    pub parts: Vec<i32>,
    /// Часть с подписью (правка подписи уходит ей).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<i32>,
}

/// Кнопки бота: `inline` — под сообщением (`replyInlineMarkup`), иначе — клавиатура вместо обычной
/// (`replyKeyboardMarkup`: нажатие — отправить подпись кнопки).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Keyboard {
    pub inline: bool,
    pub rows: Vec<Vec<Button>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Клавиатура прячется после нажатия.
    #[serde(default)]
    pub single_use: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    pub text: String,
    pub action: ButtonAction,
}

/// Что делает кнопка.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ButtonAction {
    /// Ответ боту (`messages.getBotCallbackAnswer` с `data`); `password` — бот просит пароль двухэтапной проверки.
    Callback {
        data: Vec<u8>,
        #[serde(default)]
        password: bool,
    },
    /// Открыть ссылку.
    Url {
        url: String,
    },
    /// Вход на сайт через Telegram (`urlAuth`).
    LoginUrl {
        url: String,
    },
    /// Мини-приложение бота.
    WebView {
        url: String,
    },
    /// Встроенный запрос бота в поле ввода.
    SwitchInline {
        query: String,
        #[serde(default)]
        same_peer: bool,
    },
    Game,
    Buy,
    /// Карточка человека.
    UserProfile {
        user: i64,
    },
    /// Скопировать текст.
    Copy {
        text: String,
    },
    /// Кнопка клавиатуры бота: отправить её подпись.
    Text,
    RequestPhone,
    RequestGeo,
    RequestPoll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiz: Option<bool>,
    },
    RequestPeer,
    Disabled,
}

/// Опрос и его итоги на момент сообщения (обновляются `updateMessagePoll`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Poll {
    pub id: i64,
    pub question: String,
    pub answers: Vec<PollAnswer>,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub multiple: bool,
    /// Викторина: один верный ответ, после голоса — `correct` и пояснение.
    #[serde(default)]
    pub quiz: bool,
    /// Голоса не анонимны.
    #[serde(default)]
    pub public_voters: bool,
    /// Закроется (Unix, мс).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_date: Option<i64>,
    /// Всего проголосовало; `None` — итогов сервер не дал.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PollAnswer {
    pub text: String,
    /// Номер варианта у сервера (`messages.sendVote`).
    pub option: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voters: Option<u32>,
    /// Мой голос.
    #[serde(default)]
    pub chosen: bool,
    #[serde(default)]
    pub correct: bool,
}

impl Poll {
    /// Я уже голосовал.
    pub fn voted(&self) -> bool {
        self.answers.iter().any(|a| a.chosen)
    }
}

impl TgExtra {
    pub fn is_empty(&self) -> bool {
        self.album.is_none() && self.keyboard.is_none() && self.poll.is_none()
    }

    /// Байты `extra`; нечего сказать — пусто.
    pub fn to_extra(&self) -> Extra {
        if self.is_empty() {
            return Extra::default();
        }
        let me = TgExtra { v: VERSION, ..self.clone() };
        Extra(serde_json::to_vec(&me).unwrap_or_default())
    }

    /// Довесок Telegram из `extra`; пусто или чужое — `None`.
    pub fn parse(e: &Extra) -> Option<Self> {
        if e.0.is_empty() {
            return None;
        }
        serde_json::from_slice(&e.0).ok()
    }

    /// Довесок сообщения или пустой.
    pub fn of(e: &Extra) -> Self {
        Self::parse(e).unwrap_or_default()
    }
}

fn text(t: &tl::enums::TextWithEntities) -> String {
    let tl::enums::TextWithEntities::Entities(t) = t;
    t.text.clone()
}

/// Кнопки сообщения (`reply_markup`); убрать клавиатуру, «ответить» — не кнопки.
pub fn keyboard(m: &tl::enums::ReplyMarkup) -> Option<Keyboard> {
    use tl::enums::ReplyMarkup as R;
    match m {
        R::ReplyInlineMarkup(k) => {
            let rows: Vec<Vec<Button>> = k
                .rows
                .iter()
                .map(|r| {
                    let tl::enums::KeyboardInlineButtonRow::Row(r) = r;
                    r.buttons
                        .iter()
                        .map(|b| {
                            let tl::enums::KeyboardInlineButton::Button(b) = b;
                            Button { text: b.text.clone(), action: inline_action(&b.r#type) }
                        })
                        .collect()
                })
                .collect();
            (!rows.is_empty()).then_some(Keyboard { inline: true, rows, placeholder: None, single_use: false })
        }
        R::ReplyKeyboardMarkup(k) => {
            let rows: Vec<Vec<Button>> = k
                .rows
                .iter()
                .map(|r| {
                    let tl::enums::KeyboardButtonRow::Row(r) = r;
                    r.buttons
                        .iter()
                        .map(|b| {
                            let tl::enums::KeyboardButton::Button(b) = b;
                            Button { text: b.text.clone(), action: reply_action(&b.r#type) }
                        })
                        .collect()
                })
                .collect();
            (!rows.is_empty()).then(|| Keyboard { inline: false, rows, placeholder: k.placeholder.clone(), single_use: k.single_use })
        }
        R::ReplyKeyboardHide(_) | R::ReplyKeyboardForceReply(_) => None,
    }
}

fn inline_action(t: &tl::enums::InlineButtonType) -> ButtonAction {
    use tl::enums::InlineButtonType as T;
    match t {
        T::Url(u) => ButtonAction::Url { url: u.url.clone() },
        T::UrlAuth(u) => ButtonAction::LoginUrl { url: u.url.clone() },
        T::InputInlineButtonTypeUrlAuth(u) => ButtonAction::LoginUrl { url: u.url.clone() },
        T::WebView(u) => ButtonAction::WebView { url: u.url.clone() },
        T::Callback(c) => ButtonAction::Callback { data: c.data.clone(), password: c.requires_password },
        T::Game => ButtonAction::Game,
        T::Buy => ButtonAction::Buy,
        T::SwitchInline(s) => ButtonAction::SwitchInline { query: s.query.clone(), same_peer: s.same_peer },
        T::UserProfile(u) => ButtonAction::UserProfile { user: u.user_id },
        T::InputInlineButtonTypeUserProfile(u) => match &u.user_id {
            tl::enums::InputUser::User(u) => ButtonAction::UserProfile { user: u.user_id },
            _ => ButtonAction::Disabled,
        },
        T::Copy(c) => ButtonAction::Copy { text: c.copy_text.clone() },
        T::Disabled => ButtonAction::Disabled,
    }
}

fn reply_action(t: &tl::enums::ButtonType) -> ButtonAction {
    use tl::enums::ButtonType as T;
    match t {
        T::Default => ButtonAction::Text,
        T::RequestPhone => ButtonAction::RequestPhone,
        T::RequestGeoLocation => ButtonAction::RequestGeo,
        T::RequestPoll(p) => ButtonAction::RequestPoll { quiz: p.quiz },
        T::RequestPeer(_) | T::InputButtonTypeRequestPeer(_) => ButtonAction::RequestPeer,
        T::SimpleWebView(w) => ButtonAction::WebView { url: w.url.clone() },
    }
}

/// Опрос сообщения с итогами.
pub fn poll(p: &tl::enums::Poll, results: &tl::enums::PollResults) -> Poll {
    let tl::enums::Poll::Poll(p) = p;
    let mut out = Poll {
        id: p.id,
        question: text(&p.question),
        answers: p
            .answers
            .iter()
            .filter_map(|a| match a {
                tl::enums::PollAnswer::Answer(a) => Some(PollAnswer { text: text(&a.text), option: a.option.clone(), ..PollAnswer::default() }),
                tl::enums::PollAnswer::InputPollAnswer(_) => None,
            })
            .collect(),
        closed: p.closed,
        multiple: p.multiple_choice,
        quiz: p.quiz,
        public_voters: p.public_voters,
        close_date: p.close_date.map(|d| i64::from(d) * 1000),
        ..Poll::default()
    };
    apply_results(&mut out, results);
    out
}

/// Новые итоги опроса; `min` — без моих отметок (их сервер не прислал — остаются прежние).
pub fn apply_results(p: &mut Poll, results: &tl::enums::PollResults) {
    let tl::enums::PollResults::Results(r) = results;
    if let Some(total) = r.total_voters {
        p.total = u32::try_from(total).ok();
    }
    if let Some(s) = &r.solution {
        p.solution = Some(s.clone());
    }
    for v in r.results.iter().flatten() {
        let tl::enums::PollAnswerVoters::Voters(v) = v;
        if let Some(a) = p.answers.iter_mut().find(|a| a.option == v.option) {
            a.voters = v.voters.and_then(|n| u32::try_from(n).ok()).or(a.voters);
            if !r.min {
                a.chosen = v.chosen;
                a.correct = v.correct;
            }
        }
    }
}

/// Опрос по `updateMessagePoll`: сам опрос (если сервер прислал) и итоги; мои отметки при `min` — прежние.
pub fn renew(p: &mut Poll, poll: Option<&tl::enums::Poll>, results: &tl::enums::PollResults) {
    let Some(np) = poll else {
        apply_results(p, results);
        return;
    };
    let old = std::mem::take(p);
    *p = self::poll(np, results);
    let tl::enums::PollResults::Results(r) = results;
    for a in &mut p.answers {
        if let Some(o) = old.answers.iter().find(|o| o.option == a.option) {
            if r.min {
                a.chosen = o.chosen;
                a.correct = o.correct;
            }
            a.voters = a.voters.or(o.voters);
        }
    }
    p.total = p.total.or(old.total);
    p.solution = p.solution.take().or(old.solution);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пусто — пустые байты; туда и обратно; старое без новых полей читается.
    #[test]
    fn round_trip() {
        assert!(TgExtra::default().to_extra().0.is_empty());
        assert_eq!(TgExtra::parse(&Extra::default()), None);
        let e = TgExtra {
            v: 0,
            album: Some(Album { group: 7, parts: vec![10, 11], caption: Some(10) }),
            keyboard: Some(Keyboard {
                inline: true,
                rows: vec![vec![
                    Button { text: "Да".into(), action: ButtonAction::Callback { data: b"yes".to_vec(), password: false } },
                    Button { text: "Сайт".into(), action: ButtonAction::Url { url: "https://example.test".into() } },
                ]],
                placeholder: None,
                single_use: false,
            }),
            poll: None,
        };
        let bytes = e.to_extra();
        let back = TgExtra::parse(&bytes).unwrap();
        assert_eq!(back.v, VERSION);
        assert_eq!(back.album, e.album);
        assert_eq!(back.keyboard, e.keyboard);
        let old = TgExtra::parse(&Extra(br#"{"v":1,"poll":{"id":1,"question":"?","answers":[{"text":"a","option":[48]}]}}"#.to_vec())).unwrap();
        assert!(!old.poll.unwrap().voted());
    }
}
