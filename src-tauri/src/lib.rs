use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
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

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
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
        let output = Command::new("reg").args(&["query", "HKLM\\System\\CurrentControlSet\\Services\\zapret", "/v", "ImagePath"]).output();
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
    Command::new("powershell").args(&["-Command", &format!("Start-Process '{}' -Verb RunAs", bat_path.replace("'", "''"))]).spawn().map_err(|e| e.to_string())?;
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
    Command::new("powershell").args(&["-NoProfile", "-Command", &ps_command]).spawn().map_err(|e| e.to_string())?;
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

        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
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

#[tauri::command]
fn validate_list(content: String, is_ip_list: bool) -> Result<Vec<String>, String> {
    let mut errors = Vec::new();
    let domain_regex = regex::Regex::new(r"^[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(\.[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)+$").unwrap();

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
            if body.is_empty() || !is_valid_domain_body(&body, &domain_regex) {
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
    let domain_regex = regex::Regex::new(r"^[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(\.[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)+$").unwrap();

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

        if is_valid_domain_body(&body_out, &domain_regex) {
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
        let re = regex::Regex::new(r#"LOCAL_VERSION=([\d\.]+)"#).unwrap();
        if let Some(caps) = re.captures(&content) { local_version = caps[1].to_string(); }
    }
    let res = reqwest::get("https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/main/.service/version.txt").await.map_err(|e| e.to_string())?;
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
    let status = Command::new("powershell").args(&["-NoProfile", "-Command", &ps_cmd]).status();
    if status.is_err() || !status.unwrap().success() { return Err("Не удалось распаковать архив".into()); }

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
    Command::new("powershell").args(&["-NoProfile", "-Command", &ps_script]).status().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn toggle_game_filter(version_path: String, enable: bool) -> Result<(), String> {
    let utils_dir = Path::new(&version_path).join("utils");
    if !utils_dir.exists() { fs::create_dir_all(&utils_dir).map_err(|e| e.to_string())?; }
    let flag_file = utils_dir.join("game_filter.enabled");
    if enable { fs::write(flag_file, "ENABLED").map_err(|e| e.to_string())?; }
    else { if flag_file.exists() { fs::remove_file(flag_file).map_err(|e| e.to_string())?; } }
    Ok(())
}

#[tauri::command]
fn get_game_filter_status(version_path: String) -> bool {
    Path::new(&version_path).join("utils").join("game_filter.enabled").exists()
}

#[tauri::command]
fn toggle_ipset_filter(version_path: String, enable: bool) -> Result<(), String> {
    let lists_dir = Path::new(&version_path).join("lists");
    let list_file = lists_dir.join("ipset-all.txt");
    let backup_file = lists_dir.join("ipset-all.txt.backup");

    if enable {
        if backup_file.exists() {
            if list_file.exists() { let _ = fs::remove_file(&list_file); }
            fs::rename(&backup_file, &list_file).map_err(|e| e.to_string())?;
        }
    } else {
        if list_file.exists() && !backup_file.exists() {
            fs::rename(&list_file, &backup_file).map_err(|e| e.to_string())?;
        }
        fs::write(&list_file, "203.0.113.113/32\n").map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn get_ipset_filter_status(version_path: String) -> bool {
    let list_file = Path::new(&version_path).join("lists").join("ipset-all.txt");
    if let Ok(content) = fs::read_to_string(list_file) {
        return content.lines().filter(|l| !l.trim().is_empty()).count() > 1;
    }
    false
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
        Command::new("powershell").args(&["-NoProfile", "-Command", &ps_cmd]).spawn().map_err(|e| e.to_string())?;
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

const ENGLISH_DICT_FILE: &str = "google-10000-english.txt";
const RUSSIAN_DICT_FILE: &str = "100_000_russian_wordlist.txt";
const ENGLISH_DICT_URL: &str =
    "https://raw.githubusercontent.com/first20hours/google-10000-english/master/google-10000-english.txt";
const RUSSIAN_DICT_URL: &str =
    "https://raw.githubusercontent.com/hermitdave/FrequencyWords/master/content/2018/ru/ru_50k.txt";

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
    tokio::task::spawn_blocking(running_window_processes)
        .await
        .unwrap_or_default()
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

            let path = String::from_utf16_lossy(&buffer[..size as usize]);
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
    std::thread::spawn(move || run_switcher_listener(shared, app));
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
                                let is_double = last_shift_press_ms != 0
                                    && now.saturating_sub(last_shift_press_ms) <= 400
                                    && !broke_sequence
                                    && !ctrl_down
                                    && !alt_down
                                    && !meta_down;
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
        eprintln!("switcher: не удалось запустить rdev::listen: {err:?}");
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
    let (enabled, exclusions, word_exclusions) = {
        let s = lock_switcher(shared);
        (s.enabled, s.exclusions.clone(), s.word_exclusions.clone())
    };
    if !enabled {
        return None;
    }

    // Исключения: не вмешиваемся в работу выбранных программ.
    if let Some(active) = get_active_process_name() {
        if exclusions.iter().any(|ex| exclusion_matches(&active, ex)) {
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
    let fix = detect_layout_mistake(word, word_russian, dicts)?;

    // Исключением может быть как набранное слово, так и результат замены:
    // пользователь видит в документе именно исправленное слово.
    let corrected_normalized = normalize_for_lookup(&fix.corrected);
    if word_exclusions
        .iter()
        .any(|w| normalize_for_lookup(w) == corrected_normalized)
    {
        return None;
    }

    // Сразу подавляем собственные инжектированные клавиши, чтобы хук
    // не принял их за ввод пользователя и не испортил буфер.
    // 400 мс хватает на Backspace'ы и Ctrl+V; больше — и Double Shift,
    // нажатый сразу после исправления, начал бы «заедать».
    suppress_until.store(now_ms() + 400, Ordering::SeqCst);
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
        suppress_worker.store(if applied { now_ms() + 150 } else { 0 }, Ordering::SeqCst);
    });
    Some(corrected)
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

    suppress_until.store(now_ms() + 400, Ordering::SeqCst);
    let suppress_worker = Arc::clone(suppress_until);
    let app_worker = app.clone();
    std::thread::spawn(move || {
        let applied = replace_word(typed_len, &paste_text, has_space, to_russian);
        if !applied {
            notify_clipboard_failure(&app_worker);
        }
        suppress_worker.store(if applied { now_ms() + 150 } else { 0 }, Ordering::SeqCst);
    });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

/// Знаки препинания, которые в ЙЦУКЕН стоят на буквенных клавишах:
/// `.` → `ю`, `,` → `б`, `[` → `х`, `]` → `ъ`, `;` → `ж`, `'` → `э`, `` ` `` → `ё`.
/// В английской раскладке они выглядят как пунктуация, поэтому слово `.,rf`
/// должно считаться набранным не в той раскладке.
const LATIN_KEYBOARD_PUNCTUATION: [char; 7] = ['[', ']', ';', '\'', ',', '.', '`'];

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
    let (current, opposite) = if current_is_russian {
        (&dicts.russian, &dicts.english)
    } else {
        (&dicts.english, &dicts.russian)
    };

    // Слово есть в текущем языке — трогать не нужно.
    if current.contains(&normalized) {
        return None;
    }

    let corrected = convert_layout(word, !current_is_russian);
    if corrected == word {
        return None;
    }
    let corrected_normalized = normalize_for_lookup(&corrected);
    if corrected_normalized.chars().count() < 2 || !opposite.contains(&corrected_normalized) {
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
    let lower = c.to_ascii_lowercase();
    let mapped = match lower {
        'q' => 'й', 'w' => 'ц', 'e' => 'у', 'r' => 'к', 't' => 'е', 'y' => 'н',
        'u' => 'г', 'i' => 'ш', 'o' => 'щ', 'p' => 'з', '[' => 'х', ']' => 'ъ',
        'a' => 'ф', 's' => 'ы', 'd' => 'в', 'f' => 'а', 'g' => 'п', 'h' => 'р',
        'j' => 'о', 'k' => 'л', 'l' => 'д', ';' => 'ж', '\'' => 'э',
        'z' => 'я', 'x' => 'ч', 'c' => 'с', 'v' => 'м', 'b' => 'и', 'n' => 'т',
        'm' => 'ь', ',' => 'б', '.' => 'ю', '`' => 'ё',
        _ => return None,
    };
    if c.is_ascii_uppercase() {
        Some(mapped.to_uppercase().next().unwrap_or(mapped))
    } else {
        Some(mapped)
    }
}

fn ru_char_to_en(c: char) -> Option<char> {
    let lower = c.to_lowercase().next().unwrap_or(c);
    let mapped = match lower {
        'й' => 'q', 'ц' => 'w', 'у' => 'e', 'к' => 'r', 'е' => 't', 'н' => 'y',
        'г' => 'u', 'ш' => 'i', 'щ' => 'o', 'з' => 'p', 'х' => '[', 'ъ' => ']',
        'ф' => 'a', 'ы' => 's', 'в' => 'd', 'а' => 'f', 'п' => 'g', 'р' => 'h',
        'о' => 'j', 'л' => 'k', 'д' => 'l', 'ж' => ';', 'э' => '\'',
        'я' => 'z', 'ч' => 'x', 'с' => 'c', 'м' => 'v', 'и' => 'b', 'т' => 'n',
        'ь' => 'm', 'б' => ',', 'ю' => '.', 'ё' => '`',
        _ => return None,
    };
    if c.is_uppercase() {
        Some(mapped.to_ascii_uppercase())
    } else {
        Some(mapped)
    }
}

/// Физическая клавиша rdev -> символ с учётом активной раскладки.
/// Возвращает None для цифр, пунктуации (кроме букв ЙЦУКЕН) и служебных клавиш.
fn key_to_char(key: rdev::Key, shift: bool, caps_lock: bool, russian: bool) -> Option<char> {
    use rdev::Key;

    let en_lower = match key {
        Key::KeyQ => 'q', Key::KeyW => 'w', Key::KeyE => 'e', Key::KeyR => 'r',
        Key::KeyT => 't', Key::KeyY => 'y', Key::KeyU => 'u', Key::KeyI => 'i',
        Key::KeyO => 'o', Key::KeyP => 'p', Key::KeyA => 'a', Key::KeyS => 's',
        Key::KeyD => 'd', Key::KeyF => 'f', Key::KeyG => 'g', Key::KeyH => 'h',
        Key::KeyJ => 'j', Key::KeyK => 'k', Key::KeyL => 'l', Key::KeyZ => 'z',
        Key::KeyX => 'x', Key::KeyC => 'c', Key::KeyV => 'v', Key::KeyB => 'b',
        Key::KeyN => 'n', Key::KeyM => 'm',
        Key::LeftBracket => '[', Key::RightBracket => ']', Key::SemiColon => ';',
        Key::Quote => '\'', Key::Comma => ',', Key::Dot => '.', Key::BackQuote => '`',
        _ => return None,
    };

    let base = if russian {
        en_char_to_ru(en_lower).unwrap_or(en_lower)
    } else {
        en_lower
    };

    let upper = if en_lower.is_ascii_alphabetic() {
        shift ^ caps_lock
    } else {
        shift
    };
    let ch = if upper {
        base.to_uppercase().next().unwrap_or(base)
    } else {
        base
    };

    // Буквы, а также клавиши, которые в ЙЦУКЕН дают буквы (`.,rf` → `юбка`).
    // В русской раскладке такие клавиши уже вернули букву выше, поэтому
    // пунктуацию пропускаем только для английской.
    let keep = ch.is_alphabetic() || (!russian && LATIN_KEYBOARD_PUNCTUATION.contains(&ch));
    if keep {
        Some(ch)
    } else {
        None
    }
}

#[cfg(windows)]
fn active_layout_is_russian() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        let thread_id = GetWindowThreadProcessId(hwnd, None);
        if thread_id == 0 {
            return false;
        }
        let hkl = GetKeyboardLayout(thread_id);
        // Младшее слово HKL — идентификатор языка; 0x0419 — ru-RU.
        ((hkl.0 as usize) & 0xFFFF) == 0x0419
    }
}

#[cfg(not(windows))]
fn active_layout_is_russian() -> bool {
    false
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

/// Диагностика свитчера. В GUI-сборке stderr никуда не выводится,
/// поэтому параллельно пишем файл в TEMP.
fn switcher_log(message: impl AsRef<str>) {
    let message = message.as_ref();
    eprintln!("switcher: {message}");

    static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
    let path = LOG_PATH.get_or_init(|| std::env::temp_dir().join("zapret-tools-switcher.log"));
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(file, "[{}] {message}", now_ms());
    }
}

/// Прямая работа с буфером обмена через WinAPI.
///
/// `arboard` периодически не может открыть буфер: Windows отдаёт его занятым
/// на несколько десятков миллисекунд. Здесь три попытки с паузой 50 мс,
/// а все ошибки попадают в лог.
#[cfg(windows)]
mod win_clipboard {
    use std::thread::sleep;
    use std::time::Duration;
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    /// CF_UNICODETEXT из WinUser.h (стабильное значение Win32 API).
    const CF_UNICODETEXT: u32 = 13;
    const OPEN_ATTEMPTS: u32 = 3;
    const RETRY_DELAY: Duration = Duration::from_millis(50);
    /// Предохранитель от незакрытой строки в чужом буфере.
    const MAX_TEXT_CHARS: usize = 1 << 20;

    /// Открывает буфер обмена (с повторами), выполняет операцию и закрывает его.
    fn with_open<T>(operation: impl FnOnce() -> T) -> Option<T> {
        for attempt in 1..=OPEN_ATTEMPTS {
            // SAFETY: обычный вызов WinAPI; буфер закрывается в конце ветки.
            match unsafe { OpenClipboard(None) } {
                Ok(()) => {
                    let result = operation();
                    // SAFETY: буфер был успешно открыт выше.
                    unsafe {
                        let _ = CloseClipboard();
                    }
                    return Some(result);
                }
                Err(err) => {
                    crate::switcher_log(format!(
                        "clipboard: OpenClipboard, попытка {attempt}/{OPEN_ATTEMPTS}: {err}"
                    ));
                    if attempt < OPEN_ATTEMPTS {
                        sleep(RETRY_DELAY);
                    }
                }
            }
        }
        None
    }

    /// Читает Unicode-текст. `None` — буфер недоступен или в нём не текст.
    pub fn get_text() -> Option<String> {
        with_open(|| {
            // SAFETY: работаем только с валидным буфером, открытым в with_open.
            unsafe {
                let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
                let global = HGLOBAL(handle.0);
                let ptr = GlobalLock(global) as *const u16;
                if ptr.is_null() {
                    return None;
                }

                let mut len = 0usize;
                while len < MAX_TEXT_CHARS && *ptr.add(len) != 0 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(global);
                Some(text)
            }
        })
        .flatten()
    }

    /// Кладёт Unicode-текст в буфер. `false` — не удалось после всех попыток.
    pub fn set_text(text: &str) -> bool {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = wide.len() * std::mem::size_of::<u16>();

        with_open(|| {
            // SAFETY: буфер открыт; память под строку выделяем через GlobalAlloc,
            // после успешного SetClipboardData владельцем становится система.
            unsafe {
                if let Err(err) = EmptyClipboard() {
                    crate::switcher_log(format!("clipboard: EmptyClipboard: {err}"));
                    return false;
                }

                let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
                    crate::switcher_log("clipboard: GlobalAlloc не удался");
                    return false;
                };

                let ptr = GlobalLock(hmem) as *mut u16;
                if ptr.is_null() {
                    crate::switcher_log("clipboard: GlobalLock не удался");
                    let _ = GlobalFree(hmem);
                    return false;
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                let _ = GlobalUnlock(hmem);

                if SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0)).is_err() {
                    crate::switcher_log("clipboard: SetClipboardData не удался");
                    let _ = GlobalFree(hmem);
                    return false;
                }
                true
            }
        })
        .unwrap_or(false)
    }

    /// Полностью очищает буфер (если прежнее содержимое восстановить нельзя).
    pub fn clear() -> bool {
        with_open(|| {
            // SAFETY: буфер открыт в with_open.
            unsafe { EmptyClipboard().is_ok() }
        })
        .unwrap_or(false)
    }
}

/// Содержимое буфера обмена до подмены (нужно, чтобы вернуть его обратно).
#[cfg(windows)]
struct SavedClipboard {
    text: Option<String>,
}

/// Кладёт текст в буфер обмена и возвращает прежнее содержимое.
/// `None` — буфер занят другим приложением или недоступен.
#[cfg(windows)]
fn clipboard_put(text: &str) -> Option<SavedClipboard> {
    let previous = win_clipboard::get_text();
    if !win_clipboard::set_text(text) {
        switcher_log("clipboard: не удалось положить исправленный текст");
        return None;
    }
    Some(SavedClipboard { text: previous })
}

/// Возвращает прежнее содержимое буфера с задержкой: приложению нужно
/// успеть прочитать вставленный текст.
#[cfg(windows)]
fn restore_clipboard(saved: SavedClipboard) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        match saved.text {
            Some(text) => {
                if !win_clipboard::set_text(&text) {
                    switcher_log("clipboard: не удалось восстановить прежний текст");
                }
            }
            // Текст прочитать не удалось (в буфере было что-то другое) —
            // лучше очистить, чем оставить там наше слово.
            None => {
                if !win_clipboard::clear() {
                    switcher_log("clipboard: не удалось очистить буфер");
                }
            }
        }
    });
}

/// Отправляет Ctrl+<виртуальная клавиша>.
///
/// Ctrl отпускается в любом случае: явным `Release`, а если что-то пойдёт
/// не так — `Drop` у `Enigo` (в `Settings::default()` включён
/// `release_keys_when_dropped`). Именно незакрытый Ctrl давал «фантомный»
/// Ctrl+F в браузере.
#[cfg(windows)]
fn ctrl_key(enigo: &mut enigo::Enigo, vk: u32, label: &str) -> bool {
    use enigo::{Direction, Key, Keyboard};

    // Double Shift срабатывает, пока второй Shift ещё физически зажат.
    // Если сразу отправить Ctrl+C, Windows увидит Ctrl+Shift+C (в браузерах
    // это DevTools), а Ctrl+V превратится в Ctrl+Shift+V. Поэтому сначала
    // принудительно отпускаем Shift/Alt/Win и даём приложению зафиксировать
    // отпускание.
    for key in [Key::Shift, Key::Alt, Key::Meta] {
        let _ = enigo.key(key, Direction::Release);
    }
    std::thread::sleep(Duration::from_millis(20));

    let mut ok = match enigo.key(Key::Control, Direction::Press) {
        Ok(()) => true,
        Err(err) => {
            switcher_log(format!("{label}: Ctrl down не удался: {err}"));
            false
        }
    };

    if ok {
        // Небольшая пауза, чтобы приложение успело зафиксировать модификатор.
        std::thread::sleep(Duration::from_millis(15));
        if let Err(err) = enigo.key(Key::Other(vk), Direction::Click) {
            switcher_log(format!("{label}: клавиша не нажалась: {err}"));
            ok = false;
        }
    }

    // Ctrl отпускаем в любом случае.
    if let Err(err) = enigo.key(Key::Control, Direction::Release) {
        switcher_log(format!("{label}: Ctrl up не удался: {err}"));
        ok = false;
    }
    ok
}

/// Отправляет Ctrl+V.
///
/// VK_V (0x56) — виртуальный код клавиши V, он не зависит от раскладки.
/// Key::Unicode('v') использовать нельзя: VkKeyScanExW('v') в русской
/// раскладке возвращает 0xFFFF, и enigo 0.3 вставляет символ как текст,
/// из-за чего Ctrl+V не срабатывает.
#[cfg(windows)]
fn paste_clipboard(enigo: &mut enigo::Enigo) -> bool {
    ctrl_key(enigo, 0x56, "paste")
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
    let original = win_clipboard::get_text();
    if !win_clipboard::clear() {
        switcher_log("selection: не удалось очистить буфер обмена");
        return false;
    }

    let restore = || restore_clipboard(SavedClipboard { text: original.clone() });

    let Ok(mut enigo) = Enigo::new(&Settings::default()) else {
        switcher_log("selection: не удалось создать Enigo");
        restore();
        return false;
    };

    // 2. Копируем выделение. Ctrl+C в приложении без выделения просто
    //    ничего не положит в буфер.
    let copied = if ctrl_key(&mut enigo, 0x43, "copy") {
        let mut value = None;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(20));
            if let Some(text) = win_clipboard::get_text() {
                if !text.trim().is_empty() {
                    value = Some(text);
                    break;
                }
            }
        }
        value
    } else {
        None
    };

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
    suppress_until.store(now_ms() + 1_500, Ordering::SeqCst);
    let suppress_worker = Arc::clone(suppress_until);
    std::thread::spawn(move || {
        let converted = convert_selection_blocking();
        if !converted {
            switcher_log("selection: выделение не преобразовано");
        }
        // Не оставляем подавление «висеть»: пользователь может печатать сразу.
        suppress_worker.store(if converted { now_ms() + 150 } else { 0 }, Ordering::SeqCst);
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

    // 3. Вставляем через Ctrl+V.
    let pasted = paste_clipboard(&mut enigo);

    // 4. Ctrl+V не сработал — вводим текст одним SendInput через Unicode
    //    (enigo 0.3 собирает все события в один батч, это не посимвольные лаги).
    if !pasted {
        switcher_log("paste: Ctrl+V не отправился, вводим текст через enigo.text()");
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

fn candidate_dict_paths(app: &tauri::AppHandle, file_name: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        // Ищем рядом с exe и до 6 уровней выше: dev-сборка лежит в
        // src-tauri/target/debug, а словари — в корне проекта.
        for _ in 0..6 {
            let Some(current) = dir.clone() else { break };
            candidates.push(current.join(file_name));
            candidates.push(current.join("dictionaries").join(file_name));
            dir = current.parent().map(|p| p.to_path_buf());
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(file_name));
        candidates.push(cwd.join("dictionaries").join(file_name));
        candidates.push(cwd.join("..").join(file_name));
        candidates.push(cwd.join("..").join("dictionaries").join(file_name));
    }

    if let Ok(resources) = app.path().resource_dir() {
        candidates.push(resources.join(file_name));
        candidates.push(resources.join("dictionaries").join(file_name));
    }

    candidates
}

fn read_dictionary_text(app: &tauri::AppHandle, file_name: &str, url: &str) -> Result<String, String> {
    // 1. Готовый файл рядом с приложением или в ресурсах.
    for path in candidate_dict_paths(app, file_name) {
        if path.is_file() {
            return fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()));
        }
    }

    // 2. Скачиваем в app_data_dir/dictionaries при первом запуске.
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("не удалось получить папку данных: {e}"))?
        .join("dictionaries");
    let target = data_dir.join(file_name);
    if target.is_file() {
        return fs::read_to_string(&target).map_err(|e| format!("{}: {e}", target.display()));
    }

    fs::create_dir_all(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .user_agent("ZapretTools/1.0")
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(url).send().map_err(|e| format!("сеть: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let text = response.text().map_err(|e| format!("чтение ответа: {e}"))?;
    fs::write(&target, &text).map_err(|e| format!("{}: {e}", target.display()))?;
    Ok(text)
}

fn load_dictionaries(app: &tauri::AppHandle) -> Result<Dictionaries, String> {
    let english_text = read_dictionary_text(app, ENGLISH_DICT_FILE, ENGLISH_DICT_URL)?;
    let russian_text = read_dictionary_text(app, RUSSIAN_DICT_FILE, RUSSIAN_DICT_URL)?;

    let english = parse_english_dictionary(&english_text);
    let russian = parse_russian_dictionary(&russian_text);

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
    unsafe { IsUserAnAdmin().as_bool() }
}

#[cfg(not(windows))]
fn is_running_as_admin() -> bool {
    true
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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
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
                admin,
            }));
            app.manage(Arc::clone(&switcher_state));

            // Словари грузим в фоне: старт приложения не должен ждать диск/сеть.
            let dict_app = app.handle().clone();
            std::thread::spawn(move || {
                use tauri::Emitter;
                match load_dictionaries(&dict_app) {
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

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
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
                        "start" => { let _ = Command::new("powershell").args(&["-NoProfile", "-Command", "Start-Process sc -ArgumentList 'start zapret' -Verb RunAs"]).spawn(); }
                        "stop" => { let _ = Command::new("powershell").args(&["-NoProfile", "-Command", "Start-Process sc -ArgumentList 'stop zapret' -Verb RunAs"]).spawn(); }
                        "quit" => { app.exit(0); }
                        _ => {}
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                window.hide().unwrap();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config, save_config, scan_versions, find_running_winws,
            open_folder, run_as_admin, manage_service, read_list_files, read_list_file,
            write_list_file, merge_folders, sync_github, validate_list,
            check_zapret_version, download_zapret, delete_version, create_start_menu_shortcut,
            read_external_file, toggle_game_filter, get_game_filter_status,
            toggle_ipset_filter, get_ipset_filter_status, list_strategies, install_strategy,
            smart_parse_domains, clean_and_validate,
            consolidate_subdomains, check_dead_domains,
            get_trash_files, restore_trash_file, empty_trash,
            update_switcher_config, get_switcher_config, start_switcher_listener,
            add_switcher_word_exclusion, remove_switcher_word_exclusion,
            get_running_processes
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
        // Цифры и прочие знаки по-прежнему разрывают слово.
        assert_eq!(key_to_char(Key::Num1, false, false, false), None);
        assert_eq!(key_to_char(Key::Slash, false, false, false), None);
    }

    #[test]
    fn detects_word_with_punctuation_keys() {
        let dicts = Dictionaries {
            english: parse_english_dictionary("hello\nworld\n"),
            russian: parse_russian_dictionary("привет 100\nюбка 50\n"),
        };

        let fix = detect_layout_mistake(".,rf", false, &dicts).expect("must detect");
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
    fn detects_wrong_layout_word() {
        let dicts = Dictionaries {
            english: parse_english_dictionary("hello\nworld\n"),
            russian: parse_russian_dictionary("привет 100\nмир 50\n"),
        };

        let fix = detect_layout_mistake("ghbdtn", false, &dicts).expect("must detect");
        assert_eq!(fix.corrected, "привет");
        assert!(fix.to_russian);
        assert_eq!(fix.typed_len, 6);

        // Слово есть в текущем словаре — не трогаем.
        assert!(detect_layout_mistake("hello", false, &dicts).is_none());
        // Нет ни в одном словаре — тоже не трогаем.
        assert!(detect_layout_mistake("zzzzzz", false, &dicts).is_none());
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
