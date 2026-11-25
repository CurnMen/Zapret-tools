# AGENTS.md — база знаний проекта

Единая точка входа для агентов и людей. Читай этот файл перед работой и **дописывай
журнал в конце** после значимых правок (правила ведения — раздел 8).

Проект: **Zapret Tools** — Windows-приложение для управления сборкой обхода блокировок
Zapret (Flowseal). Tauri 2 + Rust, фронтенд — HTML/CSS и отдельный `app.js` без фреймворков,
без сборщика и без npm-зависимостей во фронтенде.

Репозиторий: https://github.com/CurnMen/Zapret-tools

---

## 1. Быстрый старт

| Задача | Команда | Ожидаемо |
|---|---|---|
| Проверка типов | `cd src-tauri && cargo check` | exit 0, без предупреждений |
| Тесты | `cd src-tauri && cargo test --lib` | `57 passed; 0 failed` |
| Dev-запуск | `npm run tauri dev` | окно + консоль (в debug-сборке `eprintln!` виден в терминале) |
| Релиз + инсталлятор | `npm run tauri build` | `Finished release` + NSIS (нужен ключ подписи, см. ниже) |
| Пересборка установленной копии | `npm run redeploy` | сборка с ключом, тихая установка поверх прежней + запуск |
| Лог свитчера | `%TEMP%\zapret-tools-switcher.log` | пишется всегда, в том числе в релизе; при 2 МБ уезжает в `.log.1` |
| Проверка фронтенда | `node --check src/app.js` | JS синтаксически валиден (сборщика нет) |

Артефакты: `src-tauri/target/release/zapret-tools.exe`,
`src-tauri/target/release/bundle/nsis/Zapret Tools_1.3.2_x64-setup.exe`.

**Перед релизной сборкой обязательно остановить запущенный `zapret-tools.exe`** — иначе
линковщик не перезапишет `.exe` («отказано в доступе»). Приложение живёт в трее, поэтому
одного закрытия окна недостаточно: `Stop-Process -Id <pid> -Force`.

**Локальная сборка и ключ подписи.** Приватный ключ лежит в
`src-tauri/keys/zapret-tools.key` (каталог в `.gitignore`). `npm run redeploy` сам
подставляет его в `TAURI_SIGNING_PRIVATE_KEY`. При ручном `npm run tauri build` без этой
переменной сборка завершается кодом 1 на шаге подписи updater (`.exe` и NSIS при этом уже
готовы). Сборка без подписи и без инсталлятора: `npm run build -- --no-bundle`.

**Установленная копия.** `npm run redeploy` находит каталог установки по записи
деинсталлятора (нестандартный путь вроде `E:\zapret\Zapret Tools` тоже), останавливает
только процессы из этого каталога, ставит NSIS тихо (`/S /D=<каталог>`) и запускает.
Настройки в `%APPDATA%\com.zapret.tools` переустановка не затрагивает.

Минимум перед сдачей задачи: `cargo check` + `cargo test --lib`. GUI-прогон и ручные
сценарии ввода остаются за человеком (см. раздел 7).

---

## 2. Карта кода

### `src-tauri/src/lib.rs` (~5820 строк) — свитчер, конфиг, списки, hosts, blockcheck

Разделы помечены маркерами `// ---------- Имя ----------` (номера строк на 21.11.2026,
после правок они плывут — ищи по маркеру или по имени функции):

| Что | Как найти |
|---|---|
| Конфиг, `Config`, команды версий/списков/корзины | начало файла, команды `scan_versions`, `read_list_files`, `write_list_file` |
| Свитчер раскладки (хук, словари, подавление) | `run_switcher_listener`, `start_switcher_thread` |
| hosts | `// ---------- hosts ----------` |
| Авто-обновление hosts из Zapret | `// ---------- Авто-обновление hosts ----------` |
| Встроенные тесты Zapret | `// ---------- Встроенные тесты Zapret ----------` |
| Blockcheck | `// ---------- Blockcheck ----------` |
| Инструменты (`.exe` в папке версии) | `// ---------- Инструменты ----------` |
| Тесты | модули `game_filter_tests`, `service_flags_tests`, `switcher_tests`, `blockcheck_parser_tests`, `clipboard_restore_tests`, `double_shift_tests`, `security_helpers_tests`, `hosts_temp_file_tests` |

Ключевые функции свитчера:

| Функция | Роль |
|---|---|
| `run_switcher_listener` | единственный поток `rdev::listen`, вся логика распознавания |
| `start_switcher_thread` | запуск хука + `SwitcherStartedGuard` (снимает флаг при выходе) |
| `is_double_shift` | условие второго нажатия Shift (окно 400 мс, без других клавиш) |
| `handle_finished_word` | автоправка завершённого слова (по пробелу/пунктуации) |
| `trigger_double_shift` | Double Shift: правка последнего слова или конвертация выделения |
| `replace_word` | Backspace'ы + вставка исправленного текста через буфер |
| `convert_selection_blocking` | Ctrl+Insert → чтение буфера → Shift+Insert |
| `clipboard_put` / `restore_clipboard` | подмена буфера и возврат прежнего содержимого |
| `restore_action` / `restore_is_current` | чистые решения «вернуть/очистить/не трогать» и проверка поколения |
| `suppress_for` / `end_suppression` | окно подавления хука (`fetch_max` + CAS) |
| `ensure_keyboard_layout` | переключение раскладки окна + фолбэк Alt+Shift |

### `src-tauri/src/logs.rs` (~150 строк) — лог и диагностика

`switcher_log` (stderr + файл, мьютекс, один `write_all`), `log_path`, `log_timestamp`
(локальное время), `rotate_log` (2 МБ → `.log.1`), `install_panic_hook`, команды
`get_log_tail` и `open_switcher_log` для вкладки «Диагностика».

### `src-tauri/src/win_cmd.rs` (~130 строк) — внешние процессы

`hide_console` (`CREATE_NO_WINDOW`), `ps_quote`, `decode_console_output` (UTF-8 → OEM),
`run_powershell` (UTF-8, без прав), `run_elevated_script` (UAC, результат через временный
файл, код передаётся через `-EncodedCommand`).

### `src-tauri/src/network.rs` (~385 строк) — сеть

DNS/DoH и блокировка QUIC. Адаптеры и их DNS читаются через `GetAdaptersAddresses`
(`system_adapters`), состояние правила — через COM `INetFwPolicy2`
(`firewall_rule_enabled`). Пути через PowerShell оставлены резервными
(`list_network_interfaces_blocking`, `get_dns_state_blocking`, `get_quic_block_state_blocking`).

### `src-tauri/src/clipboard.rs` (~250 строк) — буфер обмена через WinAPI

Отдельный модуль (`#[cfg(windows)]`), подключён как `use self::clipboard as win_clipboard`.
Содержит `ReadOutcome`, `read_text`, `get_text`, `set_text`, `clear` и приватный `with_open`.

### `src-tauri/src/main.rs` — 2 строки

`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` — в релизе консоли нет.

### `src/index.html` (~1150 строк) + `src/app.js` (~2530 строк) — фронтенд

HTML и CSS в `index.html`, скрипт — отдельный `app.js` (`<script src>`; CSP без
`unsafe-inline` для скриптов), состояние в объекте `state`, обращения к бэкенду через
`invoke(...)`. Вкладки переключаются обработчиком `document.querySelectorAll('.tab')`,
каждая вкладка при активации грузит свои данные (`loadListsTab`, `loadNetworkTab`,
`loadHostsTab`, `loadToolsTab`, `loadTrashTab`, `loadDiagnosticsTab`).

---

## 3. Жёсткие правила проекта

1. **Фронтенд без фреймворков и сборки.** Никаких React/Vue/npm-пакетов в `src/`.
2. **Никаких видимых консольных окон.** Любой `Command::new("cmd"/"powershell"/...)`
   оборачивается в `hide_console(&mut command)` (`CREATE_NO_WINDOW`). Исключение —
   UAC-путь: `run_elevated_script` (там окно нужно для запроса прав). Тело скрипта
   передаётся через `-EncodedCommand` (UTF-16LE + Base64), без файла в `%TEMP%` —
   иначе его можно подменить до нажатия «Да» в UAC (TOCTOU). Файл hosts для UAC
   создаётся со случайным именем и удерживается открытым с `FILE_SHARE_READ`, пока
   привилегированный процесс его читает. Читать его можно только через
   `ps_read_locked_temp` (`FileShare.ReadWrite`): `[IO.File]::ReadAllText` открывает
   файл с `FileShare.Read` и падает с «файл используется другим процессом».
3. **Не ломать `showConfirm()`** и передачу `rootPath`/`versionPath` с фронтенда —
   на них завязаны все файловые операции.
4. **Боевых `unwrap()`/`expect()` в рабочем коде нет.** Ошибки — `Result<_, String>` для
   команд и `switcher_log` для фоновых потоков. `expect` допустим только у regex-констант
   (`OnceLock`) и в `#[cfg(test)]`.
5. **Писать в лог только через `switcher_log`** (`crate::logs`). Он потокобезопасен
   (мьютекс + один `write_all`), ротируется и дублирует в `%TEMP%\zapret-tools-switcher.log`.
   `eprintln!` в релизе уходит в никуда.
6. **Tauri-команды, которые запускают PowerShell или долго читают диск, обязаны быть
   `async` + `tokio::task::spawn_blocking`.** Синхронная команда выполняется в главном
   потоке и подвешивает окно (так было с вкладкой «Сеть»: 8,6 с фриза).
7. **Кодировки и переводы строк.** `lib.rs` — UTF-8 **с BOM и CRLF**; `clipboard.rs`,
   `logs.rs`, `win_cmd.rs`, `network.rs`, `AGENTS.md` — LF без BOM; `src/index.html`,
   `src/app.js` — CRLF без BOM. Правя `lib.rs` скриптом, читай байты, нормализуй CRLF→LF для замен и возвращай
   LF→CRLF при записи, BOM сохраняй.
8. **`package.json` — без BOM.** Из него Tauri берёт версию (`"version": "../package.json"`);
   BOM ломает разбор и валит build-скрипт («expected value at line 1 column 1»).
9. **Один экземпляр приложения** (`tauri-plugin-single-instance`), один слушатель
   клавиатуры (`SWITCHER_STARTED` + `SwitcherStartedGuard`).
10. **Зависимости не раздувать.** Уже есть: tauri 2 (трей), tokio (full), regex, rdev,
    enigo, windows 0.58, sysinfo, reqwest (json), idna, psl.
11. **Словари встроены в бинарь** (`include_str!` + тест `embedded_dictionaries_load`),
    `bundle.resources` для них больше не используется — не возвращать.
12. **Команды из подмодулей регистрируются по полному пути**:
    `generate_handler![network::list_network_interfaces, ...]`. Модуль должен быть
    `pub(crate) mod`, иначе макрос не найдёт сгенерированный обработчик.
13. **Вывод внешних процессов декодируется через `decode_console_output`.** PowerShell 5.1
    при перенаправлении stdout/stderr пишет OEM-кодировку (cp866), а не UTF-8; чтение
    «как UTF-8» превращает кириллицу в `����`. Функция сначала пробует UTF-8, затем
    `MultiByteToWideChar(CP_OEMCP)`.
14. **Blockcheck с правами не дублирует логику.** Без админки `run_blockcheck` сразу
    показывает UAC и через `run_elevated_script` запускает тот же бинарь с
    `--blockcheck-worker` (без Tauri и без single-instance). Задание/результат — JSON
    во временной папке: файл задания держится открытым (`create_locked_temp_file`),
    результат создаётся с `create_new` (подставленный симлинк не перезапишется).
    Аргументы `Start-Process -ArgumentList` заключаются в кавычки вручную: путь с
    пробелами иначе разваливается на два аргумента.
    Внутри стратегии домены опрашиваются параллельно (`JoinSet`), диагностика окружения
    в духе `service.bat` собирается параллельно с базовым зондированием. Ожидание
    готовности `winws` адаптивное: первая проба через 1,2 с, повторы до 2,5 с, упавший
    процесс виден за 300 мс. Клиент **не держит idle-соединения**
    (`pool_max_idle_per_host(0)`): иначе соединение, открытое удачной стратегией,
    переиспользуется следующей и та «проходит» без своего обхода. Домены резолвятся
    заранее, IPv4 ставится перед IPv6: на машинах с «чёрной дырой» IPv6 первый connect
    съедал весь таймаут и рабочая стратегия ложно считалась нерабочей.
15. **Вывод `sc` разбирается по значению состояния, а не по подписи.** Строка
    локализована («STATE» / «СОСТОЯНИЕ» / «STATUS») и приходит в OEM-кодировке;
    `parse_sc_state` ищет последний токен `RUNNING/STOPPED/…`. Конфликты ищутся только
    по списку служб из `service.bat` (`GoodbyeDPI`, `discordfix_zapret`, `winws1`,
    `winws2`), а не по всему выводу `sc query`: иначе системная «Network Connectivity
    Assistant» попадала в конфликты.

---

## 4. Инварианты свитчера (почему сделано именно так)

- **Подавление хука** — не «флаг на время», а число-дедлайн в `AtomicU64`; ставится через
  `suppress_for` (`fetch_max`, окно не укорачивается) и снимается через `end_suppression`
  (CAS по своему значению). Правка держит 400 мс, конвертация выделения — 1,5 с + хвост.
- **Поколение буфера** — `CLIPBOARD_GENERATION`; `restore_clipboard` ждёт 250 мс и выходит,
  если поколение изменилось. Без этого быстрая печать затирала содержимое следующей правки.
- **Трёхзначное чтение буфера** — `ReadOutcome::{Text, Empty, Unavailable}`. «Пусто»
  подтверждается только `EnumClipboardFormats(0) == 0` (или терминатором в CF_UNICODETEXT);
  `GlobalSize < 2`, отсутствие текста при наличии других форматов, занятый буфер — это
  `Unavailable`, и такой буфер **не очищается**: восстановить прочитанное нельзя.
  Решение закреплено таблицей `restore_action` и тестами `clipboard_restore_tests`.
- **Порядок правки** — сначала готовится буфер (`clipboard_put`), и только потом удаляются
  символы; если буфер занят, правка пропускается, чтобы не съесть слово.
- **Вставка** — Shift+Insert (не Shift+Ctrl+V и не Ctrl+V), чтобы не открывать DevTools в
  браузерах и не ломаться от зажатого Shift; фолбэк — `enigo.text()` одним батчем.
- **Double Shift** — ровно два нажатия Shift в пределах 400 мс, между ними не было других
  клавиш и колеса, модификаторы не зажаты (`is_double_shift`). Автоповтор зажатого Shift
  вторым нажатием не считается.
- **Очередь предложений словаря** — каждое событие `switcher-suggest-custom-word` попадает
  в `state.suggestions` (лимит 30, дубликаты по паре слов склеиваются), а не только в тост:
  тосты живут 12 секунд и исчезают, список — нет. Счётчик висит на вкладке «Свитчер».
  «Добавить» сохраняет только переведённое слово; «Скрыть» и закрытие тоста вызывают
  `decline_custom_word(original, translated, to_russian)`, поэтому пара больше не предлагается.
- **Отклонённое можно вернуть.** `DeclinedWord` хранит `original`, `translated` и
  `to_russian`; старые конфиги со строками читаются через `deserialize_declined_words`.
  В интерфейсе это компактные чипы в отдельном блоке «Отклонённые предложения» (виден
  и когда очередь предложений пуста), у каждого — «↺»: `remove_declined_word` снимает
  отказ и возвращает пару в `state.suggestions`.
- **Списки слов ограничены по высоте** (`.list-bounded`, 220 px): пользовательский словарь
  RU/EN — один общий список с меткой языка, а не две колонки, поэтому сотня слов не
  расталкивает вкладку «Свитчер». Строки `.exception-row` фиксированной высоты 32 px,
  длинный текст обрезается многоточием — высота списков не «пляшет».
- **Геометрия окна** живёт в `localStorage` (`zapretTools.windowGeometry`): сохраняем
  `innerSize()` и `outerPosition()` (в `setSize` уходит именно клиентский размер, иначе
  окно растёт на рамку), разрешения `allow-set-size`/`allow-set-position` даны в
  `capabilities/default.json`. Минимальный размер 960×680 задан в `tauri.conf.json`.
- **Глобальная консоль** подстраивает отступ под число строк: CSS-переменная
  `--log-drawer-space` = 36 px заголовка + до 168 px тела (`scrollHeight`, пересчёт
  при resize), поэтому одна строка не оставляет 220 px пустого места. Ограничение в
  100 строк единообразно (`>=`) и в `logTo`, и в списке стратегий. `[WARN]` (включая
  `[DIAG][WARN]`) подсвечивается жёлтым, а не зелёным.

---

## 5. Сборка, конфиг, релиз

- `src-tauri/tauri.conf.json`: `frontendDist: "../src"`, `withGlobalTauri: true`,
  `version: "../package.json"` (единственный источник версии), у окна `devtools: true`.
  В релизе DevTools требуют feature `devtools` в `Cargo.toml` (сейчас не включён намеренно).
  `security.csp` — не `null`: скрипты только внешние (`script-src 'self'`, без
  `unsafe-inline`), инлайн-стили и `style`-атрибуты разрешены (`style-src-attr`), внешние
  источники закрыты,
  IPC разрешён через `connect-src 'self' ipc: http://ipc.localhost`. Tauri добавляет nonce
  в `style-src`, из-за чего `'unsafe-inline'` там игнорируется; поэтому `style`-атрибуты
  разрешены отдельной директивой `style-src-attr 'unsafe-inline'`.
- CI: `.github/workflows/ci.yml` на Windows — `cargo fmt --check`, `cargo clippy
  --all-targets -D warnings`, `cargo test --lib`, `cargo build --release` (запускается
  на push в `main`, пулл-реквестах и вручную).
- `switcher_declined_words` в `config.json` — массив объектов
  `{"original", "translated", "to_russian"}`; массив строк из старых версий читается
  тем же полем и пересохраняется в новом формате.
- `Cargo.toml` держит версию крейта (`1.0.0`) — она не равна версии приложения автоматически;
  при релизе править `package.json` и `CHANGELOG.md`.
- `bundle.targets: ["nsis"]`, иконки в `src-tauri/icons/`.
- Плагины: dialog, opener, autostart, single-instance, updater.
- Лог: `switcher_log` пишет в `%TEMP%\zapret-tools-switcher.log`; при 2 МБ файл
  переименовывается в `zapret-tools-switcher.log.1` (прежний `.1` удаляется). Время —
  локальное `2026-11-21 18:05:34.889`.
- `.gitignore`: `node_modules/`, `src-tauri/target/`, `src-tauri/gen/`, `src-tauri/target-fix/`,
  `dist/`, `files/` (пользовательские документы), `*.log`.

---

## 6. Текущее состояние версии

Версия **1.0.0**. Источник — `package.json`; `CHANGELOG.md` описывает состав версии.

Закоммичено:

- `c6d87e9` — модуль `clipboard.rs` (трёхзначное чтение, фикс OOB по `GlobalSize`),
  безопасное восстановление буфера, `switcher_log` вместо `eprintln!`, `devtools`, встроенные словари.
- `e4512a0` — потокобезопасная запись в лог свитчера (мьютекс + один `write_all`).
- `f32a878` — сетевые команды стали `async` + `spawn_blocking`, `loadNetworkTab`
  грузит данные параллельно (было ~8,6 с фриза окна).
- `fcfa386` — этот файл.
- `6e27b92` — `SwitcherStartedGuard` (перезапуск свитчера после сбоя) и `install_panic_hook`.
- `b187565` — локальное время в логе и ротация при 2 МБ.
- `3ab888d` — вкладка «Диагностика» и индикация загрузки вкладки «Сеть».
- `64ffcd9` — сеть через `GetAdaptersAddresses` и COM вместо PowerShell.
- `a1cc008` — чистые `restore_action`/`restore_is_current`/`is_double_shift` + 8 тестов.
- `d4ffdfb` — версия из `package.json`, `CHANGELOG.md`.
- `9bbc76e` — BOM убран из `package.json`.
- `c735072` — модули `logs.rs`, `win_cmd.rs`, `network.rs`.

Рабочая копия чистая: `files/` и `src-tauri/target-fix/` в `.gitignore`.

---

## 7. Известные проблемы и ограничения

| Проблема | Детали |
|---|---|
| Настройка «проверять обновления» мертва | `auto_check_updates` читается/сохраняется, но `tauri-plugin-updater` нигде не вызывает `check()`; эндпоинт `github.com/example/...` и pubkey-заглушка в конфиге |
| ~~Вкладка «Сеть» медленная~~ | Решено в `64ffcd9`: IP Helper + COM, три запроса ~0,04 с (было 8,6 с). Резервные пути через PowerShell остались |
| `lib.rs` всё ещё ~5060 строк | Первый этап разбиения сделан (`logs`, `win_cmd`, `network`). Дальше: `switcher.rs` (хук и правка), `hosts.rs`, `blockcheck.rs`, `versions.rs`. Двигать по одному модулю за коммит с прогоном тестов |
| GUI не тестируется агентом | Окно Tauri недоступно обычному браузеру. Запуск можно проверить через WebView2 CDP: `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222`, затем `curl http://127.0.0.1:9222/json`. Клики, свитчер и UAC — руками |
| Updater не настроен | `security.csp` закрывает внешние источники; скрипты только `app.js` (`script-src 'self'`), инлайн-стили и `style`-атрибуты разрешены. Плагин updater всё ещё без `check()`; эндпоинт `github.com/example/...` и pubkey-заглушка в конфиге |

---

## 8. Как вести журнал

1. После значимой правки добавь **одну строку** в таблицу журнала (раздел 9): дата,
   версия, что и зачем, файлы, чем проверено.
2. Если правка меняет правило или инвариант — обнови соответствующий раздел выше.
   Журнал не заменяет правила.
3. Историю не переписывай: устаревшее помечай в разделе 7, а не удаляй.
4. Раздел 6 держи в согласии с кодом: кто меняет состояние «закоммичено/нет», тот правит.
5. В журнал не пиши секреты, токены и содержимое пользовательских файлов.

---

## 9. Журнал

| Дата | Версия | Что сделано | Файлы | Проверено |
|---|---|---|---|---|
| 2026-11-21 | 1.0.0 | Буфер обмена вынесен в модуль; `ReadOutcome` различает «пусто» и «не прочитано»; `GlobalSize < 2` больше не считается пустым буфером | `clipboard.rs`, `lib.rs` | `cargo check`, `cargo test --lib` (32) |
| 2026-11-21 | 1.0.0 | Восстановление буфера не стирает содержимое, если пустота не доказана `EnumClipboardFormats` | `lib.rs` | `cargo test --lib` (32) |
| 2026-11-21 | 1.0.0 | `switcher_log` сериализован мьютексом — строки в логе больше не склеиваются | `lib.rs` | `cargo check`, `cargo test --lib` (32) |
| 2026-11-21 | 1.0.0 | Сетевые команды стали `async` + `spawn_blocking`, вкладка «Сеть» грузит данные параллельно (было 8,6 с фриза UI) | `lib.rs`, `index.html` | `cargo check`, `cargo test --lib` (32), релизная сборка |
| 2026-11-21 | 1.0.0 | Паники и сбои `rdev::listen` больше не теряются, свитчер перезапускается без рестарта приложения | `lib.rs` | `cargo check`, `cargo test --lib` (32) |
| 2026-11-21 | 1.0.0 | Лог: локальное время и ротация при 2 МБ (файл копился с 14.11) | `lib.rs` | `cargo check`, `cargo test --lib` (32) |
| 2026-11-21 | 1.0.0 | Вкладка «Диагностика» (200 строк лога, открыть, копировать) и индикация загрузки сети | `lib.rs`, `index.html` | `cargo check`, JS-синтаксис через `new Function`, `cargo test --lib` (32) |
| 2026-11-21 | 1.0.0 | Сеть через `GetAdaptersAddresses` + COM `INetFwPolicy2`: три запроса 0,04 с вместо 8,6 с | `lib.rs`, `Cargo.toml` | `cargo check`, `cargo test --lib` (32), замер пробным тестом (адаптеры и DNS совпали с PowerShell) |
| 2026-11-21 | 1.0.0 | Решения о восстановлении буфера и Double Shift вынесены в чистые функции и покрыты тестами (32 → 40) | `lib.rs` | `cargo test --lib` (40) |
| 2026-11-21 | 1.0.0 | Версия из `package.json` (`tauri.conf.json` → `../package.json`), `CHANGELOG.md`; BOM из `package.json` убран | `tauri.conf.json`, `package.json`, `CHANGELOG.md` | `cargo check` (build-скрипт читает версию) |
| 2026-11-21 | 1.0.0 | `logs.rs`, `win_cmd.rs`, `network.rs` вынесены из `lib.rs` (−630 строк), поведение не менялось | `lib.rs`, `logs.rs`, `win_cmd.rs`, `network.rs` | `cargo check`, `cargo test --lib` (40) |
| 2026-11-21 | 1.0.0 | Авто-обновление hosts из Flowseal (блок между маркерами, запись через UAC) и запуск `utils\test zapret.ps1` со скрытым окном и вводом по умолчанию | `lib.rs`, `index.html` | `cargo test --lib` (41), превью всех вкладок |
| 2026-11-21 | 1.0.0 | Пользовательский словарь RU/EN, запоминание отклонённых предложений, тост «Добавить в словарь?» | `lib.rs`, `index.html` | `cargo test --lib` (41), JS-синтаксис, ID-проверка |
| 2026-11-21 | 1.0.0 | Закрыт TOCTOU в UAC: `run_elevated_script` передаёт код через `-EncodedCommand` вместо `.ps1` в `%TEMP%`; добавлен тест Base64 UTF-16LE | `win_cmd.rs` | `cargo test --lib` (41), живой UAC-прогон |
| 2026-11-21 | 1.0.0 | CSP в `tauri.conf.json`: инлайн-стили/скрипты разрешены, внешние источники и `object-src` закрыты, IPC разрешён | `tauri.conf.json` | превью с meta-CSP по всем вкладкам, 0 ошибок консоли |
| 2026-11-21 | 1.0.0 | GitHub Actions CI: fmt, clippy `-D warnings`, тесты, release-сборка на `windows-latest` | `.github/workflows/ci.yml` | локально `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --lib` (41) |
| 2026-11-22 | 1.0.0 | Консоль: вывод PowerShell декодируется из OEM (cp866), а не только UTF-8 — кириллица в тестах больше не `����`; текст ошибки UAC пишется в отдельный UTF-8-файл | `win_cmd.rs`, `lib.rs`, `Cargo.toml` | `cargo test --lib` (42), байты cp866 в тесте |
| 2026-11-22 | 1.0.0 | Предложение «Добавить в словарь?» добавляет только переведённое слово; исходная опечатка в словарь не попадает | `index.html` | JS-синтаксис, превью |
| 2026-11-22 | 1.0.0 | Перед тестами Zapret появилось подтверждение (до 30 минут, остановка `winws`, IPSet `any`) | `index.html` | превью: «Отмена» — 0 вызовов, «Да» — 1 вызов |
| 2026-11-22 | 1.0.0 | Очередь предложений словаря: список «Предложения словаря», счётчик на вкладке «Свитчер», «Добавить»/«Скрыть»/«Добавить все», дедупликация, лимит 30 | `index.html` | превью: 2 предложения, дубль, добавление из тоста, отказ, «Добавить все» (2 слова) |
| 2026-11-22 | 1.0.0 | Словари RU/EN объединены в один список с меткой языка и ограничением высоты 220 px; список предложений и «отклонённые» собраны в одном блоке | `index.html` | превью: 2 слова, метки RU/EN, удаление, «Добавить все», `max-height: 220px` |
| 2026-11-22 | 1.0.0 | Отклонённое предложение хранит пару слов и возвращается по «↺» (раньше `remove_declined_word` не существовала); отдельный компактный блок не пропадает вместе с очередью; строки списков — фиксированные 32 px | `lib.rs`, `index.html`, `AGENTS.md`, `CHANGELOG.md` | `cargo check`, `cargo test --lib` (44), JS-синтаксис |
| 2026-11-22 | 1.0.0 | P0-безопасность: скрипт вынесен в `app.js`, CSP без `unsafe-inline`; логи и имена файлов выводятся текстом; пути IPC проверяются; файл hosts для UAC — случайное имя + lock; `error_for_status` и проверка тега при скачивании | `lib.rs`, `win_cmd.rs`, `index.html`, `app.js`, `tauri.conf.json`, `AGENTS.md`, `CHANGELOG.md` | `cargo check`, `cargo test --lib` (49), `clippy -D warnings`, `node --check`, релиз; CDP: `app.js` загружен, вкладки работают, 0 CSP-ошибок |
| 2026-11-23 | 1.3.2 | Авто-обновление hosts падало с «файл используется другим процессом»: `ReadAllText` (FileShare.Read) конфликтовал с нашим дескриптором на запись. Чтение вынесено в `ps_read_locked_temp` (FileShare.ReadWrite), запись и удаление по-прежнему заблокированы | `lib.rs`, `AGENTS.md` | `cargo test --lib` (51), PowerShell читает залоченный файл и UTF-8 кириллицу; `fmt`, `clippy -D warnings` |
| 2026-11-23 | 1.3.2 | `npm run redeploy`: подстановка локального ключа подписи из `src-tauri/keys/`, поиск каталога установки по записи деинсталлятора, тихая установка с `/S /D=`, перезапуск; инструкции в README и AGENTS.md | `scripts/redeploy.ps1`, `README.md`, `AGENTS.md` | `npm run redeploy`: `.sig` создан, установлено в `E:\zapret\Zapret Tools` (версия 1.3.2), процесс запущен из каталога установки, `winws` жив |
| 2026-11-23 | 1.3.2 | Blockcheck без прав больше не сыплет `os error 740`: сразу UAC и привилегированный воркер того же бинаря (`--blockcheck-worker`), JSON-задание под lock. Домены внутри стратегии опрашиваются параллельно, добавлена диагностика окружения в духе `service.bat` | `lib.rs`, `main.rs`, `index.html`, `AGENTS.md` | `cargo test --lib` (53), `fmt`, `clippy`; воркер напрямую вернул UTF-8 отчёт; полный UAC-прогон: `OK`, отчёт без предупреждения о правах |
| 2026-11-23 | 1.3.2 | Геометрия окна восстанавливается (разрешения на set-size/set-position, сохранение `innerSize`), минимальный размер 960×680; консоль подстраивает отступ под число строк, убрана неоднородность при 1–2 сообщениях | `app.js`, `index.html`, `tauri.conf.json`, `capabilities/default.json` | превью: 1 строка — 94 px отступа, 13 строк — 212 px; перезапуск сохранения размера/позиции; `node --check` |
| 2026-11-25 | 1.3.3 | Аудит blockcheck: `sc` разбирается по значению состояния (OEM-вывод, любая локаль), конфликты ищутся только по службам из `service.bat`, добавлены BFE и залипший WinDivert; `hosts` предупреждает только о блокирующих `0.0.0.0`/`127.0.0.1`. Ожидание `winws` адаптивное (1,2–2,5 с), клиент без idle-соединений, IPv4 пробуется раньше IPv6 (иначе «чёрная дыра» IPv6 съедала таймаут и стратегия ложно проваливалась); путь `winws.exe` с пробелами больше не даёт `[SKIP]`; файлы задания/результата удаляются, аргументы `-ArgumentList` в кавычках, воркер без аргументов завершается с кодом 2, а не открывает GUI. Консоль: `[WARN]` жёлтый, лимит 100 без потери новой строки, отступ 36 + 1 + тело | `lib.rs`, `main.rs`, `app.js`, `index.html`, `AGENTS.md`, `CHANGELOG.md` | `cargo test --lib` (57), `fmt`, `clippy -D warnings`, `node --check`, превью, воркер на пути с пробелами |
