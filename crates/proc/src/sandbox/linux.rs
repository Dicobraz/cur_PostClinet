//! Linux (LIN-04, LIN-04b, ADR 0010): user/net/ipc namespaces, rlimits, Landlock (ФС, сеть, scopes), `no_new_privs`,
//! seccomp-bpf со списком разрешённых вызовов. Без rlimits, Landlock, `no_new_privs` или seccomp воркер не работает:
//! `isolate`/`lockdown` вернут причину, клиент покажет её текстом («песочница: …»), процесс выйдет с кодом 3.
//! Namespaces — по режиму [`Namespaces`] (аргумент `--namespaces=…`): `optional` (по умолчанию) — запрет дистрибутива
//! пишется в журнал (WARN, какой sysctl) и воркер работает без них; `required` — отказ, как без остальных механизмов.
//!
//! Порядок: `isolate` — первым делом, пока процесс однопоточный (`unshare(CLONE_NEWUSER)` в многопоточном процессе
//! запрещён ядром); потом сторож времени (новый поток), потом `lockdown`.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use seccompiler::{BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule};

use super::{Launch, Level, Namespaces, Profile};
use crate::Process;

/// Предел адресного пространства воркера (`RLIMIT_AS`), если клиент не передал свой.
pub const MEMORY_LIMIT: usize = 1 << 30;

/// Старший ABI Landlock, правила которого ставим: ФС (V1–V5), сеть TCP (V4), scopes — абстрактные сокеты и сигналы
/// (V6). На ядре старше — эти же правила, на младшем — что ядро умеет (ФС закрывает уже V1, ядро 5.13).
const LANDLOCK_ABI: landlock::ABI = landlock::ABI::V6;

/// Диагностика (только отладочные сборки, по образцу `POSTMAN_SANDBOX_DROP` на Windows): `userns,rlimits,landlock,nnp,
/// seccomp` — снять перечисленные механизмы, `all` — все; `userns-deny` — вести себя так, будто namespaces запрещены. Так видно, какой запрет мешает воркеру стартовать
/// (`tests/linux_probe.rs`). В release переменная не читается и не передаётся — ослабить песочницу снаружи нельзя.
const DROP: &str = "POSTMAN_SANDBOX_DROP";

fn dropped(what: &str) -> bool {
    cfg!(debug_assertions) && std::env::var(DROP).is_ok_and(|v| v.split(',').map(str::trim).any(|d| d == what || d == "all"))
}

/// Ослабленного режима нет. `memory` — `RLIMIT_AS` процесса, `namespaces` — режим
/// namespaces: уходят аргументами `--memory=…` и `--namespaces=…`, окружение очищено.
pub fn spawn(launch: &Launch) -> io::Result<Process> {
    launch.check_args()?;
    let mut cmd = Command::new(launch.exe);
    cmd.env_clear()
        .current_dir("/")
        .args(launch.args)
        .arg(format!("--memory={}", launch.memory))
        .arg(format!("--namespaces={}", launch.namespaces.as_arg()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if cfg!(debug_assertions)
        && let Some(drop) = std::env::var_os(DROP)
    {
        cmd.env(DROP, drop);
    }
    // SAFETY: в pre_exec — только async-signal-safe close_range.
    unsafe {
        cmd.pre_exec(|| {
            // Дескрипторы родителя сверх stdin/stdout/stderr воркеру не достаются (SBX-02): закрываются при exec.
            // `CLOSE_RANGE_CLOEXEC`, а не закрытие: канал, по которому `Command` узнаёт об ошибке exec, должен жить.
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) != 0 {
                return Err(io::Error::last_os_error());
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
        background: Box::new(move |on| background(pid, on)),
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        kill: Box::new(move || {
            let mut c = child.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = c.kill();
            let _ = c.wait();
        }),
        // Код выхода (сторож времени — `EXIT_TIME`, отказ песочницы — 3) для текста ошибки; сигнал — как в оболочке,
        // 128 + номер (seccomp — `SIGSYS`, 159). Ждём завершения не дольше 500 мс, как на Windows и macOS.
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
        // Без полной песочницы воркер не работает (`isolate`/`lockdown` → выход 3), поэтому ответивший воркер — в полной
        // изоляции.
        level: Level::Full,
        fallback: None,
    })
}

/// Режим namespaces из аргумента `--namespaces=…`; без аргумента — [`Namespaces::Optional`], неизвестное значение — отказ.
fn namespaces_mode() -> Result<Namespaces, String> {
    let arg = std::env::args_os().find_map(|a| a.to_str()?.strip_prefix("--namespaces=").map(str::to_owned));
    match arg.as_deref() {
        None => Ok(Namespaces::default()),
        Some(v) => Namespaces::from_arg(v).ok_or_else(|| format!("неизвестный режим namespaces: {v}")),
    }
}

/// Отдельные user/net/ipc namespaces: без сети и без System V IPC даже при ошибке в остальных механизмах.
/// Вызывается, пока процесс однопоточный. Недоступны: `required` — отказ с причиной, `optional` — WARN в журнал.
pub fn isolate(profile: Profile) -> Result<(), String> {
    let mode = namespaces_mode()?;
    // «Сеть без файлов» остаётся в сети хоста: своего network namespace нет (ADR 0013).
    let flags = match profile {
        Profile::Sealed => libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWIPC,
        Profile::Network { .. } => libc::CLONE_NEWUSER | libc::CLONE_NEWIPC,
    };
    if dropped("userns") {
        return Ok(());
    }
    // Диагностика (отладочная сборка): как будто дистрибутив запретил namespaces — проверка обоих режимов на машине,
    // где они разрешены.
    // SAFETY: простой системный вызов без указателей.
    let err = if dropped("userns-deny") {
        io::Error::from_raw_os_error(libc::EPERM)
    } else if unsafe { libc::unshare(flags) } == 0 {
        return Ok(());
    } else {
        io::Error::last_os_error()
    };
    let sysctl = userns_sysctl();
    if mode == Namespaces::Required {
        let why = sysctl.map(|s| format!(": запрещены дистрибутивом ({s})")).unwrap_or_default();
        return Err(format!("user namespaces недоступны ({err}){why}, а режим namespaces — required"));
    }
    // Журнал — по-английски, без содержимого; stderr воркера унаследован от приложения.
    eprintln!(
        "WARN {}: user namespaces unavailable ({err}; {}), running with Landlock + seccomp + rlimits only",
        process_name(),
        sysctl.unwrap_or("no sysctl forbids them")
    );
    Ok(())
}

/// Какой sysctl дистрибутива запрещает user namespaces (читается до запирания).
fn userns_sysctl() -> Option<&'static str> {
    let read = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned());
    [
        ("/proc/sys/kernel/apparmor_restrict_unprivileged_userns", "1", "kernel.apparmor_restrict_unprivileged_userns = 1"),
        ("/proc/sys/kernel/unprivileged_userns_clone", "0", "kernel.unprivileged_userns_clone = 0"),
        ("/proc/sys/user/max_user_namespaces", "0", "user.max_user_namespaces = 0"),
    ]
    .into_iter()
    .find(|(path, bad, _)| read(path).as_deref() == Some(*bad))
    .map(|(_, _, name)| name)
}

/// Имя процесса для строк журнала (`postman-worker`, `postman-plugins`).
fn process_name() -> String {
    std::env::args_os().next().and_then(|a| Path::new(&a).file_stem().map(|s| s.to_string_lossy().into_owned())).unwrap_or_else(|| "sandbox".into())
}

/// Запирание процесса. Landlock и seccomp действуют на вызвавший поток и потоки, созданные после: в профиле
/// [`Profile::Network`] звать до первого своего потока (иначе отказ); в [`Profile::Sealed`] раньше создан только сторож.
pub fn lockdown(profile: Profile) -> Result<(), String> {
    if let Profile::Network { .. } = profile {
        return lockdown_network();
    }
    // `args_os`: `args` упал бы на аргументе не в UTF-8.
    let memory = std::env::args_os().find_map(|a| a.to_str()?.strip_prefix("--memory=")?.parse::<u64>().ok()).unwrap_or(MEMORY_LIMIT as u64);
    if !dropped("rlimits") {
        set_rlimits(memory)?;
    }
    if !dropped("landlock") {
        landlock()?;
    }
    // SAFETY: простой prctl без указателей.
    if !dropped("nnp") && unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!("no_new_privs: {}", io::Error::last_os_error()));
    }
    if dropped("seccomp") {
        return Ok(());
    }
    seccompiler::apply_filter(&filter()?).map_err(|e| format!("seccomp: {e}"))
}

fn set_rlimits(memory: u64) -> Result<(), String> {
    let set = |res, name: &str, v: u64| {
        let lim = libc::rlimit { rlim_cur: v, rlim_max: v };
        // SAFETY: передаём указатель на локальную структуру.
        if unsafe { libc::setrlimit(res, &lim) } != 0 {
            return Err(format!("setrlimit({name}): {}", io::Error::last_os_error()));
        }
        Ok(())
    };
    // Предел — частная выделенная память (SBX-03, аудит №17): `RLIMIT_DATA` с ядра 4.7 считает частные анонимные
    // отображения, как commit на Windows. `RLIMIT_AS` — запасной потолок с запасом на резервы адресного пространства.
    set(libc::RLIMIT_DATA, "RLIMIT_DATA", memory)?;
    set(libc::RLIMIT_AS, "RLIMIT_AS", (memory * 2).max(4 << 30))?;
    set(libc::RLIMIT_CORE, "RLIMIT_CORE", 0)?;
    set(libc::RLIMIT_FSIZE, "RLIMIT_FSIZE", 0)?;
    set(libc::RLIMIT_NOFILE, "RLIMIT_NOFILE", 16)?;
    set(libc::RLIMIT_NPROC, "RLIMIT_NPROC", 0)?;
    set(libc::RLIMIT_MEMLOCK, "RLIMIT_MEMLOCK", 0)?;
    set(libc::RLIMIT_MSGQUEUE, "RLIMIT_MSGQUEUE", 0)
}

/// Запрет любого доступа к ФС, TCP (bind/connect), абстрактных unix-сокетов и сигналов вне процесса — всё, что знает ядро
/// до [`LANDLOCK_ABI`]; правил-исключений нет. Ядро без Landlock — отказ с причиной.
fn landlock() -> Result<(), String> {
    use landlock::{Access, AccessFs, AccessNet, LandlockStatus, Ruleset, RulesetAttr, RulesetStatus, Scope};
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(LANDLOCK_ABI))
        .and_then(|r| r.handle_access(AccessNet::from_all(LANDLOCK_ABI)))
        .and_then(|r| r.scope(Scope::from_all(LANDLOCK_ABI)))
        .and_then(|r| r.create())
        .and_then(|r| r.restrict_self())
        .map_err(|e| format!("landlock: {e}"))?;
    if status.ruleset != RulesetStatus::NotEnforced {
        return Ok(());
    }
    Err(match status.landlock {
        LandlockStatus::NotEnabled => "landlock: выключен в ядре (нет в параметре загрузки lsm=)".into(),
        LandlockStatus::NotImplemented => "landlock: ядро собрано без него (нужно 5.13+ с CONFIG_SECURITY_LANDLOCK)".into(),
        LandlockStatus::Available { .. } => "landlock: правила не применены".into(),
    })
}

fn filter() -> Result<BpfProgram, String> {
    let cond = |arg, op| SeccompCondition::new(arg, SeccompCmpArgLen::Qword, op, 0).map_err(|e| e.to_string());
    let rule = |c: Vec<SeccompCondition>| SeccompRule::new(c).map_err(|e| e.to_string());
    let eq = |arg, v: u64| -> Result<SeccompCondition, String> {
        SeccompCondition::new(arg, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, v).map_err(|e| e.to_string())
    };
    // Ни одной исполняемой страницы: JIT и подгрузка кода невозможны.
    let no_exec = || -> Result<Vec<SeccompRule>, String> { Ok(vec![rule(vec![cond(2, SeccompCmpOp::MaskedEq(libc::PROT_EXEC as u64))?])?]) };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = [
        libc::SYS_close,
        libc::SYS_brk,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigprocmask,
        libc::SYS_sigaltstack,
        libc::SYS_clock_gettime,
        libc::SYS_getrandom,
        libc::SYS_sched_yield,
        libc::SYS_exit,
        libc::SYS_exit_group,
        // abort() при панике: raise(SIGABRT) самому себе (`tgkill` — только своему процессу, ниже).
        libc::SYS_getpid,
        libc::SYS_gettid,
        // Жив ли клиент (сторож, SBX-03).
        libc::SYS_getppid,
    ]
    .into_iter()
    .map(|s| (s, vec![]))
    .collect();
    rules.insert(libc::SYS_mmap, no_exec()?);
    rules.insert(libc::SYS_mprotect, no_exec()?);
    // Ввод — только кадры клиента (stdin), вывод — ответы (stdout) и журнал (stderr): других дескрипторов у воркера нет.
    rules.insert(libc::SYS_read, vec![rule(vec![eq(0, 0)?])?]);
    for sys in [libc::SYS_write, libc::SYS_writev] {
        rules.insert(sys, vec![rule(vec![eq(0, 1)?])?, rule(vec![eq(0, 2)?])?]);
    }
    // SAFETY: getpid без аргументов. После CLONE_NEWUSER pid тот же (pid namespace не меняется).
    let pid = unsafe { libc::getpid() };
    rules.insert(libc::SYS_tgkill, vec![rule(vec![eq(0, pid as u64)?])?]);
    let arch = std::env::consts::ARCH.try_into().map_err(|e| format!("seccomp arch: {e:?}"))?;
    SeccompFilter::new(rules, SeccompAction::KillProcess, SeccompAction::Allow, arch)
        .and_then(BpfProgram::try_from)
        .map_err(|e| format!("seccomp: {e}"))
}

/// Родитель процесса — клиент; сменился — клиент умер (сторож, SBX-03).
pub fn parent() -> u32 {
    // SAFETY: простой системный вызов без аргументов.
    unsafe { libc::getppid() as u32 }
}

/// Открыт ли дескриптор (`fcntl(F_GETFD)`; проверка `Probe::Fd`, отладочная сборка).
#[cfg(debug_assertions)]
pub fn probe_fd(fd: i32) -> Result<(), String> {
    // SAFETY: fcntl с F_GETFD только читает флаги; номер может быть закрыт — тогда ошибка EBADF.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 { Err(io::Error::last_os_error().to_string()) } else { Ok(()) }
}

/// Пониженный приоритет процесса `pid` и его возврат: планировщик `SCHED_BATCH` ↔ `SCHED_OTHER` и класс ввода-вывода
/// idle ↔ best-effort — у каждого потока процесса. `nice` не трогаем: без прав его не вернуть обратно.
fn background(pid: u32, on: bool) -> io::Result<()> {
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    // IOPRIO_PRIO_VALUE(class, data): idle — класс 3; best-effort — класс 2, уровень 4 (по умолчанию).
    let ioprio: libc::c_int = if on { 3 << 13 } else { (2 << 13) | 4 };
    let policy = if on { libc::SCHED_BATCH } else { libc::SCHED_OTHER };
    let param = libc::sched_param { sched_priority: 0 };
    let mut first = None;
    for entry in std::fs::read_dir(format!("/proc/{pid}/task"))? {
        let Some(tid) = entry?.file_name().to_str().and_then(|n| n.parse::<libc::pid_t>().ok()) else { continue };
        // SAFETY: системные вызовы с номером потока и указателем на локальную структуру.
        let failed = unsafe {
            libc::sched_setscheduler(tid, policy, &param) != 0 || libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, tid, ioprio) != 0
        };
        if failed {
            first.get_or_insert_with(io::Error::last_os_error);
        }
    }
    first.map_or(Ok(()), Err)
}

/// Что читает процесс «сеть без файлов»: настройки разрешения имён и корневые сертификаты (ADR 0013). Чего нет на
/// машине — пропускается.
const NETWORK_READ: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/run/systemd/resolve",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/usr/share/ca-certificates",
    "/usr/local/share/ca-certificates",
    "/usr/share/pki",
    "/var/lib/ca-certificates",
];

/// Сколько дескрипторов у процесса «сеть без файлов»: соединения всех учёток вида.
const NETWORK_NOFILE: u64 = 1024;

fn lockdown_network() -> Result<(), String> {
    // `args_os`: `args` упал бы на аргументе не в UTF-8.
    let memory = std::env::args_os().find_map(|a| a.to_str()?.strip_prefix("--memory=")?.parse::<u64>().ok()).unwrap_or(MEMORY_LIMIT as u64);
    let threads = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Threads:")?.trim().parse::<u32>().ok()))
        .ok_or("не прочитано число потоков процесса")?;
    if threads != 1 {
        return Err(format!("запирание после запуска потоков ({threads}): Landlock и seccomp их не охватят"));
    }
    if !dropped("rlimits") {
        set_network_rlimits(memory)?;
    }
    if !dropped("landlock") {
        landlock_network()?;
    }
    // SAFETY: простой prctl без указателей.
    if !dropped("nnp") && unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!("no_new_privs: {}", io::Error::last_os_error()));
    }
    if dropped("seccomp") {
        return Ok(());
    }
    // Сначала запреты с завершением процесса, потом список разрешённого: после него новый фильтр уже не поставить.
    seccompiler::apply_filter(&network_kill_filter()?).map_err(|e| format!("seccomp: {e}"))?;
    seccompiler::apply_filter(&network_filter()?).map_err(|e| format!("seccomp: {e}"))
}

fn set_network_rlimits(memory: u64) -> Result<(), String> {
    let set = |res, name: &str, v: u64| {
        let lim = libc::rlimit { rlim_cur: v, rlim_max: v };
        // SAFETY: передаём указатель на локальную структуру.
        if unsafe { libc::setrlimit(res, &lim) } != 0 {
            return Err(format!("setrlimit({name}): {}", io::Error::last_os_error()));
        }
        Ok(())
    };
    // Как у воркера, кроме числа дескрипторов (соединения) и `RLIMIT_NPROC` (потоки нужны; процессы закрывает seccomp).
    set(libc::RLIMIT_DATA, "RLIMIT_DATA", memory)?;
    set(libc::RLIMIT_AS, "RLIMIT_AS", (memory * 2).max(4 << 30))?;
    set(libc::RLIMIT_CORE, "RLIMIT_CORE", 0)?;
    set(libc::RLIMIT_FSIZE, "RLIMIT_FSIZE", 0)?;
    set(libc::RLIMIT_NOFILE, "RLIMIT_NOFILE", NETWORK_NOFILE)?;
    set(libc::RLIMIT_MEMLOCK, "RLIMIT_MEMLOCK", 0)?;
    set(libc::RLIMIT_MSGQUEUE, "RLIMIT_MSGQUEUE", 0)
}

/// ФС — только чтение [`NETWORK_READ`]; TCP — исходящие на любой порт (`connect` правилами не ограничен), `bind`
/// запрещён: входящих соединений нет; абстрактные unix-сокеты и сигналы вне процесса — как у воркера.
fn landlock_network() -> Result<(), String> {
    use landlock::{Access, AccessFs, AccessNet, LandlockStatus, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules};
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(LANDLOCK_ABI))
        .and_then(|r| r.handle_access(AccessNet::BindTcp))
        .and_then(|r| r.scope(Scope::from_all(LANDLOCK_ABI)))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(NETWORK_READ, read)))
        .and_then(|r| r.restrict_self())
        .map_err(|e| format!("landlock: {e}"))?;
    if status.ruleset != RulesetStatus::NotEnforced {
        return Ok(());
    }
    Err(match status.landlock {
        LandlockStatus::NotEnabled => "landlock: выключен в ядре (нет в параметре загрузки lsm=)".into(),
        LandlockStatus::NotImplemented => "landlock: ядро собрано без него (нужно 5.13+ с CONFIG_SECURITY_LANDLOCK)".into(),
        LandlockStatus::Available { .. } => "landlock: правила не применены".into(),
    })
}

/// Запуск программ и чтение чужой памяти — признак взлома: процесс завершается (остальное решает [`network_filter`]).
fn network_kill_filter() -> Result<BpfProgram, String> {
    let rules: BTreeMap<i64, Vec<SeccompRule>> =
        [libc::SYS_execve, libc::SYS_execveat, libc::SYS_ptrace, libc::SYS_process_vm_readv, libc::SYS_process_vm_writev]
            .into_iter()
            .map(|s| (s, vec![]))
            .collect();
    let arch = std::env::consts::ARCH.try_into().map_err(|e| format!("seccomp arch: {e:?}"))?;
    SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::KillProcess, arch)
        .and_then(BpfProgram::try_from)
        .map_err(|e| format!("seccomp: {e}"))
}

/// Список разрешённых вызовов процесса «сеть без файлов»: память, потоки, время, чтение файлов (какие — решает
/// Landlock), сокеты IPv4/IPv6 наружу, опрос готовности. Остальное — отказ `ENOSYS`, а не завершение: кода много
/// (сеть, TLS, библиотеки протоколов), и незнакомый безобидный вызов на другом дистрибутиве не должен ронять почту —
/// так же поступают профили seccomp по умолчанию у Docker и Podman. Нет: `bind`, `listen`, `accept` (входящие),
/// `fork` и `clone` без `CLONE_THREAD` (процессы), исполняемых страниц (подгрузка кода), записи в файлы.
fn network_filter() -> Result<BpfProgram, String> {
    let rule = |c: Vec<SeccompCondition>| SeccompRule::new(c).map_err(|e| e.to_string());
    let eq = |arg, v: u64| -> Result<SeccompCondition, String> {
        SeccompCondition::new(arg, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, v).map_err(|e| e.to_string())
    };
    let masked = |arg, mask: u64, v: u64| -> Result<SeccompCondition, String> {
        SeccompCondition::new(arg, SeccompCmpArgLen::Qword, SeccompCmpOp::MaskedEq(mask), v).map_err(|e| e.to_string())
    };
    let no_exec = || -> Result<Vec<SeccompRule>, String> { Ok(vec![rule(vec![masked(2, libc::PROT_EXEC as u64, 0)?])?]) };
    let any_of = |arg, values: &[u64]| -> Result<Vec<SeccompRule>, String> { values.iter().map(|v| rule(vec![eq(arg, *v)?])).collect() };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = [
        // Память, потоки, сигналы, время — как у воркера.
        libc::SYS_close,
        libc::SYS_brk,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigaction,
        libc::SYS_sigaltstack,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_gettimeofday,
        libc::SYS_getrandom,
        libc::SYS_sched_yield,
        libc::SYS_sched_getaffinity,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getppid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_uname,
        libc::SYS_sysinfo,
        libc::SYS_set_robust_list,
        libc::SYS_set_tid_address,
        libc::SYS_rseq,
        libc::SYS_membarrier,
        libc::SYS_getcpu,
        libc::SYS_restart_syscall,
        // Ввод-вывод по дескрипторам: кадры клиента, сокеты, чтение настроек имён и сертификатов.
        libc::SYS_read,
        libc::SYS_readv,
        libc::SYS_pread64,
        libc::SYS_write,
        libc::SYS_writev,
        libc::SYS_lseek,
        libc::SYS_openat,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_getdents64,
        libc::SYS_readlinkat,
        libc::SYS_faccessat,
        libc::SYS_faccessat2,
        libc::SYS_fcntl,
        libc::SYS_dup,
        // Сокеты наружу (вид сокета — ниже).
        libc::SYS_connect,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_getsockopt,
        libc::SYS_setsockopt,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_shutdown,
        // Опрос готовности (tokio, mio).
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_pwait,
        libc::SYS_epoll_pwait2,
        libc::SYS_eventfd2,
        libc::SYS_pipe2,
        libc::SYS_ppoll,
    ]
    .into_iter()
    .map(|s| (s, vec![]))
    .collect();
    // Вызовы старого вида есть только на x86_64; glibc и Rust ещё зовут часть из них.
    #[cfg(target_arch = "x86_64")]
    for sys in [libc::SYS_poll, libc::SYS_epoll_wait, libc::SYS_time, libc::SYS_stat, libc::SYS_lstat, libc::SYS_access, libc::SYS_readlink] {
        rules.insert(sys, vec![]);
    }
    // Ни одной исполняемой страницы: JIT и подгрузка кода (в том числе модулей NSS) невозможны.
    rules.insert(libc::SYS_mmap, no_exec()?);
    rules.insert(libc::SYS_mprotect, no_exec()?);
    // Только потоки: `clone` без `CLONE_THREAD` (новый процесс) и `clone3` (флаги в структуре, не проверить) — отказ;
    // glibc после отказа `clone3` создаёт поток через `clone`.
    rules.insert(libc::SYS_clone, vec![rule(vec![masked(0, libc::CLONE_THREAD as u64, libc::CLONE_THREAD as u64)?])?]);
    // Сокеты — только IPv4 и IPv6: ни unix-сокетов (службы машины), ни netlink, ни «сырых» пакетов канального уровня.
    rules.insert(libc::SYS_socket, any_of(0, &[libc::AF_INET as u64, libc::AF_INET6 as u64])?);
    // `ioctl` — только режим и состояние дескриптора (std ставит неблокирующий режим через FIONBIO).
    // Тип номера запроса `ioctl` зависит от libc (glibc — u64, musl — i32).
    #[allow(clippy::unnecessary_cast)]
    rules.insert(libc::SYS_ioctl, any_of(1, &[libc::FIONBIO as u64, libc::FIONREAD as u64, libc::FIOCLEX as u64, libc::FIONCLEX as u64])?);
    // Имя потока.
    rules.insert(libc::SYS_prctl, any_of(0, &[libc::PR_SET_NAME as u64])?);
    // Пределы — только читать (размер стека потока).
    rules.insert(libc::SYS_prlimit64, vec![rule(vec![eq(2, 0)?])?]);
    // SAFETY: getpid без аргументов. После CLONE_NEWUSER pid тот же (pid namespace не меняется).
    let pid = unsafe { libc::getpid() };
    rules.insert(libc::SYS_tgkill, vec![rule(vec![eq(0, pid as u64)?])?]);
    let arch = std::env::consts::ARCH.try_into().map_err(|e| format!("seccomp arch: {e:?}"))?;
    SeccompFilter::new(rules, SeccompAction::Errno(libc::ENOSYS as u32), SeccompAction::Allow, arch)
        .and_then(BpfProgram::try_from)
        .map_err(|e| format!("seccomp: {e}"))
}
