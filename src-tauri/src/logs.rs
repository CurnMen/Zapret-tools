//! Лог свитчера и диагностика: единая точка записи, ротация и чтение.
//!
//! Модуль намеренно не знает о свитчере: сюда пишут хук, рабочие потоки правки
//! и сетевые команды.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Паника в любом потоке попадает в лог свитчера и в окно.
///
/// По умолчанию паника в фоновом потоке только печатается в stderr, которого
/// у GUI-сборки нет: свитчер молча перестаёт работать, и причину не найти.
pub(crate) fn install_panic_hook(app: &tauri::AppHandle) {
    use tauri::Emitter;

    let app = app.clone();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let place = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()))
            .unwrap_or_else(|| "неизвестное место".to_string());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "без текста".to_string());
        // Хук не должен паниковать сам: только запись в лог и событие в окно.
        switcher_log(format!("ПАНИКА {place}: {message}"));
        let _ = app.emit(
            "switcher-error",
            format!("Внутренняя ошибка ({place}): {message}. Подробности — в диагностике."),
        );
        default_hook(info);
    }));
}

/// Предел размера лога: свитчер живёт в трее месяцами и без ограничения
/// файл растёт без предела.
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Локальное время для строки лога: `2026-11-21 18:05:34.889`.
///
/// epoch-миллисекунды нечитаемы и не позволяют быстро понять свежесть
/// записи; отдельная зависимость ради форматирования не нужна — GetLocalTime
/// есть в Windows.
#[cfg(windows)]
fn log_timestamp() -> String {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::SystemInformation::GetLocalTime;

    // SAFETY: GetLocalTime заполняет собственную структуру и не падает.
    let st: SYSTEMTIME = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}

#[cfg(not(windows))]
fn log_timestamp() -> String {
    format!("{}", now_ms())
}

/// Переименовывает лог в `.log.1`, когда он перерос порог.
///
/// Вызывается под LOG_LOCK и до открытия файла: Windows не даёт переименовать
/// открытый файл, а прежний `.1` просто заменяется.
fn rotate_log(path: &Path) {
    let too_big = fs::metadata(path)
        .map(|meta| meta.len() >= LOG_MAX_BYTES)
        .unwrap_or(false);
    if !too_big {
        return;
    }
    let backup = path.with_extension("log.1");
    let _ = fs::remove_file(&backup);
    let _ = fs::rename(path, &backup);
}

/// Путь к логу свитчера: один источник для записи и для чтения.
pub(crate) fn log_path() -> PathBuf {
    std::env::temp_dir().join("zapret-tools-switcher.log")
}

/// Последние строки лога для вкладки «Диагностика».
#[tauri::command]
pub(crate) async fn get_log_tail(lines: usize) -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(move || {
        let path = log_path();
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        // Старые сборки могли писать не-UTF-8: терять весь лог из-за одной
        // строки нельзя.
        let text = String::from_utf8_lossy(&bytes);
        let mut tail: Vec<String> = text
            .lines()
            .rev()
            .take(lines.clamp(1, 500))
            .map(str::to_string)
            .collect();
        tail.reverse();
        Ok(tail)
    })
    .await
    .map_err(|err| format!("не удалось прочитать лог: {err}"))?
}

/// Открывает папку с логом и выделяет сам файл.
#[tauri::command]
pub(crate) fn open_switcher_log() -> Result<(), String> {
    let path = log_path();
    if !path.exists() {
        return Err("Лог пока пуст: свитчер ещё не запускался.".into());
    }
    Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Диагностика свитчера. В GUI-сборке stderr никуда не выводится,
/// поэтому параллельно пишем файл в TEMP.
pub(crate) fn switcher_log(message: impl AsRef<str>) {
    use std::io::Write;
    use std::sync::Mutex;

    let message = message.as_ref();
    eprintln!("switcher: {message}");

    static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
    // Сообщения приходят из хука, из рабочих потоков правки и из потоков
    // восстановления буфера. Без блокировки части строк перемешивались в файле
    // (в логе видны склеенные строки с двумя таймстампами).
    static LOG_LOCK: Mutex<()> = Mutex::new(());

    let path = LOG_PATH.get_or_init(log_path);
    // Отравленный мьютекс не повод терять сообщение: логирование продолжается.
    let _guard = LOG_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    // Строку собираем целиком и пишем одним write_all — тогда даже короткая
    // запись одной ниткой не вклинится в середину чужой.
    rotate_log(path);
    let line = format!("[{}] {message}\n", log_timestamp());
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}
