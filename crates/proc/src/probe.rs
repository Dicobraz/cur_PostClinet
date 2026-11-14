//! Договор процесса проверок песочницы (`postman-proc-probe`, только тесты): клиент вызывает проверку, процесс
//! отвечает, что у него получилось.

use serde::{Deserialize, Serialize};

/// Что попробовать сделать изнутри песочницы.
#[derive(Debug, Serialize, Deserialize)]
pub enum Check {
    /// Вернуть двоичную часть как есть.
    Echo,
    /// Соединиться по TCP с адресом `host:port`.
    Connect(String),
    /// Разрешить имя.
    Resolve(String),
    /// Прочитать файл.
    ReadFile(String),
    /// Создать файл.
    WriteFile(String),
    /// Запустить программу.
    Spawn(String),
    /// Принять входящие: `bind` и `listen` на всех адресах.
    Listen,
    /// Запустить поток и дождаться его.
    Thread,
    /// Не отвечать (зависший процесс).
    Hang,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Done,
    /// Слушает порт (проверка [`Check::Listen`] удалась — входящие проверяет клиент).
    Listening(u16),
    /// Не получилось: текст ошибки ОС.
    Refused(String),
}
