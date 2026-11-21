//! Сеть: DNS/DoH, блокировка QUIC и чтение адаптеров через IP Helper.

use crate::switcher_log;
use crate::win_cmd::{ps_quote, run_elevated_script, run_powershell};

// ---------- DNS / DoH ----------

fn doh_template_for(ip: &str) -> Option<&'static str> {
    match ip.trim() {
        "1.1.1.1" | "1.0.0.1" => Some("https://cloudflare-dns.com/dns-query"),
        "8.8.8.8" | "8.8.4.4" => Some("https://dns.google/dns-query"),
        "9.9.9.9" | "149.112.112.112" => Some("https://dns.quad9.net/dns-query"),
        "94.140.14.14" | "94.140.15.15" => Some("https://dns.adguard-dns.com/dns-query"),
        _ => None,
    }
}

#[tauri::command]
pub(crate) async fn list_network_interfaces() -> Vec<String> {
    // Чтение через IP Helper занимает миллисекунды, но команда остаётся
    // асинхронной: резервный путь через PowerShell по-прежнему может ждать
    // секунды, а главный поток блокировать нельзя.
    match tokio::task::spawn_blocking(list_network_interfaces_blocking).await {
        Ok(names) => names,
        Err(err) => {
            switcher_log(format!("сеть: не удалось перечислить интерфейсы: {err}"));
            Vec::new()
        }
    }
}

fn list_network_interfaces_blocking() -> Vec<String> {
    #[cfg(windows)]
    if let Some(adapters) = system_adapters() {
        let mut names: Vec<String> = adapters.into_iter().map(|adapter| adapter.name).collect();
        if !names.is_empty() {
            names.sort();
            names.dedup();
            return names;
        }
    }

    // Резерв, если IP Helper не ответил: прежний путь через PowerShell.
    let script = "Get-NetIPConfiguration -ErrorAction SilentlyContinue | \
Where-Object { $_.NetAdapter.Status -eq 'Up' } | \
Sort-Object InterfaceIndex | ForEach-Object { $_.InterfaceAlias } | Select-Object -Unique";
    match run_powershell(script) {
        Ok(out) => out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Адаптер и его DNS-серверы, прочитанные напрямую из системы.
#[cfg(windows)]
struct SystemAdapter {
    name: String,
    dns: Vec<String>,
}

/// Перечисляет активные адаптеры и их DNS-серверы через `GetAdaptersAddresses`.
///
/// PowerShell-эквивалент на холодную занимает 4,4 с (`Get-NetIPConfiguration`)
/// и 1,7 с (`Get-DnsClientServerAddress`), IP Helper — единицы миллисекунд.
/// `None` означает, что API не ответил: вызывающий код уйдёт на PowerShell.
#[cfg(windows)]
fn system_adapters() -> Option<Vec<SystemAdapter>> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::AF_UNSPEC;

    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    const MAX_TRIES: usize = 3;

    let flags = GAA_FLAG_SKIP_UNICAST | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut size: u32 = 16 * 1024;
    // `Vec<u64>` вместо `Vec<u8>`: структуры выровнены по 8 байт, а Vec<u8>
    // гарантирует только выравнивание 1.
    let mut storage: Vec<u64> = Vec::new();
    let mut code = ERROR_BUFFER_OVERFLOW;

    for _ in 0..MAX_TRIES {
        storage.clear();
        storage.resize((size as usize).div_ceil(8), 0);
        let head = storage.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        // SAFETY: буфер выровнен под u64 и не меньше затребованного API размера.
        code = unsafe { GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(head), &mut size) };
        if code == 0 {
            break;
        }
        if code != ERROR_BUFFER_OVERFLOW {
            return None;
        }
        // size уже содержит требуемый объём — цикл перевыделит буфер.
    }
    if code != 0 {
        return None;
    }

    let head = storage.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
    let mut adapters = Vec::new();
    let mut current = unsafe { head.as_ref() };
    while let Some(adapter) = current {
        // SAFETY: связный список выделен API в `storage`, узлы живы до конца обхода.
        let up = adapter.OperStatus == windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
        // Loopback — не сетевой интерфейс пользователя: в выпадающем списке
        // он был бы лишней строкой, как и в прежнем выводе PowerShell.
        // IF_TYPE_SOFTWARE_LOOPBACK из ipifcons.h: в windows crate константы нет.
        const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
        let loopback = adapter.IfType == IF_TYPE_SOFTWARE_LOOPBACK;
        if up && !loopback {
            // SAFETY: строка принадлежит адаптеру и живёт до конца обхода.
            if let Ok(name) = unsafe { adapter.FriendlyName.to_string() } {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    // SAFETY: список DNS-серверов принадлежит тому же аллокатору.
                    let dns = unsafe { dns_servers(adapter.FirstDnsServerAddress) };
                    adapters.push(SystemAdapter { name, dns });
                }
            }
        }
        // SAFETY: Next либо нулевой, либо указывает на следующий узел списка.
        current = unsafe { adapter.Next.as_ref() };
    }
    Some(adapters)
}

#[cfg(windows)]
unsafe fn dns_servers(
    mut node: *mut windows::Win32::NetworkManagement::IpHelper::IP_ADAPTER_DNS_SERVER_ADDRESS_XP,
) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    while let Some(entry) = unsafe { node.as_ref() } {
        // SAFETY: lpSockaddr указывает на sockaddr внутри того же узла.
        if let Some(ip) = unsafe { socket_address_to_ip(&entry.Address) } {
            if !servers.contains(&ip) {
                servers.push(ip);
            }
        }
        node = entry.Next;
    }
    servers
}

/// Разворачивает `SOCKET_ADDRESS` (указатель + длина) в строку адреса.
#[cfg(windows)]
unsafe fn socket_address_to_ip(
    addr: &windows::Win32::Networking::WinSock::SOCKET_ADDRESS,
) -> Option<String> {
    let sockaddr = unsafe { addr.lpSockaddr.as_ref() }?;
    unsafe { sockaddr_to_ip(sockaddr) }
}

#[cfg(windows)]
unsafe fn sockaddr_to_ip(addr: &windows::Win32::Networking::WinSock::SOCKADDR) -> Option<String> {
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6};

    match addr.sa_family {
        AF_INET => {
            // SAFETY: sa_family == AF_INET, значит структура — SOCKADDR_IN.
            let sin = unsafe { &*(addr as *const _ as *const SOCKADDR_IN) };
            // S_addr лежит в сетевом порядке, а to_ne_bytes отдаёт байты в том же
            // порядке, в котором они пришли из сети.
            let octets = sin.sin_addr.S_un.S_addr.to_ne_bytes();
            Some(std::net::Ipv4Addr::from(octets).to_string())
        }
        AF_INET6 => {
            // SAFETY: sa_family == AF_INET6, значит структура — SOCKADDR_IN6.
            let sin6 = unsafe { &*(addr as *const _ as *const SOCKADDR_IN6) };
            Some(std::net::Ipv6Addr::from(sin6.sin6_addr.u.Byte).to_string())
        }
        _ => None,
    }
}

#[tauri::command]
pub(crate) async fn get_dns_state(interface_name: String) -> Result<Vec<String>, String> {
    match tokio::task::spawn_blocking(move || get_dns_state_blocking(&interface_name)).await {
        Ok(result) => result,
        Err(err) => Err(format!("не удалось прочитать DNS: {err}")),
    }
}

fn get_dns_state_blocking(interface_name: &str) -> Result<Vec<String>, String> {
    #[cfg(windows)]
    if let Some(dns) = system_adapters().and_then(|adapters| {
        adapters
            .into_iter()
            .find(|adapter| adapter.name == interface_name)
            .map(|adapter| adapter.dns)
    }) {
        // Интерфейс найден: пустой список — это DNS по DHCP, а не ошибка.
        // IPv6 отбрасываем — в окне показываются IPv4-серверы, как и раньше.
        return Ok(dns
            .into_iter()
            .filter(|ip| !ip.contains(':'))
            .collect());
    }

    // Резерв, если IP Helper не ответил.
    let script = format!(
        "(Get-DnsClientServerAddress -InterfaceAlias {} -AddressFamily IPv4 -ErrorAction Stop).ServerAddresses",
        ps_quote(interface_name)
    );
    let out = run_powershell(&script)?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn validate_dns_address(value: &str, label: &str) -> Result<(), String> {
    value
        .parse::<std::net::IpAddr>()
        .map(|_| ())
        .map_err(|_| format!("{label}: некорректный IP-адрес «{value}»"))
}

#[tauri::command]
pub(crate) fn set_dns(
    interface_name: String,
    dns1: String,
    dns2: Option<String>,
    doh: bool,
) -> Result<(), String> {
    let dns1 = dns1.trim().to_string();
    if dns1.is_empty() {
        return Err("Укажите основной DNS-сервер".into());
    }
    validate_dns_address(&dns1, "Основной DNS")?;
    let dns2 = dns2
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(second) = &dns2 {
        validate_dns_address(second, "Дополнительный DNS")?;
    }

    let mut script = format!(
        "$name = {name}\r\n\
netsh interface ip set dns name=\"$name\" source=static address={dns1} register=primary | Out-Null\r\n\
if ($LASTEXITCODE -ne 0) {{ throw \"Не удалось изменить DNS (код $LASTEXITCODE)\" }}\r\n",
        name = ps_quote(&interface_name),
        dns1 = ps_quote(&dns1)
    );

    if let Some(second) = &dns2 {
        script.push_str(&format!(
            "netsh interface ip add dns name=\"$name\" address={second} index=2 | Out-Null\r\n",
            second = ps_quote(second)
        ));
    }

    // Шифрование DNS (Windows 11). На старых системах netsh просто вернёт
    // ошибку — её игнорируем, обычный DNS при этом уже применён.
    if doh {
        if let Some(template) = doh_template_for(&dns1) {
            script.push_str(&format!(
                "try {{ netsh dns add encryption server={server} dohtemplate={template} autoupgrade=yes | Out-Null }} catch {{ }}\r\n",
                server = ps_quote(&dns1),
                template = ps_quote(template)
            ));
        }
        if let Some(second) = &dns2 {
            if let Some(template) = doh_template_for(second) {
                script.push_str(&format!(
                    "try {{ netsh dns add encryption server={server} dohtemplate={template} autoupgrade=yes | Out-Null }} catch {{ }}\r\n",
                    server = ps_quote(second),
                    template = ps_quote(template)
                ));
            }
        }
    }

    script.push_str("ipconfig /flushdns | Out-Null\r\n");
    run_elevated_script(&script)
}

#[tauri::command]
pub(crate) fn reset_dns(interface_name: String) -> Result<(), String> {
    let script = format!(
        "$name = {name}\r\n\
netsh interface ip set dns name=\"$name\" source=dhcp | Out-Null\r\n\
if ($LASTEXITCODE -ne 0) {{ throw \"Не удалось сбросить DNS (код $LASTEXITCODE)\" }}\r\n\
ipconfig /flushdns | Out-Null\r\n",
        name = ps_quote(&interface_name)
    );
    run_elevated_script(&script)
}

// ---------- Блокировка QUIC ----------

const QUIC_RULE_NAME: &str = "Zapret Tools Block QUIC";

#[tauri::command]
pub(crate) fn toggle_quic_block(enable: bool) -> Result<(), String> {
    let rule = ps_quote(QUIC_RULE_NAME);
    let script = if enable {
        format!(
            "netsh advfirewall firewall delete rule name={rule} | Out-Null\r\n\
netsh advfirewall firewall add rule name={rule} dir=out action=block protocol=UDP localport=443,80 profile=any | Out-Null\r\n\
if ($LASTEXITCODE -ne 0) {{ throw \"Не удалось создать правило брандмауэра (код $LASTEXITCODE)\" }}\r\n"
        )
    } else {
        format!("netsh advfirewall firewall delete rule name={rule} | Out-Null\r\n")
    };
    run_elevated_script(&script)
}

#[tauri::command]
pub(crate) async fn get_quic_block_state() -> bool {
    match tokio::task::spawn_blocking(get_quic_block_state_blocking).await {
        Ok(enabled) => enabled,
        Err(err) => {
            switcher_log(format!("сеть: не удалось проверить правило QUIC: {err}"));
            false
        }
    }
}

/// Включено ли правило брандмауэра, по данным COM-интерфейса Windows Firewall.
///
/// `Get-NetFirewallRule` тратит ~2,5 с на загрузку модуля и разбор CIM, а вывод
/// netsh локализован и приходит в OEM-кодировке. COM отвечает за миллисекунды
/// и не зависит от языка системы.
#[cfg(windows)]
fn firewall_rule_enabled(name: &str) -> Option<bool> {
    use windows::core::BSTR;
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        INetFwPolicy2, INetFwRule, INetFwRules, NetFwPolicy2,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL,
        COINIT_APARTMENTTHREADED,
    };

    /// Освобождает COM-апартамент, если именно мы его инициализировали.
    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: парность с успешным CoInitializeEx выше.
                unsafe { CoUninitialize() };
            }
        }
    }

    // SAFETY: COM-объекты освобождаются автоматически, guard идёт последним.
    unsafe {
        // RPC_E_CHANGED_MODE значит, что поток уже в другой модели, — читать
        // свойства это не мешает.
        let _com = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_ALL).ok()?;
        let rules: INetFwRules = policy.Rules().ok()?;
        let Ok(rule) = rules.Item(&BSTR::from(name)) else {
            // Правила нет — блокировка выключена. Это не сбой API: вернуть
            // здесь None значило бы каждый раз уходить в медленный PowerShell.
            return Some(false);
        };
        let rule: INetFwRule = rule;
        rule.Enabled().ok().map(|enabled| enabled.as_bool())
    }
}

fn get_quic_block_state_blocking() -> bool {
    #[cfg(windows)]
    if let Some(enabled) = firewall_rule_enabled(QUIC_RULE_NAME) {
        return enabled;
    }

    // Резерв, если COM недоступен.
    let script = format!(
        "$rules = @(Get-NetFirewallRule -DisplayName {rule} -ErrorAction SilentlyContinue | Where-Object {{ $_.Enabled -eq 'True' }}); if ($rules.Count -gt 0) {{ 'YES' }} else {{ 'NO' }}",
        rule = ps_quote(QUIC_RULE_NAME)
    );
    matches!(run_powershell(&script), Ok(out) if out.contains("YES"))
}

