//! Журнал дочернего процесса: его stderr — канал к клиенту (SBX-01); строки идут в журнал приложения с уровнем и
//! пределами длины и частоты.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Приёмник строк журнала процесса песочницы (клиента).
pub type Journal = Arc<dyn Fn(&str) + Send + Sync>;

/// Наибольшая длина строки stderr процесса песочницы в журнале, байт; длиннее — обрезается с пометкой.
pub const STDERR_LINE: usize = 2048;
/// Наибольшее число строк stderr процесса песочницы в журнале за секунду; сверх — одна строка WARN за секунду.
pub const STDERR_RATE: u32 = 100;

/// Важность строки stderr процесса песочницы для журнала приложения (`pets-shell/LOGGING.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalLevel {
    /// Паника, отказ в памяти: разбор не состоялся.
    Error,
    /// Остальное: процесс песочницы пишет в stderr только о необычном (запасной путь, предел частоты строк).
    Warn,
}

impl JournalLevel {
    /// Важность строки по её началу: строки процесса — `WARN …`, паника std — `thread '…' panicked at`, отказ в памяти —
    /// `memory allocation of … failed`.
    pub fn of(line: &str) -> Self {
        if line.contains("panicked at") || line.starts_with("memory allocation of ") || line.contains("memory limit exceeded") {
            Self::Error
        } else {
            Self::Warn
        }
    }
}

/// Приёмник журнала по умолчанию для всех клиентов процесса ([`set_default_journal`]).
static DEFAULT_JOURNAL: std::sync::OnceLock<Journal> = std::sync::OnceLock::new();

/// Куда писать строки stderr процессов песочницы всем клиентам, у которых не задан свой клиента: приложение
/// ставит один раз при запуске (журнал `tracing`). Второй вызов не меняет приёмник; `false` — уже был задан.
pub fn set_default_journal(sink: impl Fn(JournalLevel, &str) + Send + Sync + 'static) -> bool {
    DEFAULT_JOURNAL.set(Arc::new(move |line: &str| sink(JournalLevel::of(line), line))).is_ok()
}

/// Приёмник по умолчанию: заданный [`set_default_journal`], иначе stderr приложения.
pub fn default_journal(line: &str) {
    if let Some(sink) = DEFAULT_JOURNAL.get() {
        return sink(line);
    }
    // Без приёмника приложения — его stderr (в файл его направляет приложение); строка уже ограничена по длине.
    let _ = writeln!(io::stderr().lock(), "worker: {line}");
}

/// Ограничитель строк журнала: длина — [`STDERR_LINE`], частота — [`STDERR_RATE`] в секунду; первая лишняя строка
/// за секунду заменяется одной строкой WARN, остальные до конца секунды отбрасываются.
struct StderrLog {
    journal: Journal,
    window: Instant,
    lines: u32,
}

impl StderrLog {
    fn line(&mut self, bytes: &[u8], cut: bool) {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.lines = 0;
        }
        self.lines = self.lines.saturating_add(1);
        if self.lines > STDERR_RATE {
            if self.lines == STDERR_RATE + 1 {
                (self.journal)(&format!("WARN stderr rate limit: over {STDERR_RATE} lines/s, dropping the rest of this second"));
            }
            return;
        }
        let text = String::from_utf8_lossy(bytes.strip_suffix(b"\r").unwrap_or(bytes));
        if cut { (self.journal)(&format!("{text} [line truncated]")) } else { (self.journal)(&text) }
    }
}

/// Фоновое чтение stderr процесса песочницы до его закрытия: канал не переполняется (процесс не встанет на записи),
/// UI не ждёт. Строки — в `journal` с ограничением [`StderrLog`].
/// Строка, которую std пишет в stderr перед аварийным завершением при отказе в памяти (все ОС): по ней клиент называет
/// исход «превышен предел памяти» одинаково везде (SBX-03, аудит №45).
const OOM_LINE: &[u8] = b"memory allocation of ";

pub fn drain_stderr(mut stderr: Box<dyn Read + Send>, journal: Journal, oom: Arc<AtomicBool>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("worker-stderr".into())
        .spawn(move || {
            let mut log = StderrLog { journal, window: Instant::now(), lines: 0 };
            let mut buf = [0u8; 4096];
            let mut line = Vec::with_capacity(256);
            let mut cut = false;
            loop {
                let n = match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                for part in buf[..n].split_inclusive(|&b| b == b'\n') {
                    let (body, end) = match part.strip_suffix(b"\n") {
                        Some(body) => (body, true),
                        None => (part, false),
                    };
                    let room = STDERR_LINE.saturating_sub(line.len());
                    line.extend_from_slice(&body[..body.len().min(room)]);
                    cut |= body.len() > room;
                    if end {
                        if line.starts_with(OOM_LINE) && line.ends_with(b" failed") {
                            oom.store(true, Ordering::SeqCst);
                        }
                        log.line(&line, cut);
                        line.clear();
                        cut = false;
                    }
                }
            }
            if !line.is_empty() {
                log.line(&line, cut);
            }
        })
        .map(drop)
}
