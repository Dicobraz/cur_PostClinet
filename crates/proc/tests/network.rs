//! Профиль песочницы «сеть без файлов» (ADR 0013, PLG-01) через настоящий процесс `postman-proc-probe`: сеть наружу
//! есть; файлов пользователя, дочерних процессов и входящих соединений нет. Заодно — длинная связь с процессом.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // интеграционный тест: «не падать» — для кода приложения (S2-03)

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use postman_proc::link::{CallError, Incoming, Link};
use postman_proc::probe::{Answer, Check};
use postman_proc::sandbox::{self, Launch, Level, Profile};

const WAIT: Duration = Duration::from_secs(20);

struct Probe {
    link: Link<Check, Answer>,
    kill: Box<dyn FnMut() + Send>,
    exit_code: Box<dyn FnMut() -> Option<u32> + Send>,
    background: Box<dyn FnMut(bool) -> std::io::Result<()> + Send>,
    journal: Arc<Mutex<Vec<String>>>,
}

impl Probe {
    fn start(auth: bool) -> Self {
        let exe = Path::new(env!("CARGO_BIN_EXE_postman-proc-probe"));
        let args: &[&str] = if auth { &["--auth"] } else { &[] };
        let profile = Profile::Network { enterprise_auth: auth };
        let p = sandbox::spawn(&Launch { args, profile, ..Launch::sealed(exe) }).expect("запуск процесса проверок");
        assert_eq!(p.level, Level::Full, "{:?}", p.fallback);
        let journal = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&journal);
        postman_proc::journal::drain_stderr(
            p.stderr,
            Arc::new(move |l: &str| sink.lock().unwrap().push(l.to_owned())),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let link = Link::new(p.stdout, p.stdin, 1 << 20).unwrap();
        Self { link, kill: p.kill, exit_code: p.exit_code, background: p.background, journal }
    }

    fn check(&mut self, check: Check) -> Answer {
        match self.link.call(&check, &[], WAIT) {
            Ok((answer, _)) => answer,
            Err(e) => panic!("{check:?}: {e}; код выхода {:?}; журнал: {:?}", (self.exit_code)(), self.journal.lock().unwrap()),
        }
    }

    fn refused(&mut self, check: Check) {
        let what = format!("{check:?}");
        assert!(matches!(self.check(check), Answer::Refused(_)), "{what} удалось в песочнице");
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        (self.kill)();
    }
}

/// Адрес для проверки исходящего соединения. Unix — свой слушатель на `127.0.0.1`. Windows: AppContainer к своей
/// машине не подключается (изоляция loopback), нужен внешний адрес; нет сети у самого теста — проверка пропускается.
fn outbound() -> Option<(String, Option<TcpListener>)> {
    if cfg!(windows) {
        let addr = "github.com:443";
        let reachable = std::net::ToSocketAddrs::to_socket_addrs(addr)
            .ok()
            .and_then(|mut a| a.next())
            .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_secs(5)).is_ok());
        return reachable.then(|| (addr.to_owned(), None));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    Some((format!("127.0.0.1:{}", listener.local_addr().unwrap().port()), Some(listener)))
}

#[test]
fn network_profile_reaches_out_and_nothing_else() {
    let mut p = Probe::start(false);

    // Связь: двоичная часть туда и обратно, поток внутри песочницы, `Ping`.
    let (answer, bin) = p.link.call(&Check::Echo, b"\x00\x01binary", WAIT).unwrap();
    assert_eq!((answer, bin.as_slice()), (Answer::Done, b"\x00\x01binary".as_slice()));
    assert_eq!(p.check(Check::Thread), Answer::Done);
    assert!(p.link.ping(WAIT));

    // Сеть наружу есть.
    match outbound() {
        Some((addr, _listener)) => assert_eq!(p.check(Check::Connect(addr)), Answer::Done),
        None => eprintln!("нет сети у теста — исходящее соединение из песочницы не проверено"),
    }
    #[cfg(unix)]
    assert_eq!(p.check(Check::Resolve("localhost".into())), Answer::Done);

    // Файлов пользователя нет: ни прочитать, ни создать.
    let dir = std::env::temp_dir().join(format!("postman-proc-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (secret, new) = (dir.join("secret.txt"), dir.join("new.txt"));
    std::fs::write(&secret, b"secret").unwrap();
    p.refused(Check::ReadFile(secret.to_string_lossy().into_owned()));
    p.refused(Check::WriteFile(new.to_string_lossy().into_owned()));
    assert!(!new.exists());
    let _ = std::fs::remove_dir_all(&dir);

    // Дочерних процессов нет.
    let shell = if cfg!(windows) { r"C:\Windows\System32\cmd.exe" } else { "/bin/sh" };
    p.refused(Check::Spawn(shell.into()));

    // Входящих соединений нет: либо слушать нельзя, либо до слушателя не достучаться.
    match p.check(Check::Listen) {
        Answer::Refused(_) => {}
        Answer::Listening(port) => {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            assert!(TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err(), "входящее соединение в песочницу прошло");
        }
        Answer::Done => panic!("неожиданный ответ"),
    }

    // Остановка по просьбе: процесс выходит сам, с кодом 0.
    p.link.shutdown().unwrap();
    assert!(matches!(p.link.recv(), Incoming::Closed));
    assert_eq!((p.exit_code)(), Some(0), "журнал: {:?}", p.journal.lock().unwrap());
}

/// На Linux процесс читает настройки имён и корневые сертификаты — и только их.
#[cfg(target_os = "linux")]
#[test]
fn network_profile_reads_only_resolver_and_certificate_files() {
    let mut p = Probe::start(false);
    for path in ["/etc/hosts", "/etc/resolv.conf", "/etc/ssl/certs/ca-certificates.crt"] {
        if Path::new(path).exists() {
            assert_eq!(p.check(Check::ReadFile(path.into())), Answer::Done, "{path}");
        }
    }
    for path in ["/etc/passwd", "/proc/self/environ", "/etc/machine-id"] {
        p.refused(Check::ReadFile(path.into()));
    }
}

#[test]
fn enterprise_auth_variant_starts_and_priority_goes_down_and_back() {
    let mut p = Probe::start(true);
    assert_eq!(p.check(Check::Echo), Answer::Done);
    // Пониженный приоритет на время прогрева и его возврат.
    (p.background)(true).unwrap();
    assert_eq!(p.check(Check::Thread), Answer::Done);
    (p.background)(false).unwrap();
    assert_eq!(p.check(Check::Echo), Answer::Done);
}

#[test]
fn a_hung_or_killed_process_is_seen_by_the_client() {
    let mut p = Probe::start(false);
    // Обработчик завис: вызов не отвечает вовремя.
    assert_eq!(p.link.call(&Check::Hang, &[], Duration::from_millis(300)).map(drop), Err(CallError::Timeout));
    // Клиент убивает процесс: связь закрывается, вызовы отказывают сразу.
    (p.kill)();
    assert!(matches!(p.link.recv(), Incoming::Closed));
    assert_eq!(p.link.call(&Check::Echo, &[], WAIT).map(drop), Err(CallError::Closed));
    assert!((p.exit_code)().is_some());
}

#[test]
fn arguments_with_spaces_or_quotes_are_refused() {
    let exe = Path::new(env!("CARGO_BIN_EXE_postman-proc-probe"));
    for bad in ["--plugin imap", "\"x\"", "", "a;b"] {
        let launch = Launch { args: &[bad], ..Launch::sealed(exe) };
        assert_eq!(sandbox::spawn(&launch).err().map(|e| e.kind()), Some(std::io::ErrorKind::InvalidInput), "{bad:?}");
    }
}

/// Замер для `docs/DEV.md` (PLG-01): создание процесса в песочнице → ответ на первый вызов.
/// `cargo test -p postman-proc --test network start_time -- --ignored --nocapture`.
#[test]
#[ignore = "замер, не проверка"]
fn start_time() {
    let mut ms: Vec<f64> = (0..21)
        .map(|_| {
            let t = std::time::Instant::now();
            let mut p = Probe::start(false);
            assert_eq!(p.check(Check::Echo), Answer::Done);
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    let first = ms[0];
    ms.sort_by(f64::total_cmp);
    println!("start→hello: first {first:.1} ms, min {:.1}, median {:.1}, max {:.1} ms (n = {})", ms[0], ms[ms.len() / 2], ms[ms.len() - 1], ms.len());
}
