use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{ProcessesToUpdate, System};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WindowEvent};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VersionInfo {
    pub name: String,
    pub path: String,
    pub has_service: bool,
    pub has_winws: bool,
    pub has_lists: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TrashItem {
    pub id: String,
    pub original_name: String,
    pub time: u64,
    pub deleted_lines: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub root_path: Option<String>,
    #[serde(default)]
    pub active_version: Option<String>,
    /// Настройки свитчера переживают перезапуск приложения.
    #[serde(default)]
    pub switcher_enabled: bool,
    #[serde(default)]
    pub switcher_exclusions: Vec<String>,
    /// Слова, которые свитчер не должен исправлять автоматически.
    #[serde(default)]
    pub switcher_word_exclusions: Vec<String>,
    /// Пользовательский словарь: слова, которые свитчер считает «своими».
    #[serde(default)]
    pub switcher_custom_dict_ru: Vec<String>,
    #[serde(default)]
    pub switcher_custom_dict_en: Vec<String>,
    /// Слова, добавление которых пользователь отклонил: больше не предлагаем.
    #[serde(default)]
    pub switcher_declined_words: Vec<String>,
    /// Автопроверка обновлений самой программы при старте.
    /// У старых конфигов поля нет — тогда считаем, что проверка включена.
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
}

fn default_true() -> bool {
    true
}

// `Default` руками: вывод `#[derive(Default)]` дал бы auto_check_updates = false,
// а по умолчанию автопроверка должна быть включена.
impl Default for Config {
    fn default() -> Self {
        Self {
            root_path: None,
            active_version: None,
            switcher_enabled: false,
            switcher_exclusions: Vec::new(),
            switcher_word_exclusions: Vec::new(),
            switcher_custom_dict_ru: Vec::new(),
            switcher_custom_dict_en: Vec::new(),
            switcher_declined_words: Vec::new(),
            auto_check_updates: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileInfo {
    pub name: String,
    pub lines: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeadDomainReport {
    pub checked: usize,
    pub dead: Vec<String>,
}

// --- ЛОГИКА СЛИЯНИЯ ---
fn merge_text(old_t: &str, new_t: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let add = |t: &str, out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
        for line in t.lines() {
            let s = line.trim();
            if !s.is_empty() && seen.insert(s.to_string()) {
                out.push(s.to_string());
            }
        }
    };
    add(old_t, &mut out, &mut seen);
    add(new_t, &mut out, &mut seen);
    out.join("\n") + "\n"
}

// Разбивает строку Zapret на (префикс, тело-домена, суффикс).
// Примеры:
//   "full:example.com"    -> ("full:", "example.com", "")
//   "^example.com@cn"     -> ("^", "example.com", "@cn")
//   "@cn.example.com"     -> ("@cn.", "example.com", "")
//   "example.com"         -> ("", "example.com", "")
fn split_zapret_line(line: &str) -> (String, String, String) {
    let mut prefix = String::new();
    let mut rest = line;

    while let Some(s) = rest.strip_prefix('^') {
        prefix.push('^');
        rest = s;
    }

    if let Some(pos) = rest.find(':') {
        prefix.push_str(&rest[..=pos]);
        rest = &rest[pos + 1..];
    }

    if let Some(stripped) = rest.strip_prefix('@') {
        if let Some(dot) = stripped.find('.') {
            let tag = &stripped[..dot];
            if !tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                prefix.push('@');
                prefix.push_str(tag);
                prefix.push('.');
                rest = &stripped[dot + 1..];
            }
        }
    }

    let mut suffix = String::new();
    if let Some(pos) = rest.rfind('@') {
        suffix = rest[pos..].to_string();
        rest = &rest[..pos];
    }

    (prefix, rest.to_string(), suffix)
}

fn is_valid_domain_body(body: &str, domain_regex: &regex::Regex) -> bool {
    domain_regex.is_match(body.trim())
}

fn merge_dirs(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() { fs::create_dir_all(dst)?; }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if path.is_file() {
            if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                let old_text = fs::read_to_string(&path).unwrap_or_default();
                let new_text = if dst_path.exists() { fs::read_to_string(&dst_path).unwrap_or_default() } else { String::new() };
                fs::write(&dst_path, merge_text(&old_text, &new_text))?;
            } else if !dst_path.exists() {
                fs::copy(&path, &dst_path)?;
            }
        } else if path.is_dir() {
            merge_dirs(&path, &dst_path)?;
        }
    }
    Ok(())
}

// --- КОМАНДЫ ---
/// Читает config.json с диска. Отсутствующий или битый файл — значения по умолчанию.
fn read_config(app: &tauri::AppHandle) -> Config {
    let Ok(dir) = app.path().app_data_dir() else {
        return Config::default();
    };
    let Ok(text) = fs::read_to_string(dir.join("config.json")) else {
        return Config::default();
    };
    // Старые конфиги читаются благодаря #[serde(default)] на новых полях.
    serde_json::from_str(&text).unwrap_or_default()
}

#[tauri::command]
fn get_config(app: tauri::AppHandle) -> Result<Config, String> {
    Ok(read_config(&app))
}

#[tauri::command]
fn save_config(
    app: tauri::AppHandle,
    state: tauri::State<Arc<Mutex<SwitcherState>>>,
    config: Config,
) -> Result<(), String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    fs::write(dir.join("config.json"), text).map_err(|e| e.to_string())?;

    // Держим состояние хука в синхроне с файлом, чтобы порядок вызовов
    // update_switcher_config / save_config не влиял на результат.
    {
        let mut s = lock_switcher(&state);
        s.enabled = config.switcher_enabled;
        s.exclusions = config.switcher_exclusions.clone();
        s.word_exclusions = config.switcher_word_exclusions.clone();
        s.custom_dict_ru = config.switcher_custom_dict_ru.clone();
        s.custom_dict_en = config.switcher_custom_dict_en.clone();
        s.declined_words = config.switcher_declined_words.clone();
    }
    Ok(())
}

#[tauri::command]
fn scan_versions(base_path: String) -> Result<Vec<VersionInfo>, String> {
    let path = Path::new(&base_path);
    if !path.exists() { return Err("Path not found".into()); }
    let mut versions = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "zapret-data" { continue; }
                let has_service = p.join("service.bat").exists();
                let has_winws = p.join("bin").join("winws.exe").exists();
                let has_lists = p.join("lists").exists() || p.join("zapret-data").join("lists").exists();
                if has_service || has_winws || has_lists {
                    versions.push(VersionInfo { name, path: p.to_string_lossy().to_string(), has_service, has_winws, has_lists });
                }
            }
        }
    }
    Ok(versions)
}

#[tauri::command]
async fn find_running_winws() -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let mut sys = System::new_all();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        for (_, process) in sys.processes() {
            let name = process.name().to_string_lossy().to_lowercase();
            if name.contains("winws") {
                if let Some(exe_path) = process.exe() {
                    if !exe_path.to_string_lossy().is_empty() { return Some(exe_path.to_string_lossy().to_string()); }
                }
            }
        }
        let mut reg = Command::new("reg");
        hide_console(&mut reg);
        let output = reg.args(&["query", "HKLM\\System\\CurrentControlSet\\Services\\zapret", "/v", "ImagePath"]).output();
        if let Ok(out) = output {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                for line in stdout.lines() {
                    if line.contains("ImagePath") {
                        if let Some(start_quote) = line.find('"') {
                            let rest = &line[start_quote + 1..];
                            if let Some(end_quote) = rest.find('"') {
                                let path = &rest[..end_quote];
                                if path.contains("winws.exe") { return Some(path.to_string()); }
                            }
                        }
                    }
                }
            }
        }
        None
    }).await.unwrap_or(None)
}

#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    Command::new("explorer").arg(path).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn run_as_admin(bat_path: String) -> Result<(), String> {
    // UAC-запрос должен быть видимым, а консоль самого PowerShell — нет.
    let mut command = Command::new("powershell");
    hide_console(&mut command);
    command
        .args(["-Command", &format!("Start-Process '{}' -Verb RunAs", bat_path.replace("'", "''"))])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn manage_service(version_path: String, action: String) -> Result<(), String> {
    let bat_path = Path::new(&version_path).join("service.bat");
    if !bat_path.exists() { return Err("service.bat не найден".into()); }
    let bat_str = bat_path.to_string_lossy().to_string();
    let cmd = match action.as_str() {
        "start" => "sc start zapret".to_string(),
        "stop" => "sc stop zapret".to_string(),
        "remove" => "sc stop zapret & sc delete zapret".to_string(),
        "install" => format!("\"\"{}\"\" admin", bat_str),
        _ => return Err("Неизвестное действие".into()),
    };
    let ps_command = format!("Start-Process cmd -ArgumentList '/c {}' -Verb RunAs", cmd);
    let mut command = Command::new("powershell");
    hide_console(&mut command);
    command
        .args(["-NoProfile", "-Command", &ps_command])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn read_list_files(version_path: String) -> Result<Vec<FileInfo>, String> {
    tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let p1 = Path::new(&version_path).join("lists");
        let p2 = Path::new(&version_path).join("zapret-data").join("lists");
        let target = if p1.exists() { p1 } else { p2 };
        if target.exists() {
            if let Ok(entries) = fs::read_dir(target) {
                for entry in entries.flatten() {
                    if entry.path().extension().and_then(|s| s.to_str()) == Some("txt") {
                        let name = entry.file_name().to_string_lossy().to_string();
                        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                        // Файлы больше 2 МБ не читаем: подсчёт строк по ним слишком долгий.
                        // 999999 — маркер «огромный файл» для фронтенда.
                        if size > 2 * 1024 * 1024 {
                            files.push(FileInfo { name, lines: 999999 });
                            continue;
                        }
                        let mut lines = 0;
                        if let Ok(file) = fs::File::open(entry.path()) {
                            let reader = BufReader::new(file);
                            lines = reader.lines().filter(|l| l.is_ok()).count();
                        }
                        files.push(FileInfo { name, lines });
                    }
                }
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(files)
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
fn read_list_file(version_path: String, file_name: String) -> Result<String, String> {
    let p1 = Path::new(&version_path).join("lists").join(&file_name);
    let p2 = Path::new(&version_path).join("zapret-data").join("lists").join(&file_name);
    let target = if p1.exists() { p1 } else { p2 };
    fs::read_to_string(target).map_err(|e| e.to_string())
}

#[tauri::command]
fn write_list_file(root_path: String, version_path: String, file_name: String, content: String) -> Result<String, String> {
    let p1 = Path::new(&version_path).join("lists").join(&file_name);
    let p2 = Path::new(&version_path).join("zapret-data").join("lists").join(&file_name);
    let target = if p1.exists() { p1 } else { p2 };

    if !target.exists() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }

    let old_text = fs::read_to_string(&target).unwrap_or_default();

    let mut seen = std::collections::HashSet::new();
    let mut cleaned_lines = Vec::new();
    let mut removed_count = 0;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let lower = trimmed.to_lowercase();
        if seen.contains(&lower) { removed_count += 1; }
        else {
            seen.insert(lower);
            cleaned_lines.push(trimmed.to_string());
        }
    }

    let mut new_lines_set = std::collections::HashSet::new();
    for line in &cleaned_lines { new_lines_set.insert(line.to_lowercase()); }

    let mut deleted_lines = Vec::new();
    for old_line in old_text.lines() {
        let s = old_line.trim();
        if !s.is_empty() && !new_lines_set.contains(&s.to_lowercase()) {
            deleted_lines.push(s.to_string());
        }
    }

    if !deleted_lines.is_empty() {
        let trash_dir = Path::new(&root_path).join("zapret_trash");
        if !trash_dir.exists() { let _ = fs::create_dir_all(&trash_dir); }

        // Часы могут быть сбиты (дата до 1970 года) — имя файла не повод падать.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp = now.as_secs();
        let trash_id = format!("trash_{}_{:09}.json", timestamp, now.subsec_nanos());
        let trash_data = TrashItem {
            id: trash_id.clone(),
            original_name: file_name.clone(),
            time: timestamp,
            deleted_lines: deleted_lines.clone(),
        };

        if let Ok(json_str) = serde_json::to_string_pretty(&trash_data) {
            let _ = fs::write(trash_dir.join(trash_id), json_str);
        }
    }

    let final_content = cleaned_lines.join("\n") + "\n";
    fs::write(target, final_content).map_err(|e| e.to_string())?;

    Ok(format!("Сохранено. Удалено дубликатов: {}. В корзину отправлено строк: {}", removed_count, deleted_lines.len()))
}

// --- КОМАНДЫ КОРЗИНЫ ---
#[tauri::command]
fn get_trash_files(root_path: String) -> Result<Vec<TrashItem>, String> {
    let trash_dir = Path::new(&root_path).join("zapret_trash");
    let mut files = Vec::new();
    if trash_dir.exists() {
        if let Ok(entries) = fs::read_dir(trash_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(item) = serde_json::from_str::<TrashItem>(&content) {
                            files.push(item);
                        }
                    }
                }
            }
        }
    }
    files.sort_by(|a, b| b.time.cmp(&a.time));
    Ok(files)
}

#[tauri::command]
fn restore_trash_file(root_path: String, version_path: String, trash_id: String, original_name: String, deleted_lines: Vec<String>) -> Result<String, String> {
    let trash_file = Path::new(&root_path).join("zapret_trash").join(&trash_id);
    let p1 = Path::new(&version_path).join("lists").join(&original_name);
    let p2 = Path::new(&version_path).join("zapret-data").join("lists").join(&original_name);
    let target = if p1.exists() { p1 } else { p2 };

    if let Some(parent) = target.parent() {
        if !parent.exists() { fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
    }

    let mut current_content = fs::read_to_string(&target).unwrap_or_default();
    if !current_content.ends_with('\n') && !current_content.is_empty() {
        current_content.push('\n');
    }

    let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in current_content.lines() {
        let s = line.trim();
        if !s.is_empty() { existing.insert(s.to_lowercase()); }
    }

    let mut restored = 0usize;
    let mut skipped = 0usize;
    for line in deleted_lines {
        let s = line.trim();
        if s.is_empty() { continue; }
        if existing.insert(s.to_lowercase()) {
            current_content.push_str(s);
            current_content.push('\n');
            restored += 1;
        } else {
            skipped += 1;
        }
    }

    fs::write(&target, current_content).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(trash_file);
    Ok(format!("Восстановлено строк: {}. Пропущено дубликатов: {}.", restored, skipped))
}

#[tauri::command]
fn empty_trash(root_path: String) -> Result<(), String> {
    let trash_dir = Path::new(&root_path).join("zapret_trash");
    if trash_dir.exists() {
        fs::remove_dir_all(&trash_dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// --- ОСТАЛЬНЫЕ КОМАНДЫ ---
#[tauri::command]
fn read_external_file(path: String) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| e.to_string())
}

#[tauri::command]
fn merge_folders(src: String, dst: String) -> Result<String, String> {
    merge_dirs(Path::new(&src), Path::new(&dst)).map_err(|e| e.to_string())?;
    Ok("Merged successfully".into())
}

#[tauri::command]
async fn sync_github(version_path: String, file_name: String, url: String) -> Result<String, String> {
    let res = reqwest::get(url).await.map_err(|e| e.to_string())?;
    if !res.status().is_success() { return Err("Download failed".into()); }
    let downloaded_text = res.text().await.map_err(|e| e.to_string())?;
    let p1 = Path::new(&version_path).join("lists").join(&file_name);
    let p2 = Path::new(&version_path).join("zapret-data").join("lists").join(&file_name);
    let target = if p1.exists() { p1 } else { p2 };
    let existing_text = if target.exists() { fs::read_to_string(&target).unwrap_or_default() } else { String::new() };
    let merged = merge_text(&existing_text, &downloaded_text);
    if let Some(parent) = target.parent() {
        if !parent.exists() { fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
    }
    fs::write(target, merged).map_err(|e| e.to_string())?;
    Ok(file_name)
}

/// Шаблон домена общий для `validate_list` и `smart_parse_domains`.
///
/// Раньше `Regex::new` вызывался на каждую проверку списка, то есть собирал
/// автомат заново на каждый клик. Теперь шаблон компилируется один раз.
static DOMAIN_RE: OnceLock<regex::Regex> = OnceLock::new();

fn domain_regex() -> &'static regex::Regex {
    DOMAIN_RE.get_or_init(|| {
        regex::Regex::new(
            r"^[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(\.[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)+$",
        )
        // Шаблон — константа: сломаться может только при правке кода.
        .expect("доменный regex должен компилироваться")
    })
}

#[tauri::command]
fn validate_list(content: String, is_ip_list: bool) -> Result<Vec<String>, String> {
    let mut errors = Vec::new();
    let domain_regex = domain_regex();

    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        if is_ip_list {
            let ip_part = if line.contains('/') { line.split('/').next().unwrap_or("") } else { line };
            if std::net::IpAddr::from_str(ip_part).is_err() {
                errors.push(format!("Строка {}: '{}' — неверный IP", index + 1, line));
            }
        } else {
            let (_, body, _) = split_zapret_line(line);
            if body.is_empty() || !is_valid_domain_body(&body, domain_regex) {
                errors.push(format!("Строка {}: '{}' — неверный домен", index + 1, line));
            }
        }
    }
    Ok(errors)
}

#[tauri::command]
fn smart_parse_domains(input: String) -> Result<(Vec<String>, Vec<String>, usize), String> {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    let mut converted = 0;

    let tokens = input.split(|c: char| c.is_whitespace() || c == ',' || c == ';');
    let domain_regex = domain_regex();

    for token in tokens {
        let t = token.trim();
        if t.is_empty() { continue; }

        let (prefix, body, suffix) = split_zapret_line(t);
        if body.is_empty() {
            invalid.push(t.to_string());
            continue;
        }

        let mut body_out = body;
        if !body_out.is_ascii() {
            match idna::domain_to_ascii(&body_out) {
                Ok(puny) => { body_out = puny; converted += 1; }
                Err(_) => { invalid.push(t.to_string()); continue; }
            }
        }

        if is_valid_domain_body(&body_out, domain_regex) {
            valid.push(format!("{}{}{}", prefix, body_out, suffix));
        } else {
            invalid.push(t.to_string());
        }
    }
    Ok((valid, invalid, converted))
}

#[tauri::command]
fn clean_and_validate(content: String, is_ip_list: bool) -> Result<(String, Vec<String>, usize), String> {
    let mut seen = std::collections::HashSet::new();
    let mut cleaned_lines = Vec::new();
    let mut removed_count = 0;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            removed_count += 1;
            continue;
        }

        let processed = if is_ip_list {
            trimmed.to_string()
        } else {
            let (prefix, body, suffix) = split_zapret_line(trimmed);
            if body.is_empty() { removed_count += 1; continue; }

            let mut body_out = body;
            if !body_out.is_ascii() {
                if let Ok(puny) = idna::domain_to_ascii(&body_out) {
                    body_out = puny;
                }
            }
            format!("{}{}{}", prefix, body_out, suffix)
        };

        let lower = processed.to_lowercase();
        if seen.insert(lower) {
            cleaned_lines.push(processed);
        } else {
            removed_count += 1;
        }
    }

    cleaned_lines.sort();

    let final_text = cleaned_lines.join("\n") + if cleaned_lines.is_empty() { "" } else { "\n" };
    let errors = validate_list(final_text.clone(), is_ip_list)?;

    Ok((final_text, errors, removed_count))
}

// Оставляет от каждого домена только регистрируемый корень (psl) и убирает дубли.
#[tauri::command]
fn consolidate_subdomains(content: String) -> Result<(String, usize), String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut input_count = 0usize;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() { continue; }
        input_count += 1;

        if line.starts_with('#') {
            if seen.insert(line.to_lowercase()) { out.push(line.to_string()); }
            continue;
        }

        let (prefix, body, suffix) = split_zapret_line(line);
        // '^' — строгий якорь: только этот домен, без поддоменов.
        // Сжатие ^dns.nextdns.io до ^nextdns.io сломало бы правило, поэтому не трогаем.
        if prefix.contains('^') {
            if seen.insert(line.to_lowercase()) { out.push(line.to_string()); }
            continue;
        }
        let body_lower = body.trim().to_lowercase();
        let is_ip = body_lower.contains('/') || std::net::IpAddr::from_str(&body_lower).is_ok();
        // Не трогаем всё, что не похоже на обычный домен (wildcard, порт, мусор).
        let is_plain_domain = body_lower.contains('.')
            && body_lower.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');

        let consolidated = if is_ip || !is_plain_domain {
            line.to_string()
        } else {
            match psl::domain_str(&body_lower) {
                Some(root) => format!("{}{}{}", prefix, root, suffix),
                None => line.to_string(),
            }
        };

        if seen.insert(consolidated.to_lowercase()) {
            out.push(consolidated);
        }
    }

    let removed = input_count.saturating_sub(out.len());
    let text = if out.is_empty() { String::new() } else { out.join("\n") + "\n" };
    Ok((text, removed))
}

async fn probe_domain(client: &reqwest::Client, domain: &str) -> Option<String> {
    let https = format!("https://{}/", domain);
    if client.head(&https).send().await.is_ok() {
        return None;
    }
    let http = format!("http://{}/", domain);
    if client.head(&http).send().await.is_ok() {
        return None;
    }
    Some(domain.to_string())
}

// Проверяет домены из содержимого файла HEAD-запросами (таймаут 3 c, 32 параллельно).
#[tauri::command]
async fn check_dead_domains(content: String) -> Result<DeadDomainReport, String> {
    let mut seen = std::collections::HashSet::new();
    let mut targets: Vec<String> = Vec::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let (_, body, _) = split_zapret_line(line);
        let body = body.trim().trim_end_matches('.').to_lowercase();
        if body.is_empty() || !body.contains('.') { continue; }
        if body.contains('/') || std::net::IpAddr::from_str(&body).is_ok() { continue; }
        if !body.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') { continue; }
        if seen.insert(body.clone()) { targets.push(body); }
    }

    let checked = targets.len();
    if checked == 0 {
        return Ok(DeadDomainReport { checked, dead: Vec::new() });
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .connect_timeout(std::time::Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("ZapretTools/1.0")
        .build()
        .map_err(|e| e.to_string())?;

    const CONCURRENCY: usize = 32;
    let mut iter = targets.into_iter();
    let mut set = tokio::task::JoinSet::new();
    let mut dead: Vec<String> = Vec::new();

    for _ in 0..CONCURRENCY {
        match iter.next() {
            Some(domain) => {
                let client = client.clone();
                set.spawn(async move { probe_domain(&client, &domain).await });
            }
            None => break,
        }
    }

    while let Some(result) = set.join_next().await {
        if let Ok(Some(domain)) = result {
            dead.push(domain);
        }
        if let Some(domain) = iter.next() {
            let client = client.clone();
            set.spawn(async move { probe_domain(&client, &domain).await });
        }
    }

    dead.sort();
    Ok(DeadDomainReport { checked, dead })
}

#[tauri::command]
async fn check_zapret_version(version_path: String) -> Result<(String, String), String> {
    let bat_path = Path::new(&version_path).join("service.bat");
    let mut local_version = String::from("Unknown");
    if let Ok(content) = fs::read_to_string(bat_path) {
        static LOCAL_VERSION_RE: OnceLock<regex::Regex> = OnceLock::new();
        let re = LOCAL_VERSION_RE.get_or_init(|| {
            regex::Regex::new(r#"LOCAL_VERSION=([\d\.]+)"#)
                // Шаблон — константа, компилируется всегда.
                .expect("regex версии должен компилироваться")
        });
        if let Some(caps) = re.captures(&content) { local_version = caps[1].to_string(); }
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client
        .get("https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/main/.service/version.txt")
        .send()
        .await
        .map_err(|e| format!("Сервер не отвечает: {e}"))?;
    if !res.status().is_success() { return Err("Не удалось проверить версию на GitHub".into()); }
    let remote_version = res.text().await.map_err(|e| e.to_string())?.trim().to_string();
    Ok((local_version, remote_version))
}

#[tauri::command]
async fn download_zapret(root_path: String, active_name: String, new_version: String) -> Result<String, String> {
    let root = Path::new(&root_path);
    let old_dir = root.join(&active_name);
    let new_dir_name = format!("zapret-discord-youtube-{}", new_version);
    let new_dir = root.join(&new_dir_name);
    let zip_path = root.join("update.zip");
    let extract_dir = root.join("temp_extracted");

    let url = "https://github.com/Flowseal/zapret-discord-youtube/archive/refs/heads/main.zip";
    let res = reqwest::get(url).await.map_err(|e| e.to_string())?;
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    fs::write(&zip_path, bytes).map_err(|e| e.to_string())?;

    if extract_dir.exists() { let _ = fs::remove_dir_all(&extract_dir); }
    fs::create_dir_all(&extract_dir).map_err(|e| e.to_string())?;
    let ps_cmd = format!("Expand-Archive -Path '{}' -DestinationPath '{}' -Force", zip_path.to_string_lossy(), extract_dir.to_string_lossy());
    let mut extract = Command::new("powershell");
    hide_console(&mut extract);
    let status = extract.args(&["-NoProfile", "-Command", &ps_cmd]).status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            return Err(format!(
                "Не удалось распаковать архив (код {})",
                status.code().unwrap_or(-1)
            ))
        }
        Err(err) => return Err(format!("Не удалось запустить PowerShell: {err}")),
    }

    let extracted_content = extract_dir.join("zapret-discord-youtube-main");

    if new_dir.exists() {
        let _ = fs::remove_file(&zip_path);
        let _ = fs::remove_dir_all(&extract_dir);
        return Err(format!(
            "Папка '{}' уже существует. Удалите её вручную и повторите обновление.",
            new_dir_name
        ));
    }
    fs::rename(&extracted_content, &new_dir).map_err(|e| e.to_string())?;

    let _ = fs::remove_file(&zip_path);
    let _ = fs::remove_dir_all(&extract_dir);

    let old_lists = old_dir.join("lists");
    let new_lists = new_dir.join("lists");
    if old_lists.exists() { let _ = merge_dirs(&old_lists, &new_lists); }
    let old_utils = old_dir.join("utils");
    let new_utils = new_dir.join("utils");
    if old_utils.exists() { let _ = merge_dirs(&old_utils, &new_utils); }
    Ok(new_dir_name)
}

#[tauri::command]
fn delete_version(version_path: String) -> Result<(), String> {
    let path = Path::new(&version_path);
    if path.exists() { fs::remove_dir_all(path).map_err(|e| e.to_string())?; }
    Ok(())
}

#[tauri::command]
fn create_start_menu_shortcut() -> Result<(), String> {
    let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let ps_script = format!(
        r#"$WshShell = New-Object -ComObject WScript.Shell; $Shortcut = $WshShell.CreateShortcut([Environment]::GetFolderPath('StartMenu') + '\Programs\Zapret Tools.lnk'); $Shortcut.TargetPath = '{}'; $Shortcut.Save()"#,
        exe_path.to_string_lossy()
    );
    let mut command = Command::new("powershell");
    hide_console(&mut command);
    command
        .args(["-NoProfile", "-Command", &ps_script])
        .status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Порты Game Filter по умолчанию — те же, что в service.bat сборки Flowseal.
const GAME_FILTER_DEFAULT_PORTS: &str = "1024-65535";

fn game_filter_defaults() -> (String, String, String) {
    (
        "all".to_string(),
        GAME_FILTER_DEFAULT_PORTS.to_string(),
        GAME_FILTER_DEFAULT_PORTS.to_string(),
    )
}

/// Проверяет диапазон портов в формате service.bat:
/// `1024-65535`, `443`, `80,443,1000-2000`.
///
/// Пустая строка и мусор не принимаются: service.bat читает этот файл
/// без проверок, и опечатка в портах ломает запуск службы.
fn validate_port_range(value: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Диапазон портов не может быть пустым.".to_string());
    }

    for part in trimmed.split(',') {
        let token = part.trim();
        if token.is_empty() {
            return Err(format!("Пустой элемент в списке портов: «{trimmed}»"));
        }
        let (start, end) = match token.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (token, token),
        };
        let start_port = parse_port(start, trimmed)?;
        let end_port = parse_port(end, trimmed)?;
        if start_port > end_port {
            return Err(format!("Начало диапазона больше конца: «{token}»"));
        }
    }
    Ok(())
}

fn parse_port(value: &str, context: &str) -> Result<u16, String> {
    // u16 сам отсекает всё больше 65535.
    let port: u16 = value
        .parse()
        .map_err(|_| format!("Некорректный порт «{value}» в «{context}»"))?;
    if port == 0 {
        return Err(format!("Порт 0 недопустим в «{context}»"));
    }
    Ok(port)
}

/// Читает `utils/game_filter.enabled` в формате service.bat:
/// ```text
/// mode=all
/// tcp=1024-65535
/// udp=1024-65535
/// ```
/// Возвращает (включён, режим, tcp, udp). Файла нет — фильтр выключен,
/// а в UI подставляются значения по умолчанию.
fn read_game_filter_config(version_path: &str) -> (bool, String, String, String) {
    let flag_file = Path::new(version_path).join("utils").join("game_filter.enabled");
    let (default_mode, default_tcp, default_udp) = game_filter_defaults();

    let Ok(content) = fs::read_to_string(&flag_file) else {
        return (false, default_mode, default_tcp, default_udp);
    };

    let mut mode = default_mode;
    let mut tcp = default_tcp;
    let mut udp = default_udp;

    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "mode" => mode = value.to_string(),
            "tcp" => tcp = value.to_string(),
            "udp" => udp = value.to_string(),
            _ => {}
        }
    }

    // Старая заглушка ("ENABLED") ключей не содержит: остаются значения
    // по умолчанию, но фильтр при этом считается включённым.
    (true, mode, tcp, udp)
}

#[tauri::command]
fn toggle_game_filter(
    version_path: String,
    enabled: bool,
    mode: Option<String>,
    tcp_range: Option<String>,
    udp_range: Option<String>,
) -> Result<(), String> {
    let utils_dir = Path::new(&version_path).join("utils");
    let flag_file = utils_dir.join("game_filter.enabled");

    // Выключение = удалить файл: без него service.bat фильтр не применяет.
    if !enabled {
        if flag_file.exists() {
            fs::remove_file(&flag_file).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }

    // Аргументы не передали (старый вызов) — берём то, что уже в файле.
    let (_, file_mode, file_tcp, file_udp) = read_game_filter_config(&version_path);
    let mode = mode.unwrap_or(file_mode).trim().to_ascii_lowercase();
    let mode = match mode.as_str() {
        "all" | "tcp" | "udp" => mode,
        other => {
            return Err(format!(
                "Неизвестный режим фильтра: «{other}». Допустимо: all, tcp, udp."
            ))
        }
    };
    let tcp_range = tcp_range.unwrap_or(file_tcp);
    let udp_range = udp_range.unwrap_or(file_udp);

    validate_port_range(&tcp_range)?;
    validate_port_range(&udp_range)?;

    fs::create_dir_all(&utils_dir).map_err(|e| e.to_string())?;

    // Ровно тот текст, который разбирает service.bat.
    let content = format!(
        "mode={}\ntcp={}\nudp={}\n",
        mode,
        tcp_range.trim(),
        udp_range.trim()
    );
    fs::write(&flag_file, content).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_game_filter_status(version_path: String) -> bool {
    read_game_filter_config(&version_path).0
}

#[tauri::command]
fn get_game_filter_config(version_path: String) -> (bool, String, String, String) {
    read_game_filter_config(&version_path)
}

/// Заглушка «выключенного» IPSet из service.bat.
const IPSET_NONE_STUB: &str = "203.0.113.113/32";

/// Пути списка IP и его бэкапа. Бэкап нужен, чтобы вернуться из `none`/`any`
/// к рабочему списку: service.bat сам ничего не сохраняет.
fn ipset_paths(version_path: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let lists_dir = Path::new(version_path).join("lists");
    (
        lists_dir.join("ipset-all.txt"),
        lists_dir.join("ipset-all.txt.backup"),
    )
}

/// Состояние IPSet по содержимому `lists/ipset-all.txt`:
/// * `loaded` — нормальный список IP;
/// * `none` — только заглушка `203.0.113.113/32`;
/// * `any` — файл пустой (0 байт), service.bat не фильтрует ничего.
#[tauri::command]
fn get_ipset_state(version_path: String) -> String {
    let (list_file, _) = ipset_paths(&version_path);
    let Ok(content) = fs::read_to_string(&list_file) else {
        // Файла нет — фильтровать нечего.
        return "none".to_string();
    };

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return "any".to_string();
    }

    let mut lines = trimmed.lines().filter(|line| !line.trim().is_empty());
    let first = lines.next().unwrap_or("").trim();
    if first == IPSET_NONE_STUB && lines.next().is_none() {
        return "none".to_string();
    }

    "loaded".to_string()
}

#[tauri::command]
fn set_ipset_state(version_path: String, state: String) -> Result<(), String> {
    let lists_dir = Path::new(&version_path).join("lists");
    let (list_file, backup_file) = ipset_paths(&version_path);
    fs::create_dir_all(&lists_dir).map_err(|e| e.to_string())?;

    let current = get_ipset_state(version_path);
    let state = state.trim().to_ascii_lowercase();

    // Бэкап делаем один раз — при первом уходе из «загружен».
    // Перезаписывать его нельзя: иначе Any → None затрёт рабочий список
    // пустым файлом и вернуться к списку будет нечем.
    let save_backup = || -> Result<(), String> {
        if current == "loaded" && !backup_file.exists() && list_file.exists() {
            fs::copy(&list_file, &backup_file).map_err(|e| e.to_string())?;
        }
        Ok(())
    };

    match state.as_str() {
        "loaded" => {
            // Восстанавливаем только если список сейчас не в рабочем виде,
            // чтобы не затереть правки пользователя из бэкапа.
            if current != "loaded" && backup_file.exists() {
                if list_file.exists() {
                    let _ = fs::remove_file(&list_file);
                }
                fs::rename(&backup_file, &list_file).map_err(|e| e.to_string())?;
            }
        }
        "none" => {
            save_backup()?;
            fs::write(&list_file, format!("{IPSET_NONE_STUB}\n")).map_err(|e| e.to_string())?;
        }
        "any" => {
            save_backup()?;
            // Именно 0 байт: service.bat считает пустой список «все IP».
            fs::write(&list_file, "").map_err(|e| e.to_string())?;
        }
        other => {
            return Err(format!(
                "Неизвестное состояние IPSet: «{other}». Допустимо: loaded, none, any."
            ))
        }
    }

    Ok(())
}

/// Флаг `utils/check_updates.enabled` — проверка обновлений скриптов Zapret
/// в service.bat. Скрипт смотрит только на существование файла.
#[tauri::command]
fn toggle_zapret_update_check(version_path: String, enabled: bool) -> Result<(), String> {
    let utils_dir = Path::new(&version_path).join("utils");
    let flag_file = utils_dir.join("check_updates.enabled");

    if enabled {
        fs::create_dir_all(&utils_dir).map_err(|e| e.to_string())?;
        // Значение не читается, но непустой файл видно в проводнике.
        fs::write(&flag_file, "1\n").map_err(|e| e.to_string())
    } else {
        if flag_file.exists() {
            fs::remove_file(&flag_file).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

#[tauri::command]
fn get_zapret_update_check_status(version_path: String) -> bool {
    Path::new(&version_path)
        .join("utils")
        .join("check_updates.enabled")
        .exists()
}

#[tauri::command]
fn list_strategies(version_path: String) -> Result<Vec<String>, String> {
    let mut bats = Vec::new();
    if let Ok(entries) = fs::read_dir(version_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".bat") && !name.starts_with("service") { bats.push(name); }
        }
    }
    bats.sort();
    Ok(bats)
}

#[tauri::command]
fn install_strategy(version_path: String, strategy_name: String) -> Result<(), String> {
    let bats = list_strategies(version_path.clone())?;
    let index = bats.iter().position(|r| r == &strategy_name).map(|i| i + 1);
    if let Some(idx) = index {
        let bat_path = Path::new(&version_path).join("service.bat");
        let ps_cmd = format!("Start-Process cmd -ArgumentList '/c echo {} ^| \"\"{}\"\" admin' -Verb RunAs", idx, bat_path.to_string_lossy());
        let mut command = Command::new("powershell");
        hide_console(&mut command);
        command
            .args(["-NoProfile", "-Command", &ps_cmd])
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok(())
    } else { Err("Стратегия не найдена".into()) }
}

// --- СВИТЧЕР РАСКЛАДКИ ---

#[derive(Clone)]
pub struct SwitcherState {
    pub enabled: bool,
    /// Процессы, в которых свитчер не работает.
    pub exclusions: Vec<String>,
    /// Слова, которые не исправляются автоматически.
    pub word_exclusions: Vec<String>,
    /// Пользовательские словари RU/EN: пополняются из UI и тоста.
    pub custom_dict_ru: Vec<String>,
    pub custom_dict_en: Vec<String>,
    /// Слова, добавление которых пользователь отклонил.
    pub declined_words: Vec<String>,
    pub admin: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SwitcherConfig {
    pub enabled: bool,
    pub exclusions: Vec<String>,
    pub word_exclusions: Vec<String>,
    pub admin: bool,
    pub dictionaries_ready: bool,
}

/// Словари для проверки набранных слов.
pub struct Dictionaries {
    pub english: HashSet<String>,
    pub russian: HashSet<String>,
}

/// Словари вшиты в бинарник на этапе компиляции: внешние файлы и сеть не нужны.
const ENGLISH_DICT_TEXT: &str = include_str!("../../google-10000-english.txt");
const RUSSIAN_DICT_TEXT: &str = include_str!("../../100_000_russian_wordlist.txt");

// Словари загружаются один раз при старте. None — загрузить не удалось.
static DICTIONARIES: OnceLock<Option<Dictionaries>> = OnceLock::new();

// Защита от повторного запуска фонового потока rdev::listen.
static SWITCHER_STARTED: AtomicBool = AtomicBool::new(false);

fn lock_switcher(state: &Arc<Mutex<SwitcherState>>) -> std::sync::MutexGuard<'_, SwitcherState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn dictionaries_ready() -> bool {
    matches!(DICTIONARIES.get(), Some(Some(_)))
}

#[tauri::command]
fn update_switcher_config(state: tauri::State<Arc<Mutex<SwitcherState>>>, enabled: bool, exclusions: Vec<String>) {
    let mut s = lock_switcher(&state);
    s.enabled = enabled;
    s.exclusions = exclusions;
}

/// Добавляет слово в список «не исправлять». Возвращает обновлённый список.
#[tauri::command]
fn add_switcher_word_exclusion(
    state: tauri::State<Arc<Mutex<SwitcherState>>>,
    word: String,
) -> Vec<String> {
    let word = word.trim().to_string();
    let mut s = lock_switcher(&state);
    if !word.is_empty() {
        let normalized = normalize_for_lookup(&word);
        let exists = s
            .word_exclusions
            .iter()
            .any(|w| normalize_for_lookup(w) == normalized);
        if !exists {
            s.word_exclusions.push(word);
        }
    }
    s.word_exclusions.clone()
}

/// Удаляет слово из списка «не исправлять». Возвращает обновлённый список.
#[tauri::command]
fn remove_switcher_word_exclusion(
    state: tauri::State<Arc<Mutex<SwitcherState>>>,
    word: String,
) -> Vec<String> {
    let normalized = normalize_for_lookup(&word);
    let mut s = lock_switcher(&state);
    s.word_exclusions
        .retain(|w| normalize_for_lookup(w) != normalized);
    s.word_exclusions.clone()
}

/// Пользовательский словарь и список отклонённых предложений для UI.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct CustomWordsState {
    pub ru: Vec<String>,
    pub en: Vec<String>,
    pub declined: Vec<String>,
}

const CUSTOM_WORD_LIMIT: usize = 500;

/// Сравнение без учёта регистра и «ё»/«е» — как в словарях.
fn list_contains_normalized(list: &[String], normalized: &str) -> bool {
    list.iter()
        .any(|item| normalize_for_lookup(item) == normalized)
}

fn snapshot_custom_words(s: &SwitcherState) -> CustomWordsState {
    CustomWordsState {
        ru: s.custom_dict_ru.clone(),
        en: s.custom_dict_en.clone(),
        declined: s.declined_words.clone(),
    }
}

#[tauri::command]
fn get_custom_words(state: tauri::State<Arc<Mutex<SwitcherState>>>) -> CustomWordsState {
    snapshot_custom_words(&lock_switcher(&state))
}

/// Добавляет слово в пользовательский словарь `ru` или `en`.
#[tauri::command]
fn add_custom_word(
    state: tauri::State<Arc<Mutex<SwitcherState>>>,
    word: String,
    language: String,
) -> Result<CustomWordsState, String> {
    let word = word.trim().to_string();
    if word.is_empty() {
        return Err("Слово не может быть пустым".into());
    }
    if word.chars().count() > 64 {
        return Err("Слово длиннее 64 символов".into());
    }
    let normalized = normalize_for_lookup(&word);
    let lang = language.trim().to_lowercase();
    let mut s = lock_switcher(&state);
    let list = match lang.as_str() {
        "ru" => &mut s.custom_dict_ru,
        "en" => &mut s.custom_dict_en,
        _ => return Err(format!("Неизвестный язык словаря: {language}")),
    };
    if !list_contains_normalized(list, &normalized) {
        list.push(word);
        if list.len() > CUSTOM_WORD_LIMIT {
            let extra = list.len() - CUSTOM_WORD_LIMIT;
            list.drain(0..extra);
        }
    }
    // Если слово когда-то отклоняли, ручное добавление отменяет отказ.
    s.declined_words
        .retain(|w| normalize_for_lookup(w) != normalized);
    Ok(snapshot_custom_words(&s))
}

#[tauri::command]
fn remove_custom_word(
    state: tauri::State<Arc<Mutex<SwitcherState>>>,
    word: String,
    language: String,
) -> Result<CustomWordsState, String> {
    let normalized = normalize_for_lookup(&word);
    let lang = language.trim().to_lowercase();
    let mut s = lock_switcher(&state);
    let list = match lang.as_str() {
        "ru" => &mut s.custom_dict_ru,
        "en" => &mut s.custom_dict_en,
        _ => return Err(format!("Неизвестный язык словаря: {language}")),
    };
    list.retain(|w| normalize_for_lookup(w) != normalized);
    Ok(snapshot_custom_words(&s))
}

/// Запоминает отказ: слово больше не предлагается для словаря.
#[tauri::command]
fn decline_custom_word(
    state: tauri::State<Arc<Mutex<SwitcherState>>>,
    word: String,
) -> Vec<String> {
    let word = word.trim().to_string();
    if word.is_empty() {
        return lock_switcher(&state).declined_words.clone();
    }
    let normalized = normalize_for_lookup(&word);
    let mut s = lock_switcher(&state);
    if !list_contains_normalized(&s.declined_words, &normalized) {
        s.declined_words.push(word);
        if s.declined_words.len() > CUSTOM_WORD_LIMIT {
            let extra = s.declined_words.len() - CUSTOM_WORD_LIMIT;
            s.declined_words.drain(0..extra);
        }
    }
    s.declined_words.clone()
}

#[tauri::command]
fn clear_declined_words(state: tauri::State<Arc<Mutex<SwitcherState>>>) -> Vec<String> {
    let mut s = lock_switcher(&state);
    s.declined_words.clear();
    Vec::new()
}

#[tauri::command]
fn get_switcher_config(state: tauri::State<Arc<Mutex<SwitcherState>>>) -> SwitcherConfig {
    let (enabled, exclusions, word_exclusions, admin) = {
        let s = lock_switcher(&state);
        (
            s.enabled,
            s.exclusions.clone(),
            s.word_exclusions.clone(),
            s.admin,
        )
    };
    SwitcherConfig {
        enabled,
        exclusions,
        word_exclusions,
        admin,
        dictionaries_ready: dictionaries_ready(),
    }
}

#[tauri::command]
fn start_switcher_listener(
    state: tauri::State<Arc<Mutex<SwitcherState>>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    start_switcher_thread(Arc::clone(&state), app);
    Ok(())
}

/// Список .exe для выбора исключений в UI: только приложения, у которых есть
/// видимое окно с заголовком. Фоновые службы (svchost.exe, conhost.exe и т.п.)
/// отсеиваются, поэтому список совпадает с тем, что видно на панели задач.
#[tauri::command]
async fn get_running_processes() -> Vec<String> {
    // EnumWindows обходит все окна сессии — не блокируем поток UI.
    match tokio::task::spawn_blocking(running_window_processes).await {
        Ok(names) => names,
        Err(err) => {
            // Паника в задаче не должна молча превращаться в пустой список.
            switcher_log(format!("не удалось перечислить процессы: {err}"));
            Vec::new()
        }
    }
}

#[cfg(windows)]
fn running_window_processes() -> Vec<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, MAX_PATH};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
    };

    /// Состояние обхода, которое передаётся колбэку через LPARAM.
    struct Collector {
        seen: HashSet<String>,
        names: Vec<String>,
    }

    unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam — указатель на Collector, живущий до конца EnumWindows.
        let collector = unsafe { &mut *(lparam.0 as *mut Collector) };

        // SAFETY: hwnd приходит из EnumWindows и валиден на время колбэка.
        unsafe {
            // Скрытые окна и окна без заголовка — служебные.
            if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) <= 0 {
                return BOOL(1);
            }

            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return BOOL(1);
            }

            // Процессы других пользователей/системные могут не открыться —
            // это нормально, просто пропускаем такое окно.
            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return BOOL(1);
            };

            let mut buffer = [0u16; MAX_PATH as usize];
            let mut size = buffer.len() as u32;
            let resolved = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut size,
            )
            .is_ok();
            let _ = CloseHandle(process);

            if !resolved || size == 0 {
                return BOOL(1);
            }

            // Длину, названную WinAPI, зажимаем в размер буфера: защита от
            // рассинхронизации in/out-параметра стоит одного сравнения.
            let capacity = (size as usize).min(buffer.len());
            let path = String::from_utf16_lossy(&buffer[..capacity]);
            let Some(name) = Path::new(&path).file_name().and_then(|n| n.to_str()) else {
                return BOOL(1);
            };
            let lower = name.to_lowercase();
            if lower.ends_with(".exe") && collector.seen.insert(lower) {
                collector.names.push(name.to_string());
            }
        }

        // Продолжаем обход.
        BOOL(1)
    }

    let mut collector = Collector {
        seen: HashSet::new(),
        names: Vec::new(),
    };
    // SAFETY: колбэк и Collector живут до возврата EnumWindows.
    unsafe {
        let _ = EnumWindows(
            Some(collect_window),
            LPARAM(&mut collector as *mut Collector as isize),
        );
    }

    collector.names.sort_by_key(|name| name.to_lowercase());
    collector.names
}

#[cfg(not(windows))]
fn running_window_processes() -> Vec<String> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);

    let mut seen: HashSet<String> = HashSet::new();
    let mut result = Vec::new();
    for process in system.processes().values() {
        let name = process.name().to_string_lossy();
        let lower = name.to_lowercase();
        if !lower.ends_with(".exe") {
            continue;
        }
        if !seen.insert(lower) {
            continue;
        }
        result.push(name.to_string());
    }

    result.sort_by_key(|name| name.to_lowercase());
    result
}

fn start_switcher_thread(shared: Arc<Mutex<SwitcherState>>, app: tauri::AppHandle) {
    if SWITCHER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // rdev::listen блокирует поток, поэтому выносим хук в отдельный thread.
    std::thread::spawn(move || {
        // Флаг снимается при любом выходе из потока — и по ошибке listen,
        // и по панике. Без этого после сбоя перехвата кнопка «Старт»
        // молча ничего не делала до перезапуска приложения.
        let _started = SwitcherStartedGuard;
        run_switcher_listener(shared, app);
    });
}

/// Снимает `SWITCHER_STARTED` при разрушении: выход из потока хука.
struct SwitcherStartedGuard;

impl Drop for SwitcherStartedGuard {
    fn drop(&mut self) {
        SWITCHER_STARTED.store(false, Ordering::SeqCst);
    }
}

fn run_switcher_listener(shared: Arc<Mutex<SwitcherState>>, app: tauri::AppHandle) {
    use rdev::{EventType, Key};

    let suppress_until = Arc::new(AtomicU64::new(0));

    // Буфер слова и состояние модификаторов живут прямо в замыкании:
    // rdev::listen принимает FnMut, поэтому мьютекс здесь не нужен.
    let mut word = String::new();
    let mut word_russian = false;
    let mut shift_down = false;
    let mut ctrl_down = false;
    let mut alt_down = false;
    let mut meta_down = false;
    // CapsLock учитывается только при выборе регистра буквы, но не как
    // триггер перевода: свитчер реагирует на слова и Double Shift.
    let mut caps_lock = false;

    // Последнее завершённое слово (после пробела) — для Double Shift.
    let mut last_word: Option<String> = None;
    let mut last_word_russian = false;
    let mut last_word_followed_by_space = false;

    // Double Shift: два нажатия Shift подряд (до 400 мс) без других клавиш.
    let mut last_shift_press_ms: u64 = 0;
    let mut shift_break = false;

    // Двойной Backspace после автозамены — сигнал «исправлено неверно».
    let mut backspace_down = false;
    let mut last_backspace_ms: u64 = 0;

    let suppress_cb = Arc::clone(&suppress_until);
    let shared_cb = Arc::clone(&shared);
    // Клон для замыкания: сам `app` нужен после rdev::listen для события об ошибке.
    let app_cb = app.clone();

    let result = rdev::listen(move |event| {
        let suppressed = now_ms() < suppress_cb.load(Ordering::SeqCst);

        match event.event_type {
            EventType::KeyPress(key) => {
                // Модификаторы обновляем даже во время подавления, иначе после
                // инжекции клавиш состояние может «залипнуть».
                match key {
                    Key::ShiftLeft | Key::ShiftRight => {
                        // Автоповтор зажатого Shift не должен считаться вторым нажатием.
                        let repeat = shift_down;
                        shift_down = true;
                        if !repeat {
                            let broke_sequence = shift_break;
                            shift_break = false;
                            if !suppressed {
                                let now = now_ms();
                                let is_double = is_double_shift(
                                    last_shift_press_ms,
                                    now,
                                    broke_sequence,
                                    ctrl_down,
                                    alt_down,
                                    meta_down,
                                );
                                if is_double {
                                    last_shift_press_ms = 0;
                                    trigger_double_shift(
                                        &mut word,
                                        word_russian,
                                        &mut last_word,
                                        &mut last_word_russian,
                                        &mut last_word_followed_by_space,
                                        &shared_cb,
                                        &suppress_cb,
                                        &app_cb,
                                    );
                                } else {
                                    last_shift_press_ms = now;
                                }
                            }
                        }
                        return;
                    }
                    Key::ControlLeft | Key::ControlRight => {
                        ctrl_down = true;
                        if !suppressed {
                            last_word = None;
                            last_word_followed_by_space = false;
                        }
                        return;
                    }
                    Key::Alt | Key::AltGr => {
                        alt_down = true;
                        if !suppressed {
                            last_word = None;
                            last_word_followed_by_space = false;
                        }
                        return;
                    }
                    Key::MetaLeft | Key::MetaRight => {
                        meta_down = true;
                        if !suppressed {
                            last_word = None;
                            last_word_followed_by_space = false;
                        }
                        return;
                    }
                    Key::CapsLock => {
                        caps_lock = !caps_lock;
                        return;
                    }
                    _ => {
                        // Любая другая клавиша разрывает последовательность Double Shift.
                        shift_break = true;
                        // Инжектированные свитчером клавиши не должны сбрасывать
                        // ожидающее предложение об исключении.
                        if !suppressed && key != Key::Backspace {
                            last_backspace_ms = 0;
                            clear_correction();
                        }
                    }
                }

                if suppressed {
                    return;
                }

                // С зажатыми Ctrl/Alt/Meta текст не набирается — сбрасываем буфер.
                if ctrl_down || alt_down || meta_down {
                    word.clear();
                    last_backspace_ms = 0;
                    return;
                }

                if key == Key::Backspace {
                    let repeat = backspace_down;
                    backspace_down = true;

                    // Два Backspace подряд сразу после автозамены — пользователь
                    // отменяет правку. Предлагаем запомнить слово.
                    if !repeat {
                        let now = now_ms();
                        let double = last_backspace_ms != 0
                            && now.saturating_sub(last_backspace_ms) <= 600;
                        if double {
                            last_backspace_ms = 0;
                            suggest_word_exclusion(&app_cb);
                        } else {
                            last_backspace_ms = now;
                        }
                    }

                    if word.pop().is_none() {
                        // Буфер пуст: Backspace удаляет пробел или само
                        // последнее слово — поддерживаем last_word в актуальном виде.
                        if last_word_followed_by_space {
                            last_word_followed_by_space = false;
                        } else if let Some(text) = last_word.as_mut() {
                            text.pop();
                            if text.is_empty() {
                                last_word = None;
                            }
                        }
                    }
                    return;
                }

                if key == Key::Space {
                    if !word.is_empty() {
                        let typed = std::mem::take(&mut word);
                        let typed_russian = word_russian;
                        let corrected =
                            handle_finished_word(&typed, typed_russian, &shared_cb, &suppress_cb, &app_cb);
                        let final_text = corrected.clone().unwrap_or(typed);
                        last_word = Some(final_text);
                        last_word_russian = if corrected.is_some() {
                            !typed_russian
                        } else {
                            typed_russian
                        };
                        last_word_followed_by_space = true;
                    } else {
                        // Пустой буфер: курсор уже не вплотную к последнему слову.
                        last_word = None;
                        last_word_followed_by_space = false;
                    }
                    return;
                }

                if key == Key::Return || key == Key::KpReturn {
                    // Enter не исправляем: он уже мог отправить сообщение
                    // (чат, поиск), и «перепечатывать» слово после него опасно.
                    word.clear();
                    last_word = None;
                    last_word_followed_by_space = false;
                    return;
                }

                if key == Key::Tab {
                    word.clear();
                    last_word = None;
                    last_word_followed_by_space = false;
                    return;
                }

                let russian = active_layout_is_russian();
                match key_to_char(key, shift_down, caps_lock, russian) {
                    Some(ch) => {
                        if word.is_empty() {
                            word_russian = russian;
                            // Началось новое слово — предыдущее уже не под курсором.
                            last_word = None;
                            last_word_followed_by_space = false;
                        }
                        if word.chars().count() < 64 {
                            word.push(ch);
                        }
                    }
                    None => {
                        // Цифры, пунктуация и стрелки разрывают слово.
                        word.clear();
                        last_word = None;
                        last_word_followed_by_space = false;
                    }
                }
            }
            EventType::ButtonPress(_) => {
                // Клик меняет позицию курсора и снимает выделение:
                // текущее слово и Double Shift больше не актуальны.
                shift_break = true;
                word.clear();
                last_word = None;
                last_word_followed_by_space = false;
                last_shift_press_ms = 0;
                last_backspace_ms = 0;
                clear_correction();
            }
            EventType::Wheel { .. } => {
                // Колесо между двумя Shift отменяет Double Shift.
                shift_break = true;
            }
            EventType::KeyRelease(key) => match key {
                Key::ShiftLeft | Key::ShiftRight => shift_down = false,
                Key::ControlLeft | Key::ControlRight => ctrl_down = false,
                Key::Alt | Key::AltGr => alt_down = false,
                Key::MetaLeft | Key::MetaRight => meta_down = false,
                Key::Backspace => backspace_down = false,
                _ => {}
            },
            _ => {}
        }
    });

    if let Err(err) = result {
        use tauri::Emitter;
        switcher_log(format!("не удалось запустить rdev::listen: {err:?}"));
        let _ = app.emit(
            "switcher-error",
            "Не удалось запустить глобальный перехват клавиатуры. Свитчер не будет работать.".to_string(),
        );
    }
}

/// Проверяет завершённое слово и запускает исправление.
/// Возвращает текст, который окажется в документе после исправления
/// (нужен для отслеживания последнего слова / Double Shift).
fn handle_finished_word(
    word: &str,
    word_russian: bool,
    shared: &Arc<Mutex<SwitcherState>>,
    suppress_until: &Arc<AtomicU64>,
    app: &tauri::AppHandle,
) -> Option<String> {
    let (enabled, exclusions, word_exclusions, custom_ru, custom_en) = {
        let s = lock_switcher(shared);
        (
            s.enabled,
            s.exclusions.clone(),
            s.word_exclusions.clone(),
            s.custom_dict_ru.clone(),
            s.custom_dict_en.clone(),
        )
    };
    if !enabled {
        return None;
    }

    // Исключения: не вмешиваемся в работу выбранных программ.
    let active_process = get_active_process_name();
    if let Some(active) = active_process.as_deref() {
        if exclusions.iter().any(|ex| exclusion_matches(active, ex)) {
            return None;
        }
    }

    // Слова из пользовательского списка не исправляем автоматически.
    let normalized_word = normalize_for_lookup(word);
    if word_exclusions
        .iter()
        .any(|w| normalize_for_lookup(w) == normalized_word)
    {
        return None;
    }

    let dicts = DICTIONARIES.get().and_then(|d| d.as_ref())?;
    let fix = detect_layout_mistake(word, word_russian, dicts, &custom_ru, &custom_en)?;

    // Исключением может быть как набранное слово, так и результат замены:
    // пользователь видит в документе именно исправленное слово.
    let corrected_normalized = normalize_for_lookup(&fix.corrected);
    if word_exclusions
        .iter()
        .any(|w| normalize_for_lookup(w) == corrected_normalized)
    {
        return None;
    }

    // Замена должна менять раскладку. Если целевая раскладка уже включена,
    // значит при наборе раскладка была определена неверно (типично для
    // WebView2: раскладка host-потока расходится с потоком ввода) и
    // перепечатывание вернуло бы на экран тот же самый текст — «мигание».
    if fix.to_russian == active_layout_is_russian() {
        switcher_log(format!(
            "skip no-op: word='{}' word_russian={} corrected='{}' to_russian={} process='{}' | {}",
            word,
            word_russian,
            fix.corrected,
            fix.to_russian,
            active_process.as_deref().unwrap_or("?"),
            layout_diagnostics()
        ));
        return None;
    }

    switcher_log(format!(
        "fix: word='{}' -> '{}' word_russian={} to_russian={} process='{}' | {}",
        word,
        fix.corrected,
        word_russian,
        fix.to_russian,
        active_process.as_deref().unwrap_or("?"),
        layout_diagnostics()
    ));

    // Сразу подавляем собственные инжектированные клавиши, чтобы хук
    // не принял их за ввод пользователя и не испортил буфер.
    // 400 мс хватает на Backspace'ы и Ctrl+V; больше — и Double Shift,
    // нажатый сразу после исправления, начал бы «заедать».
    let my_suppress = suppress_for(suppress_until, 400);
    let suppress_worker = Arc::clone(suppress_until);
    let paste = format!("{} ", fix.corrected);
    let typed_len = fix.typed_len;
    let to_russian = fix.to_russian;
    let corrected = fix.corrected.clone();
    let typed_word = word.to_string();
    let app_worker = app.clone();
    std::thread::spawn(move || {
        let applied = replace_word(typed_len, &paste, true, to_russian);
        if applied {
            // Запоминаем исходное слово: по двойному Backspace предложим
            // добавить его в исключения.
            remember_correction(typed_word);
        } else {
            notify_clipboard_failure(&app_worker);
        }
        // Короткий хвост, чтобы поглотить последние события SendInput.
        // Если вставка не удалась, инжекции не было — снимаем блокировку сразу.
        end_suppression(&suppress_worker, my_suppress, if applied { 150 } else { 0 });
    });
    Some(corrected)
}

/// Предложение добавить пару слов в пользовательский словарь.
/// Возвращает (исходное, переведённое, перевод_на_русский).
fn custom_word_suggestion(
    original: &str,
    translated: &str,
    source_russian: bool,
    dicts: &Dictionaries,
    state: &SwitcherState,
) -> Option<(String, String, bool)> {
    let original = original.trim();
    let translated = translated.trim();
    if original.chars().count() < 3 || translated.chars().count() < 4 {
        return None;
    }

    let original_norm = normalize_for_lookup(original);
    let translated_norm = normalize_for_lookup(translated);
    let (source_base, target_base, source_custom, target_custom) = if source_russian {
        (
            &dicts.russian,
            &dicts.english,
            &state.custom_dict_ru,
            &state.custom_dict_en,
        )
    } else {
        (
            &dicts.english,
            &dicts.russian,
            &state.custom_dict_en,
            &state.custom_dict_ru,
        )
    };

    // Обычное слово исходного языка пользователь мог перевести намеренно.
    if source_base.contains(&original_norm) || list_contains_normalized(source_custom, &original_norm)
    {
        return None;
    }
    if target_base.contains(&translated_norm)
        || list_contains_normalized(target_custom, &translated_norm)
    {
        return None;
    }
    if list_contains_normalized(&state.declined_words, &original_norm)
        || list_contains_normalized(&state.declined_words, &translated_norm)
    {
        return None;
    }

    Some((original.to_string(), translated.to_string(), !source_russian))
}

/// Окно между нажатиями Shift для Double Shift.
const DOUBLE_SHIFT_WINDOW_MS: u64 = 400;

/// Считается ли текущее нажатие Shift вторым в Double Shift.
///
/// Условия отдельной функцией: их легко случайно сломать при правке хука,
/// а проверяются они таблицей тестов.
fn is_double_shift(
    last_press_ms: u64,
    now_ms: u64,
    broke_sequence: bool,
    ctrl_down: bool,
    alt_down: bool,
    meta_down: bool,
) -> bool {
    last_press_ms != 0
        && now_ms.saturating_sub(last_press_ms) <= DOUBLE_SHIFT_WINDOW_MS
        && !broke_sequence
        && !ctrl_down
        && !alt_down
        && !meta_down
}

/// Double Shift: конвертирует последнее набранное слово в другую раскладку.
fn trigger_double_shift(
    word: &mut String,
    word_russian: bool,
    last_word: &mut Option<String>,
    last_word_russian: &mut bool,
    last_word_followed_by_space: &mut bool,
    shared: &Arc<Mutex<SwitcherState>>,
    suppress_until: &Arc<AtomicU64>,
    app: &tauri::AppHandle,
) {
    let (enabled, exclusions) = {
        let s = lock_switcher(shared);
        (s.enabled, s.exclusions.clone())
    };
    if !enabled {
        return;
    }
    let active_process = get_active_process_name();
    if let Some(active) = active_process.as_deref() {
        if exclusions.iter().any(|ex| exclusion_matches(active, ex)) {
            return;
        }
    }

    let (text, source_russian, has_space) = if !word.is_empty() {
        (std::mem::take(word), word_russian, false)
    } else if let Some(text) = last_word.as_ref() {
        (text.clone(), *last_word_russian, *last_word_followed_by_space)
    } else {
        // Буфера нет (например, после Enter) — пробуем преобразовать выделение.
        // В терминалах Ctrl+C без выделения прерывает команду, поэтому туда не лезем.
        if active_process
            .as_deref()
            .is_some_and(is_terminal_process)
        {
            switcher_log("selection: терминал пропущен, Ctrl+C может прервать команду");
            return;
        }
        convert_selected_text(suppress_until);
        return;
    };

    // Минимальной длины здесь нет: Double Shift — ручной триггер,
    // пользователь сам решил перевести даже одиночный символ (`b` -> `и`).
    if text.is_empty() {
        return;
    }

    let converted = convert_layout(&text, !source_russian);
    if converted == text {
        return;
    }

    // Если переведённого слова нет в словарях, предложим запомнить пару.
    // Отклонённые слова сюда уже не попадут (проверка внутри helper'а).
    let suggestion = DICTIONARIES.get().and_then(|d| d.as_ref()).and_then(|dicts| {
        let s = lock_switcher(shared);
        custom_word_suggestion(&text, &converted, source_russian, dicts, &s)
    });

    let paste_text = if has_space {
        format!("{converted} ")
    } else {
        converted.clone()
    };
    let typed_len = text.chars().count();
    let to_russian = !source_russian;

    // Запоминаем результат: повторный Double Shift вернёт исходный вариант.
    *last_word = Some(converted);
    *last_word_russian = to_russian;
    *last_word_followed_by_space = has_space;

    let my_suppress = suppress_for(suppress_until, 400);
    let suppress_worker = Arc::clone(suppress_until);
    let app_worker = app.clone();
    std::thread::spawn(move || {
        let applied = replace_word(typed_len, &paste_text, has_space, to_russian);
        if !applied {
            notify_clipboard_failure(&app_worker);
        } else if let Some((original, translated, to_russian)) = suggestion {
            use tauri::Emitter;
            let _ = app_worker.emit(
                "switcher-suggest-custom-word",
                serde_json::json!({
                    "original": original,
                    "translated": translated,
                    "to_russian": to_russian,
                }),
            );
        }
        end_suppression(&suppress_worker, my_suppress, if applied { 150 } else { 0 });
    });
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Продлевает окно подавления и возвращает собственный дедлайн.
///
/// `suppress_until` читается из хука, а пишется из нескольких фоновых потоков.
/// Прямой `store` мог укоротить окно, выставленное более длинной операцией
/// (конвертация выделения держит 1,5 с): хук успевал принять нашу же
/// инъекцию за ввод пользователя и портил буфер слова.
fn suppress_for(until: &AtomicU64, ms: u64) -> u64 {
    let deadline = now_ms().saturating_add(ms);
    until.fetch_max(deadline, Ordering::SeqCst);
    deadline
}

/// Завершает «своё» окно подавления: `tail_ms` от текущего момента, 0 — снять.
///
/// Запись выполняется, только если дедлайн всё ещё наш: иначе его уже продлила
/// более новая операция, и обнуление открыло бы хук для её инъекций.
fn end_suppression(until: &AtomicU64, mine: u64, tail_ms: u64) {
    let target = if tail_ms == 0 {
        0
    } else {
        now_ms().saturating_add(tail_ms)
    };
    let _ = until.compare_exchange(mine, target, Ordering::SeqCst, Ordering::SeqCst);
}

/// Слово, которое только что было автоматически исправлено.
/// Хранится недолго: если пользователь сразу отменил правку двойным
/// Backspace, ему предлагается добавить слово в исключения.
struct PendingCorrection {
    typed: String,
    at_ms: u64,
}

static LAST_CORRECTION: OnceLock<Mutex<Option<PendingCorrection>>> = OnceLock::new();

fn last_correction() -> std::sync::MutexGuard<'static, Option<PendingCorrection>> {
    let slot = LAST_CORRECTION.get_or_init(|| Mutex::new(None));
    match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn remember_correction(typed: String) {
    *last_correction() = Some(PendingCorrection {
        typed,
        at_ms: now_ms(),
    });
}

fn clear_correction() {
    *last_correction() = None;
}

/// Забирает недавнее исправление (не старше 2,5 с). Повторно не отдаётся.
fn take_recent_correction() -> Option<String> {
    let mut slot = last_correction();
    match slot.as_ref() {
        Some(pending) if now_ms().saturating_sub(pending.at_ms) <= 2_500 => {
            slot.take().map(|pending| pending.typed)
        }
        Some(_) => {
            *slot = None;
            None
        }
        None => None,
    }
}

/// Предлагает пользователю добавить неверно исправленное слово в исключения.
fn suggest_word_exclusion(app: &tauri::AppHandle) {
    use tauri::Emitter;

    if let Some(word) = take_recent_correction() {
        let _ = app.emit("switcher-suggest-exclusion", word);
    }
}

/// Сообщает пользователю, что слово не удалось исправить.
/// Не чаще одного раза в минуту, чтобы не спамить тостами.
fn notify_clipboard_failure(app: &tauri::AppHandle) {
    use tauri::Emitter;

    static LAST_WARNING_MS: AtomicU64 = AtomicU64::new(0);

    let now = now_ms();
    if now.saturating_sub(LAST_WARNING_MS.load(Ordering::SeqCst)) < 60_000 {
        return;
    }
    LAST_WARNING_MS.store(now, Ordering::SeqCst);

    let _ = app.emit(
        "switcher-error",
        "Не удалось получить доступ к буферу обмена — исправление слова пропущено.".to_string(),
    );
}

/// Найденное исправление раскладки.
struct Correction {
    typed_len: usize,
    corrected: String,
    to_russian: bool,
}

fn normalize_for_lookup(word: &str) -> String {
    // «ё» и «е» считаем одной буквой при проверке по словарю.
    word.trim().to_lowercase().replace('ё', "е")
}

fn is_cyrillic_word(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| matches!(c, 'а'..='я' | 'ё' | 'А'..='Я' | 'Ё'))
}

/// Знаки, которые в английской раскладке могут оказаться частью слова:
/// в ЙЦУКЕН на тех же клавишах стоят буквы или другие знаки
/// (`.` → `ю`, `,` → `б`, `&` → `?`, `@` → `"` и т.д.).
/// Пока такой символ не встретился, буфер слова не очищается, поэтому
/// `.,rf` и `ghbdtn&` доживают до автозамены или Double Shift.
const LATIN_KEYBOARD_PUNCTUATION: [char; 27] = [
    '[', ']', ';', '\'', ',', '.', '`', '/', '&', '?', '@', '#', '$', '^', '*', '(', ')',
    '_', '+', '{', '}', '|', '<', '>', '~', ':', '"',
];

fn is_latin_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphabetic() || LATIN_KEYBOARD_PUNCTUATION.contains(&c))
}

/// Слово набрано не в той раскладке, если его нет в словаре текущего языка,
/// но его «перевод» по клавишам есть в словаре противоположного языка.
fn detect_layout_mistake(
    word: &str,
    current_is_russian: bool,
    dicts: &Dictionaries,
    custom_ru: &[String],
    custom_en: &[String],
) -> Option<Correction> {
    let word = word.trim();
    if word.chars().count() < 2 {
        return None;
    }

    if current_is_russian {
        if !is_cyrillic_word(word) {
            return None;
        }
    } else if !is_latin_word(word) {
        return None;
    }

    let normalized = normalize_for_lookup(word);
    let (current, opposite, custom_current, custom_opposite) = if current_is_russian {
        (&dicts.russian, &dicts.english, custom_ru, custom_en)
    } else {
        (&dicts.english, &dicts.russian, custom_en, custom_ru)
    };

    // Слово есть в текущем словаре (базовом или пользовательском) — не трогаем.
    if current.contains(&normalized) || list_contains_normalized(custom_current, &normalized) {
        return None;
    }

    let corrected = convert_layout(word, !current_is_russian);
    if corrected == word {
        return None;
    }
    let corrected_normalized = normalize_for_lookup(&corrected);
    let known_in_opposite = opposite.contains(&corrected_normalized)
        || list_contains_normalized(custom_opposite, &corrected_normalized);
    if corrected_normalized.chars().count() < 2 || !known_in_opposite {
        return None;
    }

    Some(Correction {
        typed_len: word.chars().count(),
        corrected,
        to_russian: !current_is_russian,
    })
}

/// Перевод слова по позициям клавиш: `ghbdtn` -> `привет` и обратно.
fn convert_layout(word: &str, to_russian: bool) -> String {
    if to_russian {
        word.chars().map(|c| en_char_to_ru(c).unwrap_or(c)).collect()
    } else {
        word.chars().map(|c| ru_char_to_en(c).unwrap_or(c)).collect()
    }
}

fn en_char_to_ru(c: char) -> Option<char> {
    // Буквы и знаки, которые не меняются от Shift: регистр сохраняем.
    let lower = c.to_ascii_lowercase();
    let letter = match lower {
        'q' => 'й', 'w' => 'ц', 'e' => 'у', 'r' => 'к', 't' => 'е', 'y' => 'н',
        'u' => 'г', 'i' => 'ш', 'o' => 'щ', 'p' => 'з', '[' => 'х', ']' => 'ъ',
        'a' => 'ф', 's' => 'ы', 'd' => 'в', 'f' => 'а', 'g' => 'п', 'h' => 'р',
        'j' => 'о', 'k' => 'л', 'l' => 'д', ';' => 'ж', '\'' => 'э',
        'z' => 'я', 'x' => 'ч', 'c' => 'с', 'v' => 'м', 'b' => 'и', 'n' => 'т',
        'm' => 'ь', ',' => 'б', '.' => 'ю', '`' => 'ё',
        _ => ' ',
    };
    if letter != ' ' {
        return Some(if c.is_ascii_uppercase() {
            letter.to_uppercase().next().unwrap_or(letter)
        } else {
            letter
        });
    }

    // Shift-варианты тех же клавиш: в ЙЦУКЕН они дают другие буквы или знаки.
    let mapped = match c {
        '{' => 'Х', '}' => 'Ъ', ':' => 'Ж', '"' => 'Э',
        '<' => 'Б', '>' => 'Ю', '~' => 'Ё',
        '@' => '"', '#' => '№', '$' => ';', '^' => ':', '&' => '?',
        '/' => '.', '?' => ',', '|' => '/',
        _ => return None,
    };
    Some(mapped)
}

fn ru_char_to_en(c: char) -> Option<char> {
    let lower = c.to_lowercase().next().unwrap_or(c);
    let letter = match lower {
        'й' => 'q', 'ц' => 'w', 'у' => 'e', 'к' => 'r', 'е' => 't', 'н' => 'y',
        'г' => 'u', 'ш' => 'i', 'щ' => 'o', 'з' => 'p', 'х' => '[', 'ъ' => ']',
        'ф' => 'a', 'ы' => 's', 'в' => 'd', 'а' => 'f', 'п' => 'g', 'р' => 'h',
        'о' => 'j', 'л' => 'k', 'д' => 'l', 'ж' => ';', 'э' => '\'',
        'я' => 'z', 'ч' => 'x', 'с' => 'c', 'м' => 'v', 'и' => 'b', 'т' => 'n',
        'ь' => 'm', 'б' => ',', 'ю' => '.', 'ё' => '`',
        _ => ' ',
    };
    if letter != ' ' {
        if c.is_uppercase() {
            // Shift для этих букв даёт знак, а не заглавную: Х → `{`, Б → `<`.
            return Some(match lower {
                'х' => '{', 'ъ' => '}', 'ж' => ':', 'э' => '"',
                'б' => '<', 'ю' => '>', 'ё' => '~',
                _ => letter.to_ascii_uppercase(),
            });
        }
        return Some(letter);
    }

    // Знаки, которые в ЙЦУКЕН стоят на Shift-клавишах.
    let mapped = match c {
        '№' => '#', ';' => '$', ':' => '^', '?' => '&', '"' => '@',
        '/' => '|', '.' => '/', ',' => '?',
        _ => return None,
    };
    Some(mapped)
}

/// Физическая клавиша rdev -> символ с учётом активной раскладки.
/// Возвращает None для цифр без Shift, клавиш без текста и служебных клавиш.
fn key_to_char(key: rdev::Key, shift: bool, caps_lock: bool, russian: bool) -> Option<char> {
    use rdev::Key;

    // Буквы: физическая клавиша -> строчная латинская.
    let letter = match key {
        Key::KeyQ => Some('q'), Key::KeyW => Some('w'), Key::KeyE => Some('e'),
        Key::KeyR => Some('r'), Key::KeyT => Some('t'), Key::KeyY => Some('y'),
        Key::KeyU => Some('u'), Key::KeyI => Some('i'), Key::KeyO => Some('o'),
        Key::KeyP => Some('p'), Key::KeyA => Some('a'), Key::KeyS => Some('s'),
        Key::KeyD => Some('d'), Key::KeyF => Some('f'), Key::KeyG => Some('g'),
        Key::KeyH => Some('h'), Key::KeyJ => Some('j'), Key::KeyK => Some('k'),
        Key::KeyL => Some('l'), Key::KeyZ => Some('z'), Key::KeyX => Some('x'),
        Key::KeyC => Some('c'), Key::KeyV => Some('v'), Key::KeyB => Some('b'),
        Key::KeyN => Some('n'), Key::KeyM => Some('m'),
        _ => None,
    };

    // US QWERTY: (без Shift, со Shift). None — клавиша не даёт символа слова.
    let en = if let Some(lower) = letter {
        // CapsLock влияет только на буквы.
        if shift ^ caps_lock {
            lower.to_ascii_uppercase()
        } else {
            lower
        }
    } else {
        let (plain, shifted) = match key {
            Key::LeftBracket => (Some('['), Some('{')),
            Key::RightBracket => (Some(']'), Some('}')),
            Key::SemiColon => (Some(';'), Some(':')),
            Key::Quote => (Some('\''), Some('"')),
            Key::Comma => (Some(','), Some('<')),
            Key::Dot => (Some('.'), Some('>')),
            Key::BackQuote => (Some('`'), Some('~')),
            Key::Slash => (Some('/'), Some('?')),
            Key::BackSlash => (None, Some('|')),
            // Цифры слово разрывают, но их Shift-варианты — реальные символы.
            Key::Num2 => (None, Some('@')),
            Key::Num3 => (None, Some('#')),
            Key::Num4 => (None, Some('$')),
            Key::Num6 => (None, Some('^')),
            Key::Num7 => (None, Some('&')),
            Key::Num8 => (None, Some('*')),
            Key::Num9 => (None, Some('(')),
            Key::Num0 => (None, Some(')')),
            Key::Minus => (None, Some('_')),
            Key::Equal => (None, Some('+')),
            _ => (None, None),
        };
        (if shift { shifted } else { plain })?
    };

    // Переводим по физической позиции клавиши в текущую раскладку.
    let ch = if russian {
        en_char_to_ru(en).unwrap_or(en)
    } else {
        en
    };

    // Буквы всегда часть слова. В английской раскладке к ним добавляются
    // знаки, которые в ЙЦУКЕН стоят на тех же клавишах (`.,rf` → `юбка`,
    // `ghbdtn&` → `привет?`). В русской раскладке знак — настоящий
    // разделитель, поэтому его в слово не пускаем.
    let keep = ch.is_alphabetic() || (!russian && LATIN_KEYBOARD_PUNCTUATION.contains(&ch));
    if keep {
        Some(ch)
    } else {
        None
    }
}

/// Поток foreground-окна и поток, чья раскладка определяет вводимые символы.
///
/// В обычных Win32-приложениях это один и тот же поток. В WebView2/Electron/CEF
/// (Ollama, Tauri, VS Code и т.п.) текст набирается в отдельном процессе
/// браузерного движка: у окна приложения раскладка может быть «своя», а
/// клавиатурный фокус принадлежит другому потоку. Раскладку нужно спрашивать
/// у потока, который владеет окном с фокусом, — именно он превращает нажатия
/// в символы.
#[cfg(windows)]
fn foreground_layout_threads() -> Option<(u32, u32)> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };

    // SAFETY: GetForegroundWindow/GetGUIThreadInfo только читают состояние
    // рабочего стола; `info` инициализирован cbSize и живёт на стеке.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let host_tid = GetWindowThreadProcessId(hwnd, None);
        if host_tid == 0 {
            return None;
        }

        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(host_tid, &mut info).is_ok() && !info.hwndFocus.is_invalid() {
            let focus_tid = GetWindowThreadProcessId(info.hwndFocus, None);
            if focus_tid != 0 {
                return Some((host_tid, focus_tid));
            }
        }

        Some((host_tid, host_tid))
    }
}

#[cfg(windows)]
fn layout_is_russian(thread_id: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;

    // SAFETY: GetKeyboardLayout — чистый геттер по идентификатору потока,
    // указателей не принимает и владения ресурсом не требует.
    unsafe {
        let hkl = GetKeyboardLayout(thread_id);
        // Младшее слово HKL — идентификатор языка; 0x0419 — ru-RU.
        ((hkl.0 as usize) & 0xFFFF) == 0x0419
    }
}

#[cfg(windows)]
fn active_layout_is_russian() -> bool {
    foreground_layout_threads().is_some_and(|(_, input_tid)| layout_is_russian(input_tid))
}

#[cfg(not(windows))]
fn active_layout_is_russian() -> bool {
    false
}

/// Раскладки потоков для лога: видно, расходятся ли «раскладка окна»
/// и «раскладка потока ввода» (типично для WebView2-приложений).
#[cfg(windows)]
fn layout_diagnostics() -> String {
    match foreground_layout_threads() {
        Some((host_tid, input_tid)) => format!(
            "host_tid={host_tid} host_ru={} input_tid={input_tid} input_ru={}",
            layout_is_russian(host_tid),
            layout_is_russian(input_tid)
        ),
        None => "foreground=<none>".to_string(),
    }
}

#[cfg(not(windows))]
fn layout_diagnostics() -> String {
    String::new()
}

/// Переключает раскладку активного окна без «слепого» тумблера.
#[cfg(windows)]
fn ensure_keyboard_layout(to_russian: bool) {
    use windows::core::w;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{LoadKeyboardLayoutW, KLF_ACTIVATE};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, PostMessageW, WM_INPUTLANGCHANGEREQUEST,
    };

    if active_layout_is_russian() == to_russian {
        return;
    }

    let layout_id = if to_russian { w!("00000419") } else { w!("00000409") };
    // SAFETY: `layout_id` — статическая строка нужной длины; HKL принадлежит
    // системе (освобождать его нельзя), PostMessageW не блокирует поток.
    unsafe {
        if let Ok(hkl) = LoadKeyboardLayoutW(layout_id, KLF_ACTIVATE) {
            let hwnd = GetForegroundWindow();
            if !hwnd.is_invalid()
                && PostMessageW(
                    hwnd,
                    WM_INPUTLANGCHANGEREQUEST,
                    WPARAM(0),
                    LPARAM(hkl.0 as isize),
                )
                .is_ok()
            {
                std::thread::sleep(Duration::from_millis(60));
                if active_layout_is_russian() == to_russian {
                    return;
                }
            }
        }
    }

    // Фолбэк — системное сочетание Alt + Shift.
    simulate_layout_switch();
}

#[cfg(not(windows))]
fn ensure_keyboard_layout(_to_russian: bool) {}

// Модули, вынесенные из этого файла. network открыт для generate_handler!,
// потому что команды регистрируются по полному пути.
mod logs;
pub(crate) mod network;
mod win_cmd;

pub(crate) use logs::{install_panic_hook, switcher_log};
pub(crate) use win_cmd::{hide_console, ps_quote, run_elevated_script};

#[cfg(windows)]
mod clipboard;
/// Прежнее имя модуля: вызывающий код свитчера не пришлось переименовывать.
#[cfg(windows)]
use self::clipboard as win_clipboard;

/// Содержимое буфера обмена до подмены (нужно, чтобы вернуть его обратно).
#[cfg(windows)]
#[derive(Clone)]
struct SavedClipboard {
    /// Прежнее содержимое буфера; `Unavailable` — прочитать не удалось.
    previous: win_clipboard::ReadOutcome,
    /// Поколение буфера на момент, когда мы забрали его под себя.
    generation: u64,
}

/// Номер последней записи в буфер обмена.
///
/// При быстрой печати правки идут вплотную: поток предыдущей правки просыпается
/// через 250 мс и возвращает старый текст, затирая содержимое следующей правки.
/// Поколение позволяет восстановлению понять, что буфер уже принадлежит более
/// новой операции, и не трогать его.
#[cfg(windows)]
static CLIPBOARD_GENERATION: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
impl SavedClipboard {
    /// Забирает буфер под себя и запоминает поколение.
    fn claim(previous: win_clipboard::ReadOutcome) -> Self {
        Self {
            previous,
            generation: CLIPBOARD_GENERATION.fetch_add(1, Ordering::SeqCst) + 1,
        }
    }
}

/// Кладёт текст в буфер обмена и возвращает прежнее содержимое.
/// `None` — буфер занят другим приложением или недоступен.
#[cfg(windows)]
fn clipboard_put(text: &str) -> Option<SavedClipboard> {
    let previous = win_clipboard::read_text();
    if !win_clipboard::set_text(text) {
        switcher_log("clipboard: не удалось положить исправленный текст");
        return None;
    }
    Some(SavedClipboard::claim(previous))
}

/// Что делать с буфером после вставки исправленного слова.
#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
enum RestoreAction {
    /// Вернуть прежний текст.
    Put(String),
    /// Буфер был доказанно пуст — очистить, чтобы наше слово не осталось в нём.
    Clear,
    /// Содержимое неизвестно или это не текст: не трогать, иначе потеряем данные.
    Leave,
}

/// Таблица решений восстановления буфера, отдельно от WinAPI.
#[cfg(windows)]
fn restore_action(previous: &win_clipboard::ReadOutcome) -> RestoreAction {
    match previous {
        // Обычный путь: возвращаем то, что прочитали до подмены.
        win_clipboard::ReadOutcome::Text(text) => RestoreAction::Put(text.clone()),
        // Буфер был точно пуст (ни одного формата или пустой текст) — возвращаем
        // его в пустое состояние, иначе наше слово останется в нём навсегда.
        win_clipboard::ReadOutcome::Empty => RestoreAction::Clear,
        // Содержимое неизвестно или это не текст: буфер был занят, в нём лежала
        // картинка/файлы либо GlobalSize вернул 0 (транзитное состояние
        // Windows). Стирать неизвестное нельзя: восстановить его мы не сможем.
        win_clipboard::ReadOutcome::Unavailable => RestoreAction::Leave,
    }
}

/// Восстановление допустимо, только если буфер с тех пор никто не занимал.
#[cfg(windows)]
fn restore_is_current(saved_generation: u64, current_generation: u64) -> bool {
    saved_generation == current_generation
}

/// Возвращает прежнее содержимое буфера с задержкой: приложению нужно
/// успеть прочитать вставленный текст.
#[cfg(windows)]
fn restore_clipboard(saved: SavedClipboard) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        // Пока шла задержка, буфер мог занять более новый поток правки:
        // возврат старого текста испортил бы его вставку.
        if !restore_is_current(saved.generation, CLIPBOARD_GENERATION.load(Ordering::SeqCst)) {
            return;
        }
        match restore_action(&saved.previous) {
            RestoreAction::Put(text) => {
                if !win_clipboard::set_text(&text) {
                    switcher_log("clipboard: не удалось восстановить прежний текст");
                }
            }
            RestoreAction::Clear => {
                if !win_clipboard::clear() {
                    switcher_log("clipboard: не удалось очистить буфер");
                }
            }
            RestoreAction::Leave => {
                switcher_log("clipboard: прежнее содержимое не прочитано, буфер не трогаем");
            }
        }
    });
}

/// Модификатор для работы с буфером обмена через Insert.
#[cfg(windows)]
#[derive(Clone, Copy)]
enum HotkeyModifier {
    /// Ctrl+Insert — копирование выделения.
    Control,
    /// Shift+Insert — вставка.
    Shift,
}

#[cfg(windows)]
const VK_SHIFT_KEY: i32 = 0x10;
#[cfg(windows)]
const VK_CONTROL_KEY: i32 = 0x11;
/// VK_INSERT — общая клавиша для копирования и вставки.
#[cfg(windows)]
const VK_INSERT: u32 = 0x2D;
/// VK_C — только запасной вариант копирования.
#[cfg(windows)]
const VK_COPY_FALLBACK: u32 = 0x43;

/// Клавиша зажата прямо сейчас.
///
/// `GetAsyncKeyState` видит и физическое удержание, и нашу инжекцию.
/// Различать нужно из-за Double Shift: второй Shift остаётся в руках
/// пользователя, пока работает поток конвертации, и лишний `Release`
/// сломал бы регистр последующего ввода.
#[cfg(windows)]
fn key_is_held(vk: i32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    // SAFETY: GetAsyncKeyState только читает состояние клавиатуры.
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

/// Отправляет Ctrl+Insert или Shift+Insert.
///
/// Insert выбран вместо C/V из-за Double Shift: он срабатывает, пока второй
/// Shift ещё физически зажат, а программный keyup удерживаемой клавиши
/// Windows может проигнорировать. С Ctrl это давало Ctrl+Shift+C — в браузерах
/// DevTools. Ctrl+Insert от лишнего Shift в опасное сочетание не превращается,
/// а Shift+Insert зажатый Shift только помогает.
///
/// Модификатор, который уже зажат физически, не пережимается и не отпускается.
#[cfg(windows)]
fn send_hotkey(
    enigo: &mut enigo::Enigo,
    modifier: HotkeyModifier,
    vk: u32,
    label: &str,
) -> bool {
    use enigo::{Direction, Key, Keyboard};

    // Alt и Win с Insert дают посторонние команды (Alt+Insert, Win+Insert).
    // Отпускаем только реально зажатые: лишний keyup без keydown — мусорное
    // событие, которое может сбить состояние модификаторов в приложении.
    for (key, vk) in [(Key::Alt, 0x12), (Key::Meta, 0x5B)] {
        if key_is_held(vk) {
            let _ = enigo.key(key, Direction::Release);
        }
    }

    // Второй модификатор только мешает: Ctrl+Shift+Insert — уже не копирование.
    let (modifier_key, modifier_vk, opposite_key, opposite_vk) = match modifier {
        HotkeyModifier::Control => (Key::Control, VK_CONTROL_KEY, Key::Shift, VK_SHIFT_KEY),
        HotkeyModifier::Shift => (Key::Shift, VK_SHIFT_KEY, Key::Control, VK_CONTROL_KEY),
    };
    if key_is_held(opposite_vk) {
        // Для Ctrl+Insert это попытка снять физически зажатый Shift.
        // Windows может её проигнорировать (клавиша реально удерживается),
        // но Ctrl+Shift+Insert безобиден — DevTools он не открывает.
        let _ = enigo.key(opposite_key, Direction::Release);
        std::thread::sleep(Duration::from_millis(20));
    }

    let press_modifier = !key_is_held(modifier_vk);
    let mut ok = true;

    if press_modifier {
        if let Err(err) = enigo.key(modifier_key, Direction::Press) {
            switcher_log(format!("{label}: модификатор не нажался: {err}"));
            ok = false;
        } else {
            // Даём приложению зафиксировать модификатор.
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    if ok {
        if let Err(err) = enigo.key(Key::Other(vk), Direction::Click) {
            switcher_log(format!("{label}: клавиша {vk:#04X} не нажалась: {err}"));
            ok = false;
        }
    }

    if press_modifier {
        // Отпускаем в любом случае, даже если Insert не отправился.
        if let Err(err) = enigo.key(modifier_key, Direction::Release) {
            switcher_log(format!("{label}: модификатор не отпустился: {err}"));
            ok = false;
        }
    }

    ok
}

/// Ждёт, пока приложение положит в буфер скопированный текст (до 400 мс).
#[cfg(windows)]
fn read_clipboard_text() -> Option<String> {
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(20));
        if let Some(text) = win_clipboard::get_text() {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Вставляет через Shift+Insert.
///
/// Shift+Insert понимают Блокнот, браузеры, Проводник, Office, Windows Terminal,
/// cmd/pwsh и VS Code, и он не ломается, если Shift уже зажат (Double Shift).
#[cfg(windows)]
fn paste_clipboard(enigo: &mut enigo::Enigo) -> bool {
    send_hotkey(enigo, HotkeyModifier::Shift, VK_INSERT, "paste")
}

/// Определяет язык выделенного текста: `Some(true)` — русский,
/// `Some(false)` — латиница, `None` — смешанный/без букв.
fn detect_text_language(text: &str) -> Option<bool> {
    let mut has_cyrillic = false;
    let mut has_latin = false;
    let mut letters = 0usize;

    for c in text.chars().filter(|c| c.is_alphabetic()) {
        letters += 1;
        if matches!(c, 'а'..='я' | 'ё' | 'А'..='Я' | 'Ё') {
            has_cyrillic = true;
        } else if c.is_ascii_alphabetic() {
            has_latin = true;
        } else {
            return None;
        }
        if has_cyrillic && has_latin {
            return None;
        }
    }

    if letters == 0 {
        None
    } else {
        Some(has_cyrillic)
    }
}

/// Терминалы, в которых Ctrl+C без выделения прерывает команду:
/// конвертацию выделения там не запускаем.
fn is_terminal_process(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "windowsterminal.exe"
            | "openconsole.exe"
            | "conhost.exe"
            | "cmd.exe"
            | "powershell.exe"
            | "pwsh.exe"
            | "mintty.exe"
            | "alacritty.exe"
            | "wezterm-gui.exe"
            | "tabby.exe"
            | "hyper.exe"
    )
}

/// Конвертирует выделенный текст: Ctrl+C -> чтение буфера -> Ctrl+V.
///
/// Выделение при вставке заменяется само, поэтому Backspace'ы не нужны.
/// Прежнее содержимое буфера восстанавливается после вставки.
#[cfg(windows)]
fn convert_selection_blocking() -> bool {
    use enigo::{Enigo, Settings};

    // 1. Запоминаем буфер и очищаем его: Ctrl+C должен дать именно выделение,
    //    а не то, что лежало в буфере раньше.
    let original = win_clipboard::read_text();
    if !win_clipboard::clear() {
        switcher_log("selection: не удалось очистить буфер обмена");
        return false;
    }

    let saved = SavedClipboard::claim(original);
    let restore = || restore_clipboard(saved.clone());

    let Ok(mut enigo) = Enigo::new(&Settings::default()) else {
        switcher_log("selection: не удалось создать Enigo");
        restore();
        return false;
    };

    // 2. Копируем выделение. Сначала Ctrl+Insert: он не превращается
    //    в Ctrl+Shift+C, даже если второй Shift после Double Shift всё ещё
    //    физически зажат — именно это открывало DevTools в браузерах.
    let mut copied = None;
    if send_hotkey(&mut enigo, HotkeyModifier::Control, VK_INSERT, "copy") {
        copied = read_clipboard_text();
    }

    // Запасной вариант для приложений, где Ctrl+Insert не назначен.
    // Ctrl+C безопасен только при отпущенном Shift: иначе снова Ctrl+Shift+C,
    // поэтому при зажатом Shift запасной путь не пробуем.
    if copied.is_none() && !key_is_held(VK_SHIFT_KEY) {
        switcher_log("copy: Ctrl+Insert ничего не скопировал, пробуем Ctrl+C");
        if send_hotkey(&mut enigo, HotkeyModifier::Control, VK_COPY_FALLBACK, "copy") {
            copied = read_clipboard_text();
        }
    }

    let Some(selected) = copied else {
        // Выделения не было — ничего не делаем.
        drop(enigo);
        restore();
        return false;
    };

    let Some(source_russian) = detect_text_language(&selected) else {
        switcher_log("selection: текст смешанный или без букв, пропускаем");
        drop(enigo);
        restore();
        return false;
    };

    let converted = convert_layout(&selected, !source_russian);
    if converted == selected {
        drop(enigo);
        restore();
        return false;
    }

    if !win_clipboard::set_text(&converted) {
        switcher_log("selection: не удалось положить текст в буфер");
        drop(enigo);
        restore();
        return false;
    }

    // 3. Вставка поверх выделения заменяет его.
    let pasted = paste_clipboard(&mut enigo);
    drop(enigo);
    restore();
    pasted
}

#[cfg(not(windows))]
fn convert_selection_blocking() -> bool {
    false
}

/// Запускает конвертацию выделения в фоне, чтобы не блокировать хук.
fn convert_selected_text(suppress_until: &Arc<AtomicU64>) {
    // Копирование и вставка занимают заметно больше времени, чем замена слова,
    // поэтому подавляем события хука с запасом. Внутри есть свои sleep'ы.
    let my_suppress = suppress_for(suppress_until, 1_500);
    let suppress_worker = Arc::clone(suppress_until);
    std::thread::spawn(move || {
        let converted = convert_selection_blocking();
        if !converted {
            switcher_log("selection: выделение не преобразовано");
        }
        // Не оставляем подавление «висеть»: пользователь может печатать сразу.
        end_suppression(&suppress_worker, my_suppress, if converted { 150 } else { 0 });
    });
}

/// Стирает N символов (и, при необходимости, пробел), переключает раскладку
/// и вставляет новый текст через буфер обмена.
///
/// Возвращает `false`, если исправление не применено: буфер обмена занят
/// и текст остался как набран. Ничего не удаляется, пока буфер не готов,
/// чтобы не потерять слово.
#[cfg(windows)]
fn replace_word(
    char_count: usize,
    text: &str,
    delete_trailing_space: bool,
    to_russian: bool,
) -> bool {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};

    // 1. Готовим буфер ДО удаления: если он занят — просто пропускаем правку.
    let Some(saved) = clipboard_put(text) else {
        return false;
    };

    let Ok(mut enigo) = Enigo::new(&Settings::default()) else {
        switcher_log("paste: не удалось создать Enigo");
        restore_clipboard(saved);
        return false;
    };

    // 2. Удаляем набранное слово (и пробел, который его завершил).
    let deletions = char_count + usize::from(delete_trailing_space);
    for _ in 0..deletions {
        if enigo.key(Key::Backspace, Direction::Click).is_err() {
            switcher_log("paste: Backspace не отправился");
            break;
        }
    }

    ensure_keyboard_layout(to_russian);
    std::thread::sleep(Duration::from_millis(30));

    // 3. Вставляем через Shift+Insert.
    let pasted = paste_clipboard(&mut enigo);

    // 4. Горячая клавиша не отправилась — вводим текст одним SendInput через
    //    Unicode (enigo 0.3 собирает все события в один батч, это не
    //    посимвольные лаги).
    if !pasted {
        switcher_log("paste: Shift+Insert не отправился, вводим текст через enigo.text()");
        if let Err(err) = enigo.text(text) {
            switcher_log(format!("paste: enigo.text тоже не удался: {err}"));
        }
    }

    drop(enigo);
    restore_clipboard(saved);
    pasted
}

#[cfg(not(windows))]
fn replace_word(_char_count: usize, _text: &str, _delete_trailing_space: bool, _to_russian: bool) -> bool {
    false
}


fn parse_english_dictionary(content: &str) -> HashSet<String> {
    content
        .lines()
        .map(|line| line.trim().to_lowercase())
        .filter(|word| !word.is_empty() && word.chars().all(|c| c.is_ascii_alphabetic()))
        .collect()
}

fn parse_russian_dictionary(content: &str) -> HashSet<String> {
    let mut words = HashSet::new();
    for line in content.lines() {
        // Первая строка файла может начинаться с BOM.
        let line = line.trim_start_matches('\u{feff}');
        // Формат файла — «слово частота», поэтому берём только первый токен.
        let Some(raw) = line.split_whitespace().next() else {
            continue;
        };
        let word = raw.to_lowercase().replace('ё', "е");
        if word.is_empty() {
            continue;
        }
        if word.chars().all(|c| matches!(c, 'а'..='я' | 'ё' | '-')) {
            words.insert(word);
        }
    }
    words
}

fn load_dictionaries() -> Result<Dictionaries, String> {
    let english = parse_english_dictionary(ENGLISH_DICT_TEXT);
    let russian = parse_russian_dictionary(RUSSIAN_DICT_TEXT);

    if english.len() < 100 {
        return Err(format!("английский словарь пуст ({} слов)", english.len()));
    }
    if russian.len() < 100 {
        return Err(format!("русский словарь пуст ({} слов)", russian.len()));
    }

    Ok(Dictionaries { english, russian })
}

#[cfg(windows)]
fn is_running_as_admin() -> bool {
    use windows::Win32::UI::Shell::IsUserAnAdmin;
    // SAFETY: IsUserAnAdmin читает токен текущего процесса, побочных
    // эффектов и указателей нет.
    unsafe { IsUserAnAdmin().as_bool() }
}

#[cfg(not(windows))]
fn is_running_as_admin() -> bool {
    true
}

// --- СЕТЬ, HOSTS И ИНСТРУМЕНТЫ ---

// ---------- hosts ----------

const HOSTS_BEGIN: &str = "# >>> Zapret Tools";
const HOSTS_END: &str = "# <<< Zapret Tools";

#[derive(Debug, Serialize)]
pub struct HostsState {
    pub path: String,
    pub entries: Vec<String>,
    pub content: String,
}

fn hosts_file_path() -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    Path::new(&root)
        .join("System32")
        .join("drivers")
        .join("etc")
        .join("hosts")
}

/// Делит hosts на «всё остальное» и управляемый блок Zapret Tools.
fn split_hosts_entries(content: &str) -> (String, Vec<String>) {
    let mut base = Vec::new();
    let mut entries = Vec::new();
    let mut inside = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case(HOSTS_BEGIN) {
            inside = true;
            continue;
        }
        if trimmed.eq_ignore_ascii_case(HOSTS_END) {
            inside = false;
            continue;
        }
        if inside {
            if !trimmed.is_empty() {
                entries.push(trimmed.to_string());
            }
            continue;
        }
        // Подчищаем и старый формат: строки, помеченные комментарием.
        if trimmed.to_lowercase().contains("# zapret tools") {
            continue;
        }
        base.push(line.to_string());
    }

    (base.join("\n"), entries)
}

fn validate_hosts_entry(entry: &str) -> Result<(), String> {
    if entry.contains('\r') || entry.contains('\n') {
        return Err("Запись не должна содержать перевод строки".into());
    }
    let trimmed = entry.trim();
    if trimmed.starts_with('#') {
        return Ok(());
    }
    let mut parts = trimmed.split_whitespace();
    let ip = parts.next().ok_or("Пустая запись")?;
    let ip_ok = ip.contains('.') || ip.contains(':');
    let ip_chars_ok = ip
        .chars()
        .all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':' || c == '%');
    if !ip_ok || !ip_chars_ok {
        return Err(format!("Некорректный IP-адрес: {ip}"));
    }
    if parts.next().is_none() {
        return Err(format!("После IP нужен хотя бы один домен: {trimmed}"));
    }
    Ok(())
}

#[tauri::command]
fn read_hosts() -> Result<HostsState, String> {
    let path = hosts_file_path();
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let content = String::from_utf8_lossy(&bytes).to_string();
    let (_, entries) = split_hosts_entries(&content);
    Ok(HostsState {
        path: path.display().to_string(),
        entries,
        content,
    })
}

#[tauri::command]
async fn update_hosts(entries: Vec<String>) -> Result<(), String> {
    const HOSTS_TIMEOUT: Duration = Duration::from_secs(30);

    let normalized: Vec<String> = entries
        .iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect();
    for entry in &normalized {
        validate_hosts_entry(entry)?;
    }

    let path = hosts_file_path();
    let current = fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_default();
    let (base, _) = split_hosts_entries(&current);

    let mut lines: Vec<String> = base
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect();
    while lines.last().map(|line| line.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }

    let mut text = lines.join("\r\n");
    if !normalized.is_empty() {
        text.push_str("\r\n\r\n");
        text.push_str(HOSTS_BEGIN);
        text.push_str("\r\n");
        for entry in &normalized {
            text.push_str(entry);
            text.push_str("\r\n");
        }
        text.push_str(HOSTS_END);
        text.push_str("\r\n");
    } else if !text.is_empty() {
        text.push_str("\r\n");
    }

    let temp = std::env::temp_dir().join(format!("zapret-tools-hosts-{}.txt", std::process::id()));
    fs::write(&temp, &text).map_err(|e| format!("{}: {e}", temp.display()))?;

    let script = format!(
        "$content = [IO.File]::ReadAllText({temp}, [Text.Encoding]::UTF8)\r\n\
$enc = New-Object System.Text.UTF8Encoding -ArgumentList $false\r\n\
[IO.File]::WriteAllText({hosts}, $content, $enc)\r\n\
ipconfig /flushdns | Out-Null\r\n",
        temp = ps_quote(&temp.to_string_lossy()),
        hosts = ps_quote(&path.to_string_lossy())
    );

    // UAC + запись выполняются в блокирующем потоке, но UI ждёт ответ
    // асинхронно. Таймаут не даёт кнопке «Применить» зависнуть навсегда.
    let temp_for_task = temp.clone();
    let task = tokio::task::spawn_blocking(move || {
        let result = run_elevated_script(&script);
        let _ = fs::remove_file(&temp_for_task);
        result
    });

    match tokio::time::timeout(HOSTS_TIMEOUT, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err(
            "PowerShell не ответил за 30 секунд. Проверьте запрос UAC и повторите.".to_string(),
        ),
    }
}

// ---------- Авто-обновление hosts из Zapret ----------

const AUTO_HOSTS_BEGIN: &str = "# >>> Zapret Tools Auto-Hosts >>>";
const AUTO_HOSTS_END: &str = "# <<< Zapret Tools Auto-Hosts <<<";
const AUTO_HOSTS_URL: &str =
    "https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/refs/heads/main/.service/hosts";

/// Приводит скачанный hosts к строкам без CR и без наших маркеров.
fn sanitize_auto_hosts(downloaded: &str) -> Vec<String> {
    let mut lines: Vec<String> = downloaded
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(|line| line.trim_end().to_string())
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.eq_ignore_ascii_case(AUTO_HOSTS_BEGIN)
                && !trimmed.eq_ignore_ascii_case(AUTO_HOSTS_END)
        })
        .collect();

    while lines.first().map(|line| line.trim().is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|line| line.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// Вставляет автоматический блок hosts, заменяя старый между маркерами.
/// Остальные строки файла остаются нетронутыми.
fn apply_auto_hosts_block(current: &str, downloaded: &str) -> String {
    let body = sanitize_auto_hosts(downloaded);

    let mut block = Vec::with_capacity(body.len() + 2);
    block.push(AUTO_HOSTS_BEGIN.to_string());
    block.extend(body);
    block.push(AUTO_HOSTS_END.to_string());

    let normalized = current.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<String> = normalized.lines().map(|line| line.to_string()).collect();

    let begin = lines
        .iter()
        .position(|line| line.trim().eq_ignore_ascii_case(AUTO_HOSTS_BEGIN));

    let mut result: Vec<String> = Vec::with_capacity(lines.len() + block.len());
    if let Some(start) = begin {
        result.extend(lines[..start].iter().cloned());
        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, line)| line.trim().eq_ignore_ascii_case(AUTO_HOSTS_END))
            .map(|(index, _)| index);
        let rest = end.map(|index| index + 1).unwrap_or(lines.len());
        result.extend(block);
        result.extend(lines[rest..].iter().cloned());
    } else {
        result.extend(lines);
        while result.last().map(|line| line.trim().is_empty()).unwrap_or(false) {
            result.pop();
        }
        if !result.is_empty() {
            result.push(String::new());
        }
        result.extend(block);
    }

    while result.last().map(|line| line.trim().is_empty()).unwrap_or(false) {
        result.pop();
    }
    let mut text = result.join("\r\n");
    text.push_str("\r\n");
    text
}

#[tauri::command]
async fn auto_update_hosts() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("zapret-tools")
        .build()
        .map_err(|e| format!("Не удалось создать HTTP-клиент: {e}"))?;

    let response = client
        .get(AUTO_HOSTS_URL)
        .send()
        .await
        .map_err(|e| format!("Не удалось скачать hosts: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Сервер вернул код {}", response.status()));
    }

    let downloaded = response
        .text()
        .await
        .map_err(|e| format!("Не удалось прочитать ответ сервера: {e}"))?;
    if downloaded.len() > 2 * 1024 * 1024 {
        return Err("Скачанный файл hosts слишком большой".into());
    }
    let body = sanitize_auto_hosts(&downloaded);
    if body.is_empty() {
        return Err("Сервер вернул пустой файл hosts".into());
    }
    if downloaded.to_lowercase().contains("<html") {
        return Err("Сервер вернул HTML вместо файла hosts".into());
    }

    let path = hosts_file_path();
    let current = fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_default();
    let updated = apply_auto_hosts_block(&current, &downloaded);
    let added = body.iter().filter(|line| !line.trim().is_empty()).count();

    let temp = std::env::temp_dir().join(format!(
        "zapret-tools-auto-hosts-{}.txt",
        std::process::id()
    ));
    fs::write(&temp, &updated).map_err(|e| format!("{}: {e}", temp.display()))?;

    let script = format!(
        "$content = [IO.File]::ReadAllText({temp}, [Text.Encoding]::UTF8)\r\n\
$enc = New-Object System.Text.UTF8Encoding -ArgumentList $false\r\n\
[IO.File]::WriteAllText({hosts}, $content, $enc)\r\n\
ipconfig /flushdns | Out-Null\r\n",
        temp = ps_quote(&temp.to_string_lossy()),
        hosts = ps_quote(&path.to_string_lossy())
    );

    let temp_for_task = temp.clone();
    let task = tokio::task::spawn_blocking(move || {
        let result = run_elevated_script(&script);
        let _ = fs::remove_file(&temp_for_task);
        result
    });

    match tokio::time::timeout(Duration::from_secs(30), task).await {
        Ok(Ok(Ok(()))) => Ok(format!("Hosts обновлён: {added} записей")),
        Ok(Ok(Err(err))) => Err(err),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err(
            "PowerShell не ответил за 30 секунд. Проверьте запрос UAC и повторите.".to_string(),
        ),
    }
}

// ---------- Встроенные тесты Zapret ----------

/// Ввод для интерактивного меню `test zapret.ps1`: [1] Standard tests,
/// [1] All configs. Дальше скрипт вопросов не задаёт.
const ZAPRET_TESTS_INPUT: &str = "1\r\n1\r\n";

fn zapret_tests_paths() -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join("zapret-tools");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let base = format!("zapret-tests-{}-{seq}", std::process::id());
    let input = dir.join(format!("{base}.in"));
    let stdout = dir.join(format!("{base}.out"));
    let stderr = dir.join(format!("{base}.err"));
    fs::write(&input, ZAPRET_TESTS_INPUT).map_err(|e| format!("{}: {e}", input.display()))?;
    let _ = fs::remove_file(&stdout);
    let _ = fs::remove_file(&stderr);
    Ok((input, stdout, stderr, dir.join(format!("{base}.code"))))
}

fn read_text_lossy(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_default()
}

fn cleanup_files(paths: &[&Path]) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

/// Запускает `test zapret.ps1` без прав администратора (вывод собирается
/// через перенаправление stdin/stdout/stderr).
#[cfg(windows)]
fn run_tests_plain(script: &Path) -> Result<String, String> {
    let (input, stdout, stderr, code_path) = zapret_tests_paths()?;

    let result = (|| {
        let stdin = fs::File::open(&input).map_err(|e| format!("{}: {e}", input.display()))?;
        let out = fs::File::create(&stdout).map_err(|e| format!("{}: {e}", stdout.display()))?;
        let err = fs::File::create(&stderr).map_err(|e| format!("{}: {e}", stderr.display()))?;

        let mut command = Command::new("powershell");
        hide_console(&mut command);
        let script_arg = script.to_string_lossy().to_string();
        let status = command
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &script_arg,
            ])
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .status()
            .map_err(|e| format!("не удалось запустить PowerShell: {e}"))?;

        let mut log = read_text_lossy(&stdout);
        let err_text = read_text_lossy(&stderr);
        if !err_text.trim().is_empty() {
            log.push_str("\r\n");
            log.push_str(&err_text);
        }
        if !status.success() {
            let code = status.code().unwrap_or(-1);
            if log.trim().is_empty() {
                return Err(format!("Тесты завершились с кодом {code} без вывода"));
            }
            log = format!("[код выхода {code}]\r\n{log}");
        }
        Ok(log)
    })();

    cleanup_files(&[&input, &stdout, &stderr, &code_path]);
    result
}

#[cfg(not(windows))]
fn run_tests_plain(_script: &Path) -> Result<String, String> {
    Err("Тесты Zapret доступны только в Windows".into())
}

/// Запускает тесты через UAC: повышенный PowerShell стартует обычный процесс
/// с перенаправлением потоков в файлы. Консоль остаётся скрытой.
#[cfg(windows)]
fn run_tests_elevated(script: &Path) -> Result<String, String> {
    let (input, stdout, stderr, code_path) = zapret_tests_paths()?;

    let arg_list = ps_quote(&format!(
        "-NoProfile -ExecutionPolicy Bypass -File \"{}\"",
        script.display()
    ));
    let body = format!(
        "$argList = {arg_list}\r\n\
$p = Start-Process -FilePath 'powershell.exe' -ArgumentList $argList \
-RedirectStandardInput {input} -RedirectStandardOutput {stdout} \
-RedirectStandardError {stderr} -WindowStyle Hidden -PassThru -Wait\r\n\
if ($null -eq $p) {{ throw 'Не удалось запустить тесты Zapret' }}\r\n\
Set-Content -LiteralPath {code} -Value $p.ExitCode -Encoding ASCII\r\n",
        arg_list = arg_list,
        input = ps_quote(&input.to_string_lossy()),
        stdout = ps_quote(&stdout.to_string_lossy()),
        stderr = ps_quote(&stderr.to_string_lossy()),
        code = ps_quote(&code_path.to_string_lossy())
    );

    let result = (|| {
        run_elevated_script(&body)?;

        let mut log = read_text_lossy(&stdout);
        let err_text = read_text_lossy(&stderr);
        if !err_text.trim().is_empty() {
            log.push_str("\r\n");
            log.push_str(&err_text);
        }
        let code = read_text_lossy(&code_path).trim().to_string();
        if !code.is_empty() && code != "0" {
            if log.trim().is_empty() {
                return Err(format!("Тесты завершились с кодом {code} без вывода"));
            }
            log = format!("[код выхода {code}]\r\n{log}");
        }
        Ok(log)
    })();

    cleanup_files(&[&input, &stdout, &stderr, &code_path]);
    result
}

#[cfg(not(windows))]
fn run_tests_elevated(_script: &Path) -> Result<String, String> {
    Err("Тесты Zapret доступны только в Windows".into())
}

fn run_zapret_tests_blocking(script: &Path) -> Result<String, String> {
    let log = if is_running_as_admin() {
        run_tests_plain(script)?
    } else {
        run_tests_elevated(script)?
    };

    let log = log.trim();
    if log.is_empty() {
        return Ok("Тесты завершены, но не вывели результатов.".into());
    }
    // Не отдаём в UI мегабайты: последние 200 000 символов достаточно.
    let total = log.chars().count();
    if total > 200_000 {
        let tail: String = log.chars().skip(total - 200_000).collect();
        return Ok(format!("…[начало вывода обрезано]\r\n{tail}"));
    }
    Ok(log.to_string())
}

#[tauri::command]
async fn run_zapret_tests(version_path: String) -> Result<String, String> {
    let script = Path::new(&version_path)
        .join("utils")
        .join("test zapret.ps1");
    if !script.is_file() {
        return Err(format!(
            "Не найден скрипт тестов: {}",
            script.display()
        ));
    }

    // Скрипт взаимодействует с ipset и запускает стратегии, поэтому даём
    // до 30 минут. Сам UI ждёт асинхронно и остаётся отзывчивым.
    let task = tokio::task::spawn_blocking(move || run_zapret_tests_blocking(&script));
    match tokio::time::timeout(Duration::from_secs(30 * 60), task).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err(
            "Тесты не завершились за 30 минут. Проверьте окно UAC и повторите.".to_string(),
        ),
    }
}

// ---------- Blockcheck ----------

#[derive(Debug, Serialize)]
pub struct BlockcheckReport {
    pub log: String,
    pub strategies: Vec<String>,
    pub log_path: String,
    pub source: String,
}

/// .bat-стратегии в корне сборки. `service.bat` — служебный, его не тестируем.
fn list_strategy_bats(version_path: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(version_path) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            if !path.is_file() {
                return false;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            name.ends_with(".bat") && !name.starts_with("service")
        })
        .collect();
    files.sort();
    // Больше 30 стратегий за один прогон — это уже часы ожидания.
    files.truncate(30);
    files
}

/// Значения `%GameFilterTCP%` / `%GameFilterUDP%` как их выставляет service.bat.
fn strategy_game_filter_ports(version_path: &str) -> (String, String) {
    let (enabled, mode, tcp, udp) = read_game_filter_config(version_path);
    if !enabled {
        return ("12".to_string(), "12".to_string());
    }
    match mode.to_ascii_lowercase().as_str() {
        "all" => (tcp, udp),
        "tcp" => (tcp, "12".to_string()),
        "udp" => ("12".to_string(), udp),
        _ => ("12".to_string(), "12".to_string()),
    }
}

fn bat_root_prefix(root: &Path) -> String {
    let text = root.to_string_lossy().replace('/', "\\");
    format!("{}\\", text.trim_end_matches('\\'))
}

/// Склеивает строки .bat, разорванные символом `^`.
fn logical_bat_lines(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        let trimmed = raw.trim_end();
        if let Some(head) = trimmed.strip_suffix('^') {
            current.push_str(head);
            current.push(' ');
        } else {
            current.push_str(trimmed);
            result.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        result.push(current);
    }
    result
}

/// Переменные из `set "NAME=value"` плюс порты Game Filter.
fn bat_variables(
    lines: &[String],
    root: &Path,
    game_tcp: &str,
    game_udp: &str,
) -> HashMap<String, String> {
    let prefix = bat_root_prefix(root);
    let mut vars: HashMap<String, String> = HashMap::new();
    for line in lines {
        let trimmed = line.trim();
        if !trimmed
            .get(..4)
            .map(|head| head.eq_ignore_ascii_case("set "))
            .unwrap_or(false)
        {
            continue;
        }
        let rest = trimmed[4..].trim().trim_matches('"');
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"').replace("%~dp0", &prefix);
        vars.insert(name.trim().to_ascii_lowercase(), value);
    }

    vars.entry("bin".to_string())
        .or_insert_with(|| format!("{prefix}bin\\"));
    vars.entry("lists".to_string())
        .or_insert_with(|| format!("{prefix}lists\\"));
    vars.insert("gamefiltertcp".to_string(), game_tcp.to_string());
    vars.insert("gamefilterudp".to_string(), game_udp.to_string());
    vars.insert("gamefilter".to_string(), game_tcp.to_string());
    vars
}

fn expand_bat_variables(text: &str, vars: &HashMap<String, String>) -> String {
    static VAR_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = VAR_RE.get_or_init(|| {
        regex::Regex::new(r"%([A-Za-z_][A-Za-z0-9_]*)%").expect("variable regex")
    });
    re.replace_all(text, |caps: &regex::Captures| {
        let key = caps[1].to_ascii_lowercase();
        vars.get(&key)
            .cloned()
            .unwrap_or_else(|| caps[0].to_string())
    })
    .to_string()
}

/// Разбивает командную строку Windows на аргументы, сохраняя пробелы в кавычках.
fn split_windows_command_line(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in input.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Ищет в строке вызов winws.exe и отделяет путь к exe от аргументов.
fn extract_winws_invocation(line: &str) -> Option<(String, Vec<String>)> {
    let lowered = line.to_ascii_lowercase();
    let index = lowered.find("winws.exe")?;
    let prefix = &line[..index];
    let start = match prefix.rfind('"') {
        Some(quote) if !prefix[quote + 1..].contains(char::is_whitespace) => quote,
        _ => prefix
            .rfind(char::is_whitespace)
            .map(|space| space + 1)
            .unwrap_or(0),
    };
    let tokens = split_windows_command_line(line[start..].trim());
    let exe = tokens.first()?.clone();
    if !exe.to_ascii_lowercase().ends_with("winws.exe") {
        return None;
    }
    Some((exe, tokens[1..].to_vec()))
}

fn quote_cmd_argument(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".to_string();
    }
    if value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '&' | '|' | '<' | '>' | '^' | '%'))
    {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

fn winws_command_line(exe: &str, args: &[String]) -> String {
    let mut line = format!("\"{exe}\"");
    for arg in args {
        line.push(' ');
        line.push_str(&quote_cmd_argument(arg));
    }
    line
}

async fn probe_strategy(client: &reqwest::Client, domains: &[String]) -> (bool, String) {
    let mut failure = String::new();
    for domain in domains {
        let url = format!("https://{domain}/");
        match client.get(&url).send().await {
            Ok(response) => {
                let code = response.status().as_u16();
                // Любой HTTP-ответ означает, что соединение прошло. DPI-блок
                // обычно рвёт TLS, а не отдаёт страницу.
                if code < 500 {
                    return (true, format!("{domain} — HTTP {code}"));
                }
                if failure.is_empty() {
                    failure = format!("{domain} — HTTP {code}");
                }
            }
            Err(err) => {
                if failure.is_empty() {
                    failure = format!("{domain} — {err}");
                }
            }
        }
    }
    (false, failure)
}

/// Проверяет .bat-стратегии из корня сборки: запускает winws, ждёт 2,5 с,
/// запрашивает домены с таймаутом 3 с и гасит процесс.
async fn run_strategy_blockcheck(
    version_path: &str,
    domains: &[String],
) -> Result<BlockcheckReport, String> {
    let bats = list_strategy_bats(version_path);
    if bats.is_empty() {
        return Err(
            "Для работы авто-подбора скачайте папку blockcheck с GitHub bol-van и положите её в папку с Zapret (или добавьте .bat-стратегии в корень сборки)."
                .to_string(),
        );
    }

    let root = Path::new(version_path);
    let (game_tcp, game_udp) = strategy_game_filter_ports(version_path);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .connect_timeout(Duration::from_secs(3))
        .user_agent("ZapretTools/1.0")
        .build()
        .map_err(|e| e.to_string())?;

    let mut log = String::from(
        "[INFO] blockcheck.cmd не найден — проверяются .bat-стратегии из корня сборки.\r\n",
    );
    if !is_running_as_admin() {
        log.push_str("[WARN] Нет прав администратора: winws может не примениться.\r\n");
    }
    if let Some(winws) = find_running_winws().await {
        log.push_str(&format!(
            "[WARN] Уже запущен {winws}. Остановите службу Zapret, иначе проверка будет неточной.\r\n"
        ));
    }
    let (baseline_ok, baseline_detail) = probe_strategy(&client, domains).await;
    if baseline_ok {
        log.push_str(&format!(
            "[WARN] Домены доступны без стратегии ({baseline_detail}) — возможны ложноположительные результаты.\r\n"
        ));
    }

    let total = bats.len();
    let mut working: Vec<String> = Vec::new();

    for (index, bat) in bats.iter().enumerate() {
        let name = bat
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        log.push_str(&format!("\r\n[TEST {}/{}] {name}\r\n", index + 1, total));

        let text = match fs::read(bat) {
            Ok(bytes) => String::from_utf8_lossy(&bytes)
                .trim_start_matches('\u{feff}')
                .to_string(),
            Err(err) => {
                log.push_str(&format!("[SKIP] не удалось прочитать: {err}\r\n"));
                continue;
            }
        };

        let lines = logical_bat_lines(&text);
        let vars = bat_variables(&lines, root, &game_tcp, &game_udp);
        let mut invocation = None;
        for line in &lines {
            let expanded = expand_bat_variables(line, &vars);
            if let Some(found) = extract_winws_invocation(&expanded) {
                invocation = Some(found);
                break;
            }
        }

        let Some((exe, args)) = invocation else {
            log.push_str("[SKIP] в файле нет команды winws.exe\r\n");
            continue;
        };
        if exe.contains('%') {
            log.push_str("[SKIP] не удалось подставить переменные в команду winws\r\n");
            continue;
        }
        let exe_path = if Path::new(&exe).is_absolute() {
            PathBuf::from(&exe)
        } else {
            root.join(&exe)
        };
        if !exe_path.is_file() {
            log.push_str(&format!("[SKIP] не найден {}\r\n", exe_path.display()));
            continue;
        }

        let mut command = Command::new(&exe_path);
        hide_console(&mut command);
        // Как в .bat: рабочий каталог bin, чтобы относительные пути к .bin
        // и спискам совпадали с обычным запуском стратегии.
        let work_dir = if root.join("bin").is_dir() {
            root.join("bin")
        } else {
            root.to_path_buf()
        };
        command
            .current_dir(work_dir)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                log.push_str(&format!("[SKIP] не удалось запустить winws: {err}\r\n"));
                continue;
            }
        };

        tokio::time::sleep(Duration::from_millis(2500)).await;
        if let Ok(Some(status)) = child.try_wait() {
            log.push_str(&format!(
                "[FAIL] winws завершился с кодом {}\r\n",
                status.code().unwrap_or(-1)
            ));
            continue;
        }

        let (ok, detail) = probe_strategy(&client, domains).await;
        let _ = child.kill();
        let _ = child.wait();

        if ok {
            log.push_str(&format!("[OK] {detail}\r\n"));
            working.push(winws_command_line(&exe_path.to_string_lossy(), &args));
        } else {
            log.push_str(&format!("[FAIL] {detail}\r\n"));
        }
    }

    log.push_str(&format!(
        "\r\nSUMMARY: проверено стратегий: {total}, рабочих: {}\r\n",
        working.len()
    ));

    let log_path = root.join("blockcheck.log");
    fs::write(&log_path, &log).map_err(|e| format!("{}: {e}", log_path.display()))?;

    Ok(BlockcheckReport {
        log,
        strategies: working,
        log_path: log_path.display().to_string(),
        source: "bat-strategies".to_string(),
    })
}

fn sanitize_domain(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
        .collect()
}

fn find_blockcheck_cmd(version_path: &str) -> Option<PathBuf> {
    let root = Path::new(version_path);
    let dir = root.join("blockcheck");
    for name in ["blockcheck.cmd", "blockcheck.bat"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(entries) = fs::read_dir(&dir) {
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("cmd"))
                        .unwrap_or(false)
            })
            .collect();
        found.sort();
        if let Some(path) = found.into_iter().next() {
            return Some(path);
        }
    }
    for name in ["blockcheck.cmd", "blockcheck.bat"] {
        let candidate = root.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn extract_blockcheck_strategies(log: &str) -> Vec<String> {
    let mut strategies: Vec<String> = Vec::new();
    for line in log.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("::") {
            continue;
        }
        let lowered = line.to_lowercase();
        if lowered.starts_with("rem ") || !lowered.contains("winws") {
            continue;
        }
        let command = line.trim_start_matches('>').trim().to_string();
        if command.is_empty() || strategies.iter().any(|item| item == &command) {
            continue;
        }
        strategies.push(command);
        if strategies.len() >= 50 {
            break;
        }
    }
    strategies
}

fn run_blockcheck_process(dir: &Path, cmd_name: &str, domains: &[String]) -> Result<String, String> {
    // Одна строка для cmd: иначе кавычки вокруг имени файла ломаются при
    // передаче через CreateProcess.
    let mut line = format!("chcp 65001 >nul & \"{cmd_name}\"");
    for domain in domains {
        line.push(' ');
        line.push_str(domain);
    }

    let mut command = Command::new("cmd");
    hide_console(&mut command);
    command
        .current_dir(dir)
        .arg("/c")
        .arg(line)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if !domains.is_empty() {
        let joined = domains.join(",");
        command.env("DOMAINS", &joined);
        command.env("DOMAINS_DEFAULT", &joined);
    }

    let child = command
        .spawn()
        .map_err(|e| format!("не удалось запустить {cmd_name}: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("ошибка ожидания {cmd_name}: {e}"))?;

    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push_str("\r\n");
        }
        text.push_str(&stderr);
    }
    Ok(text)
}

#[tauri::command]
async fn run_blockcheck(
    version_path: String,
    domains: Vec<String>,
) -> Result<BlockcheckReport, String> {
    static RUNNING: AtomicBool = AtomicBool::new(false);

    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            RUNNING.store(false, Ordering::SeqCst);
        }
    }

    if !Path::new(&version_path).is_dir() {
        return Err("Папка версии не найдена".into());
    }
    if domains.is_empty() {
        return Err("Укажите хотя бы один домен для проверки".into());
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("Blockcheck уже выполняется".into());
    }
    let _guard = Guard;

    let domains: Vec<String> = domains
        .iter()
        .map(|domain| sanitize_domain(domain))
        .filter(|domain| !domain.is_empty())
        .collect();
    if domains.is_empty() {
        return Err("Укажите корректные домены для проверки".into());
    }

    let Some(cmd_path) = find_blockcheck_cmd(&version_path) else {
        return run_strategy_blockcheck(&version_path, &domains).await;
    };

    let dir = cmd_path
        .parent()
        .map(|path| path.to_path_buf())
        .ok_or("Не удалось определить папку blockcheck")?;
    let cmd_name = cmd_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or("Некорректное имя blockcheck.cmd")?;
    let log_path = dir.join("blockcheck.log");

    let dir_for_task = dir.clone();
    let domains_for_task = domains.clone();
    let cmd_for_task = cmd_name.clone();

    let log = tokio::task::spawn_blocking(move || {
        run_blockcheck_process(&dir_for_task, &cmd_for_task, &domains_for_task)
    })
    .await
    .map_err(|e| e.to_string())??;

    fs::write(&log_path, &log).map_err(|e| format!("{}: {e}", log_path.display()))?;

    Ok(BlockcheckReport {
        strategies: extract_blockcheck_strategies(&log),
        log,
        log_path: log_path.display().to_string(),
        source: cmd_path.display().to_string(),
    })
}

#[tauri::command]
fn save_blockcheck_strategy(version_path: String, name: String, command: String) -> Result<String, String> {
    if !command.to_lowercase().contains("winws") {
        return Err("Команда не содержит winws.exe".into());
    }
    let mut stem: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if stem.is_empty() {
        stem = "blockcheck-strategy".to_string();
    }
    let path = Path::new(&version_path).join(format!("{stem}.bat"));
    let content = format!("@echo off\r\nchcp 65001 >nul\r\n{command}\r\npause\r\n");
    fs::write(&path, content).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

// ---------- Инструменты (.exe в папке версии) ----------

#[derive(Debug, Serialize)]
pub struct RunningTool {
    pub pid: u32,
    pub name: String,
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_lowercase()
}

#[tauri::command]
fn list_tools(version_path: String) -> Vec<String> {
    let mut tools = Vec::new();
    if let Ok(entries) = fs::read_dir(&version_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let is_exe = path
                .extension()
                .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("exe"))
                .unwrap_or(false);
            if !is_exe {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // winws.exe запускается стратегиями, отдельная кнопка ему не нужна.
            if name.eq_ignore_ascii_case("winws.exe") {
                continue;
            }
            tools.push(name);
        }
    }
    tools.sort_by_key(|name| name.to_lowercase());
    tools
}

#[tauri::command]
fn start_tool(version_path: String, exe_name: String) -> Result<u32, String> {
    if exe_name.contains('\\')
        || exe_name.contains('/')
        || exe_name.contains("..")
        || exe_name.trim().is_empty()
    {
        return Err("Некорректное имя файла".into());
    }
    let exe = Path::new(&version_path).join(&exe_name);
    if !exe.is_file() {
        return Err(format!("Файл не найден: {}", exe.display()));
    }
    let child = Command::new(&exe)
        .current_dir(&version_path)
        .spawn()
        .map_err(|e| format!("Не удалось запустить {exe_name}: {e}"))?;
    Ok(child.id())
}

#[tauri::command]
fn stop_tool(pid: u32) -> Result<(), String> {
    let mut sys = System::new_all();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let pid = sysinfo::Pid::from_u32(pid);
    match sys.process(pid) {
        Some(process) => {
            if process.kill() {
                Ok(())
            } else {
                Err("Не удалось завершить процесс".into())
            }
        }
        None => Err("Процесс уже завершён".into()),
    }
}

#[tauri::command]
async fn find_tool_processes(version_path: String) -> Vec<RunningTool> {
    tokio::task::spawn_blocking(move || {
        let base = normalized_path(Path::new(&version_path));
        let mut sys = System::new_all();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let mut result = Vec::new();
        for (pid, process) in sys.processes() {
            let Some(exe) = process.exe() else { continue };
            let Some(parent) = exe.parent() else { continue };
            if normalized_path(parent) != base {
                continue;
            }
            let name = exe
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            result.push(RunningTool {
                pid: pid.as_u32(),
                name,
            });
        }
        result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        result
    })
    .await
    .unwrap_or_default()
}

fn exclusion_matches(active: &str, exclusion: &str) -> bool {
    let active = active.trim().to_lowercase();
    let mut ex = exclusion.trim().trim_matches('"').trim().to_lowercase();
    if ex.is_empty() { return false; }
    // Разрешаем указывать процесс без расширения: «cs2» == «cs2.exe».
    if !ex.contains('.') { ex.push_str(".exe"); }
    active == ex
}

// Имя активного процесса (.exe) окна на переднем плане.
#[cfg(windows)]
fn get_active_process_name() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, FALSE};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    // SAFETY: хэндл процесса открыт с PROCESS_QUERY_LIMITED_INFORMATION
    // и закрывается ниже; буфер для имени — на стеке, его размер передаётся API.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() { return None; }

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 { return None; }

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        result.ok()?;

        let full_path = String::from_utf16_lossy(&buffer[..(size as usize).min(buffer.len())]);
        let full_path = full_path.trim_end_matches('\0');
        let name = full_path.rsplit(['\\', '/']).next().unwrap_or(full_path);
        if name.is_empty() { None } else { Some(name.to_string()) }
    }
}

#[cfg(not(windows))]
fn get_active_process_name() -> Option<String> {
    None
}

#[cfg(windows)]
fn simulate_layout_switch() {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};

    // Эмулируем Alt + Shift — системное переключение раскладки Windows.
    let Ok(mut enigo) = Enigo::new(&Settings::default()) else {
        return;
    };
    let _ = enigo.key(Key::Alt, Direction::Press);
    let _ = enigo.key(Key::Shift, Direction::Press);
    std::thread::sleep(std::time::Duration::from_millis(30));
    let _ = enigo.key(Key::Shift, Direction::Release);
    let _ = enigo.key(Key::Alt, Direction::Release);
    // Если что-то не отправилось, Drop у Enigo отпустит оставшиеся клавиши.
}

#[cfg(not(windows))]
fn simulate_layout_switch() {}

pub fn run() {
    tauri::Builder::default()
        // Single instance обязан идти первым: плагин перехватывает запуск
        // второй копии до инициализации остальных плагинов и до создания окна.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Второй процесс сразу завершается, а уже открытое окно
            // поднимаем на передний план — в том числе из трея/свёрнутого вида.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // Паника в фоновом потоке (хук, правка слова, восстановление буфера)
            // раньше терялась целиком: stderr у GUI-сборки никуда не виден.
            install_panic_hook(app.handle());
            let args: Vec<String> = std::env::args().collect();
            if args.contains(&"--minimized".to_string()) {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            // Свитчер раскладки: общее состояние + фоновый хук клавиатуры.
            // Настройки берём из config.json, чтобы они переживали перезапуск.
            let admin = is_running_as_admin();
            let saved = read_config(app.handle());
            let switcher_state = Arc::new(Mutex::new(SwitcherState {
                enabled: saved.switcher_enabled,
                exclusions: saved.switcher_exclusions,
                word_exclusions: saved.switcher_word_exclusions,
                custom_dict_ru: saved.switcher_custom_dict_ru,
                custom_dict_en: saved.switcher_custom_dict_en,
                declined_words: saved.switcher_declined_words,
                admin,
            }));
            app.manage(Arc::clone(&switcher_state));

            // Словари грузим в фоне: старт приложения не должен ждать диск/сеть.
            let dict_app = app.handle().clone();
            std::thread::spawn(move || {
                use tauri::Emitter;
                match load_dictionaries() {
                    Ok(dicts) => {
                        let _ = DICTIONARIES.set(Some(dicts));
                        let _ = dict_app.emit("switcher-dictionaries-ready", ());
                    }
                    Err(err) => {
                        let _ = DICTIONARIES.set(None);
                        let _ = dict_app.emit(
                            "switcher-error",
                            format!("Не удалось загрузить словари свитчера: {err}"),
                        );
                    }
                }
            });

            start_switcher_thread(Arc::clone(&switcher_state), app.handle().clone());

            if !admin {
                let warn_app = app.handle().clone();
                // Даём фронтенду время подписаться на событие.
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(2500));
                    use tauri::Emitter;
                    let _ = warn_app.emit(
                        "switcher-warning",
                        "Свитчер запущен без прав администратора: в окнах, запущенных от имени администратора, перехват клавиатуры может не работать.".to_string(),
                    );
                });
            }

            let show = MenuItem::with_id(app, "show", "Открыть", true, None::<&str>)?;
            let start = MenuItem::with_id(app, "start", "Старт", true, None::<&str>)?;
            let stop = MenuItem::with_id(app, "stop", "Стоп", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &start, &stop, &quit])?;

            // Отсутствие иконки не должно обрывать setup: раньше `unwrap()`
            // здесь давал панику и пустое окно на старте.
            let tray_builder = match app.default_window_icon() {
                Some(icon) => TrayIconBuilder::new().icon(icon.clone()),
                None => {
                    switcher_log("tray: у приложения нет иконки по умолчанию");
                    TrayIconBuilder::new()
                }
            };
            let _tray = tray_builder
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    match event.id.as_ref() {
                        "show" => {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        "start" => {
                            let mut command = Command::new("powershell");
                            hide_console(&mut command);
                            let _ = command
                                .args(["-NoProfile", "-Command", "Start-Process sc -ArgumentList 'start zapret' -Verb RunAs"])
                                .spawn();
                        }
                        "stop" => {
                            let mut command = Command::new("powershell");
                            hide_console(&mut command);
                            let _ = command
                                .args(["-NoProfile", "-Command", "Start-Process sc -ArgumentList 'stop zapret' -Verb RunAs"])
                                .spawn();
                        }
                        "quit" => { app.exit(0); }
                        _ => {}
                    }
                })
                .build(app);
            if let Err(err) = &_tray {
                // Трей — необязательная часть: приложение продолжает работать.
                switcher_log(format!("tray: не удалось создать иконку в трее: {err}"));
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Окно уже могло быть уничтожено (выход из трея) — падать нельзя.
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config, save_config, scan_versions, find_running_winws,
            open_folder, run_as_admin, manage_service, read_list_files, read_list_file,
            write_list_file, merge_folders, sync_github, validate_list,
            check_zapret_version, download_zapret, delete_version, create_start_menu_shortcut,
            read_external_file, toggle_game_filter, get_game_filter_status,
            get_game_filter_config, set_ipset_state, get_ipset_state,
            toggle_zapret_update_check, get_zapret_update_check_status, list_strategies, install_strategy,
            smart_parse_domains, clean_and_validate,
            consolidate_subdomains, check_dead_domains,
            get_trash_files, restore_trash_file, empty_trash,
            update_switcher_config, get_switcher_config, start_switcher_listener,
            add_switcher_word_exclusion, remove_switcher_word_exclusion,
            get_custom_words, add_custom_word, remove_custom_word,
            decline_custom_word, clear_declined_words,
            get_running_processes,
            run_blockcheck, save_blockcheck_strategy,
            network::list_network_interfaces, network::get_dns_state,
            network::set_dns, network::reset_dns,
            network::toggle_quic_block, network::get_quic_block_state,
            logs::get_log_tail, logs::open_switcher_log,
            read_hosts, update_hosts, auto_update_hosts, run_zapret_tests,
            list_tools, start_tool, stop_tool, find_tool_processes
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod game_filter_tests {
    use super::*;

    /// Временная папка вида `%TEMP%\zapret-game-filter-<n>`.
    fn temp_version_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zapret-game-filter-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("utils")).expect("temp dir");
        dir
    }

    #[test]
    fn accepts_valid_port_ranges() {
        for value in ["1024-65535", "443", "80,443,1000-2000", " 1024 - 65535 "] {
            assert!(validate_port_range(value).is_ok(), "должен принять «{value}»");
        }
    }

    #[test]
    fn rejects_invalid_port_ranges() {
        for value in ["", "  ", "abc", "0", "65536", "2000-1000", "80,", "-", "80-"] {
            assert!(validate_port_range(value).is_err(), "должен отклонить «{value}»");
        }
    }

    #[test]
    fn writes_and_reads_service_bat_format() {
        let dir = temp_version_dir("roundtrip");
        let path = dir.to_string_lossy().to_string();

        // Файла нет — фильтр выключен, значения по умолчанию.
        let (enabled, mode, tcp, udp) = read_game_filter_config(&path);
        assert!(!enabled);
        assert_eq!(mode, "all");
        assert_eq!(tcp, "1024-65535");
        assert_eq!(udp, "1024-65535");

        toggle_game_filter(
            path.clone(),
            true,
            Some("tcp".to_string()),
            Some("443".to_string()),
            Some("1024-65535".to_string()),
        )
        .expect("toggle on");

        let flag_file = dir.join("utils").join("game_filter.enabled");
        assert_eq!(
            fs::read_to_string(&flag_file).unwrap(),
            "mode=tcp\ntcp=443\nudp=1024-65535\n"
        );

        let (enabled, mode, tcp, udp) = read_game_filter_config(&path);
        assert!(enabled);
        assert_eq!(mode, "tcp");
        assert_eq!(tcp, "443");
        assert_eq!(udp, "1024-65535");

        // Выключение убирает файл.
        toggle_game_filter(path.clone(), false, None, None, None).expect("toggle off");
        assert!(!flag_file.exists());
        assert!(!read_game_filter_config(&path).0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_old_stub_file_as_enabled() {
        let dir = temp_version_dir("legacy");
        let path = dir.to_string_lossy().to_string();
        fs::write(dir.join("utils").join("game_filter.enabled"), "ENABLED").unwrap();

        let (enabled, mode, tcp, udp) = read_game_filter_config(&path);
        assert!(enabled);
        // Ключей в старой заглушке нет — остаются значения по умолчанию.
        assert_eq!(mode, "all");
        assert_eq!(tcp, "1024-65535");
        assert_eq!(udp, "1024-65535");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unknown_mode() {
        let dir = temp_version_dir("mode");
        let path = dir.to_string_lossy().to_string();
        let err = toggle_game_filter(
            path,
            true,
            Some("icmp".to_string()),
            None,
            None,
        )
        .expect_err("icmp не поддерживается service.bat");
        assert!(err.contains("icmp"));
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod service_flags_tests {
    use super::*;

    fn temp_version_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zapret-flags-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("lists")).expect("lists");
        fs::create_dir_all(dir.join("utils")).expect("utils");
        dir
    }

    const REAL_LIST: &str = "1.1.1.1/32\n2.2.2.2/32\n";

    #[test]
    fn ipset_reports_three_states() {
        let dir = temp_version_dir("states");
        let path = dir.to_string_lossy().to_string();

        // Файла нет — фильтровать нечего.
        assert_eq!(get_ipset_state(path.clone()), "none");

        let list_file = dir.join("lists").join("ipset-all.txt");
        fs::write(&list_file, REAL_LIST).unwrap();
        assert_eq!(get_ipset_state(path.clone()), "loaded");

        fs::write(&list_file, "203.0.113.113/32\n").unwrap();
        assert_eq!(get_ipset_state(path.clone()), "none");

        fs::write(&list_file, "").unwrap();
        assert_eq!(get_ipset_state(path.clone()), "any");

        // Пробелы и переводы строк пустым файл не делают.
        fs::write(&list_file, "  \r\n\n").unwrap();
        assert_eq!(get_ipset_state(path), "any");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ipset_keeps_backup_across_none_and_any() {
        let dir = temp_version_dir("backup");
        let path = dir.to_string_lossy().to_string();
        let list_file = dir.join("lists").join("ipset-all.txt");
        let backup_file = dir.join("lists").join("ipset-all.txt.backup");
        fs::write(&list_file, REAL_LIST).unwrap();

        set_ipset_state(path.clone(), "none".into()).expect("none");
        assert_eq!(fs::read_to_string(&list_file).unwrap(), "203.0.113.113/32\n");
        assert_eq!(fs::read_to_string(&backup_file).unwrap(), REAL_LIST);

        // Переход none → any не должен затереть бэкап пустым файлом.
        set_ipset_state(path.clone(), "any".into()).expect("any");
        assert_eq!(fs::read_to_string(&list_file).unwrap(), "");
        assert_eq!(fs::read_to_string(&backup_file).unwrap(), REAL_LIST);
        assert_eq!(get_ipset_state(path.clone()), "any");

        // Возврат к рабочему списку восстанавливает исходное содержимое.
        set_ipset_state(path.clone(), "loaded".into()).expect("loaded");
        assert_eq!(fs::read_to_string(&list_file).unwrap(), REAL_LIST);
        assert!(!backup_file.exists());
        assert_eq!(get_ipset_state(path), "loaded");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ipset_allows_loaded_to_any() {
        let dir = temp_version_dir("loaded-any");
        let path = dir.to_string_lossy().to_string();
        let list_file = dir.join("lists").join("ipset-all.txt");
        let backup_file = dir.join("lists").join("ipset-all.txt.backup");
        fs::write(&list_file, REAL_LIST).unwrap();

        set_ipset_state(path.clone(), "any".into()).expect("any");
        assert_eq!(fs::read_to_string(&list_file).unwrap(), "");
        assert_eq!(fs::read_to_string(&backup_file).unwrap(), REAL_LIST);

        set_ipset_state(path, "loaded".into()).expect("loaded");
        assert_eq!(fs::read_to_string(&list_file).unwrap(), REAL_LIST);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ipset_rejects_unknown_state() {
        let dir = temp_version_dir("bad-state");
        let path = dir.to_string_lossy().to_string();
        let err = set_ipset_state(path, "everything".into()).expect_err("неизвестное состояние");
        assert!(err.contains("everything"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zapret_update_flag_toggles_file() {
        let dir = temp_version_dir("updates");
        let path = dir.to_string_lossy().to_string();
        let flag = dir.join("utils").join("check_updates.enabled");

        assert!(!get_zapret_update_check_status(path.clone()));

        toggle_zapret_update_check(path.clone(), true).expect("on");
        assert!(flag.exists());
        assert!(get_zapret_update_check_status(path.clone()));

        toggle_zapret_update_check(path.clone(), false).expect("off");
        assert!(!flag.exists());
        assert!(!get_zapret_update_check_status(path));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_defaults_to_enabled_autocheck() {
        // Новый конфиг и старый файл без поля — автопроверка включена.
        assert!(Config::default().auto_check_updates);
        let old: Config = serde_json::from_str(r#"{"root_path":null}"#).expect("old config");
        assert!(old.auto_check_updates);

        // Явное false переживает сериализацию.
        let mut config = Config::default();
        config.auto_check_updates = false;
        let json = serde_json::to_string(&config).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert!(!parsed.auto_check_updates);
    }
}

#[cfg(test)]
mod switcher_tests {
    use super::*;

    #[test]
    fn converts_english_layout_to_russian() {
        assert_eq!(convert_layout("ghbdtn", true), "привет");
        assert_eq!(convert_layout("rjhjdf", true), "корова");
        assert_eq!(convert_layout("Ghbdtn", true), "Привет");
        // Знаки препинания на буквенных клавишах: `.,rf` → `юбка`.
        assert_eq!(convert_layout(".,rf", true), "юбка");
        assert_eq!(convert_layout("юбка", false), ".,rf");
    }

    #[test]
    fn keeps_punctuation_keys_in_word_buffer() {
        use rdev::Key;

        // Английская раскладка: это пунктуация, но она может быть буквами ЙЦУКЕН.
        assert_eq!(key_to_char(Key::Dot, false, false, false), Some('.'));
        assert_eq!(key_to_char(Key::Comma, false, false, false), Some(','));
        assert_eq!(key_to_char(Key::SemiColon, false, false, false), Some(';'));
        // Русская раскладка: те же клавиши уже дают буквы.
        assert_eq!(key_to_char(Key::Dot, false, false, true), Some('ю'));
        assert_eq!(key_to_char(Key::Comma, false, false, true), Some('б'));
        // Shift-варианты цифровых клавиш — настоящие символы слова.
        assert_eq!(key_to_char(Key::Num2, true, false, false), Some('@'));
        assert_eq!(key_to_char(Key::Num7, true, false, false), Some('&'));
        // В русской раскладке это уже настоящие знаки-разделители.
        assert_eq!(key_to_char(Key::Num2, true, false, true), None);
        assert_eq!(key_to_char(Key::Num7, true, false, true), None);
        // Shift на знаковых клавишах.
        assert_eq!(key_to_char(Key::LeftBracket, true, false, false), Some('{'));
        assert_eq!(key_to_char(Key::LeftBracket, true, false, true), Some('Х'));
        assert_eq!(key_to_char(Key::Slash, true, false, false), Some('?'));
        // Цифры без Shift и прочие служебные клавиши слово разрывают.
        assert_eq!(key_to_char(Key::Num1, false, false, false), None);
        assert_eq!(key_to_char(Key::Num5, false, false, true), None);
        assert_eq!(key_to_char(Key::BackSlash, false, false, false), None);
    }

    #[test]
    fn converts_special_characters() {
        // Пары из карты соответствия QWERTY <-> ЙЦУКЕН.
        assert_eq!(convert_layout("ghbdtn&", true), "привет?");
        assert_eq!(convert_layout("ghbdtn@", true), "привет\"");
        assert_eq!(convert_layout("привет\"", false), "ghbdtn@");
        assert_eq!(convert_layout("привет?", false), "ghbdtn&");

        for (en, ru) in [
            ("#", "№"),
            ("$", ";"),
            ("^", ":"),
            (":", "Ж"),
            ("<", "Б"),
            (">", "Ю"),
            ("~", "Ё"),
            ("{", "Х"),
            ("}", "Ъ"),
            ("|", "/"),
            // Клавиша Slash: `.` в ЙЦУКЕН, `/` — без Shift в QWERTY.
            ("/", "."),
            ("?", ","),
        ] {
            assert_eq!(convert_layout(en, true), ru, "EN->RU: {en}");
            assert_eq!(convert_layout(ru, false), en, "RU->EN: {ru}");
        }
        // Повторный Double Shift возвращает исходный текст.
        assert_eq!(convert_layout(&convert_layout("ghbdtn&", true), false), "ghbdtn&");
        assert_eq!(convert_layout(&convert_layout("ghbdtn/", true), false), "ghbdtn/");
    }

    #[test]
    fn detects_word_with_punctuation_keys() {
        let dicts = Dictionaries {
            english: parse_english_dictionary("hello\nworld\n"),
            russian: parse_russian_dictionary("привет 100\nюбка 50\n"),
        };

        let fix = detect_layout_mistake(".,rf", false, &dicts, &[], &[]).expect("must detect");
        assert_eq!(fix.corrected, "юбка");
        assert!(fix.to_russian);
        assert_eq!(fix.typed_len, 4);
    }

    #[test]
    fn converts_russian_layout_to_english() {
        assert_eq!(convert_layout("привет", false), "ghbdtn");
        assert_eq!(convert_layout("Привет", false), "Ghbdtn");
        assert_eq!(convert_layout("ёлка", false), "`krf");
    }

    #[test]
    fn converts_single_character() {
        // Double Shift — ручной триггер, одиночный символ тоже переводим.
        assert_eq!(convert_layout("b", true), "и");
        assert_eq!(convert_layout("и", false), "b");
        assert_eq!(convert_layout("B", true), "И");
    }

    #[test]
    fn parses_russian_dictionary_with_frequencies() {
        let words = parse_russian_dictionary("и 999999\r\nпривет 123\r\nёлка 10\r\nnot-latin 1\r\n");
        assert!(words.contains("и"));
        assert!(words.contains("привет"));
        // «ё» нормализуется в «е» для проверки.
        assert!(words.contains("елка"));
        assert!(!words.contains("not-latin"));
    }

    #[test]
    fn embedded_dictionaries_load() {
        // include_str!-словари должны реально парситься, а не быть пустыми.
        let dicts = load_dictionaries().expect("embedded dictionaries");
        assert!(dicts.english.len() > 5_000, "english: {}", dicts.english.len());
        assert!(dicts.russian.len() > 50_000, "russian: {}", dicts.russian.len());
    }

    #[test]
    fn detects_wrong_layout_word() {
        let dicts = Dictionaries {
            english: parse_english_dictionary("hello\nworld\n"),
            russian: parse_russian_dictionary("привет 100\nмир 50\n"),
        };

        let fix = detect_layout_mistake("ghbdtn", false, &dicts, &[], &[]).expect("must detect");
        assert_eq!(fix.corrected, "привет");
        assert!(fix.to_russian);
        assert_eq!(fix.typed_len, 6);

        // Слово есть в текущем словаре — не трогаем.
        assert!(detect_layout_mistake("hello", false, &dicts, &[], &[]).is_none());
        // Нет ни в одном словаре — тоже не трогаем.
        assert!(detect_layout_mistake("zzzzzz", false, &dicts, &[], &[]).is_none());
    }

    #[test]
    fn custom_dictionary_is_used_for_detection_and_suggestion() {
        let dicts = Dictionaries {
            english: parse_english_dictionary("hello\n"),
            russian: parse_russian_dictionary("привет 100\n"),
        };

        // Пара, которой нет в базовых словарях.
        let typed = "rdflhjlbswbkzwbz";
        let translated = convert_layout(typed, true);

        // Если переведённое слово есть в пользовательском словаре RU,
        // набор в EN-раскладке исправляется автоматически.
        let custom_ru = vec![translated.clone()];
        let fix = detect_layout_mistake(typed, false, &dicts, &custom_ru, &[])
            .expect("custom word must be detected");
        assert_eq!(fix.corrected, translated);
        assert!(fix.to_russian);

        // Слово из пользовательского словаря текущего языка не трогаем.
        let custom_en = vec!["zzzzzz".to_string()];
        assert!(detect_layout_mistake("zzzzzz", false, &dicts, &[], &custom_en).is_none());

        let mut state = SwitcherState {
            enabled: true,
            exclusions: Vec::new(),
            word_exclusions: Vec::new(),
            custom_dict_ru: Vec::new(),
            custom_dict_en: Vec::new(),
            declined_words: Vec::new(),
            admin: true,
        };

        let pair_typed = "qwertyuiopasdf";
        let pair_translated = convert_layout(pair_typed, true);
        let suggestion =
            custom_word_suggestion(pair_typed, &pair_translated, false, &dicts, &state)
                .expect("unknown pair should be suggested");
        assert_eq!(suggestion.0, pair_typed);
        assert_eq!(suggestion.1, pair_translated);
        assert!(suggestion.2);

        // После отказа предложение больше не отправляется.
        state.declined_words.push(pair_typed.to_string());
        assert!(custom_word_suggestion(pair_typed, &pair_translated, false, &dicts, &state).is_none());

        // Настоящее слово исходного языка не предлагаем.
        assert!(
            custom_word_suggestion("hello", &convert_layout("hello", true), false, &dicts, &state)
                .is_none()
        );
    }

    #[test]
    fn auto_hosts_block_appends_and_replaces() {
        let first = apply_auto_hosts_block(
            "127.0.0.1 localhost\r\n",
            "1.2.3.4 example.com\n5.6.7.8 discord.com\n",
        );
        assert!(first.contains("127.0.0.1 localhost"));
        assert!(first.contains(AUTO_HOSTS_BEGIN));
        assert!(first.contains(AUTO_HOSTS_END));
        assert!(first.contains("1.2.3.4 example.com"));
        assert_eq!(first.matches(AUTO_HOSTS_BEGIN).count(), 1);

        let second = apply_auto_hosts_block(&first, "9.9.9.9 youtube.com\n");
        assert_eq!(second.matches(AUTO_HOSTS_BEGIN).count(), 1);
        assert_eq!(second.matches(AUTO_HOSTS_END).count(), 1);
        assert!(!second.contains("1.2.3.4 example.com"));
        assert!(second.contains("9.9.9.9 youtube.com"));
        assert!(second.contains("127.0.0.1 localhost"));
        // Файл hosts в Windows должен остаться с CRLF.
        assert!(!second.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn auto_hosts_strips_its_own_markers_from_download() {
        let downloaded = format!(
            "{begin}\n1.1.1.1 one.example\n{end}\n2.2.2.2 two.example\n",
            begin = AUTO_HOSTS_BEGIN,
            end = AUTO_HOSTS_END
        );
        let result = apply_auto_hosts_block("", &downloaded);
        assert_eq!(result.matches(AUTO_HOSTS_BEGIN).count(), 1);
        assert_eq!(result.matches(AUTO_HOSTS_END).count(), 1);
        assert!(result.contains("1.1.1.1 one.example"));
        assert!(result.contains("2.2.2.2 two.example"));
    }

    #[test]
    fn reads_old_config_without_switcher_fields() {
        // Конфиг, записанный до появления настроек свитчера.
        let parsed: Config =
            serde_json::from_str(r#"{"root_path":"C:\\zapret","active_version":"v1"}"#)
                .expect("old config must parse");
        assert!(!parsed.switcher_enabled);
        assert!(parsed.switcher_exclusions.is_empty());
        assert!(parsed.switcher_word_exclusions.is_empty());
        assert!(parsed.switcher_custom_dict_ru.is_empty());
        assert!(parsed.switcher_custom_dict_en.is_empty());
        assert!(parsed.switcher_declined_words.is_empty());
    }

    #[test]
    fn detects_selected_text_language() {
        assert_eq!(detect_text_language("ghbdtn"), Some(false));
        assert_eq!(detect_text_language("привет"), Some(true));
        assert_eq!(detect_text_language("Привет, мир!"), Some(true));
        assert_eq!(detect_text_language("hello, world!"), Some(false));
        // Смешанный текст и текст без букв не конвертируем.
        assert_eq!(detect_text_language("привет world"), None);
        assert_eq!(detect_text_language("123 !?"), None);
    }
}

#[cfg(test)]
mod blockcheck_parser_tests {
    use super::*;

    #[test]
    fn joins_caret_continuation() {
        let lines = logical_bat_lines(
            "@echo off\r\nstart \"x\" winws.exe --a ^\r\n--b=1\r\n",
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("winws.exe --a  --b=1")),
            "{lines:?}"
        );
    }

    #[test]
    fn extracts_winws_from_quoted_start_line() {
        let (exe, args) = extract_winws_invocation(
            r#"start "zapret: general" /min "C:\z\bin\winws.exe" --wf-tcp=80,443 --hostlist="C:\z\lists\list.txt""#,
        )
        .expect("winws command");
        assert_eq!(exe, r"C:\z\bin\winws.exe");
        assert_eq!(
            args,
            vec!["--wf-tcp=80,443", r"--hostlist=C:\z\lists\list.txt"]
        );
    }

    #[test]
    fn expands_bat_variables() {
        let lines = logical_bat_lines(
            "set \"BIN=%~dp0bin\\\"\r\nstart winws.exe --bin=\"%BIN%\" --port=%GameFilterTCP%\r\n",
        );
        let vars = bat_variables(&lines, Path::new(r"C:\zapret"), "1024-65535", "12");
        let expanded = expand_bat_variables(
            "start winws.exe --bin=\"%BIN%\" --port=%GameFilterTCP%",
            &vars,
        );
        assert!(expanded.contains(r#"--bin="C:\zapret\bin\""#), "{expanded}");
        assert!(expanded.contains("--port=1024-65535"), "{expanded}");
    }

    #[test]
    fn splits_quoted_arguments() {
        let args = split_windows_command_line(r#"--hostlist="C:\my lists\list.txt" --new"#);
        assert_eq!(
            args,
            vec![r"--hostlist=C:\my lists\list.txt", "--new"]
        );
    }
}

#[cfg(all(test, windows))]
mod clipboard_restore_tests {
    use super::*;
    use win_clipboard::ReadOutcome;

    #[test]
    fn previous_text_is_restored_verbatim() {
        let previous = ReadOutcome::Text("привет, мир".to_string());
        assert_eq!(
            restore_action(&previous),
            RestoreAction::Put("привет, мир".to_string())
        );
    }

    #[test]
    fn empty_string_is_text_not_cleared_buffer() {
        // Пустая строка — валидный текст: её возвращаем текстом, а не очисткой.
        assert_eq!(
            restore_action(&ReadOutcome::Text(String::new())),
            RestoreAction::Put(String::new())
        );
    }

    #[test]
    fn proven_empty_buffer_is_cleared() {
        // Только доказанная пустота (нет ни одного формата) даёт право чистить.
        assert_eq!(restore_action(&ReadOutcome::Empty), RestoreAction::Clear);
    }

    #[test]
    fn unknown_or_non_text_content_is_never_wiped() {
        // Картинка/файлы/GlobalSize = 0: неизвестное не стираем.
        assert_eq!(restore_action(&ReadOutcome::Unavailable), RestoreAction::Leave);
    }

    #[test]
    fn stale_generation_stops_restore() {
        assert!(restore_is_current(7, 7));
        assert!(!restore_is_current(7, 8));
        assert!(!restore_is_current(7, 0));
    }
}

#[cfg(test)]
mod double_shift_tests {
    use super::*;

    #[test]
    fn accepts_second_press_within_window() {
        assert!(is_double_shift(1_000, 1_000, false, false, false, false));
        assert!(is_double_shift(1_000, 1_400, false, false, false, false));
    }

    #[test]
    fn rejects_slow_second_press_and_first_press() {
        assert!(!is_double_shift(1_000, 1_401, false, false, false, false));
        // Ноль — нажатий ещё не было: первое нажатие двойным не считается.
        assert!(!is_double_shift(0, 1_000, false, false, false, false));
    }

    #[test]
    fn rejects_when_sequence_was_broken_or_modifier_held() {
        // Другая клавиша или колесо между нажатиями.
        assert!(!is_double_shift(1_000, 1_100, true, false, false, false));
        assert!(!is_double_shift(1_000, 1_100, false, true, false, false));
        assert!(!is_double_shift(1_000, 1_100, false, false, true, false));
        assert!(!is_double_shift(1_000, 1_100, false, false, false, true));
    }
}
