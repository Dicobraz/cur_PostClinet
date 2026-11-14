# PLG-08: услуги ядра почтовому пути, EWS — отдельным процессом

lane: x/plg-ews             # core (договор, очередь отправки), backend-ews, app-gpui
status: todo
deps: [PLG-07]
class: B                    # услуга хранилища учётки, окно «Отменить» в ядре
files: [crates/core/src/backend/*, crates/core/src/sync.rs, crates/core/src/send_later.rs, crates/core/src/tasks/*, crates/core/src/invite.rs, crates/core/src/lifecycle.rs, crates/core/src/rules.rs, crates/core/src/migrate.rs, crates/core/src/smime/*, crates/cache/src/*, crates/backend-ews/*, crates/plugins/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/signin.rs, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0006-single-store.md, docs/decisions/2026-11-14-all-in-db.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-08.md: Exchange — почта, календарь, вход учётной записью Windows

## Задача
- Первым шагом — опись: какие методы кэша и функции ядра достижимы из почтовых путей (сейчас 143 обращения к
  35 методам, плюс `tasks`, `invite`, `send_later`, `rules`, `lifecycle`, `migrate`). Опись — в карточку.
- Услуга хранилища учётки: эти методы — вызовами по обмену; база открыта только главным процессом. Записи без ответа
  идут подряд, ошибка — на ближайшем вызове с ответом; порядок записей сохраняется.
- Услуги: индекс поиска, разбор письма в воркере (плагин процессов не запускает), подпись и шифрование S/MIME
  (`seal_outgoing`, `sign_outgoing` — у ядра: ключи пользователя плагину недоступны).
- Окно «Отменить» и очередь отправки — в ядре (в базе профиля): плагин получает письмо в момент отправки и
  подтверждает итог; `holding()` и `leave()` плагинов уходят, выход дожидается очереди ядра.
- `backend-ews` — в наборе плагинов (почта и свои области в одном процессе). `is_ews_url` — строкой описания плагина;
  отказы входа (`WINDOWS_SIGN_IN_UNAVAILABLE`, `CREDENTIALS_REJECTED`) — в `model::ErrorKind`.
- Вход учётной записью Windows — в профиле песочницы с корпоративной проверкой подлинности.
- Сторож скорости: `first_sync_ms` и время первой синхронизации демо-ящика Exchange до и после — не хуже чем на 10 %.

## Готово
Exchange — почта, папки, календарь, занятость, справочник, отправка с «Отменить» — через процесс; падение процесса
при письме в окне «Отменить» письмо не теряет (тест); тесты стенда EWS зелёные; `postman-ews` нет в дереве
зависимостей `postman-gpui`.
