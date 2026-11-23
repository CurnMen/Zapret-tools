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

/// Декодирует вывод консольного процесса.
///
/// PowerShell 5.1 при перенаправлении stdout/stderr пишет в OEM-кодировке
/// (cp866), а не в UTF-8: кириллица иначе превращается в «����». Поэтому
/// сначала пробуем UTF-8, затем OEM-кодировку системы.
pub(crate) fn decode_console_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    #[cfg(windows)]
    if let Some(text) = decode_oem(bytes) {
        return text;
    }
    String::from_utf8_lossy(bytes).to_string()
}

#[cfg(windows)]
fn decode_oem(bytes: &[u8]) -> Option<String> {
    use windows::Win32::Globalization::CP_OEMCP;
    decode_with_code_page(CP_OEMCP, bytes)
}

/// Декодирует байты в заданной кодовой странице Windows.
///
/// Вынесено отдельно, чтобы тест не зависел от системной OEM-кодировки:
/// на англоязычной Windows `CP_OEMCP` — это 437, а не 866.
#[cfg(windows)]
fn decode_with_code_page(code_page: u32, bytes: &[u8]) -> Option<String> {
    use windows::Win32::Globalization::{MultiByteToWideChar, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};

    if bytes.len() > i32::MAX as usize {
        return None;
    }
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    let len = unsafe { MultiByteToWideChar(code_page, flags, bytes, None) };
    if len <= 0 {
        return None;
    }
    let mut buffer = vec![0u16; len as usize];
    let written = unsafe { MultiByteToWideChar(code_page, flags, bytes, Some(&mut buffer)) };
    if written <= 0 {
        return None;
    }
    buffer.truncate(written as usize);
    Some(String::from_utf16_lossy(&buffer))
}

/// Base64 от UTF-16LE для `-EncodedCommand`.
///
/// Скрипт передаётся прямо в аргументе процесса, а не через .ps1 в %TEMP%:
/// файл в общей временной папке можно было подменить между записью и нажатием
/// «Да» в UAC (TOCTOU), после чего код выполнялся бы с правами администратора.
fn encode_powershell_command(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();

    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let value = (b0 << 16) | (b1 << 8) | b2;
        encoded.push(ALPHABET[(value >> 18) as usize & 63] as char);
        encoded.push(ALPHABET[(value >> 12) as usize & 63] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(value >> 6) as usize & 63] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[value as usize & 63] as char
        } else {
            '='
        });
    }
    encoded
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
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &wrapped,
        ])
        .output()
        .map_err(|e| format!("не удалось запустить PowerShell: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "PowerShell завершился с кодом {}",
                output.status.code().unwrap_or(-1)
            )
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Непредсказуемый суффикс для временных файлов UAC.
fn random_suffix() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default(),
    );
    hasher.finish()
}

/// Выполняет скрипт в отдельном процессе PowerShell с запросом UAC.
/// Результат (успех/текст ошибки) скрипт пишет во временный файл, потому что
/// `Start-Process -Verb RunAs` не умеет перенаправлять stdout.
///
/// Тело скрипта передаётся через `-EncodedCommand`, без файла в %TEMP%:
/// так злоумышленник без прав админа не может подменить код до появления UAC.
pub(crate) fn run_elevated_script(body: &str) -> Result<(), String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join("zapret-tools");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let base = format!(
        "admin-{}-{seq}-{:016x}",
        std::process::id(),
        random_suffix()
    );
    let result_path = dir.join(format!("{base}.log"));
    let outer_path = dir.join(format!("{base}.outer.log"));
    let _ = fs::remove_file(&result_path);
    let _ = fs::remove_file(&outer_path);

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
    let encoded = encode_powershell_command(&script);

    let outer = format!(
        "try {{\r\n\
$argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', {encoded})\r\n\
$p = Start-Process powershell -Verb RunAs -PassThru -Wait -WindowStyle Hidden -ErrorAction Stop -ArgumentList $argList\r\n\
exit $p.ExitCode\r\n\
}} catch {{\r\n\
  ('ERR: ' + $_.Exception.Message) | Set-Content -LiteralPath {outer} -Encoding UTF8\r\n\
  [Console]::Error.WriteLine('ERR: ' + $_.Exception.Message)\r\n\
  exit 1\r\n\
}}\r\n",
        encoded = ps_quote(&encoded),
        outer = ps_quote(&outer_path.to_string_lossy())
    );
    let mut command = Command::new("powershell");
    hide_console(&mut command);
    let output = command
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &outer,
        ])
        .output()
        .map_err(|e| format!("не удалось запустить PowerShell: {e}"))?;

    let report = fs::read_to_string(&result_path).unwrap_or_default();
    let report = report.trim_start_matches('\u{feff}').trim().to_string();
    let outer_report = fs::read_to_string(&outer_path).unwrap_or_default();
    let outer_report = outer_report
        .trim_start_matches('\u{feff}')
        .trim()
        .to_string();
    let _ = fs::remove_file(&result_path);
    let _ = fs::remove_file(&outer_path);

    if output.status.success() && report.starts_with("OK") {
        return Ok(());
    }

    let stderr = decode_console_output(&output.stderr).trim().to_string();
    let stderr = stderr
        .strip_prefix("ERR:")
        .map(str::trim)
        .unwrap_or(&stderr)
        .to_string();
    let detail = if let Some(rest) = report.strip_prefix("ERR:") {
        rest.trim().to_string()
    } else if let Some(rest) = outer_report.strip_prefix("ERR:") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_utf16le_base64_for_encoded_command() {
        assert_eq!(encode_powershell_command(""), "");
        // Так же, как [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes(...)).
        assert_eq!(encode_powershell_command("A"), "QQA=");
        assert_eq!(encode_powershell_command("AB"), "QQBCAA==");
        // «Й» = U+0419: байты UTF-16LE 19 04 -> Base64 GQQ=.
        assert_eq!(encode_powershell_command("Й"), "GQQ=");
    }

    #[cfg(windows)]
    #[test]
    fn decodes_oem_console_output() {
        // cp866 «Привет»: проверяем явную кодовую страницу, иначе результат
        // зависит от системной OEM-кодировки (437 на англоязычной Windows).
        let oem = [0x8F, 0xE0, 0xA8, 0xA2, 0xA5, 0xE2];
        assert_eq!(decode_with_code_page(866, &oem).as_deref(), Some("Привет"));
        assert_eq!(decode_console_output("Привет".as_bytes()), "Привет");
        assert_eq!(decode_console_output(b"plain ascii"), "plain ascii");
        assert_eq!(decode_console_output(&[]), "");
    }
}
