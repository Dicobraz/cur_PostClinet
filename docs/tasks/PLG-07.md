# PLG-07: сериализация команд и событий, области через ядро, DAV — отдельным процессом

lane: x/plg-areas           # core (договор), backend-dav, app-gpui
status: todo
deps: [PLG-06]
class: B                    # договор sync::Command / sync::Event и путей областей (ADR 0004)
files: [crates/core/src/sync.rs, crates/core/src/model.rs, crates/core/src/backend/*, crates/core/src/schedule.rs, crates/core/src/settings.rs, crates/core/src/tasks/*, crates/backend-dav/*, crates/plugins/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/remote_images.rs, app-gpui/src/meeting.rs, app-gpui/src/calendar/subscriptions.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0004-protocol-plugins.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-07.md: календарь и контакты IMAP + CalDAV/CardDAV, картинки письма, подписки

## Задача
- `serde` на `sync::Command` (86 вариантов), `sync::Event` (71) и вложенных типах; тест «туда и обратно» на каждый
  вариант. Смысл договора не меняется.
- Области через ядро: команды областей учётки разводит ядро (сначала область — «не моя, назад», затем почтовый
  путь), `Features` объединяет ядро, опрос `changed` раз в 2 минуты — таймер ядра. Почтовые пути внутри
  `postman.exe` (до своих волн) работают с областью-процессом так же.
- `backend-dav` — в наборе плагинов; задачи поверх кэша (`tasks::handle`) — услугой ядра.
- Загрузка по открытой ссылке (картинки письма, подписки на календари, внешняя занятость) — командой плагина DAV;
  `fetch_public`, `fetch_changed`, `public_url_ok` из интерфейса не зовутся. Процесс стартует по первой загрузке и
  завершается после 5 минут без учёток с областью и без загрузок.

## Готово
Календарь, задачи и контакты учётки IMAP + CalDAV/CardDAV, картинки письма и подписки работают через процесс; тесты
стенда `stand_standards` зелёные; `postman-dav` нет в дереве зависимостей `postman-gpui`; замер `first_sync_ms` учётки
«стандарты» до и после.
