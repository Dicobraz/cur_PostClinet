# PLG-02: хост плагинов, договор каналов по обмену, запуск после первого кадра

lane: x/plg-host            # core (договор), новый крейт crates/plugins, demo, app-gpui (корень, запуск), deps, lanes.toml
status: todo
deps: [PLG-01]
class: B                    # договор плагинов (ADR 0004, 0012)
files: [crates/core/src/backend/*, crates/core/src/channel/*, crates/core/src/bench.rs, crates/core/tests/layers.rs, crates/plugins/*, crates/demo/*, app-gpui/src/plugins.rs, app-gpui/src/live.rs, app-gpui/src/main.rs, app-gpui/src/channels.rs, Cargo.toml, Cargo.lock, lanes.toml, docs/MEMORY.md, docs/DEV.md]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/adr/0012-channels.md, docs/adr/0004-protocol-plugins.md]
checks: [nextest, bench]
manual: no

## Задача
- `postman_core::backend::wire` — сообщения хоста и плагина (ADR 0013 «Договор»); `serde` на `ChannelEvent`, `Action`,
  `Prompt`, `Answer`, `Page` и вложенных типах. Тест: каждый вариант проходит «туда и обратно».
- `postman_core::backend::remote` — хост плагинов и заместитель `ChannelPlugin`: тот же трейт, внутри — процесс
  (`postman-proc`). Услуга `PluginStore` по обмену; секрет — в `Open`; журнал, политика и настройки сети — в `Hello`;
  сверка версии и номера сборки; падение → «без связи», перезапуск с паузами 1 с, 5 с, 30 с, 2 мин; пять падений за
  10 минут — «проблема → причина → исправление» с «Повторить»; нет `Pong` 30 с — убить и перезапустить.
- Крейт `crates/plugins`: исполняемый файл `postman-plugins` — набор плагинов, `--plugin <id>`; у плагина точка входа
  `serve(link)`. До первого сообщения — только запирание песочницы и ответ на `Hello`; tokio и TLS — при первом `Open`.
- Запуск (ADR 0013 «Запуск и загрузка плагинов»): заместитель возвращает канал сразу; процессы создаёт фоновый поток
  после отметки «первый кадр показан»; только виды с учётками; первым — плагин активной учётки и учёток с исходящим;
  не больше двух стартов сразу, остальным — пониженный приоритет до первой синхронизации; пока плагин поднимается —
  «подключение», команды в очереди.
- Строка плагина в `app-gpui/src/plugins.rs`: идентификатор, вид, как узнать свой адрес, файл набора — без запуска
  процесса. Тест набора сверяет строку с `claims` плагина.
- Первый плагин через процесс — демо-канал (`crates/demo`): без сети, для тестов и замера.
- Метрики `--bench`: `plugin_start_ms`, `plugins_rss_mb` рядом с `first_frame_ms` и `first_sync_ms`. Замер до и после
  на демо-канале — в `docs/MEMORY.md`.
- Сторож CI: тест с подставным запуском, который не отвечает, — `Registry::spawn` возвращается сразу, до отметки
  первого кадра счётчик запусков равен нулю, команды копятся. На счётчиках, не на времени.
- `layers.rs`: `postman-plugins` — корень плагинов (без интерфейса); `interface_does_not_link_plugins` со списком
  исключений (все нынешние `backend-*`).

## Готово
Демо-канал работает отдельным процессом в dev-сборке; `first_frame_ms` не хуже прежнего (5 тихих прогонов до и после);
сторож в `gate`; остальные плагины — внутри `postman.exe`, как были.
