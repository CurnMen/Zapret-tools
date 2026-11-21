# AGENTS.md — база знаний проекта

Единая точка входа для агентов и людей. Читай этот файл перед работой и **дописывай
журнал в конце** после значимых правок (правила ведения — раздел 8).

Проект: **Zapret Tools** — Windows-приложение для управления сборкой обхода блокировок
Zapret (Flowseal). Tauri 2 + Rust, фронтенд — один HTML-файл без фреймворков,
без сборщика и без npm-зависимостей во фронтенде.

Репозиторий: https://github.com/CurnMen/Zapret-tools

---

## 1. Быстрый старт

| Задача | Команда | Ожидаемо |
|---|---|---|
| Проверка типов | `cd src-tauri && cargo check` | exit 0, без предупреждений |
| Тесты | `cd src-tauri && cargo test --lib` | `40 passed; 0 failed` |
| Dev-запуск | `npm run tauri dev` | окно + консоль (в debug-сборке `eprintln!` виден в терминале) |
| Релиз + инсталлятор | `npm run tauri build` | `Finished release` + NSIS |
| Лог свитчера | `%TEMP%\zapret-tools-switcher.log` | пишется всегда, в том числе в релизе; при 2 МБ уезжает в `.log.1` |
| Проверка фронтенда | `node -e` + `new Function(code)` по `<script>` из `index.html` | JS синтаксически валиден (сборщика нет) |

Артефакты: `src-tauri/target/release/zapret-tools.exe`,
`src-tauri/target/release/bundle/nsis/Zapret Tools_1.0.0_x64-setup.exe`.

**Перед релизной сборкой обязательно остановить запущенный `zapret-tools.exe`** — иначе
линковщик не перезапишет `.exe` («отказано в доступе»). Приложение живёт в трее, поэтому
одного закрытия окна недостаточно: `Stop-Process -Id <pid> -Force`.

Минимум перед сдачей задачи: `cargo check` + `cargo test --lib`. GUI-прогон и ручные
сценарии ввода остаются за человеком (см. раздел 7).

---

## 2. Карта кода

### `src-tauri/src/lib.rs` (~5060 строк) — свитчер, конфиг, списки, hosts, blockcheck

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
| Тесты | модули `game_filter_tests`, `service_flags_tests`, `switcher_tests`, `blockcheck_parser_tests`, `clipboard_restore_tests`, `double_shift_tests` |

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

`hide_console` (`CREATE_NO_WINDOW`), `ps_quote`, `run_powershell` (UTF-8, без прав),
`run_elevated_script` (UAC, результат через временный файл).

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

### `src/index.html` (~3460 строк) — весь фронтенд

HTML + CSS + JS в одном файле, состояние в объекте `state`, обращения к бэкенду через
`invoke(...)`. Вкладки переключаются обработчиком `document.querySelectorAll('.tab')`,
каждая вкладка при активации грузит свои данные (`loadListsTab`, `loadNetworkTab`,
`loadHostsTab`, `loadToolsTab`, `loadTrashTab`, `loadDiagnosticsTab`).

---

## 3. Жёсткие правила проекта

1. **Фронтенд без фреймворков и сборки.** Никаких React/Vue/npm-пакетов в `src/`.
2. **Никаких видимых консольных окон.** Любой `Command::new("cmd"/"powershell"/...)`
   оборачивается в `hide_console(&mut command)` (`CREATE_NO_WINDOW`). Исключение —
   UAC-путь: `run_elevated_script` (там окно нужно для запроса прав).
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
   `logs.rs`, `win_cmd.rs`, `network.rs`, `AGENTS.md` — LF без BOM; `src/index.html` — как
   есть. Правя `lib.rs` скриптом, читай байты, нормализуй CRLF→LF для замен и возвращай
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

---

## 5. Сборка, конфиг, релиз

- `src-tauri/tauri.conf.json`: `frontendDist: "../src"`, `withGlobalTauri: true`,
  `version: "../package.json"` (единственный источник версии), у окна `devtools: true`.
  В релизе DevTools требуют feature `devtools` в `Cargo.toml` (сейчас не включён намеренно).
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

Рабочая копия чистая: `files/` и `src-tauri/target-fix/` теперь в `.gitignore`.

---

## 7. Известные проблемы и ограничения

| Проблема | Детали |
|---|---|
| Настройка «проверять обновления» мертва | `auto_check_updates` читается/сохраняется, но `tauri-plugin-updater` нигде не вызывает `check()`; эндпоинт `github.com/example/...` и pubkey-заглушка в конфиге |
| ~~Вкладка «Сеть» медленная~~ | Решено в `64ffcd9`: IP Helper + COM, три запроса ~0,04 с (было 8,6 с). Резервные пути через PowerShell остались |
| `lib.rs` всё ещё ~5060 строк | Первый этап разбиения сделан (`logs`, `win_cmd`, `network`). Дальше: `switcher.rs` (хук и правка), `hosts.rs`, `blockcheck.rs`, `versions.rs`. Двигать по одному модулю за коммит с прогоном тестов |
| GUI не тестируется агентом | Окно Tauri недоступно инструментам браузера; проверка вкладок, свитчера и UAC — руками |
| Updater и CSP не настроены | `security.csp: null`; при появлении внешнего контента задать CSP |

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
