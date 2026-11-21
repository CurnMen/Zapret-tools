//! Прямая работа с буфером обмена через WinAPI.
//!
//! `arboard` периодически не может открыть буфер: Windows отдаёт его занятым
//! на несколько десятков миллисекунд. Здесь три попытки с паузой 50 мс,
//! а все ошибки попадают в лог.
//!
//! Инвариант модуля: любой `unsafe`-блок выполняется только внутри успешно
//! открытого буфера. `with_open` парно вызывает `OpenClipboard`/`CloseClipboard`
//! на всех путях, включая ранние `return`, поэтому хэндл буфера не утекает.
//! Память, выделенная под нашу строку, либо передаётся системе вместе с
//! `SetClipboardData`, либо освобождается `GlobalFree` — третьего пути нет.
//!
//! Второй инвариант: «прочитать не удалось» и «в буфере пусто» — разные
//! состояния (`ReadOutcome`). Владелец буфера может на мгновение отдавать блок
//! нулевого размера, а картинка и пустой буфер одинаково не отдают текст,
//! поэтому пустота подтверждается через `EnumClipboardFormats`: только ноль
//! форматов означает, что стирать в буфере действительно нечего.

use std::mem::size_of;
use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use crate::switcher_log;

/// CF_UNICODETEXT из WinUser.h (стабильное значение Win32 API).
const CF_UNICODETEXT: u32 = 13;
const OPEN_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(50);
/// Предохранитель от незакрытой строки в чужом буфере.
const MAX_TEXT_CHARS: usize = 1 << 20;

/// Что удалось узнать о содержимом буфера обмена.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// В буфере лежал непустой текст.
    Text(String),
    /// Буфер точно пуст: в нём нет ни одного формата или CF_UNICODETEXT
    /// содержит только терминатор. Восстанавливать нечего, поэтому вызывающий
    /// код может безопасно очистить буфер.
    Empty,
    /// Содержимое неизвестно или это не текст: буфер не открылся, в нём лежит
    /// картинка или файлы, размер блока невалиден. Стирать буфер нельзя — то,
    /// что мы не сохранили, восстановить не получится.
    Unavailable,
}

/// Открывает буфер обмена (с повторами), выполняет операцию и закрывает его.
fn with_open<T>(operation: impl FnOnce() -> T) -> Option<T> {
    for attempt in 1..=OPEN_ATTEMPTS {
        // SAFETY: обычный вызов WinAPI без указателей; буфер закрывается
        // в ветке успеха ниже.
        match unsafe { OpenClipboard(None) } {
            Ok(()) => {
                let result = operation();
                // SAFETY: буфер был успешно открыт выше и ещё не закрыт.
                unsafe {
                    let _ = CloseClipboard();
                }
                return Some(result);
            }
            Err(err) => {
                switcher_log(format!(
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

/// Сколько элементов `u16` можно прочитать из блока размером `bytes`.
///
/// `None` — размер невалиден: меньше одного `u16`, то есть владелец буфера
/// ещё не заполнил блок. Пустая строка — это терминатор, ровно 2 байта:
/// такой блок валиден и означает «в буфере лежит пустой текст».
fn capacity_for(bytes: usize) -> Option<usize> {
    if bytes < size_of::<u16>() {
        return None;
    }
    Some((bytes / size_of::<u16>()).min(MAX_TEXT_CHARS))
}

/// Читает содержимое буфера, различая «прочитать не удалось» и «пусто».
pub fn read_text() -> ReadOutcome {
    with_open(|| {
        // SAFETY: буфер открыт в with_open. Хэндл из GetClipboardData
        // принадлежит буферу обмена — освобождать его нельзя.
        unsafe {
            let Ok(handle) = GetClipboardData(CF_UNICODETEXT) else {
                // Отсутствие текста ещё ничего не говорит о самом буфере:
                // пустой буфер и буфер с картинкой отвечают одинаково.
                // SAFETY: буфер открыт в with_open, перечисление форматов
                // допустимо; 0 — это «форматов нет вовсе».
                if EnumClipboardFormats(0) == 0 {
                    // Ни одного формата — буфер точно пуст, терять нечего.
                    return ReadOutcome::Empty;
                }
                // Форматы есть, но не текст (картинка, файлы, чужой формат):
                // мы это не сохраняли и восстановить не сможем.
                switcher_log("clipboard: в буфере нет текста, но есть другие форматы");
                return ReadOutcome::Unavailable;
            };
            let global = HGLOBAL(handle.0);

            // Длину ограничиваем реальным размером блока: в чужом буфере может
            // лежать строка без нулевого терминатора, и поиск «до первого нуля»
            // вышел бы за пределы выделенной памяти (чтение мусора, а не текста).
            let bytes = GlobalSize(global);
            // GlobalSize == 0 (и любой размер меньше одного u16) — невалидный
            // блок, а не пустой буфер: Windows отдаёт его так на мгновение,
            // пока владелец заполняет данные. Читать нечего, но и считать
            // буфер пустым нельзя — иначе вызывающий код сотрёт содержимое.
            let Some(capacity) = capacity_for(bytes) else {
                switcher_log(format!(
                    "clipboard: GlobalSize вернул {bytes} байт, содержимое не читаем"
                ));
                return ReadOutcome::Unavailable;
            };

            // SAFETY: GlobalLock отдаёт указатель на блок размером не меньше
            // `bytes`; читаем не больше `capacity` элементов по 2 байта.
            let ptr = GlobalLock(global) as *const u16;
            if ptr.is_null() {
                switcher_log("clipboard: GlobalLock вернул null");
                return ReadOutcome::Unavailable;
            }

            let mut len = 0usize;
            while len < capacity && *ptr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            // SAFETY: парный к GlobalLock выше; блок принадлежит буферу,
            // поэтому память не освобождаем.
            let _ = GlobalUnlock(global);

            if text.is_empty() {
                ReadOutcome::Empty
            } else {
                ReadOutcome::Text(text)
            }
        }
    })
    .unwrap_or(ReadOutcome::Unavailable)
}

/// Читает Unicode-текст. `None` — буфер недоступен или в нём не текст.
///
/// Для восстановления буфера нужен `read_text`: здесь «пусто» и «не удалось
/// прочитать» снова сливаются в одно значение.
pub fn get_text() -> Option<String> {
    match read_text() {
        ReadOutcome::Text(text) => Some(text),
        ReadOutcome::Empty => Some(String::new()),
        ReadOutcome::Unavailable => None,
    }
}

/// Кладёт Unicode-текст в буфер. `false` — не удалось после всех попыток.
pub fn set_text(text: &str) -> bool {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * size_of::<u16>();

    with_open(|| {
        // SAFETY: буфер открыт; память под строку выделяем через GlobalAlloc,
        // после успешного SetClipboardData владельцем становится система.
        unsafe {
            if let Err(err) = EmptyClipboard() {
                switcher_log(format!("clipboard: EmptyClipboard: {err}"));
                return false;
            }

            let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
                switcher_log("clipboard: GlobalAlloc не удался");
                return false;
            };

            // SAFETY: hmem выделен выше, блок не меньше `bytes`.
            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                switcher_log("clipboard: GlobalLock не удался");
                // SAFETY: блок ещё не передан системе, освобождаем сами.
                let _ = GlobalFree(hmem);
                return false;
            }
            // SAFETY: приёмник вмещает wide.len() элементов (см. `bytes`),
            // источник и приёмник не пересекаются.
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            // SAFETY: парный к GlobalLock; после разблокировки блок можно
            // передать системе или освободить.
            let _ = GlobalUnlock(hmem);

            if SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0)).is_err() {
                switcher_log("clipboard: SetClipboardData не удался");
                // SAFETY: система не забрала блок — освобождаем сами,
                // иначе это утечка памяти на каждую неудачную вставку.
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

#[cfg(test)]
mod tests {
    use super::{capacity_for, MAX_TEXT_CHARS};

    #[test]
    fn zero_size_block_is_invalid() {
        // GlobalSize == 0 — транзитное состояние чужого буфера, а не пустая
        // строка: читать нечего, но и считать буфер пустым нельзя.
        assert_eq!(capacity_for(0), None);
        assert_eq!(capacity_for(1), None);
    }

    #[test]
    fn terminator_only_block_is_empty_text() {
        assert_eq!(capacity_for(2), Some(1));
        assert_eq!(capacity_for(6), Some(3));
    }

    #[test]
    fn capacity_is_capped() {
        assert_eq!(capacity_for(usize::MAX), Some(MAX_TEXT_CHARS));
    }
}
