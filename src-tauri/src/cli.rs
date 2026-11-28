//! Консольный интерфейс: те же операции, что у вкладок приложения, но без окна.
//!
//! Запуск: `zapret-tools.exe cli <команда> [опции]`. Агенты и скрипты получают
//! JSON в stdout (кроме `get --raw` и `help`), а ошибку — JSON в stderr и код 1.
//!
//! Модуль не создаёт Tauri-приложение: он вызывает те же функции команд, что и
//! `invoke` из интерфейса, поэтому поведение консоли и окна не расходится.
//! Команды свитчера (хук клавиатуры) живут только внутри GUI: отсюда доступно
//! чтение их настроек.

use std::io::Write;
use std::path::PathBuf;

use serde_json::{json, Value};

/// Идентификатор приложения из `tauri.conf.json`: по нему Tauri выбирает папку
/// данных, поэтому путь к `config.json` считается так же.
const APP_IDENTIFIER: &str = "com.zapret.tools";

const HELP: &str = "\
Zapret Tools — консольный интерфейс для агентов и скриптов.

Использование:
  zapret-tools.exe cli <команда> [опции]

Команды:
  status                        Состояние: версия, конфиг, запущенный winws, QUIC
  versions [--root PATH]        Папки версий Zapret в корне
  lists [--version PATH]        Файлы списков и число строк
  get <файл> [--raw]            Содержимое списка (--raw — без JSON)
  set <файл> --from FILE|-      Сохранить список: дубликаты удаляются, удалённые
                                строки уходят в корзину
  strategies [--version PATH]   Стратегии (.bat) версии
  service <start|stop|install|remove> --version PATH
                                Управление службой zapret (запрос UAC)
  interfaces                    Сетевые адаптеры
  dns get --interface NAME      DNS-серверы адаптера
  dns set --interface NAME --dns1 IP [--dns2 IP] [--doh]
  dns reset --interface NAME
  quic <on|off|status>          Блокировка QUIC в брандмауэре (UAC)
  log [--lines N]               Последние строки лога свитчера (1..500, по умолчанию 200)
  help                          Эта справка

Опции:
  --root PATH       корень Zapret (по умолчанию из config.json)
  --version PATH    папка версии (по умолчанию активная из config.json)

Корень и активная версия по умолчанию берутся из
%APPDATA%\\com.zapret.tools\\config.json.
";

/// Результат команды: JSON или уже готовый текст (справка, `get --raw`).
enum Output {
    Json(Value),
    Raw(String),
}

enum CliError {
    /// Неверный вызов: код возврата 2.
    Usage(String),
    /// Команда не выполнилась: код возврата 1.
    Failed(String),
}

type CliResult<T> = Result<T, CliError>;

/// Разобранные аргументы команды.
struct Args {
    flags: Vec<(String, Option<String>)>,
    positional: Vec<String>,
}

impl Args {
    fn parse(raw: &[String]) -> CliResult<Self> {
        // Значение через пробел берём только у известных опций: иначе флаг
        // вроде `--raw` съел бы следующий аргумент.
        const VALUE_OPTIONS: [&str; 8] = [
            "root",
            "version",
            "from",
            "name",
            "lines",
            "interface",
            "dns1",
            "dns2",
        ];

        let mut flags = Vec::new();
        let mut positional = Vec::new();
        let mut iter = raw.iter();

        while let Some(item) = iter.next() {
            if item == "--" {
                positional.extend(iter.cloned());
                break;
            }
            if let Some(rest) = item.strip_prefix("--") {
                if let Some((key, value)) = rest.split_once('=') {
                    flags.push((key.to_string(), Some(value.to_string())));
                } else if VALUE_OPTIONS.contains(&rest) {
                    match iter.next() {
                        Some(value) => flags.push((rest.to_string(), Some(value.clone()))),
                        None => {
                            return Err(CliError::Usage(format!("У опции --{rest} нет значения")))
                        }
                    }
                } else {
                    flags.push((rest.to_string(), None));
                }
            } else if item.starts_with('-') && item.len() > 1 {
                return Err(CliError::Usage(format!("Неизвестная опция: {item}")));
            } else {
                positional.push(item.clone());
            }
        }

        Ok(Self { flags, positional })
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(name, _)| name == key)
            .and_then(|(_, value)| value.as_deref())
    }

    fn has(&self, key: &str) -> bool {
        self.flags.iter().any(|(name, _)| name == key)
    }

    /// Позиционный аргумент по его месту в разборе: `0` — сама команда,
    /// `1` — первый аргумент после неё.
    fn arg(&self, index: usize) -> Option<&str> {
        self.positional.get(index).map(String::as_str)
    }

    fn named(&self, key: &str, position: usize) -> Option<String> {
        match self.value(key) {
            Some(value) => Some(value.to_string()),
            None => self.arg(position).map(str::to_string),
        }
    }
}

/// Точка входа консольного режима. Возвращает код возврата процесса.
pub fn run(raw: &[String]) -> i32 {
    attach_console();

    match dispatch(raw) {
        Ok(Output::Json(value)) => {
            write_stream(&format!("{}\n", render_json(&value)), false);
            0
        }
        Ok(Output::Raw(text)) => {
            write_stream(&text, false);
            0
        }
        Err(CliError::Usage(message)) => {
            write_stream(
                &format!("{}\n", render_json(&json!({ "error": message }))),
                true,
            );
            write_stream("Подробнее: zapret-tools.exe cli help\n", true);
            2
        }
        Err(CliError::Failed(message)) => {
            write_stream(
                &format!("{}\n", render_json(&json!({ "error": message }))),
                true,
            );
            1
        }
    }
}

fn render_json(value: &Value) -> String {
    // Сериализация структуры из json! не может упасть на данных; на всякий
    // случай не паникуем, а печатаем текст ошибки.
    serde_json::to_string_pretty(value).unwrap_or_else(|err| format!("{{\"error\":\"{err}\"}}"))
}

fn dispatch(raw: &[String]) -> CliResult<Output> {
    let args = Args::parse(raw)?;
    let command = args
        .positional
        .first()
        .map(String::as_str)
        .unwrap_or("help");

    match command {
        "help" | "--help" | "-h" | "?" => Ok(Output::Raw(HELP.to_string())),
        "status" => cmd_status(&args),
        "versions" => cmd_versions(&args),
        "lists" => cmd_lists(&args),
        "get" | "list-get" => cmd_get(&args),
        "set" | "list-set" => cmd_set(&args),
        "strategies" => cmd_strategies(&args),
        "service" => cmd_service(&args),
        "interfaces" => cmd_interfaces(),
        "dns" => cmd_dns(&args),
        "quic" => cmd_quic(&args),
        "log" | "logs" => cmd_log(&args),
        other => Err(CliError::Usage(format!("Неизвестная команда: {other}"))),
    }
}

// ---------- Конфиг и пути ----------

fn config_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join(APP_IDENTIFIER)
            .join("config.json"),
    )
}

/// Конфиг читается тем же способом, что и в окне: битый или отсутствующий
/// файл даёт значения по умолчанию, а не ошибку.
fn read_config() -> crate::Config {
    let Some(path) = config_path() else {
        return crate::Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return crate::Config::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn resolve_root(args: &Args, config: &crate::Config) -> CliResult<PathBuf> {
    if let Some(root) = args.value("root") {
        return Ok(PathBuf::from(root));
    }
    match &config.root_path {
        Some(root) => Ok(PathBuf::from(root)),
        None => Err(CliError::Failed(
            "Папка Zapret не выбрана: укажите --root PATH".to_string(),
        )),
    }
}

/// Папка версии: `--version`, затем активная версия из конфига, затем
/// единственная найденная версия.
fn resolve_version(args: &Args, config: &crate::Config) -> CliResult<PathBuf> {
    if let Some(path) = args.value("version") {
        let path = PathBuf::from(path);
        if !path.is_dir() {
            return Err(CliError::Failed(format!(
                "Папка версии не найдена: {}",
                path.display()
            )));
        }
        return Ok(path);
    }

    let root = resolve_root(args, config)?;
    let versions =
        crate::scan_versions(root.to_string_lossy().to_string()).map_err(CliError::Failed)?;

    if let Some(active) = &config.active_version {
        if let Some(found) = versions.iter().find(|version| &version.name == active) {
            return Ok(PathBuf::from(&found.path));
        }
    }

    match versions.len() {
        0 => Err(CliError::Failed(format!(
            "В папке {} нет версий Zapret",
            root.display()
        ))),
        1 => Ok(PathBuf::from(&versions[0].path)),
        _ => Err(CliError::Usage(
            "В корне несколько версий: укажите --version PATH".to_string(),
        )),
    }
}

fn runtime() -> CliResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| CliError::Failed(format!("не удалось запустить runtime: {err}")))
}

// ---------- Команды ----------

fn cmd_status(args: &Args) -> CliResult<Output> {
    let config = read_config();
    let root = match &config.root_path {
        Some(root) => Some(PathBuf::from(root)),
        None => args.value("root").map(PathBuf::from),
    };

    let versions = match &root {
        Some(root) => crate::scan_versions(root.to_string_lossy().to_string()).unwrap_or_default(),
        None => Vec::new(),
    };

    let rt = runtime()?;
    let running_winws = rt.block_on(crate::find_running_winws());
    let quic_block = rt.block_on(crate::network::get_quic_block_state());

    let active = config.active_version.clone();
    let active_path = active
        .as_ref()
        .and_then(|name| versions.iter().find(|version| &version.name == name))
        .map(|version| version.path.clone());

    Ok(Output::Json(json!({
        "app": {
            "name": "Zapret Tools",
            "version": env!("CARGO_PKG_VERSION"),
            "exe": std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        },
        "config": {
            "path": config_path().map(|path| path.display().to_string()).unwrap_or_default(),
            "root_path": config.root_path,
            "active_version": active,
            "active_path": active_path,
            "auto_check_updates": config.auto_check_updates,
        },
        "switcher": {
            "enabled": config.switcher_enabled,
            "exclusions": config.switcher_exclusions,
            "word_exclusions_count": config.switcher_word_exclusions.len(),
            "custom_words_ru": config.switcher_custom_dict_ru.len(),
            "custom_words_en": config.switcher_custom_dict_en.len(),
            "declined_words": config.switcher_declined_words.len(),
        },
        "versions": versions,
        "running_winws": running_winws,
        "quic_block": quic_block,
        "log_path": std::env::temp_dir()
            .join("zapret-tools-switcher.log")
            .display()
            .to_string(),
    })))
}

fn cmd_versions(args: &Args) -> CliResult<Output> {
    let config = read_config();
    let root = resolve_root(args, &config)?;
    let versions =
        crate::scan_versions(root.to_string_lossy().to_string()).map_err(CliError::Failed)?;
    Ok(Output::Json(json!({
        "root": root.display().to_string(),
        "versions": versions,
    })))
}

fn cmd_lists(args: &Args) -> CliResult<Output> {
    let config = read_config();
    let version = resolve_version(args, &config)?;
    let rt = runtime()?;
    let files = rt
        .block_on(crate::read_list_files(
            version.to_string_lossy().to_string(),
        ))
        .map_err(CliError::Failed)?;
    Ok(Output::Json(json!({
        "version": version.display().to_string(),
        "files": files,
    })))
}

fn cmd_get(args: &Args) -> CliResult<Output> {
    let name = args.named("name", 1).ok_or_else(|| {
        CliError::Usage("Укажите файл: cli get <файл> [--version PATH]".to_string())
    })?;
    let config = read_config();
    let version = resolve_version(args, &config)?;
    let content = crate::read_list_file(version.to_string_lossy().to_string(), name.clone())
        .map_err(CliError::Failed)?;

    if args.has("raw") {
        return Ok(Output::Raw(content));
    }

    Ok(Output::Json(json!({
        "version": version.display().to_string(),
        "file": name,
        "lines": content.lines().count(),
        "content": content,
    })))
}

fn cmd_set(args: &Args) -> CliResult<Output> {
    let name = args.named("name", 1).ok_or_else(|| {
        CliError::Usage("Укажите файл и содержимое: cli set <файл> --from FILE|-".to_string())
    })?;
    let source = args.value("from").ok_or_else(|| {
        CliError::Usage("Укажите источник содержимого: --from FILE или --from -".to_string())
    })?;

    // Для записи нужен корень: удалённые строки уходят в `zapret_trash`, а путь
    // проверяется на принадлежность выбранной папке Zapret.
    let config = read_config();
    let root = resolve_root(args, &config)?;
    let version = resolve_version(args, &config)?;

    let content = if source == "-" {
        let mut buffer = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
            .map_err(|err| CliError::Failed(format!("не удалось прочитать stdin: {err}")))?;
        buffer
    } else {
        std::fs::read_to_string(source)
            .map_err(|err| CliError::Failed(format!("{source}: {err}")))?
    };

    let message = crate::write_list_file(
        root.to_string_lossy().to_string(),
        version.to_string_lossy().to_string(),
        name.clone(),
        content,
    )
    .map_err(CliError::Failed)?;

    Ok(Output::Json(json!({
        "version": version.display().to_string(),
        "file": name,
        "result": message,
    })))
}

fn cmd_strategies(args: &Args) -> CliResult<Output> {
    let config = read_config();
    let version = resolve_version(args, &config)?;
    let strategies =
        crate::list_strategies(version.to_string_lossy().to_string()).map_err(CliError::Failed)?;
    Ok(Output::Json(json!({
        "version": version.display().to_string(),
        "strategies": strategies,
    })))
}

fn cmd_service(args: &Args) -> CliResult<Output> {
    let action = args.arg(1).unwrap_or_default();
    if !matches!(action, "start" | "stop" | "install" | "remove") {
        return Err(CliError::Usage(
            "Действие службы: start, stop, install или remove".to_string(),
        ));
    }

    let config = read_config();
    let version = resolve_version(args, &config)?;
    crate::manage_service(version.to_string_lossy().to_string(), action.to_string())
        .map_err(CliError::Failed)?;

    Ok(Output::Json(json!({
        "version": version.display().to_string(),
        "action": action,
        "result": "Команда отправлена через запрос прав администратора",
    })))
}

fn cmd_interfaces() -> CliResult<Output> {
    let rt = runtime()?;
    let interfaces = rt.block_on(crate::network::list_network_interfaces());
    Ok(Output::Json(json!({ "interfaces": interfaces })))
}

fn cmd_dns(args: &Args) -> CliResult<Output> {
    let action = args.arg(1).unwrap_or("get");
    let rt = runtime()?;

    match action {
        "get" => {
            let interface = require_interface(args)?;
            let servers = rt
                .block_on(crate::network::get_dns_state(interface.clone()))
                .map_err(CliError::Failed)?;
            Ok(Output::Json(json!({
                "interface": interface,
                "servers": servers,
            })))
        }
        "set" => {
            let interface = require_interface(args)?;
            let dns1 = args
                .value("dns1")
                .ok_or_else(|| CliError::Usage("Укажите основной DNS: --dns1 IP".to_string()))?;
            let dns2 = args.value("dns2").map(str::to_string);
            rt.block_on(crate::network::set_dns(
                interface.clone(),
                dns1.to_string(),
                dns2.clone(),
                args.has("doh"),
            ))
            .map_err(CliError::Failed)?;
            Ok(Output::Json(json!({
                "interface": interface,
                "dns1": dns1,
                "dns2": dns2,
                "doh": args.has("doh"),
                "result": "DNS изменён",
            })))
        }
        "reset" => {
            let interface = require_interface(args)?;
            rt.block_on(crate::network::reset_dns(interface.clone()))
                .map_err(CliError::Failed)?;
            Ok(Output::Json(json!({
                "interface": interface,
                "result": "DNS возвращён к DHCP",
            })))
        }
        other => Err(CliError::Usage(format!(
            "Неизвестное действие DNS: {other} (get, set, reset)"
        ))),
    }
}

fn require_interface(args: &Args) -> CliResult<String> {
    args.value("interface")
        .map(str::to_string)
        .ok_or_else(|| CliError::Usage("Укажите адаптер: --interface NAME".to_string()))
}

fn cmd_quic(args: &Args) -> CliResult<Output> {
    let action = args.arg(1).unwrap_or("status");
    let rt = runtime()?;

    match action {
        "status" => Ok(Output::Json(json!({
            "enabled": rt.block_on(crate::network::get_quic_block_state()),
        }))),
        "on" | "off" => {
            let enable = action == "on";
            rt.block_on(crate::network::toggle_quic_block(enable))
                .map_err(CliError::Failed)?;
            Ok(Output::Json(json!({
                "enabled": enable,
                "result": if enable {
                    "Правило блокировки QUIC создано"
                } else {
                    "Правило блокировки QUIC удалено"
                },
            })))
        }
        other => Err(CliError::Usage(format!(
            "Неизвестное действие QUIC: {other} (on, off, status)"
        ))),
    }
}

fn cmd_log(args: &Args) -> CliResult<Output> {
    let lines = match args.value("lines") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| CliError::Usage(format!("--lines: не число «{value}»")))?,
        None => 200,
    };

    let rt = runtime()?;
    let tail = rt
        .block_on(crate::logs::get_log_tail(lines))
        .map_err(CliError::Failed)?;
    Ok(Output::Json(json!({ "lines": tail })))
}

// ---------- Вывод ----------

/// GUI-сборка (`windows_subsystem = "windows"`) при запуске из терминала без
/// перенаправления может не иметь рабочего stdout: подключаемся к консоли
/// родителя, чтобы вывод не терялся. Ошибка здесь не критична.
#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: вызов только подключает процесс к консоли родителя и ничего не
    // освобождает; при уже подключённой консоли вернёт ошибку.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn write_stream(text: &str, stderr: bool) {
    let mut handle: Box<dyn Write> = if stderr {
        Box::new(std::io::stderr())
    } else {
        Box::new(std::io::stdout())
    };

    if handle
        .write_all(text.as_bytes())
        .and_then(|_| handle.flush())
        .is_ok()
    {
        return;
    }

    // Нет стандартного дескриптора — пишем прямо в экран консоли.
    #[cfg(windows)]
    if let Ok(mut console) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
        let _ = console.write_all(text.as_bytes());
        let _ = console.flush();
    }
}
