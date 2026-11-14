//! Сквозное шифрование на подставном сервере (ADR 0012 «Проверка договора», Matrix): Анна — второй клиент
//! (`OlmMachine` в тесте) — шифрует сообщение в комнату с `m.room.encryption`; ключа ещё нет — сообщение приходит
//! `Undecryptable`; ключ комнаты приходит сообщением устройству — то же сообщение (тот же id) приходит снова
//! расшифрованным. Ключи нашего устройства — из своего хранилища канала (одноразовый ключ, которым Анна открыла сеанс
//! Olm, должен быть у нас).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест (S2-03)

use std::rc::Rc;
use std::time::Duration;

use matrix_sdk::ruma::api::IncomingResponseExt as _;
use matrix_sdk::ruma::api::client::keys::{claim_keys, get_keys, upload_keys};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{device_id, room_id, user_id};
use matrix_sdk_base::crypto::types::requests::AnyOutgoingRequest;
use matrix_sdk_base::crypto::{EncryptionSettings, OlmMachine};
use postman_backend_matrix::MatrixChannel;
use postman_backend_matrix::fake::{self, FakeMatrix};
use postman_cache::Cache;
use postman_core::channel::{AccountId, ChannelEvent, ChannelId, ChannelPlugin, ConnectCtx, LocalStore, MessageKind};
use serde_json::json;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const SECRET: &str = "!secret:example.test";

fn response<R: matrix_sdk::ruma::api::IncomingResponse>(body: serde_json::Value) -> R {
    let bytes = serde_json::to_vec(&body).unwrap();
    let r = http::Response::builder().status(200).body(bytes.as_slice()).unwrap();
    R::try_from_http_response(r).unwrap()
}

async fn until(rx: &mut UnboundedReceiver<ChannelEvent>, mut f: impl FnMut(&ChannelEvent) -> bool) -> Vec<ChannelEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("event in time").expect("events open");
        let stop = f(&ev);
        out.push(ev);
        if stop {
            return out;
        }
    }
}

#[test]
fn encrypted_message_arrives_again_with_the_key() {
    let server = FakeMatrix::new();
    server.add_encrypted_room(SECRET);
    let plugin = MatrixChannel::new();
    let cache = Cache::open_in_memory().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        let (tx, mut rx) = unbounded_channel();
        let store: Rc<dyn postman_core::channel::PluginStore> =
            Rc::new(LocalStore::new(cache.clone(), &ChannelId::new("matrix"), &AccountId::new(fake::ME)));
        let ctx = ConnectCtx { account: AccountId::new(fake::ME), secret: fake::session(&server), incoming: tx, store };
        let session = plugin.connect(ctx).await.unwrap();
        session.catch_up(None).await.unwrap();
        let evs = until(&mut rx, |e| matches!(e, ChannelEvent::Cursor(_))).await;
        let secret = evs
            .iter()
            .find_map(|e| if let ChannelEvent::Conversations(cs) = e { cs.iter().find(|c| c.id.0 == SECRET).cloned() } else { None })
            .unwrap();
        assert!(secret.encrypted, "комната с m.room.encryption — шифрованная");
        // Ключи устройства и одноразовые ключи выгружаются после первого sync.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while server.device_keys(fake::ME, fake::DEVICE).is_none() {
            assert!(tokio::time::Instant::now() < deadline, "ключи устройства не выгружены");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Анна: узнаёт наше устройство, открывает сеанс Olm нашим одноразовым ключом, делится ключом комнаты.
        let anna = OlmMachine::new(user_id!("@anna:example.test"), device_id!("ANNADEV")).await;
        let me = user_id!("@ivan:example.test");
        anna.update_tracked_users([me]).await.unwrap();
        for req in anna.outgoing_requests().await.unwrap() {
            match req.request() {
                AnyOutgoingRequest::KeysQuery(_) => {
                    let keys = server.device_keys(fake::ME, fake::DEVICE).unwrap();
                    let r: get_keys::v3::Response = response(json!({"device_keys": {fake::ME: {fake::DEVICE: keys}}, "failures": {}}));
                    anna.mark_request_as_sent(req.request_id(), &r).await.unwrap();
                }
                AnyOutgoingRequest::KeysUpload(_) => {
                    let r: upload_keys::v3::Response = response(json!({"one_time_key_counts": {"signed_curve25519": 50}}));
                    anna.mark_request_as_sent(req.request_id(), &r).await.unwrap();
                }
                _ => {}
            }
        }
        let (txn, _) = anna.get_missing_sessions([me].into_iter()).await.unwrap().expect("нужен сеанс с нашим устройством");
        let (key_id, key) = server.claim_otk(fake::ME, fake::DEVICE).expect("одноразовый ключ на сервере");
        let r: claim_keys::v3::Response = response(json!({"one_time_keys": {fake::ME: {fake::DEVICE: {key_id: key}}}, "failures": {}}));
        anna.mark_request_as_sent(&txn, &r).await.unwrap();
        let room = room_id!("!secret:example.test");
        let shares = anna.share_room_key(room, [me].into_iter(), EncryptionSettings::default()).await.unwrap();
        assert!(!shares.is_empty());
        let content: Raw<matrix_sdk::ruma::events::AnyMessageLikeEventContent> =
            Raw::new(&json!({"msgtype": "m.text", "body": "секретное слово"})).unwrap().cast_unchecked();
        let enc = anna.encrypt_room_event_raw(room, "m.room.message", &content).await.unwrap();
        let id = server.inject(SECRET, fake::ANNA, "m.room.encrypted", serde_json::from_str(enc.content.json().get()).unwrap());

        // Ключа ещё нет — заглушка.
        let evs = until(&mut rx, |e| matches!(e, ChannelEvent::Messages { messages, .. } if messages.iter().any(|m| m.id.0 == id))).await;
        let Some(ChannelEvent::Messages { conversation, messages, .. }) = evs.last() else { panic!() };
        assert_eq!(conversation.0, SECRET);
        let m = messages.iter().find(|m| m.id.0 == id).unwrap();
        assert_eq!(m.kind, MessageKind::Undecryptable);
        assert!(m.body.plain().is_empty(), "шифр — не текст");

        // Пришёл ключ комнаты — то же сообщение заново, расшифрованное.
        for share in shares {
            for (user, devices) in &share.messages {
                for (device, content) in devices {
                    let content: serde_json::Value = serde_json::from_str(content.json().get()).unwrap();
                    server.to_device(user.as_str(), &device.to_string(), fake::ANNA, &share.event_type.to_string(), content);
                }
            }
        }
        let evs = until(
            &mut rx,
            |e| matches!(e, ChannelEvent::Messages { messages, .. } if messages.iter().any(|m| m.id.0 == id && m.kind == MessageKind::Normal)),
        )
        .await;
        let Some(ChannelEvent::Messages { messages, .. }) = evs.last() else { panic!() };
        let m = messages.iter().find(|m| m.id.0 == id).unwrap();
        assert_eq!(m.body.plain(), "секретное слово");
        assert_eq!(m.author.0, fake::ANNA);

        // Своё сообщение в шифрованную комнату уходит шифром.
        let o = postman_core::channel::Outgoing {
            conversation: secret.id.clone(),
            client_id: 42,
            body: postman_core::channel::text_doc("ответ под шифром"),
            attachments: Vec::new(),
            reply_to: None,
            quotes: Vec::new(),
            mentions: Vec::new(),
            at: None,
            silent: false,
        };
        session.act(postman_core::channel::Action::Send(o)).await.unwrap();
        let last = server.last_event(SECRET).unwrap();
        assert_eq!(last["type"], "m.room.encrypted");
        assert!(!last.to_string().contains("ответ под шифром"), "на сервере — шифр");
    });
}
