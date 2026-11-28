#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    // Привилегированный воркер blockcheck и консольный режим — тот же бинарь,
    // но без Tauri и без окна. Разбор аргументов идёт до single-instance,
    // поэтому консольный вызов не поднимает второй экземпляр GUI.
    let mut args = std::env::args_os().skip(1);
    let first = args.next();

    if first.as_deref() == Some(std::ffi::OsStr::new("cli")) {
        let rest: Vec<String> = args.map(|arg| arg.to_string_lossy().to_string()).collect();
        std::process::exit(zapret_tools_lib::cli::run(&rest));
    }

    if first.as_deref() == Some(std::ffi::OsStr::new("--blockcheck-worker")) {
        // Без обоих путей нельзя молча уходить в GUI: UAC-родитель ждёт
        // процесс и получил бы вечно висящее приложение вместо ошибки.
        match (args.next(), args.next()) {
            (Some(job), Some(result)) => {
                zapret_tools_lib::run_blockcheck_worker(
                    std::path::PathBuf::from(job),
                    std::path::PathBuf::from(result),
                );
                return;
            }
            _ => {
                eprintln!("--blockcheck-worker требует пути к заданию и результату");
                std::process::exit(2);
            }
        }
    }

    zapret_tools_lib::run()
}
