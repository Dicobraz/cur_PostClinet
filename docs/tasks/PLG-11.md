# PLG-11: уборка — `postman.exe` без протоколов

lane: x/plg-cleanup         # app-gpui, core (тест слоёв), docs
status: todo
deps: [PLG-10]
class: B                    # бюджеты размера, ADR и архитектура
files: [app-gpui/Cargo.toml, app-gpui/src/plugins.rs, crates/core/src/backend/*, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/00-ARCHITECTURE.md, docs/adr/0004-protocol-plugins.md, docs/adr/0012-channels.md, docs/adr/0013-plugins-on-demand.md, docs/SIZE.md, docs/DEV.md, docs/tasks/SIZE-01.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md]
checks: [nextest, bench, release]
manual: no

## Задача
- `postman-gpui` не зависит ни от одного `postman-backend-*`: список исключений `interface_does_not_link_plugins`
  пуст; путь «плагин внутри процесса» остаётся только в тестах плагинов.
- Признаки сборки каналов и остатки прямых вызовов плагинов из интерфейса — убрать.
- Документы: `docs/00-ARCHITECTURE.md` (слои, процессы), ADR 0004 и 0012 — ссылка на 0013, ADR 0013 — статус
  и фактические цифры, `docs/DEV.md` — как собирать и запускать с набором.
- Размер: release-замер всех файлов, разбивка `postman.exe`; новые бюджеты — строкой в `docs/SIZE.md`, решением
  владельца (правило малого шага). SIZE-01: `panic = "abort"` для процессов плагинов — в ту же задачу.

## Готово
В `postman.exe` нет протоколов и их сетевого стека (дерево зависимостей и замер); бюджеты обновлены; `gate` зелёный.
