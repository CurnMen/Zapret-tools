//! Запуск внешних процессов: скрытие консоли, PowerShell без прав и с UAC.

use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Скрывает консольное окно дочернего процесса (CREATE_NO_WINDOW),
/// чтобы cmd/powershell не мигали поверх интерфейса.
#[cfg(windows)]
pub(crate) fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
pub(crate) fn hide_console(_command: &mut Command) {}

/// Экранирование строки для одинарных кавычек PowerShell.
pub(crate) fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Запускает PowerShell без повышения прав и возвращает stdout (UTF-8).
pub(crate) fn run_powershell(script: &str) -> Result<String, String> {
    let wrapped = format!(
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding -ArgumentList $false\r\n{}",
        script
    );
    let mut command = Command::new("powershell");
    hide_console(&mut command);
    let output = command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &wrapped])
        .output()
        .map_err(|e| format!("не удалось запустить PowerShell: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("PowerShell завершился с кодом {}", output.status.code().unwrap_or(-1))
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Выполняет скрипт в отдельном процессе PowerShell с запросом UAC.
/// Результат (успех/текст ошибки) скрипт пишет во временный файл, потому что
/// `Start-Process -Verb RunAs` не умеет перенаправлять stdout.
pub(crate) fn run_elevated_script(body: &str) -> Result<(), String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join("zapret-tools");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let base = format!("admin-{}-{seq}", std::process::id());
    let script_path = dir.join(format!("{base}.ps1"));
    let result_path = dir.join(format!("{base}.log"));

    let script = format!(
        "$ErrorActionPreference = 'Stop'\r\n\
$ProgressPreference = 'SilentlyContinue'\r\n\
$resultPath = {result}\r\n\
try {{\r\n\
{body}\r\n\
  'OK' | Set-Content -LiteralPath $resultPath -Encoding UTF8\r\n\
}} catch {{\r\n\
  ('ERR: ' + $_.Exception.Message) | Set-Content -LiteralPath $resultPath -Encoding UTF8\r\n\
  exit 1\r\n\
}}\r\n",
        result = ps_quote(&result_path.to_string_lossy()),
        body = body
    );
    // UTF-8 с BOM: PowerShell 5.1 иначе читает .ps1 как ANSI и ломает
    // кириллицу в сообщениях об ошибках.
    let mut script_bytes = Vec::with_capacity(script.len() + 3);
    script_bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    script_bytes.extend_from_slice(script.as_bytes());
    fs::write(&script_path, script_bytes).map_err(|e| format!("{}: {e}", script_path.display()))?;

    let outer = format!(
        "try {{\r\n\
$argList = '-NoProfile -ExecutionPolicy Bypass -File \"' + {script} + '\"'\r\n\
$p = Start-Process powershell -Verb RunAs -PassThru -Wait -WindowStyle Hidden -ErrorAction Stop -ArgumentList $argList\r\n\
exit $p.ExitCode\r\n\
}} catch {{\r\n\
[Console]::Error.WriteLine('ERR: ' + $_.Exception.Message)\r\n\
exit 1\r\n\
}}\r\n",
        script = ps_quote(&script_path.to_string_lossy())
    );
    let mut command = Command::new("powershell");
    hide_console(&mut command);
    let output = command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &outer])
        .output()
        .map_err(|e| format!("не удалось запустить PowerShell: {e}"))?;

    let report = fs::read_to_string(&result_path).unwrap_or_default();
    let report = report.trim_start_matches('\u{feff}').trim().to_string();
    let _ = fs::remove_file(&script_path);
    let _ = fs::remove_file(&result_path);

    if output.status.success() && report.starts_with("OK") {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stderr = stderr.strip_prefix("ERR:").map(str::trim).unwrap_or(&stderr).to_string();
    let detail = if let Some(rest) = report.strip_prefix("ERR:") {
        rest.trim().to_string()
    } else if !stderr.is_empty() {
        stderr
    } else {
        String::new()
    };
    let lowered = detail.to_lowercase();
    if lowered.contains("cancel") || lowered.contains("отмен") {
        return Err("Запрос прав администратора отклонён".into());
    }
    if detail.is_empty() {
        return Err(format!(
            "Операция не выполнена (код {})",
            output.status.code().unwrap_or(-1)
        ));
    }
    Err(detail)
}

