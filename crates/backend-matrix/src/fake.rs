//! Подставной сервер Matrix для тестов договора без сети (признак `testkit`): Client-Server API в объёме, который
//! трогает `matrix-sdk` и канал, — на `127.0.0.1`, своим потоком и рантаймом. Учётка, комнаты, история, медиа,
//! ключи устройств, сообщения устройствам, поиск; сценарий [`Script`] — события «от других» и сбои (`429`, обрыв).
//! Данные — только `example.test`.

#![allow(clippy::expect_used)] // тестовый сервер (признак `testkit`): не открылся — тесту не на чем работать

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use postman_core::channel::conformance::{Script, Step};
use postman_core::channel::{Secret, decode_segment};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::secret::Session;

pub const ME: &str = "@ivan:example.test";
pub const ME_NAME: &str = "Иван Тестов";
pub const DEVICE: &str = "PMDEV1";
pub const TOKEN: &str = "tok-ivan";
pub const LOGIN: &str = "ivan";
pub const PASSWORD: &str = "пароль ивана";
/// Токен входа через браузер (SSO): его «выдаёт» сервер после входа на своей странице.
pub const LOGIN_TOKEN: &str = "sso-token-ivan";
pub const ANNA: &str = "@anna:example.test";
pub const BORIS: &str = "@boris:example.test";
/// Личная беседа с Анной.
pub const ROOM_ANNA: &str = "!anna:example.test";
/// Группа «Команда», я — администратор.
pub const ROOM_TEAM: &str = "!team:example.test";
/// «Новости»: писать могут только владельцы.
pub const ROOM_NEWS: &str = "!news:example.test";
/// Пространство «Работа»: команда и новости.
pub const SPACE: &str = "!work:example.test";
/// Файл Анны в личной беседе.
pub const REPORT: &[u8] = b"%PDF-1.4 report of example.test\n";

/// Начало времени сервера (мс).
const T0: i64 = 1_760_000_000_000;

#[derive(Clone)]
struct Ev {
    seq: u64,
    room: String,
    json: Value,
}

#[derive(Default)]
struct St {
    seq: u64,
    events: Vec<Ev>,
    /// Квитанции и «печатает»: номер, комната, событие.
    ephemeral: Vec<(u64, String, Value)>,
    /// Сообщения устройствам: номер, кому (человек, устройство), событие.
    to_device: Vec<(u64, String, String, Value)>,
    txns: HashMap<(String, String), String>,
    media: HashMap<String, Vec<u8>>,
    device_keys: BTreeMap<String, BTreeMap<String, Value>>,
    otk_count: u64,
    /// Одноразовые ключи устройств: (человек, устройство) → (id ключа, ключ).
    otks: HashMap<(String, String), Vec<(String, Value)>>,
    tokens: HashMap<String, (String, String)>,
    next_device: u32,
    /// Комнаты, у которых в следующем sync лента «урезана» (`limited`).
    limited: Vec<String>,
    down: bool,
    wait_once: bool,
    rooms: Vec<String>,
    encrypted: Vec<String>,
}

impl St {
    fn tick(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn add(&mut self, room: &str, sender: &str, kind: &str, content: Value, state_key: Option<&str>) -> String {
        let seq = self.tick();
        let id = format!("${seq}:example.test");
        let mut j = json!({
            "type": kind, "event_id": id, "sender": sender, "origin_server_ts": T0 + seq as i64 * 1000,
            "content": content, "room_id": room, "unsigned": {}
        });
        if let Some(k) = state_key {
            j["state_key"] = json!(k);
        }
        self.events.push(Ev { seq, room: room.into(), json: j });
        id
    }

    fn text(&mut self, room: &str, sender: &str, body: &str) -> String {
        self.add(room, sender, "m.room.message", json!({"msgtype": "m.text", "body": body}), None)
    }

    fn room(&mut self, id: &str, name: Option<&str>, members: &[&str], levels: Value, space: bool) {
        let create = if space { json!({"room_version": "11", "type": "m.space"}) } else { json!({"room_version": "11"}) };
        self.add(id, members[0], "m.room.create", create, Some(""));
        for m in members {
            self.add(id, m, "m.room.member", json!({"membership": "join", "displayname": display(m)}), Some(m));
        }
        self.add(id, members[0], "m.room.power_levels", levels, Some(""));
        if let Some(n) = name {
            self.add(id, members[0], "m.room.name", json!({"name": n}), Some(""));
        }
        self.rooms.push(id.into());
    }

    fn find(&self, id: &str) -> Option<&Ev> {
        self.events.iter().find(|e| e.json["event_id"] == id)
    }

    fn redact(&mut self, target: &str) {
        if let Some(e) = self.events.iter_mut().find(|e| e.json["event_id"] == target) {
            e.json["content"] = json!({});
            e.json["unsigned"]["redacted_because"] = json!({"type": "m.room.redaction"});
        }
    }

    fn redacted(e: &Ev) -> bool {
        e.json["unsigned"].get("redacted_because").is_some()
    }

    fn members(&self, room: &str) -> Vec<String> {
        let mut out: BTreeMap<String, bool> = BTreeMap::new();
        for e in self.events.iter().filter(|e| e.room == room && e.json["type"] == "m.room.member") {
            let joined = e.json["content"]["membership"] == "join";
            out.insert(e.json["state_key"].as_str().unwrap_or_default().to_owned(), joined);
        }
        out.into_iter().filter(|(_, j)| *j).map(|(u, _)| u).collect()
    }
}

fn display(user: &str) -> &'static str {
    match user {
        ME => ME_NAME,
        ANNA => "Анна Петрова",
        BORIS => "Борис Сидоров",
        _ => "Гость",
    }
}

struct Shared {
    st: Mutex<St>,
    wake: Notify,
    url: String,
}

/// Подставной сервер: клон — тот же сервер.
#[derive(Clone)]
pub struct FakeMatrix(Arc<Shared>);

/// Сессия учётки [`ME`] на этом сервере.
pub fn session(server: &FakeMatrix) -> Secret {
    let mut s = Session::new(server.url(), ME.into(), DEVICE.into(), TOKEN.into(), None).expect("random");
    s.store_key = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into();
    s.to_secret()
}

impl Default for FakeMatrix {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeMatrix {
    /// Сервер с учёткой, тремя комнатами и пространством; слушает свободный порт `127.0.0.1`.
    pub fn new() -> Self {
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("loopback");
        let port = std_listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let _ = std_listener.set_nonblocking(true);
        let mut st = St::default();
        st.tokens.insert(TOKEN.into(), (ME.into(), DEVICE.into()));
        seed(&mut st);
        let shared = Arc::new(Shared { st: Mutex::new(st), wake: Notify::new(), url: format!("http://127.0.0.1:{port}") });
        let server = FakeMatrix(Arc::clone(&shared));
        let _ = std::thread::Builder::new().name("fake matrix".into()).spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async move {
                let Ok(listener) = TcpListener::from_std(std_listener) else { return };
                loop {
                    let Ok((sock, _)) = listener.accept().await else { continue };
                    let s = Arc::clone(&shared);
                    tokio::spawn(async move { serve(s, sock).await });
                }
            });
        });
        server
    }

    pub fn url(&self) -> String {
        self.0.url.clone()
    }

    fn lock(&self) -> MutexGuard<'_, St> {
        self.0.st.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wake(&self) {
        self.0.wake.notify_waiters();
    }

    /// Анна пишет в личную беседу.
    pub fn anna_writes(&self, text: &str) -> String {
        let id = self.lock().text(ROOM_ANNA, ANNA, text);
        self.wake();
        id
    }

    /// Шифрованная комната с Анной (`m.room.encryption`).
    pub fn add_encrypted_room(&self, id: &str) {
        let mut s = self.lock();
        let levels = json!({"users": {ME: 100}, "users_default": 0, "events_default": 0});
        s.room(id, Some("Секретно"), &[ME, ANNA], levels, false);
        s.add(id, ME, "m.room.encryption", json!({"algorithm": "m.megolm.v1.aes-sha2"}), Some(""));
        s.encrypted.push(id.into());
        drop(s);
        self.wake();
    }

    /// Событие в комнату от имени человека (сырое содержимое — например, шифр от другого клиента).
    pub fn inject(&self, room: &str, sender: &str, kind: &str, content: Value) -> String {
        let id = self.lock().add(room, sender, kind, content, None);
        self.wake();
        id
    }

    /// Сообщение устройству `(user, device)` от `sender`.
    pub fn to_device(&self, user: &str, device: &str, sender: &str, kind: &str, content: Value) {
        let mut s = self.lock();
        let seq = s.tick();
        s.to_device.push((seq, user.into(), device.into(), json!({"type": kind, "sender": sender, "content": content})));
        drop(s);
        self.wake();
    }

    /// Ключи устройства, которые оно выгрузило (`/keys/upload`).
    pub fn device_keys(&self, user: &str, device: &str) -> Option<Value> {
        self.lock().device_keys.get(user)?.get(device).cloned()
    }

    /// Забрать одноразовый ключ устройства (`/keys/claim` другого клиента).
    pub fn claim_otk(&self, user: &str, device: &str) -> Option<(String, Value)> {
        let mut s = self.lock();
        let keys = s.otks.get_mut(&(user.to_owned(), device.to_owned()))?;
        (!keys.is_empty()).then(|| keys.remove(0))
    }

    /// Последнее событие комнаты (тип, содержимое).
    pub fn last_event(&self, room: &str) -> Option<Value> {
        self.lock().events.iter().rev().find(|e| e.room == room).map(|e| e.json.clone())
    }

    /// Сообщения устройствам, ушедшие с этого сервера к человеку.
    pub fn sent_to_device(&self, user: &str) -> Vec<Value> {
        self.lock().to_device.iter().filter(|(_, u, _, _)| u == user).map(|(_, _, _, e)| e.clone()).collect()
    }
}

fn seed(s: &mut St) {
    let dm = json!({"users": {ME: 100, ANNA: 100}, "users_default": 0, "events_default": 0});
    s.room(ROOM_ANNA, None, &[ANNA, ME], dm, false);
    s.text(ROOM_ANNA, ANNA, "Привет! Как дела?");
    s.text(ROOM_ANNA, ME, "Привет, всё хорошо");
    s.media.insert("report".into(), REPORT.to_vec());
    s.add(
        ROOM_ANNA,
        ANNA,
        "m.room.message",
        json!({"msgtype": "m.file", "body": "отчёт.pdf", "url": "mxc://example.test/report", "info": {"mimetype": "application/pdf", "size": REPORT.len()}}),
        None,
    );
    s.add(
        ROOM_ANNA,
        ANNA,
        "m.room.message",
        json!({"msgtype": "m.text", "body": "Иван, посмотри отчёт", "format": "org.matrix.custom.html",
               "formatted_body": "<a href=\"https://matrix.to/#/@ivan:example.test\">Иван</a>, посмотри <b>отчёт</b>",
               "m.mentions": {"user_ids": [ME]}}),
        None,
    );
    let team = json!({"users": {BORIS: 100, ME: 50}, "users_default": 0, "events_default": 0});
    s.room(ROOM_TEAM, Some("Команда"), &[BORIS, ME, ANNA], team, false);
    let first = s.text(ROOM_TEAM, BORIS, "Совещание в 10:00");
    s.add(
        ROOM_TEAM,
        ANNA,
        "m.room.message",
        json!({"msgtype": "m.text", "body": "> <@boris:example.test> Совещание в 10:00\n\nБуду", "m.relates_to": {"m.in_reply_to": {"event_id": first}}}),
        None,
    );
    let news = json!({"users": {BORIS: 100}, "users_default": 0, "events_default": 50});
    s.room(ROOM_NEWS, Some("Новости"), &[BORIS, ME], news, false);
    s.text(ROOM_NEWS, BORIS, "Выпуск 1.0 — в пятницу");
    let space = json!({"users": {BORIS: 100}, "users_default": 0, "events_default": 0});
    s.room(SPACE, Some("Работа"), &[BORIS, ME], space, true);
    s.add(SPACE, BORIS, "m.space.child", json!({"via": ["example.test"]}), Some(ROOM_TEAM));
    s.add(SPACE, BORIS, "m.space.child", json!({"via": ["example.test"]}), Some(ROOM_NEWS));
}

impl Script for FakeMatrix {
    fn steps(&self) -> Vec<Step> {
        // Удаления без беседы у Matrix нет: `m.redaction` всегда в комнате.
        vec![Step::EditByOther, Step::ReactByOther, Step::ReadByOther, Step::TypingByOther, Step::Gap, Step::Wait, Step::LinkDrop]
    }

    fn play(&self, step: Step) {
        let mut s = self.lock();
        let annas: Vec<String> = s
            .events
            .iter()
            .filter(|e| e.room == ROOM_ANNA && e.json["sender"] == ANNA && e.json["type"] == "m.room.message" && !St::redacted(e))
            .filter(|e| e.json["content"].get("m.relates_to").is_none())
            .filter_map(|e| e.json["event_id"].as_str().map(str::to_owned))
            .collect();
        match step {
            Step::EditByOther => {
                let Some(id) = annas.first().cloned() else { return };
                s.add(
                    ROOM_ANNA,
                    ANNA,
                    "m.room.message",
                    json!({"msgtype": "m.text", "body": "* Привет! Это исправленное сообщение",
                           "m.new_content": {"msgtype": "m.text", "body": "Привет! Это исправленное сообщение"},
                           "m.relates_to": {"rel_type": "m.replace", "event_id": id}}),
                    None,
                );
            }
            Step::ReactByOther => {
                let Some(id) = annas.last().cloned() else { return };
                s.add(ROOM_ANNA, ANNA, "m.reaction", json!({"m.relates_to": {"rel_type": "m.annotation", "event_id": id, "key": "🔥"}}), None);
            }
            Step::ReadByOther => {
                let Some(last) = s.events.iter().rev().find(|e| e.room == ROOM_ANNA).map(|e| e.json["event_id"].clone()) else { return };
                let seq = s.tick();
                let ts = T0 + seq as i64 * 1000;
                let ev = json!({"type": "m.receipt", "content": {last.as_str().unwrap_or_default(): {"m.read": {ANNA: {"ts": ts}}}}});
                s.ephemeral.push((seq, ROOM_ANNA.into(), ev));
            }
            Step::TypingByOther => {
                let seq = s.tick();
                s.ephemeral.push((seq, ROOM_ANNA.into(), json!({"type": "m.typing", "content": {"user_ids": [ANNA]}})));
            }
            Step::DeleteWithoutConversation => return,
            Step::Gap => {
                s.text(ROOM_TEAM, BORIS, "Пропущенное сообщение");
                s.limited.push(ROOM_TEAM.into());
            }
            Step::Wait => s.wait_once = true,
            Step::LinkDrop => s.down = true,
        }
        drop(s);
        self.wake();
    }
}

// --- HTTP ---

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    token: Option<String>,
    body: Vec<u8>,
}

enum Reply {
    Json(u16, Value),
    Bytes(Vec<u8>, &'static str),
    /// Оборвать соединение без ответа.
    Drop,
}

fn error(code: u16, errcode: &str) -> Reply {
    Reply::Json(code, json!({"errcode": errcode, "error": errcode}))
}

async fn read_req(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Req> {
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        let mut chunk = [0u8; 16 * 1024];
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let mut len = 0usize;
    let mut token = None;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "content-length" {
            len = v.parse().unwrap_or(0);
        } else if k == "authorization" {
            token = v.strip_prefix("Bearer ").map(str::to_owned);
        }
    }
    let start = head_end + 4;
    while buf.len() < start + len {
        let mut chunk = [0u8; 64 * 1024];
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[start..start + len].to_vec();
    buf.drain(..start + len);
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query =
        q.split('&').filter_map(|kv| kv.split_once('=')).filter_map(|(k, v)| Some((k.to_owned(), decode_segment(&v.replace('+', " "))?))).collect();
    Some(Req { method, path: path.to_owned(), query, token, body })
}

async fn serve(s: Arc<Shared>, mut sock: TcpStream) {
    let mut buf = Vec::new();
    while let Some(req) = read_req(&mut sock, &mut buf).await {
        let reply = handle(&s, req).await;
        let (code, kind, body) = match reply {
            Reply::Json(code, v) => (code, "application/json", serde_json::to_vec(&v).unwrap_or_default()),
            Reply::Bytes(b, kind) => (200, kind, b),
            Reply::Drop => return,
        };
        let head = format!("HTTP/1.1 {code} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n", body.len());
        if sock.write_all(head.as_bytes()).await.is_err() || sock.write_all(&body).await.is_err() {
            return;
        }
    }
}

fn segs(path: &str) -> Vec<String> {
    path.split('/').filter(|p| !p.is_empty()).map(|p| decode_segment(p).unwrap_or_else(|| p.to_owned())).collect()
}

fn lock(s: &Shared) -> MutexGuard<'_, St> {
    s.st.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Токен → (человек, устройство).
fn who(s: &Shared, r: &Req) -> Option<(String, String)> {
    lock(s).tokens.get(r.token.as_deref()?).cloned()
}

fn body(r: &Req) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

/// Токен из `s<N>`/`t<N>`.
fn pos(t: Option<&String>) -> Option<u64> {
    t.and_then(|t| t.get(1..)).and_then(|n| n.parse().ok())
}

async fn handle(s: &Arc<Shared>, r: Req) -> Reply {
    let p = segs(&r.path);
    let p: Vec<&str> = p.iter().map(String::as_str).collect();
    // Обрыв: долгий опрос рвётся; первый запрос не-sync (новое подключение) — связь вернулась.
    if lock(s).down {
        if p.ends_with(&["sync"]) {
            return Reply::Drop;
        }
        lock(s).down = false;
    }
    match (r.method.as_str(), p.as_slice()) {
        ("GET", [".well-known", "matrix", "client"]) => Reply::Json(200, json!({"m.homeserver": {"base_url": s.url}})),
        ("GET", ["_matrix", "client", "versions"]) => {
            Reply::Json(200, json!({"versions": ["v1.1", "v1.5", "v1.8", "v1.11", "v1.12"], "unstable_features": {}}))
        }
        ("GET", ["_matrix", "client", "v3", "login"]) => {
            Reply::Json(200, json!({"flows": [{"type": "m.login.password"}, {"type": "m.login.sso"}, {"type": "m.login.token"}]}))
        }
        ("POST", ["_matrix", "client", "v3", "login"]) => login(s, &body(&r)),
        (_, ["_matrix", "client", _, "account", "whoami"]) => match who(s, &r) {
            Some((u, d)) => Reply::Json(200, json!({"user_id": u, "device_id": d})),
            None => error(401, "M_UNKNOWN_TOKEN"),
        },
        _ if r.token.is_some() && who(s, &r).is_none() => error(401, "M_UNKNOWN_TOKEN"),
        ("POST", ["_matrix", "client", _, "logout"]) => {
            if let Some(t) = &r.token {
                lock(s).tokens.remove(t);
            }
            Reply::Json(200, json!({}))
        }
        ("GET", ["_matrix", "client", _, "profile", user, rest @ ..]) => {
            let name = display(user);
            match rest {
                [] => Reply::Json(200, json!({"displayname": name})),
                ["displayname"] => Reply::Json(200, json!({"displayname": name})),
                _ => error(404, "M_NOT_FOUND"),
            }
        }
        ("GET", ["_matrix", "client", _, "capabilities"]) => Reply::Json(200, json!({"capabilities": {}})),
        ("POST", ["_matrix", "client", _, "user", _, "filter"]) => Reply::Json(200, json!({"filter_id": "1"})),
        ("GET", ["_matrix", "client", _, "sync"]) => sync(s, &r).await,
        ("PUT", ["_matrix", "client", _, "rooms", room, "send", kind, txn]) => send(s, &r, room, kind, txn),
        ("PUT", ["_matrix", "client", _, "rooms", room, "redact", target, txn]) => {
            let Some((user, _)) = who(s, &r) else { return error(401, "M_MISSING_TOKEN") };
            let mut st = lock(s);
            if let Some(id) = st.txns.get(&(user.clone(), (*txn).to_owned())).cloned() {
                return Reply::Json(200, json!({"event_id": id}));
            }
            if st.find(target).is_none() {
                return error(404, "M_NOT_FOUND");
            }
            st.redact(target);
            let id = st.add(room, &user, "m.room.redaction", json!({"redacts": target}), None);
            if let Some(e) = st.events.last_mut() {
                e.json["redacts"] = json!(target);
            }
            st.txns.insert((user, (*txn).to_owned()), id.clone());
            drop(st);
            s.wake.notify_waiters();
            Reply::Json(200, json!({"event_id": id}))
        }
        ("POST", ["_matrix", "client", _, "rooms", room, "receipt", _kind, event]) => {
            let mut st = lock(s);
            if st.wait_once {
                st.wait_once = false;
                return Reply::Json(429, json!({"errcode": "M_LIMIT_EXCEEDED", "error": "slow down", "retry_after_ms": 2000}));
            }
            let seq = st.tick();
            let ev = json!({"type": "m.receipt", "content": {*event: {"m.read": {ME: {"ts": T0 + seq as i64 * 1000}}}}});
            st.ephemeral.push((seq, (*room).to_owned(), ev));
            drop(st);
            s.wake.notify_waiters();
            Reply::Json(200, json!({}))
        }
        ("POST", ["_matrix", "client", _, "rooms", _, "read_markers"]) => Reply::Json(200, json!({})),
        ("PUT", ["_matrix", "client", _, "rooms", _, "typing", _]) => Reply::Json(200, json!({})),
        (_, ["_matrix", "client", _, "user", _, "rooms", _, "tags", ..]) => Reply::Json(200, json!({})),
        ("GET", ["_matrix", "client", _, "rooms", room, "state", kind, key @ ..]) => {
            let key = key.first().copied().unwrap_or_default();
            let st = lock(s);
            match st.events.iter().rev().find(|e| e.room == *room && e.json["type"] == *kind && e.json["state_key"] == key) {
                Some(e) => Reply::Json(200, e.json["content"].clone()),
                None => error(404, "M_NOT_FOUND"),
            }
        }
        ("GET", ["_matrix", "client", _, "rooms", room, "messages"]) => messages(s, &r, room),
        ("GET", ["_matrix", "client", _, "rooms", room, "context", event]) => {
            let st = lock(s);
            match st.events.iter().find(|e| e.room == *room && e.json["event_id"] == *event) {
                Some(e) => Reply::Json(
                    200,
                    json!({"event": e.json, "start": format!("t{}", e.seq), "end": format!("t{}", e.seq), "events_before": [], "events_after": [], "state": []}),
                ),
                None => error(404, "M_NOT_FOUND"),
            }
        }
        ("GET", ["_matrix", "client", _, "rooms", room, "event", event]) => {
            let st = lock(s);
            match st.events.iter().find(|e| e.room == *room && e.json["event_id"] == *event) {
                Some(e) => Reply::Json(200, e.json.clone()),
                None => error(404, "M_NOT_FOUND"),
            }
        }
        ("GET", ["_matrix", "client", _, "rooms", room, "relations", event, rel, ..]) => {
            let st = lock(s);
            let chunk: Vec<Value> = st
                .events
                .iter()
                .filter(|e| e.room == *room && !St::redacted(e))
                .filter(|e| e.json["content"]["m.relates_to"]["event_id"] == *event && e.json["content"]["m.relates_to"]["rel_type"] == *rel)
                .map(|e| e.json.clone())
                .collect();
            Reply::Json(200, json!({"chunk": chunk}))
        }
        ("GET", ["_matrix", "client", _, "rooms", room, "members"]) => {
            let st = lock(s);
            let chunk: Vec<Value> =
                st.events.iter().filter(|e| e.room == *room && e.json["type"] == "m.room.member").map(|e| e.json.clone()).collect();
            Reply::Json(200, json!({"chunk": chunk}))
        }
        ("GET", ["_matrix", "client", _, "rooms", room, "joined_members"]) => {
            let st = lock(s);
            let joined: serde_json::Map<String, Value> =
                st.members(room).into_iter().map(|u| (u.clone(), json!({"display_name": display(&u)}))).collect();
            Reply::Json(200, json!({"joined": joined}))
        }
        ("POST", ["_matrix", "client", _, "keys", "upload"]) => {
            let Some((user, device)) = who(s, &r) else { return error(401, "M_MISSING_TOKEN") };
            let b = body(&r);
            let mut st = lock(s);
            if let Some(k) = b.get("device_keys") {
                st.device_keys.entry(user.clone()).or_default().insert(device.clone(), k.clone());
            }
            let otks: Vec<(String, Value)> = b
                .get("one_time_keys")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            st.otk_count += otks.len() as u64;
            st.otks.entry((user, device)).or_default().extend(otks);
            Reply::Json(200, json!({"one_time_key_counts": {"signed_curve25519": st.otk_count}}))
        }
        ("POST", ["_matrix", "client", _, "keys", "query"]) => {
            let b = body(&r);
            let st = lock(s);
            let mut out = serde_json::Map::new();
            if let Some(users) = b.get("device_keys").and_then(Value::as_object) {
                for u in users.keys() {
                    let devices: serde_json::Map<String, Value> =
                        st.device_keys.get(u).map(|d| d.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
                    out.insert(u.clone(), Value::Object(devices));
                }
            }
            Reply::Json(200, json!({"device_keys": out, "failures": {}}))
        }
        ("POST", ["_matrix", "client", _, "keys", "claim"]) => Reply::Json(200, json!({"one_time_keys": {}, "failures": {}})),
        ("POST", ["_matrix", "client", _, "keys", "signatures", "upload"]) => Reply::Json(200, json!({"failures": {}})),
        ("PUT", ["_matrix", "client", _, "sendToDevice", kind, _txn]) => {
            let Some((sender, _)) = who(s, &r) else { return error(401, "M_MISSING_TOKEN") };
            let b = body(&r);
            let mut st = lock(s);
            if let Some(m) = b.get("messages").and_then(Value::as_object) {
                for (user, devices) in m {
                    for (device, content) in devices.as_object().into_iter().flatten() {
                        let seq = st.tick();
                        st.to_device.push((seq, user.clone(), device.clone(), json!({"type": kind, "sender": sender, "content": content})));
                    }
                }
            }
            drop(st);
            s.wake.notify_waiters();
            Reply::Json(200, json!({}))
        }
        (_, ["_matrix", "client", _, "room_keys", ..]) => error(404, "M_NOT_FOUND"),
        ("GET", ["_matrix", "client", "v1", "media", "config"]) | ("GET", ["_matrix", "media", "v3", "config"]) => {
            Reply::Json(200, json!({"m.upload.size": 100 * 1024 * 1024}))
        }
        ("POST", ["_matrix", "media", _, "upload"]) => {
            let mut st = lock(s);
            let id = format!("m{}", st.tick());
            st.media.insert(id.clone(), r.body.clone());
            Reply::Json(200, json!({"content_uri": format!("mxc://example.test/{id}")}))
        }
        ("GET", ["_matrix", "client", "v1", "media", "download", "example.test", id, ..])
        | ("GET", ["_matrix", "media", "v3", "download", "example.test", id, ..]) => match lock(s).media.get(*id) {
            Some(b) => Reply::Bytes(b.clone(), "application/octet-stream"),
            None => error(404, "M_NOT_FOUND"),
        },
        ("POST", ["_matrix", "client", _, "search"]) => search(s, &body(&r)),
        ("POST", ["_matrix", "client", _, "createRoom"]) => {
            let Some((user, _)) = who(s, &r) else { return error(401, "M_MISSING_TOKEN") };
            let b = body(&r);
            let mut st = lock(s);
            let id = format!("!r{}:example.test", st.tick());
            let mut members = vec![user.clone()];
            members.extend(b.get("invite").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)));
            let refs: Vec<&str> = members.iter().map(String::as_str).collect();
            st.room(&id, None, &refs, json!({"users": {user: 100}, "users_default": 0, "events_default": 0}), false);
            drop(st);
            s.wake.notify_waiters();
            Reply::Json(200, json!({"room_id": id}))
        }
        (_, ["_matrix", "client", _, "user", _, "account_data", ..]) => Reply::Json(200, json!({})),
        _ => error(404, "M_UNRECOGNIZED"),
    }
}

fn login(s: &Shared, b: &Value) -> Reply {
    let ok = match b["type"].as_str() {
        Some("m.login.password") => {
            let user = b["identifier"]["user"].as_str().or(b["user"].as_str()).unwrap_or_default();
            (user == LOGIN || user == ME) && b["password"] == PASSWORD
        }
        Some("m.login.token") => b["token"] == LOGIN_TOKEN,
        _ => false,
    };
    if !ok {
        return error(403, "M_FORBIDDEN");
    }
    let mut st = lock(s);
    st.next_device += 1;
    let (token, device) = (format!("tok-{}", st.next_device), format!("PMNEW{}", st.next_device));
    st.tokens.insert(token.clone(), (ME.into(), device.clone()));
    Reply::Json(200, json!({"user_id": ME, "access_token": token, "device_id": device}))
}

fn send(s: &Arc<Shared>, r: &Req, room: &str, kind: &str, txn: &str) -> Reply {
    let Some((user, _)) = who(s, r) else { return error(401, "M_MISSING_TOKEN") };
    let mut st = lock(s);
    if let Some(id) = st.txns.get(&(user.clone(), txn.to_owned())).cloned() {
        return Reply::Json(200, json!({"event_id": id}));
    }
    if !st.rooms.iter().any(|x| x == room) {
        return error(404, "M_NOT_FOUND");
    }
    if room == ROOM_NEWS {
        return error(403, "M_FORBIDDEN");
    }
    let id = st.add(room, &user, kind, body(r), None);
    if let Some(e) = st.events.last_mut() {
        e.json["unsigned"]["transaction_id"] = json!(txn);
    }
    st.txns.insert((user, txn.to_owned()), id.clone());
    drop(st);
    s.wake.notify_waiters();
    Reply::Json(200, json!({"event_id": id}))
}

fn messages(s: &Shared, r: &Req, room: &str) -> Reply {
    let st = lock(s);
    if !st.rooms.iter().any(|x| x == room) {
        return error(404, "M_NOT_FOUND");
    }
    let from = pos(r.query.get("from")).unwrap_or(u64::MAX);
    let limit: usize = r.query.get("limit").and_then(|l| l.parse().ok()).unwrap_or(10);
    let older: Vec<&Ev> = st.events.iter().filter(|e| e.room == room && e.seq < from).collect();
    let chunk: Vec<Value> = older.iter().rev().take(limit).map(|e| e.json.clone()).collect();
    let last = older.iter().rev().take(limit).next_back().map(|e| e.seq);
    let more = older.len() > limit;
    let mut out = json!({"start": r.query.get("from").cloned().unwrap_or_else(|| format!("s{}", st.seq)), "chunk": chunk});
    if let (true, Some(l)) = (more, last) {
        out["end"] = json!(format!("t{l}"));
    }
    Reply::Json(200, out)
}

fn search(s: &Shared, b: &Value) -> Reply {
    let term = b["search_categories"]["room_events"]["search_term"].as_str().unwrap_or_default().to_lowercase();
    let rooms: Option<Vec<String>> =
        b["search_categories"]["room_events"]["filter"]["rooms"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect());
    let st = lock(s);
    let results: Vec<Value> = st
        .events
        .iter()
        .filter(|e| e.json["type"] == "m.room.message" && !st.encrypted.contains(&e.room))
        .filter(|e| rooms.as_ref().is_none_or(|r| r.contains(&e.room)))
        .filter(|e| e.json["content"]["body"].as_str().is_some_and(|t| t.to_lowercase().contains(&term)))
        .map(|e| json!({"rank": 1.0, "result": e.json}))
        .collect();
    Reply::Json(200, json!({"search_categories": {"room_events": {"results": results, "count": results.len()}}}))
}

/// Ответ sync от `since` (нет — первый вход: состояние и последние события комнат).
fn sync_body(st: &mut St, me: &str, device: &str, since: Option<u64>) -> Option<Value> {
    let after = since.unwrap_or(0);
    let mut join = serde_json::Map::new();
    let limited = std::mem::take(&mut st.limited);
    for room in st.rooms.clone() {
        if !st.members(&room).iter().any(|m| m == me) {
            continue;
        }
        let all: Vec<&Ev> = st.events.iter().filter(|e| e.room == room).collect();
        let new: Vec<&Ev> = all.iter().copied().filter(|e| e.seq > after).collect();
        let eph: Vec<Value> = st.ephemeral.iter().filter(|(q, r, _)| *q > after && *r == room).map(|(_, _, e)| e.clone()).collect();
        if new.is_empty() && eph.is_empty() && since.is_some() {
            continue;
        }
        let tail: Vec<&Ev> = new.iter().rev().take(postman_core::channel::HISTORY_PAGE as usize).rev().copied().collect();
        let first_seq = tail.first().map_or(st.seq, |e| e.seq);
        let state: Vec<Value> = if since.is_none() {
            all.iter().filter(|e| e.json.get("state_key").is_some() && e.seq < first_seq).map(|e| e.json.clone()).collect()
        } else {
            Vec::new()
        };
        let lim = (since.is_none() && new.len() > tail.len()) || limited.contains(&room);
        let timeline: Vec<Value> = tail.iter().map(|e| e.json.clone()).collect();
        let mut account = Vec::new();
        if since.is_none() && room == ROOM_ANNA {
            account.push(json!({"type": "m.fully_read", "content": {"event_id": "$0:example.test"}}));
        }
        join.insert(
            room.clone(),
            json!({
                "timeline": {"events": timeline, "limited": lim, "prev_batch": format!("t{first_seq}")},
                "state": {"events": state},
                "ephemeral": {"events": eph},
                "account_data": {"events": account},
                "unread_notifications": {"notification_count": 0, "highlight_count": 0}
            }),
        );
    }
    let to_device: Vec<Value> =
        st.to_device.iter().filter(|(q, u, d, _)| *q > after && u == me && (d == device || d == "*")).map(|(_, _, _, e)| e.clone()).collect();
    let has_news = !join.is_empty() || !to_device.is_empty();
    if since.is_some() && !has_news {
        st.limited = limited;
        return None;
    }
    let account_data = if since.is_none() { vec![json!({"type": "m.direct", "content": {ANNA: [ROOM_ANNA]}})] } else { Vec::new() };
    Some(json!({
        "next_batch": format!("s{}", st.seq),
        "rooms": {"join": join},
        "account_data": {"events": account_data},
        "to_device": {"events": to_device},
        "device_one_time_keys_count": {"signed_curve25519": st.otk_count},
        "device_lists": {"changed": [], "left": []}
    }))
}

async fn sync(s: &Arc<Shared>, r: &Req) -> Reply {
    let Some((me, device)) = who(s, r) else { return error(401, "M_MISSING_TOKEN") };
    let since = pos(r.query.get("since"));
    let timeout: u64 = r.query.get("timeout").and_then(|t| t.parse().ok()).unwrap_or(0);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout.min(30_000));
    loop {
        let wake = s.wake.notified();
        {
            let mut st = lock(s);
            if st.down {
                return Reply::Drop;
            }
            if let Some(b) = sync_body(&mut st, &me, &device, since) {
                return Reply::Json(200, b);
            }
            if tokio::time::Instant::now() >= deadline {
                return Reply::Json(200, json!({"next_batch": format!("s{}", since.unwrap_or(st.seq)), "rooms": {}}));
            }
        }
        let _ = tokio::time::timeout_at(deadline, wake).await;
    }
}
