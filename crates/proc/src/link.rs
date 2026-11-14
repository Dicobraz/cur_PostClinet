//! Длинная двусторонняя связь с дочерним процессом (ADR 0013) на тех же кадрах, что у воркера: сообщения в обе
//! стороны, вызов с ответом по номеру, `Ping`/`Pong`, остановка по `Shutdown`. Кодек сообщений — postcard; двоичная
//! часть (тело письма, файл) идёт своим кадром без перекодирования.
//!
//! Обе стороны одинаковы: [`Link::new`] берёт поток чтения и поток записи (у клиента — stdout и stdin процесса, у
//! процесса — свои stdin и stdout, [`Link::stdio`]). Читает отдельный поток: на `Ping` он отвечает сам, ответы на
//! вызовы отдаёт ждущим, остальное складывает во входящие ([`Link::recv`]).
//!
//! В процессе песочницы связь создаётся после запирания (`sandbox::lockdown`): на Linux запреты действуют на потоки,
//! созданные после него.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::frame::{MAX_FRAME, read_frame, write_frame};

/// Заголовок кадра связи; `body` — сообщение стороны в postcard.
#[derive(Serialize, Deserialize)]
enum Head {
    /// Сообщение без ответа.
    Message(Vec<u8>),
    /// Вызов: ответ придёт с тем же номером.
    Call(u64, Vec<u8>),
    Reply(u64, Vec<u8>),
    Ping(u64),
    Pong(u64),
    /// Просьба завершиться: получатель досылает своё и выходит.
    Shutdown,
}

/// Номер вызова другой стороны: по нему уходит ответ ([`Link::reply`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallId(u64);

/// Что пришло с другой стороны.
#[derive(Debug)]
pub enum Incoming<T> {
    Message(T, Vec<u8>),
    /// Вызов: ждёт [`Link::reply`] с этим номером.
    Call(CallId, T, Vec<u8>),
    /// Другая сторона просит завершиться.
    Shutdown,
    /// Связь закрыта: процесс завершился или поток повреждён (кадр не разобран, превышен предел).
    Closed,
}

/// Почему вызов не дал ответа.
#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    /// Связь закрыта (процесс завершился) или запись не удалась.
    Closed,
    /// Ответа нет за отведённое время.
    Timeout,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Closed => "связь с процессом закрыта",
            Self::Timeout => "процесс не ответил вовремя",
        })
    }
}

impl std::error::Error for CallError {}

type Reply<T> = mpsc::Sender<(T, Vec<u8>)>;

struct Shared<In> {
    writer: Mutex<Box<dyn Write + Send>>,
    /// Ждущие ответа вызовы и `Ping` (у `Ping` ответ пустой — в `pongs`).
    calls: Mutex<HashMap<u64, Reply<In>>>,
    pongs: Mutex<HashMap<u64, mpsc::Sender<()>>>,
    next: AtomicU64,
    closed: AtomicBool,
    max_bin: usize,
}

impl<In> Shared<In> {
    fn write(&self, head: &Head, bin: &[u8]) -> io::Result<()> {
        let head = postcard::to_stdvec(head).map_err(io::Error::other)?;
        let mut w = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let done = write_frame(&mut *w, &head, MAX_FRAME).and_then(|()| write_frame(&mut *w, bin, self.max_bin));
        if done.is_err() {
            self.close();
        }
        done
    }

    /// Связь закрыта: ждущие вызовы получают [`CallError::Closed`] (их отправители исчезают).
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.pongs.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

/// Одна сторона связи: `Out` — свои сообщения, `In` — сообщения другой стороны.
pub struct Link<Out, In> {
    shared: Arc<Shared<In>>,
    inbox: Mutex<mpsc::Receiver<Incoming<In>>>,
    out: PhantomData<fn(Out)>,
}

impl<Out: Serialize, In: DeserializeOwned + Send + 'static> Link<Out, In> {
    /// Связь поверх потоков `read` и `write`; `max_bin` — предел двоичной части кадра в обе стороны.
    pub fn new(read: impl Read + Send + 'static, write: impl Write + Send + 'static, max_bin: usize) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            writer: Mutex::new(Box::new(write)),
            calls: Mutex::new(HashMap::new()),
            pongs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            max_bin,
        });
        let (tx, rx) = mpsc::channel();
        let reader = Arc::clone(&shared);
        std::thread::Builder::new().name("link-reader".into()).spawn(move || read_loop(read, &reader, &tx))?;
        Ok(Self { shared, inbox: Mutex::new(rx), out: PhantomData })
    }

    /// Связь процесса с клиентом по своим stdin и stdout.
    pub fn stdio(max_bin: usize) -> io::Result<Self> {
        Self::new(io::stdin(), io::stdout(), max_bin)
    }

    /// Сообщение без ответа.
    pub fn send(&self, msg: &Out, bin: &[u8]) -> io::Result<()> {
        self.shared.write(&Head::Message(encode(msg)?), bin)
    }

    /// Вызов: ждёт ответ другой стороны не дольше `timeout`.
    pub fn call(&self, msg: &Out, bin: &[u8], timeout: Duration) -> Result<(In, Vec<u8>), CallError> {
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.shared.calls.lock().unwrap_or_else(PoisonError::into_inner).insert(id, tx);
        let forget = || self.shared.calls.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        // Закрылась между проверкой и записью — отправителя уберёт `close`, ждать не придётся.
        if self.shared.closed.load(Ordering::SeqCst) || encode(msg).and_then(|body| self.shared.write(&Head::Call(id, body), bin)).is_err() {
            forget();
            return Err(CallError::Closed);
        }
        match rx.recv_timeout(timeout) {
            Ok(reply) => Ok(reply),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                forget();
                Err(CallError::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CallError::Closed),
        }
    }

    /// Ответ на вызов другой стороны.
    pub fn reply(&self, id: CallId, msg: &Out, bin: &[u8]) -> io::Result<()> {
        self.shared.write(&Head::Reply(id.0, encode(msg)?), bin)
    }

    /// Жива ли другая сторона: `Ping` и ожидание `Pong` не дольше `timeout`. Отвечает поток чтения — занятость
    /// обработчика сообщений ответу не мешает.
    pub fn ping(&self, timeout: Duration) -> bool {
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.shared.pongs.lock().unwrap_or_else(PoisonError::into_inner).insert(id, tx);
        let alive = !self.shared.closed.load(Ordering::SeqCst) && self.shared.write(&Head::Ping(id), &[]).is_ok() && rx.recv_timeout(timeout).is_ok();
        self.shared.pongs.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        alive
    }

    /// Попросить другую сторону завершиться.
    pub fn shutdown(&self) -> io::Result<()> {
        self.shared.write(&Head::Shutdown, &[])
    }

    /// Следующее входящее; после закрытия связи — всегда [`Incoming::Closed`].
    pub fn recv(&self) -> Incoming<In> {
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner).recv().unwrap_or(Incoming::Closed)
    }

    /// Следующее входящее не дольше `timeout`; `None` — пока ничего нет.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Incoming<In>> {
        match self.inbox.lock().unwrap_or_else(PoisonError::into_inner).recv_timeout(timeout) {
            Ok(m) => Some(m),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Incoming::Closed),
        }
    }

    /// Связь закрыта (процесс завершился или поток повреждён).
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }
}

fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    postcard::to_stdvec(msg).map_err(io::Error::other)
}

fn read_loop<In: DeserializeOwned>(mut read: impl Read, shared: &Shared<In>, inbox: &mpsc::Sender<Incoming<In>>) {
    let decode = |body: &[u8]| postcard::from_bytes::<In>(body).ok();
    loop {
        let frame = read_frame(&mut read, MAX_FRAME).and_then(|head| Ok((head, read_frame(&mut read, shared.max_bin)?)));
        let Ok((head, bin)) = frame else { break };
        let Ok(head) = postcard::from_bytes::<Head>(&head) else { break };
        let delivered = match head {
            Head::Message(body) => decode(&body).map(|m| inbox.send(Incoming::Message(m, bin)).is_ok()),
            Head::Call(id, body) => decode(&body).map(|m| inbox.send(Incoming::Call(CallId(id), m, bin)).is_ok()),
            Head::Reply(id, body) => decode(&body).map(|m| {
                // Ответ на вызов, который уже не ждут (вышло время), отбрасывается.
                if let Some(tx) = shared.calls.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
                    let _ = tx.send((m, bin));
                }
                true
            }),
            Head::Ping(id) => Some(shared.write(&Head::Pong(id), &[]).is_ok()),
            Head::Pong(id) => {
                if let Some(tx) = shared.pongs.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
                    let _ = tx.send(());
                }
                Some(true)
            }
            Head::Shutdown => Some(inbox.send(Incoming::Shutdown).is_ok()),
        };
        // Сообщение не разобрано (версии сторон разошлись) или получателя уже нет — связь закрывается.
        if delivered != Some(true) {
            break;
        }
    }
    shared.close();
    let _ = inbox.send(Incoming::Closed);
}

#[cfg(test)]
mod tests {
    use std::io::{PipeReader, PipeWriter, pipe};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Host {
        Open(String),
        Sum(u32, u32),
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Plugin {
        Event(u32),
        Total(u32),
    }

    fn pair() -> (Link<Host, Plugin>, Link<Plugin, Host>, PipeWriter) {
        let (to_plugin_r, to_plugin_w): (PipeReader, PipeWriter) = pipe().unwrap();
        let (to_host_r, to_host_w) = pipe().unwrap();
        let raw = to_host_w.try_clone().unwrap();
        (Link::new(to_host_r, to_plugin_w, 1024).unwrap(), Link::new(to_plugin_r, to_host_w, 1024).unwrap(), raw)
    }

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn messages_calls_and_ping_go_both_ways() {
        let (host, plugin, _raw) = pair();
        host.send(&Host::Open("imap".into()), b"body").unwrap();
        assert!(matches!(plugin.recv(), Incoming::Message(Host::Open(k), bin) if k == "imap" && bin == b"body"));
        plugin.send(&Plugin::Event(7), &[]).unwrap();
        assert!(matches!(host.recv(), Incoming::Message(Plugin::Event(7), _)));

        // Вызов с ответом по номеру: два вызова подряд, ответы — в обратном порядке.
        let served = std::thread::spawn(move || {
            let mut calls = Vec::new();
            for _ in 0..2 {
                let Incoming::Call(id, Host::Sum(a, b), _) = plugin.recv() else { panic!("ждали вызов") };
                calls.push((id, a + b));
            }
            for (id, total) in calls.into_iter().rev() {
                plugin.reply(id, &Plugin::Total(total), b"bin").unwrap();
            }
            plugin
        });
        let host = Arc::new(host);
        let second = {
            let host = Arc::clone(&host);
            std::thread::spawn(move || host.call(&Host::Sum(10, 20), &[], WAIT))
        };
        assert_eq!(host.call(&Host::Sum(1, 2), &[], WAIT).map(|(m, _)| m), Ok(Plugin::Total(3)));
        assert_eq!(second.join().unwrap(), Ok((Plugin::Total(30), b"bin".to_vec())));
        let plugin = served.join().unwrap();

        // `Ping` отвечает поток чтения: обработчик сообщений другой стороны занят (ничего не читает).
        assert!(host.ping(WAIT));
        assert!(plugin.ping(WAIT));

        host.shutdown().unwrap();
        assert!(matches!(plugin.recv(), Incoming::Shutdown));
    }

    #[test]
    fn a_dead_peer_closes_the_link_and_fails_calls() {
        // Другая сторона — «процесс» с голыми концами каналов: его смерть закрывает оба.
        let (mut peer_in, to_peer) = pipe().unwrap();
        let (from_peer, peer_out) = pipe().unwrap();
        let host: Arc<Link<Host, Plugin>> = Arc::new(Link::new(from_peer, to_peer, 1024).unwrap());
        let call = {
            let host = Arc::clone(&host);
            std::thread::spawn(move || host.call(&Host::Sum(1, 1), &[], WAIT))
        };
        // Вызов дошёл — и процесс умер, не ответив.
        read_frame(&mut peer_in, MAX_FRAME).unwrap();
        read_frame(&mut peer_in, MAX_FRAME).unwrap();
        drop((peer_in, peer_out));
        assert_eq!(call.join().unwrap(), Err(CallError::Closed));
        assert!(matches!(host.recv(), Incoming::Closed));
        assert!(host.is_closed());
        assert_eq!(host.call(&Host::Sum(1, 1), &[], WAIT), Err(CallError::Closed));
        assert!(!host.ping(Duration::from_millis(50)));
    }

    #[test]
    fn a_call_without_reply_times_out_and_garbage_closes() {
        let (host, plugin, mut raw) = pair();
        assert_eq!(host.call(&Host::Sum(1, 1), &[], Duration::from_millis(50)), Err(CallError::Timeout));
        assert!(matches!(plugin.recv(), Incoming::Call(..)));
        // Кадр, который не разобрать, закрывает связь у получателя.
        write_frame(&mut raw, &[0xFF; 8], MAX_FRAME).unwrap();
        write_frame(&mut raw, &[], MAX_FRAME).unwrap();
        assert!(matches!(host.recv(), Incoming::Closed));
    }
}
