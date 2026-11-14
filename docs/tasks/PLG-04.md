# PLG-04: Matrix — отдельным процессом

lane: x/plg-matrix         # crates/backend-matrix, crates/plugins, app-gpui (корень, вход), deps
status: todo
deps: [PLG-03]
class: A
files: [crates/backend-matrix/*, crates/plugins/*, app-gpui/Cargo.toml, app-gpui/src/plugins.rs, app-gpui/src/account_matrix.rs, docs/tasks/CH-05.md, crates/core/tests/layers.rs, Cargo.toml, Cargo.lock, docs/SIZE.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0012-channels.md]
checks: [nextest, bench]
manual: yes                 # docs/manual-tests/PLG-04.md

## Задача
- `backend-matrix` — в наборе плагинов, из `postman.exe` уходит: признаки сборки `matrix` и `matrix-release` убираются,
  канал включают только настройка и политика `AllowChannelMatrix`.
- Хранилища SDK (`StateStore`, `CryptoStore` поверх `PluginStore`) — через услугу хранилища по обмену; запись
  очередью, как сейчас.
- Вход через браузер: возврат на `127.0.0.1` слушает ядро и передаёт ответ плагину (песочница входящих не принимает).
- Коды отказов (`BAD_*`) — в `model`/`ChannelError`, интерфейс от крейта плагина не зависит.
- Замер: `postman.exe` и файл с Matrix — строкой в `docs/SIZE.md` (№11 и №12 закрываются: бюджет `postman.exe` не задет).

## Готово
Вход, комнаты, шифрованная переписка, проверка устройства и восстановление ключей — через процесс (ручная проверка);
`tests/conformance.rs` и `tests/e2e.rs` зелёные; `matrix-sdk` нет в дереве зависимостей `postman-gpui`; первый кадр не
хуже прежнего.
