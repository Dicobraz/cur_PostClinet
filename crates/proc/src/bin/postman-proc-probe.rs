//! Процесс проверок песочницы для тестов `postman-proc` (`tests/network.rs`): запирается в профиле «сеть без файлов»
//! и по вызовам клиента пробует сеть, файлы, процессы и потоки. В поставку не входит.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

use postman_proc::link::{Incoming, Link};
use postman_proc::probe::{Answer, Check};
use postman_proc::sandbox::{self, Profile};

/// Предел кучи на macOS: `RLIMIT_AS` там не действует — считает распределитель.
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOC: postman_proc::sandbox::Limited = postman_proc::sandbox::Limited;

fn main() {
    // Профиль в коде процесса, не в аргументе: аргумент выбирает только право входа учётной записью организации.
    let profile = Profile::Network { enterprise_auth: std::env::args().any(|a| a == "--auth") };
    if let Err(e) = sandbox::isolate(profile).and_then(|()| sandbox::lockdown(profile)) {
        eprintln!("ERROR postman-proc-probe: {}{e}", postman_proc::SANDBOX_REFUSAL);
        std::process::exit(postman_proc::EXIT_SANDBOX);
    }
    // Сторож смерти клиента и связь — после запирания: запреты Linux действуют на потоки, созданные после него.
    let _watchdog = postman_proc::watchdog(None);
    let Ok(link) = Link::<Answer, Check>::stdio(1 << 20) else { std::process::exit(1) };
    let mut listeners = Vec::new();
    loop {
        match link.recv() {
            Incoming::Call(id, check, bin) => {
                let (answer, out) = attempt(check, bin, &mut listeners);
                if link.reply(id, &answer, &out).is_err() {
                    break;
                }
            }
            Incoming::Message(..) => {}
            Incoming::Shutdown | Incoming::Closed => break,
        }
    }
}

fn attempt(check: Check, bin: Vec<u8>, listeners: &mut Vec<TcpListener>) -> (Answer, Vec<u8>) {
    let done = |r: std::io::Result<()>| match r {
        Ok(()) => Answer::Done,
        Err(e) => Answer::Refused(e.to_string()),
    };
    let answer = match check {
        Check::Echo => return (Answer::Done, bin),
        Check::Connect(addr) => done(connect(&addr)),
        Check::Resolve(host) => done((host.as_str(), 443).to_socket_addrs().map(drop)),
        Check::ReadFile(path) => done(std::fs::File::open(path).and_then(|mut f| f.read(&mut [0u8; 1]).map(drop))),
        Check::WriteFile(path) => done(std::fs::File::create(path).and_then(|mut f| f.write_all(b"x"))),
        Check::Spawn(exe) => done(std::process::Command::new(exe).spawn().map(drop)),
        Check::Listen => match TcpListener::bind(("0.0.0.0", 0)).and_then(|l| Ok((l.local_addr()?.port(), l))) {
            Ok((port, l)) => {
                listeners.push(l);
                Answer::Listening(port)
            }
            Err(e) => Answer::Refused(e.to_string()),
        },
        Check::Thread => match std::thread::Builder::new().spawn(|| 2 + 2).map(|t| t.join()) {
            Ok(Ok(4)) => Answer::Done,
            Ok(_) => Answer::Refused("поток не завершился".into()),
            Err(e) => Answer::Refused(e.to_string()),
        },
        Check::Hang => loop {
            std::thread::sleep(Duration::from_secs(60));
        },
    };
    (answer, Vec::new())
}

fn connect(addr: &str) -> std::io::Result<()> {
    let mut last = std::io::Error::other("адрес не разрешён");
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}
