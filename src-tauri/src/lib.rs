use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Command;
use std::str::FromStr;
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
    pub root_path: Option<String>,
    pub active_version: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileInfo {
    pub name: String,
    pub lines: usize,
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
#[tauri::command]
fn get_config(app: tauri::AppHandle) -> Result<Config, String> {
    let config_path = app.path().app_data_dir().unwrap().join("config.json");
    if config_path.exists() {
        let text = fs::read_to_string(config_path).map_err(|e| e.to_string())?;
        Ok(serde_json::from_str(&text).unwrap_or(Config { root_path: None, active_version: None }))
    } else {
        Ok(Config { root_path: None, active_version: None })
    }
}

#[tauri::command]
fn save_config(app: tauri::AppHandle, config: Config) -> Result<(), String> {
    let dir = app.path().app_data_dir().unwrap();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("config.json"), serde_json::to_string_pretty(&config).unwrap()).map_err(|e| e.to_string())
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

        let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let trash_id = format!("trash_{}.json", timestamp);
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
fn restore_trash_file(root_path: String, version_path: String, trash_id: String, original_name: String, deleted_lines: Vec<String>) -> Result<(), String> {
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

    for line in deleted_lines {
        current_content.push_str(&line);
        current_content.push('\n');
    }

    fs::write(&target, current_content).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(trash_file);
    Ok(())
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
            let mut check_line = line;
            if check_line.starts_with('^') { check_line = &check_line[1..]; }
            if let Some(pos) = check_line.find(':') { check_line = &check_line[pos + 1..]; }
            if let Some(pos) = check_line.find('@') { check_line = &check_line[..pos]; }

            if !domain_regex.is_match(check_line.trim()) {
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

        let mut check_domain = t.to_string();

        if !check_domain.is_ascii() {
            if let Ok(puny) = idna::domain_to_ascii(&check_domain) {
                check_domain = puny;
                converted += 1;
            } else {
                invalid.push(t.to_string());
                continue;
            }
        }

        if domain_regex.is_match(&check_domain) {
            valid.push(check_domain);
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

        let mut check_domain = trimmed.to_string();
        if !is_ip_list && !check_domain.is_ascii() {
            if let Ok(puny) = idna::domain_to_ascii(&check_domain) {
                check_domain = puny;
            }
        }

        let lower = check_domain.to_lowercase();
        if seen.insert(lower) {
            cleaned_lines.push(check_domain);
        } else {
            removed_count += 1;
        }
    }

    cleaned_lines.sort();

    let final_text = cleaned_lines.join("\n") + if cleaned_lines.is_empty() { "" } else { "\n" };
    let errors = validate_list(final_text.clone(), is_ip_list)?;

    Ok((final_text, errors, removed_count))
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
    if new_dir.exists() { let _ = fs::remove_dir_all(&new_dir); }
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

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .setup(|app| {
            // Скрываем окно при автозапуске
            let args: Vec<String> = std::env::args().collect();
            if args.contains(&"--minimized".to_string()) {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            // Создаем меню для трея
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
            get_trash_files, restore_trash_file, empty_trash // <-- ДОБАВЛЕНЫ КОМАНДЫ КОРЗИНЫ
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}