# PLG-06: LDAP — отдельным процессом

lane: x/plg-ldap            # crates/backend-ldap, core (договор каталога), app-gpui
status: todo
deps: [PLG-05]
class: A
files: [crates/backend-ldap/*, crates/plugins/*, crates/core/src/smime/directory.rs, crates/core/src/backend/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/directory.rs, app-gpui/src/settings_directory.rs, app-gpui/src/sys/*, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/decisions/2026-11-02-ldap-active-directory.md]
checks: [nextest]
manual: yes                 # docs/manual-tests/PLG-06.md: каталог организации, вход учётной записью Windows

## Задача
- Договор каталога (`smime::directory`) — запрос и ответ по обмену; `backend-ldap` — в наборе плагинов.
- Процесс стартует по первому запросу к каталогу и завершается после 5 минут простоя.
- Профиль песочницы — с корпоративной проверкой подлинности (Kerberos, вход учётной записью Windows); на macOS и
  Linux — что нужно открыть для Kerberos, по итогам PLG-01.
- `domain_controller` — функция ОС в `app-gpui/src/sys`, не в плагине.

## Готово
Поиск сертификата и «Найти в каталоге» работают через процесс на трёх ОС; `ldap3` нет в дереве зависимостей
`postman-gpui`.
