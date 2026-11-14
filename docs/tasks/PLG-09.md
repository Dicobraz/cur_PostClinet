# PLG-09: IMAP + SMTP — отдельным процессом

lane: x/plg-imap         # crates/backend-imap, crates/plugins, app-gpui (корень)
status: todo
deps: [PLG-08]
class: A
files: [crates/backend-imap/*, crates/plugins/*, crates/core/src/backend/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/signin.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/SIZE.md, docs/MEMORY.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-09.md

## Задача
- `backend-imap` — в наборе плагинов, на услугах ядра из PLG-08; из `postman.exe` уходит.
- Окно «Отменить» — уже у ядра (PLG-08); IDLE и отправка SMTP — в процессе плагина.
- Недостающие методы услуги хранилища — по описи PLG-08, без новых механизмов.
- Сторож скорости: `first_frame_ms`, `first_sync_ms`, `list_ms` до и после — первый кадр не хуже, первая
  синхронизация не хуже чем на 10 %.

## Готово
Почта IMAP + SMTP (с областями DAV и EWS) работает через процесс; тесты `stand_imap` зелёные; `postman-imap` нет в дереве зависимостей `postman-gpui`; замер — в `docs/MEMORY.md`.
