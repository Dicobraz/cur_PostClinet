# PLG-05: Telegram — отдельным процессом

lane: x/plg-telegram         # crates/backend-telegram, crates/plugins, app-gpui (корень, вход), deps
status: todo
deps: [PLG-04]
class: A
files: [crates/backend-telegram/*, crates/plugins/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/account_telegram.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/SIZE.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0012-channels.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-05.md

## Задача
- `backend-telegram` — в наборе плагинов, из `postman.exe` уходит; признак сборки `telegram` убирается.
- Ключ приложения (`api_id`/`api_hash`) остаётся в плагине; есть ли он — плагин сообщает в `Hello`, `api_key` из
  интерфейса не зовётся.
- Вход по QR, коду и паролю двухэтапной проверки — потоком `Prompt`/`Answer` по обмену.
- Замер `postman.exe` до и после — в `docs/SIZE.md`.

## Готово
Вход, диалоги, история, отправка, правки, файлы — через процесс (ручная проверка); тесты договора зелёные;
`grammers-*` нет в дереве зависимостей `postman-gpui`; пакеты трёх ОС работают с каналом.
