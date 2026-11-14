//! Общий механизм дочерних процессов Postman (ADR 0007, ADR 0013): запуск в песочнице ([`sandbox`]), кадры
//! «заголовок + двоичная часть» ([`frame`]), журнал процесса ([`journal`]), сторож времени и смерти клиента
//! ([`watchdog`]), длинная двусторонняя связь ([`link`]), приоритет на время прогрева ([`Process::background`]).
//! Им пользуются воркер, хост модулей просмотра и хост плагинов протоколов: второго способа запуска нет.

pub mod frame;
pub mod journal;
pub mod link;
#[cfg(feature = "probe")]
pub mod probe;
pub mod sandbox;

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub use sandbox::{Launch, Level, Namespaces, Profile};

/// Код выхода процесса, отказавшего в запирании песочницы (до первого сообщения).
pub const EXIT_SANDBOX: i32 = 3;
/// Код выхода при превышении времени на запрос ([`watchdog`]).
pub const EXIT_TIME: i32 = 5;
/// Код выхода при превышении предела памяти (macOS: распределитель `Limited`; на остальных ОС предел держит ОС).
pub const EXIT_MEMORY: i32 = 6;

/// Начало ответа процесса, отказавшего в запирании (до первого запроса, выход [`EXIT_SANDBOX`]).
pub const SANDBOX_REFUSAL: &str = "песочница: ";

/// Как часто процесс песочницы проверяет, жив ли клиент.
pub const PARENT_POLL: Duration = Duration::from_millis(250);

/// Запущенный дочерний процесс, как его видит клиент.
pub struct Process {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    /// Журнал процесса — канал на всех ОС (SBX-01); клиент читает его в фоне ([`journal::drain_stderr`]).
    pub stderr: Box<dyn Read + Send>,
    pub kill: Box<dyn FnMut() + Send>,
    /// Код завершения, если процесс уже завершился (для текста ошибки).
    pub exit_code: Box<dyn FnMut() -> Option<u32> + Send>,
    /// Пониженный приоритет процессора и ввода-вывода (`true`) на время прогрева и его возврат (`false`); ADR 0013.
    pub background: Box<dyn FnMut(bool) -> std::io::Result<()> + Send>,
    pub level: Level,
    /// Почему изоляция не полная (Windows: отказ AppContainer) — для журнала и текста падения тестов.
    pub fallback: Option<String>,
}

/// Сторож процесса песочницы: запрос дольше `limit` завершает процесс с [`EXIT_TIME`]; в канал — `true` в начале
/// запроса и `false` в конце. `None` в `limit` — без предела времени (длинная связь: зависание ловит `Ping` клиента).
/// В простое поток спит в `recv` и процессор не тратит. Поток не запустился — работаем без сторожа (клиент всё
/// равно убьёт процесс по своему таймауту).
/// Тот же поток следит за клиентом (SBX-03, аудит №44): клиент умер (сменился родитель) — выход не позже
/// [`PARENT_POLL`], на всех ОС одинаково (на Windows процесс к тому же в Job Object с `KILL_ON_JOB_CLOSE`).
/// Звать до запирания: потом новые потоки могут быть запрещены.
pub fn watchdog(limit: Option<Duration>) -> Option<mpsc::Sender<bool>> {
    let (tx, rx) = mpsc::channel::<bool>();
    let parent = sandbox::parent();
    let spawned = std::thread::Builder::new().name("watchdog".into()).spawn(move || {
        let mut started: Option<Instant> = None;
        loop {
            match rx.recv_timeout(PARENT_POLL) {
                Ok(true) => started = Some(Instant::now()),
                Ok(false) => started = None,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if sandbox::parent() != parent {
                std::process::exit(0);
            }
            if let (Some(t), Some(limit)) = (started, limit)
                && t.elapsed() >= limit
            {
                std::process::exit(EXIT_TIME);
            }
        }
    });
    spawned.ok().map(|_| tx)
}

/// Код завершения с расшифровкой своих кодов, частых NTSTATUS и сигналов (Unix: 128 + номер).
pub fn exit_name(code: u32) -> String {
    let name = match code {
        // `EXIT_TIME`, `EXIT_MEMORY`.
        5 => " (превышено время разбора)",
        6 => " (превышен предел памяти)",
        134 if cfg!(unix) => " (аварийное завершение, SIGABRT)",
        137 if cfg!(unix) => " (убит, SIGKILL)",
        138 | 139 if cfg!(unix) => " (нарушение доступа к памяти)",
        153 if cfg!(target_os = "linux") => " (запись в файл запрещена, SIGXFSZ)",
        159 if cfg!(target_os = "linux") => " (запрещённый системный вызов, seccomp)",
        0xC000_0005 => " (нарушение доступа к памяти)",
        0xC000_0022 => " (нет доступа — вероятно, AppContainer не может прочитать файлы воркера)",
        0xC000_0135 => " (не найдена DLL)",
        0xC000_0139 => " (не найдена точка входа DLL)",
        0xC000_0142 => " (ошибка инициализации DLL)",
        0xC000_0409 => " (аварийное завершение / fail-fast)",
        0xC000_0602 => " (fail-fast)",
        0xC000_0374 => " (повреждение кучи)",
        0xC000_00FD => " (переполнение стека)",
        0xC000_0017 | 0xC000_012D => " (не хватило памяти)",
        _ => "",
    };
    if code > 0xFFFF { format!("0x{code:08X}{name}") } else { format!("{code}{name}") }
}
