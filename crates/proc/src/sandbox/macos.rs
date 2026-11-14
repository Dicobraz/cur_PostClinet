//! macOS (MAC-01, MAC-03, `docs/adr/0009-macos-sandbox.md`): Seatbelt — `sandbox_init` с профилем «запрещено всё»
//! (`deny default`) в самом воркере, до первого запроса; пределы `setrlimit` (без дампа памяти, ≤ 16 дескрипторов) и
//! предел кучи — распределитель [`Limited`]. `RLIMIT_FSIZE = 0`, как на Linux: stderr воркера — канал к клиенту (SBX-01),
//! клиент пишет строки в журнал приложения; в обычные файлы воркер не пишет вовсе.
//!
//! После запирания у воркера остаются только уже открытые stdin, stdout и stderr: ни файлов, ни сети, ни дочерних
//! процессов, ни служб Mach, ни sysctl. Профиль ничего не открывает; понадобится открыть — только явным правилом здесь,
//! с причиной в комментарии, диагностикой и правкой ADR.
//!
//! Предел памяти: `RLIMIT_AS` macOS не соблюдает, Job Object нет. Кучу Rust считает [`Limited`]: запрос сверх предела
//! (`--memory=…` от клиента: [`MEMORY_LIMIT`] у воркера, `wasm::JOB_MEMORY` у хоста модулей) — выход с
//! [`crate::EXIT_MEMORY`], клиент пишет «превышен предел памяти».

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{Launch, Level, Profile};
use crate::Process;

/// Предел кучи воркера; хосту модулей клиент передаёт свой (`wasm::JOB_MEMORY`).
pub const MEMORY_LIMIT: usize = 1 << 30;

/// Профиль Seatbelt (SBPL): запрещено всё, исключений нет.
const PROFILE: &str = "(version 1)\n(deny default)\n";

/// Профиль «сеть без файлов» (ADR 0013): к «запрещено всё» — исходящие соединения IP, разрешение имён (сокет
/// mDNSResponder и его настройки) и проверка сертификатов системой (службы trustd, хранилища корневых сертификатов —
/// только чтение). Входящих соединений (`network-inbound`, `network-bind`), запуска процессов и файлов пользователя нет.
const NETWORK_PROFILE: &str = r#"(version 1)
(deny default)
(allow network-outbound (remote ip "*:*"))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow system-socket)
(allow mach-lookup
  (global-name "com.apple.dnssd.service")
  (global-name "com.apple.SystemConfiguration.configd")
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.trustd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.SecurityServer")
  (global-name "com.apple.ocspd"))
(allow file-read*
  (literal "/private/etc/resolv.conf")
  (literal "/private/var/run/resolv.conf")
  (literal "/private/etc/hosts")
  (subpath "/System/Library/Keychains")
  (subpath "/Library/Keychains")
  (subpath "/private/var/db/mds")
  (subpath "/System/Library/Security")
  (literal "/Library/Preferences/com.apple.security.plist")
  (literal "/Library/Preferences/SystemConfiguration/preferences.plist"))
(allow file-read-metadata)
(allow sysctl-read)
"#;

/// Сколько дескрипторов у процесса «сеть без файлов»: соединения всех учёток вида.
const NETWORK_NOFILE: u64 = 1024;

/// Диагностика (только отладочные сборки, по образцу `POSTMAN_SANDBOX_DROP` на Windows):
/// `POSTMAN_SANDBOX_ALLOW=mach-lookup,sysctl-read,…` добавляет к профилю `(allow …)` по каждой операции SBPL, `all` —
/// `(allow default)`, `off` — без `sandbox_init`. Так видно, какой запрет мешает воркеру стартовать
/// (`tests/macos_probe.rs`). В release переменная не читается и не передаётся — ослабить песочницу снаружи нельзя.
const ALLOW: &str = "POSTMAN_SANDBOX_ALLOW";

// <sandbox.h>, <sys/resource.h>, <unistd.h> — libSystem, линкуется всегда.
unsafe extern "C" {
    fn getdtablesize() -> c_int;
    fn getppid() -> c_int;
    fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
    fn setrlimit(resource: c_int, rlp: *const RLimit) -> c_int;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn _exit(status: c_int) -> !;
}

#[repr(C)]
struct RLimit {
    cur: u64,
    max: u64,
}

const RLIMIT_FSIZE: c_int = 1;
const RLIMIT_CORE: c_int = 4;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;
const RLIMIT_NOFILE: c_int = 8;

/// Ослабленного режима нет. `memory` — предел кучи процесса ([`Limited`]): уходит
/// аргументом `--memory=…`, окружение очищено.
pub fn spawn(launch: &Launch) -> io::Result<Process> {
    launch.check_args()?;
    let mut cmd = Command::new(launch.exe);
    cmd.env_clear()
        .current_dir("/")
        .args(launch.args)
        .arg(format!("--memory={}", launch.memory))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if cfg!(debug_assertions)
        && let Some(allow) = std::env::var_os(ALLOW)
    {
        cmd.env(ALLOW, allow);
    }
    // SAFETY: в pre_exec — только async-signal-safe getdtablesize и fcntl.
    unsafe {
        cmd.pre_exec(|| {
            // Дескрипторы родителя сверх stdin/stdout/stderr воркеру не достаются (SBX-02): помечаются закрываемыми
            // при exec (канал, по которому `Command` узнаёт об ошибке exec, уже такой). Ошибка на свободном номере —
            // норма.
            for fd in 3..getdtablesize().min(65_536) {
                fcntl(fd, F_SETFD, FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("worker pipes are missing"));
    };
    let pid = child.id();
    let child = Arc::new(Mutex::new(child));
    let watched = Arc::clone(&child);
    Ok(Process {
        // Фоновый режим процесса (`PRIO_DARWIN_PROCESS`, `PRIO_DARWIN_BG`): планировщик и ввод-вывод разом.
        background: Box::new(move |on| {
            const PRIO_DARWIN_PROCESS: c_int = 4;
            const PRIO_DARWIN_BG: c_int = 0x1000;
            // SAFETY: простой системный вызов с номером своего дочернего процесса.
            if unsafe { setpriority(PRIO_DARWIN_PROCESS, pid, if on { PRIO_DARWIN_BG } else { 0 }) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }),
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        kill: Box::new(move || {
            let mut c = child.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = c.kill();
            let _ = c.wait();
        }),
        // Код выхода (сторож времени — `EXIT_TIME`, предел памяти — `EXIT_MEMORY`) для текста ошибки; сигнал — как в
        // оболочке, 128 + номер. Ждём завершения не дольше 500 мс, как на Windows.
        exit_code: Box::new(move || {
            let mut c = watched.lock().unwrap_or_else(PoisonError::into_inner);
            let until = Instant::now() + Duration::from_millis(500);
            loop {
                match c.try_wait() {
                    Ok(Some(st)) => return st.code().map(|v| v as u32).or_else(|| st.signal().map(|s| 128 + s as u32)),
                    Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
                    _ => return None,
                }
            }
        }),
        // Без песочницы воркер не работает (`lockdown` → выход 3), поэтому ответивший воркер — в полной изоляции.
        level: Level::Full,
        fallback: None,
    })
}

pub fn lockdown(kind: Profile) -> Result<(), String> {
    // `args_os`: `args` упал бы на аргументе не в UTF-8.
    let memory = std::env::args_os().find_map(|a| a.to_str()?.strip_prefix("--memory=")?.parse::<usize>().ok()).unwrap_or(MEMORY_LIMIT);
    LIMIT.store(memory, Relaxed);
    set_rlimits(kind)?;
    let allow = if cfg!(debug_assertions) { std::env::var(ALLOW).unwrap_or_default() } else { String::new() };
    let Some(profile) = profile(kind, &allow)? else { return Ok(()) };
    let mut err: *mut c_char = null_mut();
    // SAFETY: профиль — строка C, живая до конца вызова; `err` — указатель на локальную переменную, строку ошибки
    // освобождает `sandbox_free_error`.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut err) };
    if rc == 0 {
        return Ok(());
    }
    let os = io::Error::last_os_error();
    let text = if err.is_null() {
        String::new()
    } else {
        // SAFETY: при ошибке `sandbox_init` кладёт в `err` строку C; копируем её и освобождаем один раз.
        let s = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        unsafe { sandbox_free_error(err) };
        format!(": {s}")
    };
    // Что мешает: текст профиля (ошибка разбора), errno (процесс уже в другой песочнице — `EPERM`); какой запрет не даёт
    // работать после запирания — `tests/macos_probe.rs` и журнал Seatbelt (`docs/DEV.md`, «Диагностика песочницы»).
    Err(format!("sandbox_init: код {rc}, {os}{text}"))
}

/// Профиль с исключениями диагностики (`allow` — из [`ALLOW`]); `None` — без Seatbelt (`off`).
fn profile(kind: Profile, allow: &str) -> Result<Option<CString>, String> {
    let mut text = String::from(match kind {
        Profile::Sealed => PROFILE,
        Profile::Network { .. } => NETWORK_PROFILE,
    });
    for op in allow.split(',').map(str::trim).filter(|op| !op.is_empty()) {
        match op {
            "off" => return Ok(None),
            "all" => text.push_str("(allow default)\n"),
            // Только имя операции SBPL: ни скобок, ни кавычек — не правило целиком.
            op if op.bytes().all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'*') => text.push_str(&format!("(allow {op})\n")),
            op => return Err(format!("{ALLOW}: не операция SBPL: {op}")),
        }
    }
    CString::new(text).map(Some).map_err(|e| e.to_string())
}

fn set_rlimits(kind: Profile) -> Result<(), String> {
    let set = |res, v: u64| {
        let lim = RLimit { cur: v, max: v };
        // SAFETY: передаём указатель на локальную структуру.
        if unsafe { setrlimit(res, &lim) } != 0 {
            return Err(format!("setrlimit({res}): {}", io::Error::last_os_error()));
        }
        Ok(())
    };
    set(RLIMIT_CORE, 0)?;
    // stderr — канал к клиенту (SBX-01), не файл журнала: запись в обычные файлы не нужна вовсе.
    set(RLIMIT_FSIZE, 0)?;
    set(RLIMIT_NOFILE, if kind == Profile::Sealed { 16 } else { NETWORK_NOFILE })
}

/// Байт кучи Rust занято сейчас.
static USED: AtomicUsize = AtomicUsize::new(0);
/// Предел кучи; до `lockdown` — без предела.
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Распределитель процессов песочницы на macOS (`#[global_allocator]` в `postman-worker` и `postman-viewer`): системный,
/// плюс счётчик занятого. Запрос сверх предела — не `null` (тогда Rust прервал бы процесс сигналом `SIGABRT` без
/// объяснения), а выход с [`crate::EXIT_MEMORY`]. Считается только куча Rust: стеки потоков и память libSystem — нет,
/// они малы и не растут от чужих данных.
pub struct Limited;

/// Занять `n` байт счётчика; сверх предела — выход.
fn take(n: usize) {
    let used = USED.fetch_add(n, Relaxed).saturating_add(n);
    if used > LIMIT.load(Relaxed) {
        over();
    }
}

fn give(n: usize) {
    USED.fetch_sub(n, Relaxed);
}

/// Предел превышен: строка в журнал (stderr) и выход. Ничего не выделяет: распределитель здесь уже недоступен.
fn over() -> ! {
    const TEXT: &[u8] = b"postman-worker: memory limit exceeded\n";
    // SAFETY: `write` получает статический буфер и его длину; `_exit` завершает процесс без обработчиков `atexit`.
    unsafe {
        let _ = write(2, TEXT.as_ptr().cast(), TEXT.len());
        _exit(crate::EXIT_MEMORY)
    }
}

// SAFETY: все вызовы — в системный распределитель с теми же аргументами; сверху только счётчик и выход при пределе.
unsafe impl GlobalAlloc for Limited {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        take(layout.size());
        // SAFETY: контракт `GlobalAlloc::alloc` передаётся как есть.
        let p = unsafe { System.alloc(layout) };
        if p.is_null() {
            give(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        take(layout.size());
        // SAFETY: контракт `GlobalAlloc::alloc_zeroed` передаётся как есть.
        let p = unsafe { System.alloc_zeroed(layout) };
        if p.is_null() {
            give(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: контракт `GlobalAlloc::dealloc` передаётся как есть.
        unsafe { System.dealloc(ptr, layout) };
        give(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old = layout.size();
        if new_size > old {
            take(new_size - old);
        }
        // SAFETY: контракт `GlobalAlloc::realloc` передаётся как есть.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        match (p.is_null(), new_size > old) {
            // Не вышло: блок прежний, занятое сверх него — вернуть.
            (true, true) => give(new_size - old),
            (false, false) => give(old - new_size),
            _ => {}
        }
        p
    }
}

/// Проверки песочницы для тестов (только отладочная сборка): служба Mach и sysctl должны быть недоступны.
#[cfg(debug_assertions)]
pub mod probe {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::ptr::null_mut;

    unsafe extern "C" {
        static bootstrap_port: u32;
        fn bootstrap_look_up(bp: u32, service_name: *const c_char, sp: *mut u32) -> c_int;
        fn sysctlbyname(name: *const c_char, oldp: *mut c_void, oldlenp: *mut usize, newp: *mut c_void, newlen: usize) -> c_int;
    }

    /// Найти службу Mach `name` у launchd: `Ok` — порт получен.
    pub fn mach_lookup(name: &str) -> Result<(), String> {
        let name = CString::new(name).map_err(|e| e.to_string())?;
        let mut port = 0u32;
        // SAFETY: `bootstrap_port` — порт процесса от libSystem, только читается; имя — строка C, `port` — локальная.
        let kr = unsafe { bootstrap_look_up(bootstrap_port, name.as_ptr(), &mut port) };
        if kr == 0 { Ok(()) } else { Err(format!("bootstrap_look_up: {kr}")) }
    }

    /// Прочитать sysctl `name`: `Ok` — значение получено.
    pub fn sysctl(name: &str) -> Result<(), String> {
        let name = CString::new(name).map_err(|e| e.to_string())?;
        // Только размер значения: проверка Seatbelt — та же, а мал ли буфер, не важно.
        let mut len = 0usize;
        // SAFETY: имя — строка C, длина — локальная; ни буфера, ни нового значения.
        let rc = unsafe { sysctlbyname(name.as_ptr(), null_mut(), &mut len, null_mut(), 0) };
        if rc == 0 { Ok(()) } else { Err(format!("sysctlbyname: {}", std::io::Error::last_os_error())) }
    }
}

/// Родитель процесса — клиент; сменился — клиент умер (сторож, SBX-03).
pub fn parent() -> u32 {
    // SAFETY: простой системный вызов без аргументов.
    unsafe { getppid() as u32 }
}

/// Открыт ли дескриптор (`fcntl(F_GETFD)`; проверка `Probe::Fd`, отладочная сборка).
#[cfg(debug_assertions)]
pub fn probe_fd(fd: i32) -> Result<(), String> {
    const F_GETFD: c_int = 1;
    // SAFETY: fcntl с F_GETFD только читает флаги; номер может быть закрыт — тогда ошибка EBADF.
    if unsafe { fcntl(fd, F_GETFD) } < 0 { Err(io::Error::last_os_error().to_string()) } else { Ok(()) }
}
