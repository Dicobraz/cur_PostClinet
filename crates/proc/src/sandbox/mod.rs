//! Платформенная изоляция: `spawn` — на стороне клиента, `isolate` и `lockdown` — в самом дочернем процессе.
//! Профиль ([`Profile`]) задан при запуске и вшит в код процесса: аргументом или окружением его не понизить.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(target_os = "linux", debug_assertions))]
pub use linux::probe_fd;
#[cfg(target_os = "linux")]
pub use linux::{MEMORY_LIMIT, isolate, lockdown, parent, spawn};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{MEMORY_LIMIT, lockdown, parent, remove_profile, spawn};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Limited;
#[cfg(all(target_os = "macos", debug_assertions))]
pub use macos::probe;
#[cfg(all(target_os = "macos", debug_assertions))]
pub use macos::probe_fd;
#[cfg(target_os = "macos")]
pub use macos::{MEMORY_LIMIT, lockdown, parent, spawn};

/// Профиль песочницы дочернего процесса (ADR 0013, таблица «Песочница»).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Воркер и хост модулей просмотра: ни сети, ни файлов, ни дочерних процессов — только stdin, stdout, stderr.
    #[default]
    Sealed,
    /// «Сеть без файлов» — плагины протоколов: исходящая сеть, разрешение имён и проверка сертификатов; файлов
    /// пользователя, дочерних процессов и входящих соединений нет.
    Network {
        /// Вход учётной записью организации (Windows: право `enterpriseAuthentication` — SSPI, Kerberos); нужен EWS
        /// и LDAP, остальным плагинам не даётся. На macOS и Linux пока ничего не открывает (ADR 0013).
        enterprise_auth: bool,
    },
}

/// Что и как запустить.
#[derive(Debug, Clone, Copy)]
pub struct Launch<'a> {
    pub exe: &'a std::path::Path,
    /// Аргументы процесса (вид плагина: `--plugin=imap`): только латиница, цифры и `-`, `_`, `=`, `.` — иначе отказ.
    pub args: &'a [&'a str],
    /// Предел памяти процесса.
    pub memory: usize,
    /// Режим namespaces (Linux).
    pub namespaces: Namespaces,
    pub profile: Profile,
}

impl<'a> Launch<'a> {
    /// Запуск в профиле [`Profile::Sealed`] с пределом памяти платформы и без аргументов.
    pub fn sealed(exe: &'a std::path::Path) -> Self {
        Self { exe, args: &[], memory: MEMORY_LIMIT, namespaces: Namespaces::default(), profile: Profile::Sealed }
    }

    /// Аргументы допустимы: ни пробелов, ни кавычек — командную строку Windows собираем без экранирования.
    pub(crate) fn check_args(&self) -> std::io::Result<()> {
        let ok = |a: &&str| !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'=' | b'.'));
        if self.args.iter().all(ok) {
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "недопустимый аргумент процесса"))
        }
    }
}

/// Профиль песочницы на диске есть только на Windows (AppContainer); на остальных платформах удалять нечего.
#[cfg(not(windows))]
pub fn remove_profile() -> Result<(), String> {
    Ok(())
}

/// Шаг изоляции, которому нужен однопоточный процесс (Linux: namespaces); на остальных платформах его нет.
#[cfg(not(target_os = "linux"))]
pub fn isolate(_: Profile) -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn lockdown(_: Profile) -> Result<(), String> {
    Err("платформа не поддерживается".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn parent() -> u32 {
    0
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub const MEMORY_LIMIT: usize = 1 << 30;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn spawn(_: &Launch) -> std::io::Result<crate::Process> {
    Err(std::io::Error::other("платформа не поддерживается"))
}

/// Что удалось включить при запуске воркера (для статус-строки и логов).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    None,
    /// Все механизмы платформы.
    Full,
    /// Часть механизмов недоступна (например, нет AppContainer-профиля или user namespaces).
    Partial,
}

/// Нужны ли процессу песочницы отдельные user/net/ipc namespaces (Linux, ADR 0010). Политику администратора
/// (`SandboxNamespaces`) читает приложение (LIN-03) и передаёт клиенту (`Client::namespaces` воркера); на остальных
/// платформах режим не используется.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Namespaces {
    /// Дистрибутив запретил — работаем без них (WARN в журнале); Landlock, seccomp, rlimits остаются обязательными.
    #[default]
    Optional,
    /// Без namespaces процесс не работает.
    Required,
}

impl Namespaces {
    /// Значение аргумента `--namespaces=…`.
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }

    pub fn from_arg(v: &str) -> Option<Self> {
        match v {
            "optional" => Some(Self::Optional),
            "required" => Some(Self::Required),
            _ => None,
        }
    }
}

/// Дескрипторы по номерам — только Unix; на Windows наследование задаёт `HANDLE_LIST` (stdin, stdout, stderr).
#[cfg(all(debug_assertions, not(unix)))]
pub fn probe_fd(_fd: i32) -> Result<(), String> {
    Err("только Unix".into())
}
