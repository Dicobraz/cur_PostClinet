# PLG-03: набор плагинов в пакетах трёх ОС

lane: x/plg-pack            # tools/pack, scripts, .github (job release) — полоса ci
status: todo
deps: [PLG-02]
class: B                    # состав пакета и выпуск
files: [tools/pack/*, scripts/windows/*, scripts/macos/*, scripts/linux/*, .github/workflows/*, docs/DEV.md, docs/SIZE.md, docs/releases/*]
read: [docs/adr/0013-plugins-on-demand.md, docs/decisions/2026-11-14-plugins-on-demand.md, docs/decisions/2026-11-07-installer-msi-winget.md, docs/decisions/2026-11-11-self-update.md]
checks: [nextest, release]
manual: yes                 # docs/manual-tests/PLG-03.md: установка, обновление, удаление на трёх ОС

## Задача
- Файлы наборов плагинов — по списку, а не по имени: архив и MSI Windows, `.app` macOS (`Contents/MacOS`, подпись
  вместе с воркером), deb/rpm и архив Linux. Сколько наборов — по ответу владельца на вопрос №1 ADR 0013
  (рекомендация — общий набор и отдельный файл Matrix компонентом установщика).
- Подпись (Authenticode, подпись Apple), minisign пакета обновления — файлы наборов в том же пакете, одна версия.
- Самообновление: набор заменяется вместе с `postman.exe`; прерванное обновление — версии не совпали, плагин не
  используется (сообщение «нужен перезапуск»), данные целы.
- Удаление: снимается и профиль AppContainer сетевой песочницы.
- job `release`: сборка набора; время job — записать; если `gate` выходит за 7 минут — набор собирается только в
  `release`.
- Строка бюджета файла набора в `docs/SIZE.md` — по факту первой сборки, решением владельца.

## Готово
Пакеты трёх ОС содержат набор; установка, обновление с прошлой версии и удаление проверены вручную; `msi-check` и
проверки пакетов в CI зелёные.
