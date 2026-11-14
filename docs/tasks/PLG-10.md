# PLG-10: JMAP — отдельным процессом

lane: x/plg-jmap         # crates/backend-jmap, crates/plugins, app-gpui (корень)
status: todo
deps: [PLG-09]
class: A
files: [crates/backend-jmap/*, crates/plugins/*, crates/core/src/backend/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/signin.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/SIZE.md, docs/MEMORY.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-10.md

## Задача
- `backend-jmap` — в наборе плагинов, на услугах ядра из PLG-08; из `postman.exe` уходит.
- Основной путь переносится последним. Push (EventSource), выгрузка и перенос между учётками (`Import`,
  `ServerCopy`) — в процессе плагина.
- `settings::ONE_TEXT_ONLY` — в `model::ErrorKind`; интерфейс от крейта плагина не зависит.
- После волны в `postman.exe` нет сетевого стека протоколов (reqwest, rustls) — проверить деревом зависимостей
  и замером.
- Сторож скорости: `first_frame_ms`, `first_sync_ms`, `list_ms` до и после — первый кадр не хуже, первая
  синхронизация не хуже чем на 10 %.

## Готово
Основной путь (The Post, JMAP) работает через процесс: почта, календарь, контакты, задачи, настройки, push;
тесты `stand` зелёные; `postman-jmap` и `reqwest` нет в дереве зависимостей `postman-gpui`; замер — в `docs/MEMORY.md`.
