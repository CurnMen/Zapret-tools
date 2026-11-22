  const { invoke } = window.__TAURI__.core;
  const { open } = window.__TAURI__.dialog;
  const { enable, disable, isEnabled } = window.__TAURI__.autostart;

  function $id(id) { return document.getElementById(id); }
  function escapeHtml(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  }
  // ---- Глобальная консоль (bottom drawer) ---------------------------------
  // Ограничение действует только на #versionsLog: остальные логи
  // (#log, #trashLog, #fileLog) остаются локальными и растут как раньше.
  const LOG_DRAWER_LIMIT = 100;

  function syncLogDrawer() {
    const drawer = $id('logDrawer');
    const body = $id('versionsLog');
    if (!drawer || !body) return;
    const count = body.childElementCount;
    drawer.hidden = count === 0;
    document.body.classList.toggle('has-log-drawer', count > 0);
    const badge = $id('logDrawerCount');
    if (badge) badge.textContent = String(count);
  }

  function setLogDrawerOpen(open) {
    const drawer = $id('logDrawer');
    if (!drawer) return;
    drawer.classList.toggle('is-open', open);
    document.body.classList.toggle('log-drawer-open', open);
    const toggle = $id('logDrawerToggle');
    if (toggle) toggle.setAttribute('aria-expanded', String(open));
    const hint = $id('logDrawerHint');
    if (hint) hint.textContent = open ? 'Свернуть' : 'Развернуть';

    if (open) {
      const body = $id('versionsLog');
      if (!body) return;
      body.scrollTop = body.scrollHeight;
      // max-height анимируется 220 мс: до конца анимации scrollHeight
      // ещё не отражает итоговую высоту панели.
      setTimeout(() => { body.scrollTop = body.scrollHeight; }, 240);
    }
  }

  $id('logDrawerToggle').onclick = () => {
    setLogDrawerOpen(!$id('logDrawer').classList.contains('is-open'));
  };

  function logTo(el, msg, cls) {
    if (!el) return;
    const d = document.createElement('div');
    d.className = 'log-line ' + (cls || '');
    // Текст приходит из файлов и вывода процессов: разметку здесь не исполняем.
    if (msg instanceof Node) d.appendChild(msg);
    else d.textContent = String(msg);

    const isDrawer = el.id === 'versionsLog';
    if (isDrawer) {
      // Держим только последние 100 строк, иначе панель растёт бесконечно.
      while (el.childElementCount >= LOG_DRAWER_LIMIT) el.firstElementChild.remove();
    }

    el.appendChild(d);

    if (isDrawer) {
      syncLogDrawer();
      // Новое сообщение само раскрывает панель: иначе лог «Старт/Стоп»
      // не видно, когда открыта вкладка «Списки» или «Импорт».
      setLogDrawerOpen(true);
    }

    el.scrollTop = el.scrollHeight;
  }

  function showToast(message, type = 'info', options = null) {
    const host = $id('toastContainer');
    if (!host) return;
    const toast = document.createElement('div');
    toast.className = 'toast toast-' + (type === 'success' || type === 'error' ? type : 'info');
    toast.setAttribute('role', type === 'error' ? 'alert' : 'status');
    toast.textContent = String(message);
    toast.addEventListener('click', () => dismissToast(toast, 'dismiss'));
    if (typeof options?.onDismiss === 'function') toast.__onDismiss = options.onDismiss;

    if (options?.actionLabel && typeof options.onAction === 'function') {
      const action = document.createElement('button');
      action.type = 'button';
      action.className = 'toast-action';
      action.textContent = options.actionLabel;
      action.addEventListener('click', (event) => {
        event.stopPropagation();
        dismissToast(toast, 'action');
        options.onAction();
      });
      toast.appendChild(action);
    }

    if (options?.cancelLabel) {
      const cancel = document.createElement('button');
      cancel.type = 'button';
      cancel.className = 'toast-action toast-cancel';
      cancel.textContent = options.cancelLabel;
      cancel.addEventListener('click', (event) => {
        event.stopPropagation();
        dismissToast(toast, 'dismiss');
      });
      toast.appendChild(cancel);
    }

    host.appendChild(toast);
    while (host.children.length > 4) host.firstElementChild.remove();
    setTimeout(() => dismissToast(toast, 'timeout'), options?.timeout ?? 4200);
  }

  function dismissToast(toast, reason = 'dismiss') {
    if (!toast || !toast.isConnected || toast.classList.contains('is-leaving')) return;
    toast.classList.add('is-leaving');
    if (reason === 'dismiss' && typeof toast.__onDismiss === 'function') {
      const callback = toast.__onDismiss;
      toast.__onDismiss = null;
      try { callback(); } catch (_) { /* отказ не должен ломать UI */ }
    }
    setTimeout(() => toast.remove(), 180);
  }

  // --- Геометрия окна ---
  const GEOMETRY_KEY = 'zapretTools.windowGeometry';
  const win = window.__TAURI__?.window;
  const appWindow = win?.appWindow ?? win?.getCurrentWindow?.() ?? null;

  async function restoreWindowGeometry() {
    if (!appWindow) return;
    let saved = null;
    try { saved = JSON.parse(localStorage.getItem(GEOMETRY_KEY) || 'null'); } catch { saved = null; }
    if (!saved || !Number.isFinite(saved.width) || !Number.isFinite(saved.height)) return;

    const width = Math.round(saved.width);
    const height = Math.round(saved.height);
    if (width < 800 || height < 600 || width > 6000 || height > 4000) return;

    try {
      if (win.PhysicalSize) await appWindow.setSize(new win.PhysicalSize(width, height));
    } catch { /* размер не применился — остаёмся на дефолтном */ }

    if (!Number.isFinite(saved.x) || !Number.isFinite(saved.y)) return;
    const x = Math.round(saved.x);
    const y = Math.round(saved.y);
    if (Math.abs(x) > 20000 || Math.abs(y) > 20000) return;

    let onScreen = true;
    try {
      if (typeof win.availableMonitors === 'function') {
        const monitors = await win.availableMonitors();
        if (Array.isArray(monitors) && monitors.length > 0) {
          onScreen = monitors.some(m => {
            const p = m.position || { x: 0, y: 0 };
            const s = m.size || { width: 0, height: 0 };
            return x < p.x + s.width && x + width > p.x && y < p.y + s.height && y + height > p.y;
          });
        }
      }
    } catch { /* нет доступа к мониторам — доверяем сохранённым координатам */ }

    if (!onScreen) return;
    try {
      if (win.PhysicalPosition) await appWindow.setPosition(new win.PhysicalPosition(x, y));
    } catch { /* позиция не применилась */ }
  }

  let geometrySaveTimer = null;
  function scheduleGeometrySave() {
    if (!appWindow) return;
    clearTimeout(geometrySaveTimer);
    geometrySaveTimer = setTimeout(async () => {
      try {
        const size = await appWindow.outerSize();
        const position = await appWindow.outerPosition();
        localStorage.setItem(GEOMETRY_KEY, JSON.stringify({
          width: size.width,
          height: size.height,
          x: position.x,
          y: position.y
        }));
      } catch { /* окно ещё не готово */ }
    }, 400);
  }

  async function watchWindowGeometry() {
    if (!appWindow) return;
    try { await appWindow.onResized(scheduleGeometrySave); } catch { /* слушатель недоступен */ }
    try { await appWindow.onMoved(scheduleGeometrySave); } catch { /* слушатель недоступен */ }
  }

  restoreWindowGeometry().then(watchWindowGeometry);

  let state = { rootPath: null, activeVersion: null, versions: [], currentListFile: null, runningWinwsPath: null, trashFiles: [], switcherEnabled: false, switcherExclusions: [], switcherWordExclusions: [], customDictRu: [], customDictEn: [], declinedWords: [], suggestions: [], switcherDictionariesReady: false, switcherAdmin: true, switcherAdminWarned: false, switcherEventsBound: false, switcherProcesses: [], autoCheckUpdates: true, hostsEntries: [], tools: [], toolProcesses: [] };

  function setAutostartUI(on) {
    $id('btnAutostartLabel').textContent = on ? 'Автозапуск: вкл' : 'Автозапуск: выкл';
    $id('btnAutostart').classList.toggle('is-on', on);
    $id('btnAutostart').setAttribute('aria-pressed', String(on));
  }

  (async function init() {
    const autoStart = await isEnabled();
    setAutostartUI(autoStart);

    const config = await invoke('get_config');

    // Автопроверка обновлений программы: у старых конфигов поля нет —
    // считаем, что она включена (см. Config::default в lib.rs).
    state.autoCheckUpdates = config.auto_check_updates !== false;
    $id('toggleAppUpdates').checked = state.autoCheckUpdates;
    // Проверка уйдёт через 2 секунды, чтобы не соревноваться со стартом;
    // при выключенном тумблере scheduleAppUpdateCheck() ничего не планирует.
    scheduleAppUpdateCheck();

    // Свитчер раскладки не зависит от папки Zapret — запускаем сразу.
    // Ждём загрузку его настроек, чтобы последующие save_config
    // не записали в файл значения по умолчанию.
    await initSwitcher();

    if (!config.root_path) return;

    state.rootPath = config.root_path;
    $id('rootStatus').textContent = state.rootPath;
    $id('rootBar').classList.add('connected');
    $id('quickActions').style.display = 'flex';
    $id('connectRootLabel').textContent = 'Сменить папку';
    $id('filterBar').style.display = 'flex';

    // Окно уже отрисовано: тяжёлые операции (скан версий, поиск winws.exe)
    // выполняем в фоне, не блокируя интерфейс.
    scanVersions(config.active_version).then(async () => {
      state.runningWinwsPath = await invoke('find_running_winws');
      if (state.runningWinwsPath && state.runningWinwsPath.toLowerCase().includes('\\bin\\winws.exe')) {
        let rootDir = state.runningWinwsPath.substring(0, state.runningWinwsPath.lastIndexOf('\\'));
        rootDir = rootDir.substring(0, rootDir.lastIndexOf('\\'));
        const runningVersion = state.versions.find(v => v.path.toLowerCase() === rootDir.toLowerCase());
        if (runningVersion) {
          state.activeVersion = runningVersion;
          $id('activeVersionBadge').style.display = '';
          $id('activeVersionText').textContent = state.activeVersion.name + ' · запущена';
          await persistConfig();
          loadListsTab();
        }
      }

      if (state.runningWinwsPath) {
        $id('runningStatus').innerHTML = '<span class="status-dot on"></span>winws.exe запущен';
      } else {
        $id('runningStatus').innerHTML = '<span class="status-dot"></span>winws.exe не запущен';
      }
    }).catch(e => showToast('Ошибка инициализации: ' + e, 'error'));
  })();

  $id('btnAutostart').onclick = async () => {
    const autoStart = await isEnabled();
    if (autoStart) { await disable(); setAutostartUI(false); }
    else { await enable(); setAutostartUI(true); }
  };

  // Проверка обновлений самой программы через tauri-plugin-updater.
  // Отдельной кнопки в шапке нет: проверку запускает только тумблер
  // «Авто-проверка Zapret Tools» через scheduleAppUpdateCheck().
  let appUpdateInFlight = false;

  // Не даёт сетевому запросу подвесить интерфейс: плагин updater не всегда
  // возвращает управление, если GitHub не отвечает.
  function withTimeout(promise, ms, message) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(message)), ms);
      promise.then(
        value => { clearTimeout(timer); resolve(value); },
        error => { clearTimeout(timer); reject(error); }
      );
    });
  }

  // silent = true — фоновая автопроверка: молчим, если обновлений нет
  // и если репозиторий недоступен, чтобы не мешать при старте.
  async function runAppUpdateCheck({ silent = false } = {}) {
    // Старт приложения и переключение тумблера могут совпасть — второй
    // параллельный запрос не нужен.
    if (appUpdateInFlight) return;
    appUpdateInFlight = true;

    try {
      const update = await withTimeout(
        invoke('plugin:updater|check'),
        5000,
        'Сервер не отвечает'
      );
      if (update && update.version) {
        showToast(`Доступна новая версия Zapret Tools ${update.version}.`, 'info', { timeout: 9000 });
      } else if (!silent) {
        showToast('У вас последняя версия программы.', 'success');
      }
    } catch (err) {
      if (silent) return;
      const text = String(err && err.message ? err.message : err);
      if (text.includes('Сервер не отвечает')) {
        showToast('Сервер не отвечает', 'error');
        return;
      }
      // Плагин не отдаёт HTTP-код: 403/404 приватного репозитория он превращает
      // в «Could not fetch a valid release JSON from the remote» (ReleaseNotFound).
      const isPrivate = /could not fetch a valid release json|releasenotfound|\b(403|404)\b|forbidden|not found|private|unauthorized|authentication/i.test(text);
      showToast(
        isPrivate
          ? 'Не удалось проверить обновления: репозиторий приватный'
          : 'Не удалось проверить обновления: ' + text,
        'error'
      );
    } finally {
      appUpdateInFlight = false;
    }
  }

  let appUpdateTimer = null;

  // Через 2 секунды после старта или включения тумблера: интерфейс успевает
  // отрисоваться. Выключение тумблера до этого момента отменяет проверку.
  function scheduleAppUpdateCheck() {
    clearTimeout(appUpdateTimer);
    appUpdateTimer = null;
    if (!state.autoCheckUpdates) return;
    appUpdateTimer = setTimeout(() => {
      appUpdateTimer = null;
      runAppUpdateCheck({ silent: true });
    }, 2000);
  }

  $id('connectRootBtn').onclick = async () => {
    const selected = await open({ directory: true, multiple: false, title: 'Выберите папку Zapret' });
    if (selected) {
      state.rootPath = selected;
      $id('rootStatus').textContent = selected;
      $id('rootBar').classList.add('connected');
      $id('quickActions').style.display = 'flex';
      $id('connectRootLabel').textContent = 'Сменить папку';
      $id('filterBar').style.display = 'flex';
      await scanVersions(null);
    }
  };

  $id('connectRootBtn').addEventListener('contextmenu', async (e) => {
    e.preventDefault();
    if (await showConfirm('Добавить ярлык Zapret Tools в меню Пуск?')) {
      await invoke('create_start_menu_shortcut');
      showToast('Ярлык создан. Ищите в меню Пуск.', 'success');
    }
  });

  async function scanVersions(preferredActive) {
    if (!state.rootPath) return;
    state.versions = await invoke('scan_versions', { basePath: state.rootPath });
    if (preferredActive) state.activeVersion = state.versions.find(v => v.name === preferredActive) || state.versions[0];
    else if (!state.activeVersion && state.versions.length > 0) state.activeVersion = state.versions[0];

    if (state.activeVersion) {
      $id('activeVersionBadge').style.display = '';
      $id('activeVersionText').textContent = state.activeVersion.name;
      $id('listsContent').style.display = 'flex';
      await persistConfig();

      await syncGameFilterUi();
      await syncIpsetState();
      await syncZapretUpdates();

      const select = $id('strategySelect');
      select.innerHTML = '<option value="">Стратегия не выбрана</option>';
      const strategies = await invoke('list_strategies', { versionPath: state.activeVersion.path });
      strategies.forEach(s => { const opt = document.createElement('option'); opt.value = s; opt.textContent = s; select.appendChild(opt); });

      loadListsTab();
    }
    renderVersions();
  }

  async function checkRunning() {
    state.runningWinwsPath = await invoke('find_running_winws');
    if (state.runningWinwsPath) {
      $id('runningStatus').innerHTML = '<span class="status-dot on"></span>winws.exe запущен';
      if (state.activeVersion && state.runningWinwsPath.toLowerCase().includes(state.activeVersion.name.toLowerCase())) {
        $id('activeVersionText').textContent = state.activeVersion.name + ' · запущена';
      }
    } else {
      $id('runningStatus').innerHTML = '<span class="status-dot"></span>winws.exe не запущен';
      if (state.activeVersion) $id('activeVersionText').textContent = state.activeVersion.name;
    }
  }

  $id('btnOpenRoot').onclick = () => { if (state.rootPath) invoke('open_folder', { path: state.rootPath }); };
  $id('btnOpenRunning').onclick = () => {
    if (state.runningWinwsPath && state.runningWinwsPath.includes('\\bin\\winws.exe')) {
      let dir = state.runningWinwsPath.substring(0, state.runningWinwsPath.lastIndexOf('\\'));
      if (dir.toLowerCase().endsWith('\\bin')) dir = dir.substring(0, dir.lastIndexOf('\\'));
      invoke('open_folder', { path: dir });
    } else showToast('Не удалось определить путь к запущенной версии.', 'error');
  };

  ['btnSrvStart', 'btnSrvStop', 'btnSrvRemove'].forEach((id, i) => {
    const btn = $id(id);
    if (!btn) return;
    const actions = ['start', 'stop', 'remove'];
    btn.onclick = async () => {
      if (!state.activeVersion) return showToast('Выберите версию.', 'info');
      if (i === 2 && !(await showConfirm('Удалить службу zapret?'))) return;
      try {
        await invoke('manage_service', { versionPath: state.activeVersion.path, action: actions[i] });
        logTo($id('versionsLog'), `Команда [${actions[i]}] отправлена`, 'ok');
        setTimeout(checkRunning, 2000);
      } catch(e) { showToast(String(e), 'error'); }
    };
  });

  $id('btnSrvInstall').onclick = async () => {
    const strat = $id('strategySelect').value;
    if (!strat) return showToast('Сначала выберите стратегию из списка.', 'info');
    try {
      await invoke('install_strategy', { versionPath: state.activeVersion.path, strategyName: strat });
      logTo($id('versionsLog'), `Установка стратегии ${strat}`, 'ok');
      setTimeout(checkRunning, 3000);
    } catch(e) { showToast('Ошибка установки стратегии: ' + e, 'error'); }
  };

  // ---- Game Filter: порты service.bat -------------------------------------
  const GAME_FILTER_DEFAULT_PORTS = '1024-65535';

  // service.bat читает `mode=all|tcp|udp`, `tcp=...`, `udp=...` без проверок,
  // поэтому мусор отсекаем здесь: иначе служба просто не запустится.
  const PORT_TOKEN = /^(\d{1,5})(?:-(\d{1,5}))?$/;

  function isValidPortRange(value) {
    if (!value) return false;
    return value.split(',').every(part => {
      const match = part.trim().match(PORT_TOKEN);
      if (!match) return false;
      const start = Number(match[1]);
      const end = match[2] ? Number(match[2]) : start;
      return start >= 1 && end <= 65535 && start <= end;
    });
  }

  function portFieldVisible(input) {
    const field = input.closest('.port-field');
    return !field || !field.hidden;
  }

  function updateGameFilterMode() {
    const mode = $id('gameFilterMode').value;
    $id('gameFilterTcpField').hidden = mode === 'udp';
    $id('gameFilterUdpField').hidden = mode === 'tcp';
  }

  function setGameFilterPortsVisible(enabled) {
    $id('gameFilterPorts').classList.toggle('is-open', Boolean(enabled));
  }

  function markGameFilterPortsValidity() {
    let ok = true;
    ['gameFilterTcp', 'gameFilterUdp'].forEach(id => {
      const input = $id(id);
      // Скрытое поле не проверяем: его значение всё равно уйдёт в файл
      // как есть, а видимое пользователь может исправить.
      const valid = !portFieldVisible(input) || isValidPortRange(input.value.trim());
      input.classList.toggle('is-invalid', !valid);
      if (!valid) ok = false;
    });
    return ok;
  }

  function gameFilterPayload() {
    const tcpInput = $id('gameFilterTcp');
    const udpInput = $id('gameFilterUdp');
    const valueOf = input => {
      const value = input.value.trim();
      // Скрытое поле: оставляем валидное значение, иначе — дефолт.
      if (!isValidPortRange(value)) return GAME_FILTER_DEFAULT_PORTS;
      return value;
    };
    return {
      versionPath: state.activeVersion.path,
      enabled: $id('toggleGame').checked,
      mode: $id('gameFilterMode').value,
      tcpRange: valueOf(tcpInput),
      udpRange: valueOf(udpInput),
    };
  }

  async function saveGameFilterConfig(toastMessage) {
    if (!state.activeVersion) return false;
    // Выключение должно срабатывать всегда, даже если в полях остался мусор:
    // файл просто удаляется, а порты в него не попадают.
    if ($id('toggleGame').checked && !markGameFilterPortsValidity()) {
      showToast('Порты: число 1–65535 или диапазон, например 1024-65535.', 'error');
      return false;
    }
    try {
      await invoke('toggle_game_filter', gameFilterPayload());
      if (toastMessage) showToast(toastMessage, 'success');
      return true;
    } catch (e) {
      showToast('Не удалось сохранить Game Filter: ' + e, 'error');
      return false;
    }
  }

  async function syncGameFilterUi() {
    if (!state.activeVersion) {
      setGameFilterPortsVisible(false);
      return;
    }
    // Rust отдаёт кортеж: (включён, режим, tcp, udp).
    const [enabled, mode, tcp, udp] = await invoke('get_game_filter_config', { versionPath: state.activeVersion.path });
    $id('toggleGame').checked = enabled;
    $id('gameFilterMode').value = ['all', 'tcp', 'udp'].includes(mode) ? mode : 'all';
    $id('gameFilterTcp').value = tcp || GAME_FILTER_DEFAULT_PORTS;
    $id('gameFilterUdp').value = udp || GAME_FILTER_DEFAULT_PORTS;
    $id('gameFilterTcp').classList.remove('is-invalid');
    $id('gameFilterUdp').classList.remove('is-invalid');
    updateGameFilterMode();
    setGameFilterPortsVisible(enabled);
  }

  $id('toggleGame').onchange = async (e) => {
    if (!state.activeVersion) return;
    const enabled = e.target.checked;
    setGameFilterPortsVisible(enabled);
    const saved = await saveGameFilterConfig();
    if (!saved) {
      // Файл не записался — возвращаем тумблер в прежнее положение.
      e.target.checked = !enabled;
      setGameFilterPortsVisible(!enabled);
      return;
    }
    showToast('Game Filter ' + (enabled ? 'включён' : 'выключен') + '. Перезапустите службу.', 'success');
  };

  $id('gameFilterMode').onchange = async () => {
    updateGameFilterMode();
    await saveGameFilterConfig();
  };

  ['gameFilterTcp', 'gameFilterUdp'].forEach(id => {
    // change срабатывает по Enter и по уходу из поля — сохраняем сразу,
    // чтобы значения не терялись при закрытии окна.
    $id(id).addEventListener('change', () => { saveGameFilterConfig(); });
    $id(id).addEventListener('input', () => {
      $id(id).classList.toggle('is-invalid', !isValidPortRange($id(id).value.trim()));
    });
  });

  $id('gameFilterApply').onclick = async () => {
    if (!state.activeVersion) return;
    if (!$id('toggleGame').checked) {
      showToast('Сначала включите Game Filter.', 'info');
      return;
    }
    await saveGameFilterConfig('Настройки Game Filter сохранены. Перезапустите службу.');
  };

  // ---- IPSet: три состояния service.bat -----------------------------------
  const IPSET_LABELS = {
    loaded: 'загружен',
    none: 'выключен',
    any: 'все IP'
  };

  async function syncIpsetState() {
    const select = $id('ipsetState');
    if (!state.activeVersion) {
      select.value = 'none';
      select.dataset.previous = 'none';
      select.disabled = true;
      return;
    }
    select.disabled = false;
    // Rust отвечает строкой "loaded" | "none" | "any".
    const value = await invoke('get_ipset_state', { versionPath: state.activeVersion.path });
    const current = IPSET_LABELS[value] ? value : 'none';
    select.value = current;
    select.dataset.previous = current;
  }

  $id('ipsetState').onchange = async (e) => {
    if (!state.activeVersion) return;
    const select = e.target;
    const previous = select.dataset.previous || 'none';
    const next = select.value;

    select.disabled = true;
    try {
      await invoke('set_ipset_state', { versionPath: state.activeVersion.path, state: next });
      select.dataset.previous = next;
      // Содержимое ipset-all.txt изменилось — перечитываем открытый файл.
      if (state.currentListFile) await selectListFile(state.currentListFile);
      showToast('IPSet: ' + IPSET_LABELS[next] + '. Перезапустите службу.', 'success');
    } catch (err) {
      select.value = previous;
      showToast('Не удалось изменить IPSet: ' + err, 'error');
    } finally {
      select.disabled = !state.activeVersion;
    }
  };

  // ---- Обновления ---------------------------------------------------------
  async function syncZapretUpdates() {
    const toggle = $id('toggleZapretUpdates');
    if (!state.activeVersion) {
      toggle.checked = false;
      toggle.disabled = true;
      return;
    }
    toggle.disabled = false;
    toggle.checked = await invoke('get_zapret_update_check_status', { versionPath: state.activeVersion.path });
  }

  $id('toggleZapretUpdates').onchange = async (e) => {
    if (!state.activeVersion) return;
    const enabled = e.target.checked;
    try {
      await invoke('toggle_zapret_update_check', { versionPath: state.activeVersion.path, enabled });
      showToast('Проверка обновлений Zapret ' + (enabled ? 'включена' : 'выключена') + '.', 'success');
    } catch (err) {
      e.target.checked = !enabled;
      showToast('Не удалось изменить настройку: ' + err, 'error');
    }
  };

  $id('toggleAppUpdates').onchange = async (e) => {
    state.autoCheckUpdates = e.target.checked;
    await persistConfig();
    // Включили — проверка через 2 секунды; выключили — таймер снимается.
    scheduleAppUpdateCheck();
    showToast(
      state.autoCheckUpdates
        ? 'Автопроверка обновлений Zapret Tools включена.'
        : 'Автопроверка обновлений Zapret Tools выключена.',
      state.autoCheckUpdates ? 'success' : 'info'
    );
  };

  async function loadListsTab() {
    if (!state.activeVersion) return;
    $id('listsActiveVersion').textContent = state.activeVersion.name;
    try {
      const files = await invoke('read_list_files', { versionPath: state.activeVersion.path });
      $id('fileList').innerHTML = files.map(f => {
        const isU = f.name.includes('-user');
        return `<button class="file-btn ${f.name === state.currentListFile ? 'active' : ''} ${isU ? 'user-file' : ''}" data-file="${escapeHtml(f.name)}">
        <span class="file-name">${escapeHtml(f.name)}</span>
        <span class="line-count">${f.lines === 999999 ? '...' : f.lines}</span>
      </button>`;
      }).join('');

      $id('fileList').querySelectorAll('.file-btn').forEach(btn => {
        btn.onclick = () => {
          $id('fileList').querySelectorAll('.file-btn').forEach(b => b.classList.remove('active'));
          btn.classList.add('active');
          selectListFile(btn.dataset.file);
        };
      });

      if (!state.currentListFile) {
        $id('currentFileName').textContent = 'Файл не выбран';
        $id('fileContent').value = '';
        $id('lineNumbers').innerHTML = '1';
        $id('fileStats').textContent = 'Строк: 0';
      }
    } catch (e) { logTo($id('fileLog'), 'Ошибка списка: ' + e, 'err'); }
  }

  async function selectListFile(filename) {
    state.currentListFile = filename;
    $id('currentFileName').textContent = filename;
    if ($id('fileLog')) $id('fileLog').innerHTML = '';

    $id('editorContainer').classList.remove('has-error');
    $id('errorIndicator').classList.remove('show');

    const spinner = $id('fileLoading');
    if (spinner) spinner.style.display = 'flex';
    const textarea = $id('fileContent');
    if (textarea) textarea.value = '';
    resetLineNumbersGutter();

    // 100 мс даём браузеру отрисовать спиннер до синхронной вставки большого текста.
    setTimeout(async () => {
      try {
        const content = await invoke('read_list_file', { versionPath: state.activeVersion.path, fileName: filename });
        if (spinner) spinner.style.display = 'flex';
        if (textarea) textarea.value = content;
        updateFileStats();
        updateLineNumbers();
      } catch (e) { if (textarea) textarea.value = ''; }
      finally { if (spinner) spinner.style.display = 'none'; }
    }, 100);
  }

  // Считает строки без создания массива; при stopAfter останавливается сразу после превышения лимита.
  function countLines(text, stopAfter) {
    let lines = 1;
    for (let i = 0; i < text.length; i++) {
      if (text.charCodeAt(i) === 10) {
        lines++;
        if (stopAfter && lines > stopAfter) return lines;
      }
    }
    return lines;
  }

  const MAX_RENDERED_LINES = 50000;
  let lastRenderedLines = 1;

  function resetLineNumbersGutter() {
    const gutter = $id('lineNumbers');
    if (!gutter) return;
    gutter.textContent = '1';
    gutter.classList.remove('is-huge');
    lastRenderedLines = 1;
  }

  function updateLineNumbers() {
    const textarea = $id('fileContent');
    const gutter = $id('lineNumbers');
    if (!textarea || !gutter) return;

    const lines = countLines(textarea.value, MAX_RENDERED_LINES);

    if (lines > MAX_RENDERED_LINES) {
      if (!gutter.classList.contains('is-huge')) {
        gutter.classList.add('is-huge');
        gutter.textContent = 'Слишком большой файл';
        lastRenderedLines = -1;
      }
    } else {
      if (gutter.classList.contains('is-huge')) gutter.classList.remove('is-huge');
      if (lastRenderedLines !== lines) {
        gutter.textContent = Array.from({ length: lines }, (_, i) => i + 1).join('\n');
        lastRenderedLines = lines;
      }
    }
    gutter.scrollTop = textarea.scrollTop;
  }

  $id('fileContent').addEventListener('scroll', () => {
    $id('lineNumbers').scrollTop = $id('fileContent').scrollTop;
  });

  // Один проход по тексту: строки и непустые фрагменты (аналог /\S+/g) без массивов.
  function measureText(text) {
    let lines = 1;
    let nonEmpty = 0;
    let inWord = false;
    for (let i = 0; i < text.length; i++) {
      const c = text.charCodeAt(i);
      if (c === 10) lines++;
      const isSpace = c === 32 || (c >= 9 && c <= 13) || c === 160 || c === 0x2028 || c === 0x2029 || c === 0xfeff;
      if (isSpace) {
        if (inWord) { nonEmpty++; inWord = false; }
      } else {
        inWord = true;
      }
    }
    if (inWord) nonEmpty++;
    return { lines, nonEmpty };
  }

  let statsTimeout;
  function updateFileStats() {
    clearTimeout(statsTimeout);
    statsTimeout = setTimeout(() => {
      const textarea = $id('fileContent');
      if (!textarea) return;
      const { lines, nonEmpty } = measureText(textarea.value);
      $id('fileStats').textContent = `Строк: ${lines} · непустых: ${nonEmpty}`;
      updateLineNumbers();
    }, 400);
  }

  const AUTO_VALIDATE_LINES_LIMIT = 20000;
  let autoValidateTimeout;
  $id('fileContent').addEventListener('input', () => {
    updateFileStats();
    clearTimeout(autoValidateTimeout);

    $id('editorContainer').classList.remove('has-error');
    $id('errorIndicator').classList.remove('show');

    autoValidateTimeout = setTimeout(async () => {
      if (!state.currentListFile) return;
      const content = $id('fileContent').value;
      if (!content) return;
      // Большие файлы на лету не валидируем — только по кнопке «Очистить и проверить».
      if (countLines(content, AUTO_VALIDATE_LINES_LIMIT) > AUTO_VALIDATE_LINES_LIMIT) return;
      const isIp = state.currentListFile.toLowerCase().includes('ipset');
      const errors = await invoke('validate_list', { content, isIpList: isIp });
      if (errors.length > 0) {
        $id('editorContainer').classList.add('has-error');
        $id('errorIndicator').classList.add('show');
      }
    }, 1500);
  });

  let searchMatches = [];
  let currentMatchIndex = -1;

  $id('searchBtn').onclick = () => {
    const query = $id('searchInput').value.trim();
    const textarea = $id('fileContent');
    if (!query) return;

    const text = textarea.value.toLowerCase();
    const searchStr = query.toLowerCase();
    searchMatches = [];

    let pos = text.indexOf(searchStr);
    while (pos !== -1) {
      searchMatches.push(pos);
      pos = text.indexOf(searchStr, pos + 1);
    }

    if (searchMatches.length > 0) {
      currentMatchIndex = 0;
      jumpToMatch();
    } else {
      showToast('Совпадений не найдено.', 'info');
    }
  };

  $id('searchNextBtn').onclick = () => {
    if (searchMatches.length === 0) return;
    currentMatchIndex = (currentMatchIndex + 1) % searchMatches.length;
    jumpToMatch();
  };

  $id('searchInput').addEventListener('keypress', (e) => {
    if (e.key === 'Enter') $id('searchBtn').click();
  });

  function jumpToMatch() {
    const textarea = $id('fileContent');
    const query = $id('searchInput').value.trim();
    const index = searchMatches[currentMatchIndex];

    if (index === undefined || index < 0) return;

    textarea.focus();
    textarea.setSelectionRange(index, index + query.length);

    // Принудительный скролл (нативный в WebView2 иногда ломается на больших файлах)
    const style = window.getComputedStyle(textarea);
    let lineHeight = parseFloat(style.lineHeight);
    if (isNaN(lineHeight)) lineHeight = parseFloat(style.fontSize) * 1.5;
    const paddingTop = parseFloat(style.paddingTop) || 0;

    const textBefore = textarea.value.substring(0, index);
    let linesBefore = 0;
    for (let i = 0; i < textBefore.length; i++) {
      if (textBefore.charCodeAt(i) === 10) linesBefore++;
    }

    const targetScroll = (linesBefore * lineHeight) + paddingTop - (textarea.clientHeight / 2) + (lineHeight / 2);

    textarea.scrollTop = Math.max(0, targetScroll);
    $id('lineNumbers').scrollTop = textarea.scrollTop;
  }

  $id('fileLog').addEventListener('click', (e) => {
    if (e.target.classList.contains('error-link')) {
      const lineNum = parseInt(e.target.dataset.line);
      if (!lineNum) return;

      const textarea = $id('fileContent');
      const lines = textarea.value.split('\n');
      let charIndex = 0;
      for (let i = 0; i < lineNum - 1; i++) {
        charIndex += lines[i].length + 1;
      }
      const lineLength = lines[lineNum - 1] ? lines[lineNum - 1].length : 0;

      textarea.focus();
      textarea.setSelectionRange(charIndex, charIndex + lineLength);
    }
  });

  $id('cleanValidateBtn').onclick = async () => {
    if (!state.currentListFile) return;
    const content = $id('fileContent').value;
    const isIp = state.currentListFile.toLowerCase().includes('ipset');

    $id('fileLog').innerHTML = '';
    const [cleanedText, errors, removedCount] = await invoke('clean_and_validate', { content, isIpList: isIp });

    $id('fileContent').value = cleanedText;
    updateFileStats();
    updateLineNumbers();

    if (errors.length === 0) {
      $id('editorContainer').classList.remove('has-error');
      $id('errorIndicator').classList.remove('show');

      if (removedCount > 0) {
        logTo($id('fileLog'), `Список очищен. Удалено дублей и мусора: ${removedCount}. Ошибок нет.`, 'ok');
      } else {
        logTo($id('fileLog'), 'Список уже чистый. Ошибок нет.', 'ok');
      }
    } else {
      $id('editorContainer').classList.add('has-error');
      $id('errorIndicator').classList.add('show');

      if (removedCount > 0) {
        logTo($id('fileLog'), `Очищено, удалено дублей: ${removedCount}. Найдено ошибок: ${errors.length}. Нажмите на ошибку, чтобы перейти к строке.`, 'err');
      } else {
        logTo($id('fileLog'), `Найдено ошибок: ${errors.length}. Нажмите на ошибку, чтобы перейти к строке.`, 'err');
      }

      errors.forEach(err => {
        const match = err.match(/Строка (\d+):/);
        if (match) {
          const line = match[1];
          const link = document.createElement('span');
          link.className = 'error-link';
          link.dataset.line = line;
          link.textContent = err;
          logTo($id('fileLog'), link, 'warn');
        } else { logTo($id('fileLog'), err, 'warn'); }
      });
    }
  };

  $id('consolidateBtn').onclick = async () => {
    if (!state.currentListFile) return showToast('Сначала выберите файл в левой панели.', 'info');
    const content = $id('fileContent').value;
    if (!content.trim()) return showToast('Файл пуст — сжимать нечего.', 'info');

    const btn = $id('consolidateBtn');
    btn.disabled = true;
    try {
      const [text, removed] = await invoke('consolidate_subdomains', { content });
      $id('fileContent').value = text;
      updateFileStats();
      updateLineNumbers();

      if (removed > 0) {
        logTo($id('fileLog'), `Поддомены сжаты. Удалено строк: ${removed}.`, 'ok');
        showToast(`Поддомены сжаты. Удалено строк: ${removed}.`, 'success');
      } else {
        logTo($id('fileLog'), 'Поддомены уже сжаты. Изменений нет.', 'ok');
        showToast('Поддомены уже сжаты.', 'info');
      }
    } catch (e) {
      logTo($id('fileLog'), 'Ошибка сжатия: ' + e, 'err');
      showToast('Ошибка сжатия: ' + String(e), 'error');
    } finally {
      btn.disabled = false;
    }
  };

  $id('checkDomainsBtn').onclick = async () => {
    if (!state.currentListFile) return showToast('Сначала выберите файл в левой панели.', 'info');
    const content = $id('fileContent').value;
    if (!content.trim()) return showToast('Файл пуст — проверять нечего.', 'info');

    const btn = $id('checkDomainsBtn');
    const spinner = $id('checkDomainsSpinner');
    btn.disabled = true;
    btn.classList.add('is-busy');
    if (spinner) spinner.hidden = false;
    logTo($id('fileLog'), 'Проверка доступности доменов…', '');

    try {
      const report = await invoke('check_dead_domains', { content });
      const checked = (report && report.checked) || 0;
      const dead = (report && report.dead) || [];

      if (checked === 0) {
        logTo($id('fileLog'), 'В файле нет доменов для проверки.', 'warn');
        showToast('В файле нет доменов для проверки.', 'info');
        return;
      }
      if (dead.length === 0) {
        logTo($id('fileLog'), `Все ${checked} доменов отвечают.`, 'ok');
        showToast(`Все ${checked} доменов отвечают.`, 'success');
        return;
      }

      const shown = dead.slice(0, 15).map(d => escapeHtml(d)).join(', ');
      const more = dead.length > 15 ? ` и ещё ${dead.length - 15}` : '';
      logTo($id('fileLog'), `Проверено ${checked}. Не отвечают ${dead.length}: ${shown}${more}.`, 'err');
      showToast(`Не отвечают ${dead.length} из ${checked} доменов.`, 'error');
    } catch (e) {
      logTo($id('fileLog'), 'Ошибка проверки: ' + e, 'err');
      showToast('Ошибка проверки доменов: ' + String(e), 'error');
    } finally {
      btn.disabled = false;
      btn.classList.remove('is-busy');
      if (spinner) spinner.hidden = true;
    }
  };

  $id('quickAddBtn').onclick = async () => {
    const input = $id('quickInput');
    const raw = input.value.trim();
    if (!raw) return;

    const [valid, invalid, converted] = await invoke('smart_parse_domains', { input: raw });

    const textarea = $id('fileContent');
    let lines = textarea.value.split(/\r?\n/).map(s => s.trim()).filter(Boolean);
    const seen = new Set(lines.map(s => s.toLowerCase()));
    let added = 0;

    valid.forEach(item => {
      if (!seen.has(item.toLowerCase())) { seen.add(item.toLowerCase()); lines.push(item); added++; }
    });

    lines.sort();
    textarea.value = lines.join('\n') + '\n';
    input.value = '';
    updateFileStats();
    updateLineNumbers();

    $id('fileLog').innerHTML = '';
    if (added > 0) logTo($id('fileLog'), `Добавлено: ${added}`, 'ok');
    if (converted > 0) logTo($id('fileLog'), `Домены .рф сконвертированы в Punycode: ${converted}`, 'ok');
    if (invalid.length > 0) logTo($id('fileLog'), `Пропущено невалидных: ${invalid.length} (${invalid.join(', ')})`, 'err');
  };

  const saveFileBtn = $id('saveFileBtn');
  if (saveFileBtn) {
    saveFileBtn.onclick = async () => {
      if (!state.currentListFile) return;
      saveFileBtn.disabled = true;
      try {
        const resultMsg = await invoke('write_list_file', {
          rootPath: state.rootPath,
          versionPath: state.activeVersion.path,
          fileName: state.currentListFile,
          content: $id('fileContent').value
        });
        logTo($id('fileLog'), resultMsg, 'ok');
        const content = await invoke('read_list_file', { versionPath: state.activeVersion.path, fileName: state.currentListFile });
        $id('fileContent').value = content;
        updateFileStats();
        updateLineNumbers();
      } catch(e) { logTo($id('fileLog'), 'Ошибка: ' + e, 'err'); }
      finally { saveFileBtn.disabled = false; }
    };
  }

  let oldDir = null, newDir = null;
  $id('pickOld').onclick = async () => { oldDir = await open({ directory: true }); if (oldDir) $id('oldName').textContent = oldDir; $id('mergeBtn').disabled = !(oldDir && newDir); };
  $id('pickNew').onclick = async () => { newDir = await open({ directory: true }); if (newDir) $id('newName').textContent = newDir; $id('mergeBtn').disabled = !(oldDir && newDir); };
  $id('mergeBtn').onclick = async () => { if(!oldDir || !newDir) return; await invoke('merge_folders', { src: oldDir, dst: newDir }); logTo($id('log'), 'Папки объединены', 'ok'); };

  function renderVersions() {
    if (!$id('versionsList')) return;
    $id('versionsList').innerHTML = state.versions.map(v => {
      const active = state.activeVersion?.name === v.name;
      const meta = [
        v.has_service ? 'service.bat' : null,
        v.has_winws ? 'winws.exe' : null,
        v.has_lists ? 'lists' : null
      ].filter(Boolean).map(x => `<span>${x}</span>`).join('');
      return `
      <div class="version-card ${active ? 'active' : ''}">
        <div class="head">
          <span class="name">${escapeHtml(v.name)}</span>
          ${active ? '<span class="vbadge">Активна</span>' : ''}
          <div class="actions">${!active ? `<button class="btn btn-secondary btn-sm activate-btn" data-version="${escapeHtml(v.name)}" type="button">Активировать</button>` : ''}</div>
        </div>
        <div class="version-meta">${meta}</div>
      </div>`;
    }).join('');
    $id('versionsList').querySelectorAll('.activate-btn').forEach(btn => {
      btn.onclick = () => window.activate(btn.dataset.version);
    });
  }

  window.activate = async (name) => { state.activeVersion = state.versions.find(v => v.name === name); await scanVersions(name); };

  const updateZapretBtn = $id('updateZapretBtn');
  if (updateZapretBtn) {
    updateZapretBtn.onclick = async () => {
      if (!state.activeVersion) return showToast('Сначала выберите версию.', 'info');

      updateZapretBtn.disabled = true;
      $id('updateZapretLabel').textContent = 'Проверка версии';

      try {
        const [local, remote] = await withTimeout(
          invoke('check_zapret_version', { versionPath: state.activeVersion.path }),
          5000,
          'Сервер не отвечает'
        );

        if (local === remote) {
          showToast(`У вас самая свежая версия: ${local}`, 'success');
          return;
        }

        if (!(await showConfirm(`Найдена новая версия.\nУ вас: ${local}\nДоступна: ${remote}\n\nСкачать, распаковать и перенести настройки?`))) return;

        const oldName = state.activeVersion.name;
        const oldPath = state.activeVersion.path;
        $id('updateZapretLabel').textContent = 'Скачивание';

        const newDirName = await invoke('download_zapret', { rootPath: state.rootPath, activeName: oldName, newVersion: remote });
        await scanVersions(newDirName);

        if (await showConfirm(`Обновлено до ${remote}.\nУдалить старую версию (${oldName})?`)) {
          await invoke('delete_version', { rootPath: state.rootPath, versionPath: oldPath });
          await scanVersions(newDirName);
        }
      } catch(e) {
        showToast('Ошибка обновления: ' + e, 'error');
      } finally {
        updateZapretBtn.disabled = false;
        $id('updateZapretLabel').textContent = 'Обновить Zapret';
      }
    };
  }

  // --- Свитчер раскладки (бэкенд в lib.rs) ---

  function renderSwitcher() {
    const toggle = $id('switcherEnabled');
    if (toggle) toggle.checked = state.switcherEnabled;

    const enabledLabel = $id('switcherEnabledLabel');
    if (enabledLabel) enabledLabel.textContent = state.switcherEnabled ? 'Включён' : 'Выключен';

    const badge = $id('switcherStatusBadge');
    if (badge) {
      badge.textContent = state.switcherEnabled ? 'Активен' : 'Выключен';
      badge.classList.toggle('muted', !state.switcherEnabled);
    }

    const notice = $id('switcherNotice');
    if (notice) {
      if (!state.switcherEnabled) {
        notice.textContent = 'Свитчер выключен. Включите его, чтобы слова, набранные не в той раскладке, исправлялись автоматически.';
      } else if (!state.switcherDictionariesReady) {
        notice.textContent = 'Свитчер включён, но словари ещё загружаются. Исправление заработает через несколько секунд.';
      } else {
        notice.textContent = 'Свитчер активен: слова проверяются по словарю и исправляются автоматически. Double Shift конвертирует последнее слово или выделенный текст. Приложения и слова из списков исключений игнорируются.';
      }
    }

    const list = $id('switcherExceptions');
    if (list) {
      list.innerHTML = state.switcherExclusions.length === 0
        ? '<div class="exception-empty">Исключений нет</div>'
        : state.switcherExclusions.map((name, index) => `
      <div class="exception-row">
        <span>${escapeHtml(name)}</span>
        <button class="icon-btn" type="button" data-remove-exclusion="${index}" aria-label="Удалить исключение">
          <svg class="icon"><use href="#i-x"></use></svg>
        </button>
      </div>`).join('');
    }

    const wordList = $id('switcherWordExceptions');
    if (wordList) {
      wordList.innerHTML = state.switcherWordExclusions.length === 0
        ? '<div class="exception-empty">Слов-исключений нет</div>'
        : state.switcherWordExclusions.map((word, index) => `
      <div class="exception-row">
        <span>${escapeHtml(word)}</span>
        <button class="icon-btn" type="button" data-remove-word="${index}" aria-label="Удалить слово-исключение">
          <svg class="icon"><use href="#i-x"></use></svg>
        </button>
      </div>`).join('');
    }

    renderCustomDictList();

    renderSwitcherSuggestions();
    renderDeclinedWords();
  }

  // ---- Очередь предложений словаря ----
  // Тосты живут 12 секунд и исчезают, поэтому каждое предложение остаётся
  // в списке: за десяток переводов подряд ничего не теряется.
  const SUGGESTION_LIMIT = 30;

  function addSwitcherSuggestion(original, translated, toRussian) {
    if (!original || !translated) return;
    const exists = state.suggestions.some(
      (item) => item.original === original && item.translated === translated
    );
    if (!exists) {
      state.suggestions.push({ original, translated, toRussian });
      if (state.suggestions.length > SUGGESTION_LIMIT) state.suggestions.shift();
      renderSwitcherSuggestions();
    }
  }

  function removeSwitcherSuggestion(original, translated) {
    const before = state.suggestions.length;
    state.suggestions = state.suggestions.filter(
      (item) => !(item.original === original && item.translated === translated)
    );
    if (state.suggestions.length !== before) renderSwitcherSuggestions();
  }

  function renderSwitcherSuggestions() {
    const field = $id('customSuggestionsField');
    const list = $id('customSuggestionsList');
    if (!field || !list) return;
    const count = state.suggestions.length;
    field.hidden = count === 0;

    const badge = $id('switcherSuggestBadge');
    if (badge) {
      badge.hidden = count === 0;
      badge.textContent = String(count);
    }
    const counter = $id('customSuggestionsCount');
    if (counter) counter.textContent = count ? `всего: ${count}` : '';
    const addAll = $id('customSuggestionsAddAll');
    if (addAll) addAll.disabled = count === 0;

    if (!count) {
      list.innerHTML = '';
      return;
    }
    list.innerHTML = state.suggestions.map((item, index) => `
      <div class="exception-row">
        <span>${escapeHtml(item.translated)}<span class="suggestion-source"> ← ${escapeHtml(item.original)}</span></span>
        <span class="suggestion-actions">
          <button class="btn btn-ghost btn-sm" type="button" data-suggestion-add="${index}">Добавить</button>
          <button class="icon-btn" type="button" data-suggestion-skip="${index}" aria-label="Скрыть предложение">
            <svg class="icon"><use href="#i-x"></use></svg>
          </button>
        </span>
      </div>`).join('');
  }

  // Отклонённые слова показаны компактными чипами: отказ мог быть случайным,
  // поэтому у каждого чипа есть «↺» — снять отказ и вернуть предложение.
  function renderDeclinedWords() {
    const field = $id('customDeclinedField');
    const list = $id('declinedList');
    if (!field || !list) return;
    const count = state.declinedWords.length;
    field.hidden = count === 0;
    const hint = $id('declinedWordsHint');
    if (hint) hint.textContent = `всего: ${count}`;
    const clear = $id('clearDeclinedWordsBtn');
    if (clear) clear.disabled = count === 0;
    list.innerHTML = state.declinedWords.map((entry, index) => `
      <span class="chip" title="${escapeHtml(entry.original)}">
        <span class="chip-text">${escapeHtml(entry.original)}</span>
        <button class="chip-x" type="button" data-declined-index="${index}" aria-label="Вернуть слово в предложения" title="Вернуть в предложения">
          <svg class="icon"><use href="#i-rotate"></use></svg>
        </button>
      </span>`).join('');
  }

  function renderCustomDictList() {
    const list = $id('customDictList');
    if (!list) return;
    const rows = [
      ...state.customDictRu.map((word) => ({ word, language: 'ru' })),
      ...state.customDictEn.map((word) => ({ word, language: 'en' }))
    ];
    if (!rows.length) {
      list.innerHTML = '<div class="exception-empty">Слов нет</div>';
      return;
    }
    list.innerHTML = rows.map(({ word, language }) => `
      <div class="exception-row">
        <span>${escapeHtml(word)}<span class="word-badge">${language.toUpperCase()}</span></span>
        <button class="icon-btn" type="button" data-custom-word="${escapeHtml(word)}" data-custom-language="${language}" aria-label="Удалить слово из словаря">
          <svg class="icon"><use href="#i-x"></use></svg>
        </button>
      </div>`).join('');
  }

  // Полный объект конфига: save_config перезаписывает файл целиком,
  // поэтому в каждом вызове должны быть и настройки свитчера.
  function configPayload() {
    return {
      root_path: state.rootPath,
      active_version: state.activeVersion ? state.activeVersion.name : null,
      switcher_enabled: state.switcherEnabled,
      switcher_exclusions: state.switcherExclusions.slice(),
      switcher_word_exclusions: state.switcherWordExclusions.slice(),
      switcher_custom_dict_ru: state.customDictRu.slice(),
      switcher_custom_dict_en: state.customDictEn.slice(),
      // Бэкенд ждёт snake_case: в состоянии поле хранится как toRussian.
      switcher_declined_words: state.declinedWords.map((entry) => ({
        original: entry.original,
        translated: entry.translated,
        to_russian: entry.toRussian
      })),
      auto_check_updates: state.autoCheckUpdates
    };
  }

  async function persistConfig() {
    try {
      await invoke('save_config', { config: configPayload() });
    } catch (e) {
      showToast('Не удалось сохранить настройки: ' + e, 'error');
    }
  }

  async function applySwitcherConfig() {
    try {
      await invoke('update_switcher_config', {
        enabled: state.switcherEnabled,
        exclusions: state.switcherExclusions
      });
      // Настройки свитчера живут в памяти хука, но должны пережить перезапуск.
      await persistConfig();
    } catch (e) {
      showToast('Не удалось применить настройки свитчера: ' + e, 'error');
    }
  }

  function warnSwitcherAdmin(message) {
    if (state.switcherAdminWarned) return;
    state.switcherAdminWarned = true;
    showToast(
      message || 'Свитчер запущен без прав администратора: в окнах, запущенных от имени администратора, перехват клавиатуры может не работать.',
      'info'
    );
  }

  function bindSwitcherEvents() {
    if (state.switcherEventsBound) return;
    const tauriEvent = window.__TAURI__?.event;
    if (!tauriEvent?.listen) return;
    state.switcherEventsBound = true;

    tauriEvent.listen('switcher-error', (event) => {
      showToast(String(event.payload), 'error');
    });
    tauriEvent.listen('switcher-warning', (event) => {
      warnSwitcherAdmin(event?.payload ? String(event.payload) : undefined);
    });
    tauriEvent.listen('switcher-dictionaries-ready', () => {
      state.switcherDictionariesReady = true;
      renderSwitcher();
    });
    tauriEvent.listen('switcher-suggest-exclusion', (event) => {
      const word = String(event?.payload || '').trim();
      if (!word) return;
      showToast(`Слово «${word}» исправлено неверно.`, 'info', {
        actionLabel: 'В исключения',
        timeout: 8000,
        onAction: async () => {
          if (await addSwitcherWordExclusion(word)) {
            showToast(`Слово «${word}» больше не будет исправляться.`, 'success');
          }
        }
      });
    });

    tauriEvent.listen('switcher-suggest-custom-word', (event) => {
      const payload = event?.payload || {};
      const original = String(payload.original || '').trim();
      const translated = String(payload.translated || '').trim();
      if (!original || !translated) return;

      // to_russian === false означает, что переведённое слово английское.
      const toRussian = payload.to_russian !== false;

      addSwitcherSuggestion(original, translated, toRussian);

      showToast(`Слово «${translated}» переведено. Добавить в словарь?`, 'info', {
        actionLabel: 'Добавить',
        cancelLabel: 'Отмена',
        timeout: 12000,
        onAction: async () => {
          // В словарь идёт только переведённое слово: исходный набор
          // («мщщм») — опечатка, ему там не место.
          if (await addSuggestedCustomWord(translated, toRussian)) {
            removeSwitcherSuggestion(original, translated);
            showToast(`Слово «${translated}» добавлено в словарь.`, 'success');
          }
        },
        // Закрытие тоста без «Добавить» = отказ: больше не предлагаем.
        onDismiss: () => {
          removeSwitcherSuggestion(original, translated);
          declineCustomWord(original, translated, toRussian);
        }
      });
    });
  }

  async function initSwitcher() {
    renderSwitcher();
    bindSwitcherEvents();
    try {
      await invoke('start_switcher_listener');
      // Состояние хука бэкенд уже восстановил из config.json при старте;
      // get_config нужен, чтобы UI показывал ровно то, что сохранено на диске.
      const [config, cfg] = await Promise.all([
        invoke('get_config'),
        invoke('get_switcher_config')
      ]);
      if (config) {
        state.switcherEnabled = !!config.switcher_enabled;
        state.switcherExclusions = Array.isArray(config.switcher_exclusions)
          ? config.switcher_exclusions.slice()
          : [];
        state.switcherWordExclusions = Array.isArray(config.switcher_word_exclusions)
          ? config.switcher_word_exclusions.slice()
          : [];
        state.customDictRu = Array.isArray(config.switcher_custom_dict_ru)
          ? config.switcher_custom_dict_ru.slice()
          : [];
        state.customDictEn = Array.isArray(config.switcher_custom_dict_en)
          ? config.switcher_custom_dict_en.slice()
          : [];
        state.declinedWords = normalizeDeclinedWords(config.switcher_declined_words);
      }
      if (cfg) {
        state.switcherDictionariesReady = !!cfg.dictionaries_ready;
        state.switcherAdmin = cfg.admin !== false;
      }
      renderSwitcher();
      if (!state.switcherAdmin) warnSwitcherAdmin();

      if (!state.switcherDictionariesReady) {
        // Если событие о готовности словарей потерялось (или загрузка не удалась),
        // через 15 секунд перепроверяем состояние на бэкенде.
        setTimeout(async () => {
          if (state.switcherDictionariesReady) return;
          try {
            const cfg = await invoke('get_switcher_config');
            if (cfg && cfg.dictionaries_ready) {
              state.switcherDictionariesReady = true;
              renderSwitcher();
            } else {
              showToast('Словари свитчера не загружены. Проверьте подключение к интернету.', 'error');
            }
          } catch (_) {}
        }, 15000);
      }
    } catch (e) {
      showToast('Свитчер недоступен: ' + e, 'error');
    }
  }

  $id('switcherEnabled').onchange = async (e) => {
    state.switcherEnabled = e.target.checked;
    renderSwitcher();
    await applySwitcherConfig();
    showToast(
      state.switcherEnabled
        ? 'Свитчер включён: слова будут исправляться по словарю.'
        : 'Свитчер выключен.',
      state.switcherEnabled ? 'success' : 'info'
    );
  };

  async function addSwitcherExclusionValue(rawValue) {
    const value = String(rawValue || '').trim();
    if (!value) return false;

    const lower = value.toLowerCase();
    const lowerNoExt = lower.replace(/\.exe$/i, '');
    const exists = state.switcherExclusions.some(ex => {
      const current = ex.toLowerCase();
      return current === lower || current.replace(/\.exe$/i, '') === lowerNoExt;
    });
    if (exists) {
      showToast('Такое исключение уже добавлено.', 'info');
      return false;
    }

    state.switcherExclusions.push(value);
    renderSwitcher();
    if (!$id('switcherProcessPicker')?.hidden) renderProcessList();
    await applySwitcherConfig();
    return true;
  }

  async function addSwitcherExclusion() {
    const input = $id('switcherException');
    const value = input.value.trim();
    if (!value) return;
    if (await addSwitcherExclusionValue(value)) input.value = '';
  }

  /// Добавляет слово в список «не исправлять» (бэкенд дедуплицирует сам).
  async function addSwitcherWordExclusion(rawValue) {
    const word = String(rawValue || '').trim();
    if (!word) return false;
    try {
      const updated = await invoke('add_switcher_word_exclusion', { word });
      state.switcherWordExclusions = Array.isArray(updated) ? updated : state.switcherWordExclusions;
      renderSwitcher();
      await persistConfig();
      return true;
    } catch (e) {
      showToast('Не удалось добавить слово-исключение: ' + e, 'error');
      return false;
    }
  }

  async function removeSwitcherWordExclusion(word) {
    try {
      const updated = await invoke('remove_switcher_word_exclusion', { word });
      state.switcherWordExclusions = Array.isArray(updated) ? updated : state.switcherWordExclusions;
      renderSwitcher();
      await persistConfig();
    } catch (e) {
      showToast('Не удалось удалить слово-исключение: ' + e, 'error');
    }
  }

  // ---- Пользовательский словарь и отклонённые предложения ----

  // Отклонённые предложения хранят пару слов; строки из старых конфигов
  // приводим к тому же виду, чтобы остальной код работал с одним форматом.
  function normalizeDeclinedWords(value) {
    if (!Array.isArray(value)) return [];
    return value.map((entry) => {
      if (typeof entry === 'string') {
        return { original: entry, translated: null, toRussian: null };
      }
      if (!entry || typeof entry !== 'object' || typeof entry.original !== 'string') return null;
      return {
        original: entry.original,
        translated: typeof entry.translated === 'string' ? entry.translated : null,
        toRussian: typeof entry.to_russian === 'boolean' ? entry.to_russian : null
      };
    }).filter(Boolean);
  }

  function applyCustomWords(data) {
    if (!data || typeof data !== 'object') return;
    if (Array.isArray(data.ru)) state.customDictRu = data.ru.slice();
    if (Array.isArray(data.en)) state.customDictEn = data.en.slice();
    if (Array.isArray(data.declined)) state.declinedWords = normalizeDeclinedWords(data.declined);
    renderSwitcher();
  }

  async function addCustomWordFromInput(language) {
    const input = $id(language === 'ru' ? 'customDictRuInput' : 'customDictEnInput');
    const word = (input?.value || '').trim();
    if (!word) return;
    try {
      const data = await invoke('add_custom_word', { word, language });
      applyCustomWords(data);
      input.value = '';
      await persistConfig();
      showToast(`Слово «${word}» добавлено в словарь.`, 'success');
    } catch (e) {
      showToast('Не удалось добавить слово: ' + e, 'error');
    }
  }

  async function removeCustomWord(language, word) {
    try {
      const data = await invoke('remove_custom_word', { word, language });
      applyCustomWords(data);
      await persistConfig();
    } catch (e) {
      showToast('Не удалось удалить слово: ' + e, 'error');
    }
  }

  /// Добавляет пару «исходное — переведённое» в словари обоих языков.
  async function addSuggestedCustomWord(word, toRussian, persist = true) {
    const language = toRussian ? 'ru' : 'en';
    try {
      const data = await invoke('add_custom_word', { word, language });
      applyCustomWords(data);
      if (persist) await persistConfig();
      return true;
    } catch (e) {
      showToast('Не удалось добавить слово в словарь: ' + e, 'error');
      return false;
    }
  }

  async function declineCustomWord(word, translated, toRussian) {
    const value = String(word || '').trim();
    if (!value) return;
    try {
      const updated = await invoke('decline_custom_word', {
        word: value,
        translated: translated ? String(translated) : null,
        toRussian: typeof toRussian === 'boolean' ? toRussian : null
      });
      state.declinedWords = normalizeDeclinedWords(updated);
      renderSwitcher();
      await persistConfig();
    } catch (e) {
      showToast('Не удалось сохранить отказ: ' + e, 'error');
    }
  }

  async function clearDeclinedWords() {
    try {
      const updated = await invoke('clear_declined_words');
      state.declinedWords = Array.isArray(updated) ? updated : [];
      renderSwitcher();
      await persistConfig();
      showToast('Список отклонённых предложений очищен.', 'success');
    } catch (e) {
      showToast('Не удалось очистить список: ' + e, 'error');
    }
  }

  function closeProcessPicker() {
    const picker = $id('switcherProcessPicker');
    if (picker) picker.hidden = true;
    $id('switcherPickProcess')?.setAttribute('aria-expanded', 'false');
  }

  function renderProcessList() {
    const listEl = $id('switcherProcessList');
    if (!listEl) return;

    const query = ($id('switcherProcessFilter')?.value || '').trim().toLowerCase();
    const items = state.switcherProcesses
      .filter(name => !query || name.toLowerCase().includes(query))
      .slice(0, 300);

    if (items.length === 0) {
      listEl.innerHTML = '<div class="exception-empty">Ничего не найдено</div>';
      return;
    }

    const existing = new Set();
    for (const ex of state.switcherExclusions) {
      const lower = ex.toLowerCase();
      existing.add(lower);
      existing.add(lower.replace(/\.exe$/i, ''));
    }

    listEl.innerHTML = items.map(name => {
      const lower = name.toLowerCase();
      const added = existing.has(lower) || existing.has(lower.replace(/\.exe$/i, ''));
      return `<button class="process-item" type="button" data-process-name="${escapeHtml(name)}" ${added ? 'disabled' : ''}>
        <span>${escapeHtml(name)}</span>
        ${added ? '<span class="process-added">уже в списке</span>' : ''}
      </button>`;
    }).join('');
  }

  async function toggleProcessPicker() {
    const picker = $id('switcherProcessPicker');
    const button = $id('switcherPickProcess');
    if (!picker || !button) return;

    if (!picker.hidden) {
      closeProcessPicker();
      return;
    }

    picker.hidden = false;
    button.setAttribute('aria-expanded', 'true');

    const listEl = $id('switcherProcessList');
    if (listEl) listEl.innerHTML = '<div class="exception-empty">Загрузка списка процессов...</div>';

    try {
      // Обновляем список при каждом открытии — игры и лаунчеры
      // могли запуститься уже после старта приложения.
      state.switcherProcesses = await invoke('get_running_processes');
      renderProcessList();
      $id('switcherProcessFilter')?.focus();
    } catch (e) {
      if (listEl) {
        listEl.innerHTML = '<div class="exception-empty error-text">Не удалось получить список процессов</div>';
      }
    }
  }

  $id('switcherAddException').onclick = addSwitcherExclusion;
  $id('switcherException').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); addSwitcherExclusion(); }
  });

  $id('switcherPickProcess').onclick = (e) => {
    e.stopPropagation();
    toggleProcessPicker();
  };
  $id('switcherProcessFilter').addEventListener('input', renderProcessList);
  $id('switcherProcessFilter').addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeProcessPicker();
  });
  $id('switcherProcessList').addEventListener('click', async (e) => {
    const btn = e.target.closest('[data-process-name]');
    if (!btn || btn.disabled) return;
    if (await addSwitcherExclusionValue(btn.dataset.processName)) closeProcessPicker();
  });
  document.addEventListener('click', (e) => {
    const picker = $id('switcherProcessPicker');
    if (!picker || picker.hidden) return;
    if (!e.target.closest('#switcherProcessPicker') && !e.target.closest('#switcherPickProcess')) {
      closeProcessPicker();
    }
  });

  $id('switcherExceptions').addEventListener('click', async (e) => {
    const btn = e.target.closest('[data-remove-exclusion]');
    if (!btn) return;
    const index = Number(btn.dataset.removeExclusion);
    if (!Number.isInteger(index) || index < 0) return;
    state.switcherExclusions.splice(index, 1);
    renderSwitcher();
    await applySwitcherConfig();
  });

  $id('switcherAddWordException').onclick = async () => {
    const input = $id('switcherWordException');
    const word = input.value.trim();
    if (!word) return;
    if (await addSwitcherWordExclusion(word)) input.value = '';
  };
  $id('switcherWordException').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      $id('switcherAddWordException').click();
    }
  });
  $id('switcherWordExceptions').addEventListener('click', async (e) => {
    const btn = e.target.closest('[data-remove-word]');
    if (!btn) return;
    const index = Number(btn.dataset.removeWord);
    if (!Number.isInteger(index) || index < 0) return;
    const word = state.switcherWordExclusions[index];
    if (word) await removeSwitcherWordExclusion(word);
  });

  // ---- Пользовательский словарь ----
  $id('customDictRuAdd').onclick = () => addCustomWordFromInput('ru');
  $id('customDictEnAdd').onclick = () => addCustomWordFromInput('en');
  $id('customDictRuInput').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      addCustomWordFromInput('ru');
    }
  });
  $id('customDictEnInput').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      addCustomWordFromInput('en');
    }
  });
  const handleCustomDictRemove = async (event) => {
    const btn = event.target.closest('[data-custom-word]');
    if (!btn || btn.disabled) return;
    btn.disabled = true;
    await removeCustomWord(btn.dataset.customLanguage, btn.dataset.customWord);
  };
  $id('customDictList').addEventListener('click', handleCustomDictRemove);
  $id('clearDeclinedWordsBtn').onclick = () => clearDeclinedWords();

  $id('customSuggestionsList').addEventListener('click', async (event) => {
    const addBtn = event.target.closest('[data-suggestion-add]');
    const skipBtn = event.target.closest('[data-suggestion-skip]');
    const button = addBtn || skipBtn;
    if (!button || button.disabled) return;
    const index = Number(addBtn ? addBtn.dataset.suggestionAdd : button.dataset.suggestionSkip);
    if (!Number.isInteger(index) || index < 0 || index >= state.suggestions.length) return;
    const item = state.suggestions[index];
    if (!item) return;
    button.disabled = true;
    if (addBtn) {
      if (await addSuggestedCustomWord(item.translated, item.toRussian)) {
        removeSwitcherSuggestion(item.original, item.translated);
      }
    } else {
      await declineCustomWord(item.original, item.translated, item.toRussian);
      removeSwitcherSuggestion(item.original, item.translated);
    }
  });

  $id('customSuggestionsAddAll').onclick = async () => {
    const button = $id('customSuggestionsAddAll');
    if (button.disabled || !state.suggestions.length) return;
    button.disabled = true;
    const items = state.suggestions.slice();
    let added = 0;
    for (const item of items) {
      if (await addSuggestedCustomWord(item.translated, item.toRussian, false)) added += 1;
    }
    state.suggestions = [];
    renderSwitcherSuggestions();
    await persistConfig();
    showToast(added ? `В словарь добавлено слов: ${added}.` : 'Ничего не добавлено.', added ? 'success' : 'info');
    button.disabled = false;
  };

  $id('declinedList').addEventListener('click', async (event) => {
    const btn = event.target.closest('[data-declined-index]');
    if (!btn || btn.disabled) return;
    const index = Number(btn.dataset.declinedIndex);
    const entry = state.declinedWords[index];
    if (!entry) return;
    btn.disabled = true;
    try {
      const result = await invoke('remove_declined_word', { word: entry.original });
      state.declinedWords = normalizeDeclinedWords(result?.declined);
      const removed = result?.removed;
      const restored = removed
        && typeof removed.translated === 'string' && removed.translated
        && typeof removed.to_russian === 'boolean';
      if (restored) {
        addSwitcherSuggestion(removed.original, removed.translated, removed.to_russian);
      }
      renderSwitcher();
      await persistConfig();
      showToast(
        restored ? 'Слово возвращено в предложения.' : 'Отказ снят: слово снова может быть предложено.',
        'info'
      );
    } catch (e) {
      showToast('Не удалось убрать отказ: ' + e, 'error');
    }
  });

  // ---- Сеть: DNS, DoH и блокировка QUIC ----
  const DNS_PRESETS = {
    cloudflare: { dns1: '1.1.1.1', dns2: '1.0.0.1', name: 'Cloudflare' },
    google: { dns1: '8.8.8.8', dns2: '8.8.4.4', name: 'Google' },
    quad9: { dns1: '9.9.9.9', dns2: '149.112.112.112', name: 'Quad9' },
    adguard: { dns1: '94.140.14.14', dns2: '94.140.15.15', name: 'AdGuard' }
  };

  function isValidDnsAddress(value) {
    const text = String(value || '').trim();
    if (!text) return false;
    if (text.includes(':')) return /^[0-9a-fA-F:]+$/.test(text);
    const parts = text.split('.');
    return parts.length === 4
      && parts.every(part => /^\d{1,3}$/.test(part) && Number(part) <= 255);
  }

  function syncDnsCustomVisibility() {
    const custom = $id('dnsPreset').value === 'custom';
    $id('dnsCustomFields').hidden = !custom;
    if (custom) $id('dnsCustom1').focus();
  }

  async function refreshDnsInterfaces(preferred) {
    const select = $id('dnsInterface');
    if (!select) return;
    let interfaces = [];
    try { interfaces = await invoke('list_network_interfaces'); } catch { interfaces = []; }
    if (!Array.isArray(interfaces) || !interfaces.length) interfaces = ['Ethernet', 'Wi-Fi'];

    const previous = preferred || select.value;
    select.innerHTML = interfaces
      .map(name => `<option value="${escapeHtml(name)}">${escapeHtml(name)}</option>`)
      .join('');
    if (previous && interfaces.includes(previous)) select.value = previous;
    await refreshDnsState();
  }

  async function refreshDnsState() {
    const select = $id('dnsInterface');
    const label = $id('dnsCurrent');
    if (!select || !label) return;
    const name = select.value;
    if (!name) { label.textContent = 'Текущие DNS: —'; return; }
    try {
      const servers = await invoke('get_dns_state', { interfaceName: name });
      label.textContent = Array.isArray(servers) && servers.length
        ? `Текущие DNS: ${servers.join(', ')}`
        : 'Текущие DNS: автоматически (DHCP)';
    } catch {
      label.textContent = 'Текущие DNS: не удалось получить';
    }
  }

  async function syncQuicState() {
    try {
      const enabled = await invoke('get_quic_block_state');
      $id('quicBlock').checked = !!enabled;
      $id('quicBlockLabel').textContent = enabled ? 'Включено' : 'Выключено';
    } catch { /* состояние недоступно — оставляем как есть */ }
  }

  async function loadNetworkTab() {
    const select = $id('dnsInterface');
    const hint = $id('dnsCurrent');
    const refreshBtn = $id('dnsRefreshBtn');
    const spinner = $id('dnsRefreshSpinner');
    // Оба вызова — отдельные процессы PowerShell (до 4 с каждый): без индикации
    // вкладка выглядит пустой и кажется, что кнопки не работают.
    if (select) select.disabled = true;
    if (refreshBtn) refreshBtn.disabled = true;
    if (spinner) spinner.hidden = false;
    if (hint) hint.textContent = 'Текущие DNS: загрузка…';
    try {
      await Promise.all([refreshDnsInterfaces(), syncQuicState()]);
    } catch (e) {
      // Раньше reject уходил в unhandled rejection и поля оставались без данных.
      if (hint) hint.textContent = 'Текущие DNS: не удалось получить';
      showToast('Не удалось загрузить настройки сети: ' + e, 'error');
    } finally {
      if (select) select.disabled = false;
      if (refreshBtn) refreshBtn.disabled = false;
      if (spinner) spinner.hidden = true;
      syncDnsCustomVisibility();
    }
  }

  async function loadDiagnosticsTab() {
    const box = $id('diagLog');
    const spinner = $id('diagSpinner');
    const button = $id('diagRefreshBtn');
    if (!box) return;
    if (button) button.disabled = true;
    if (spinner) spinner.hidden = false;
    try {
      const lines = await invoke('get_log_tail', { lines: 200 });
      box.textContent = Array.isArray(lines) && lines.length
        ? lines.join('\n')
        : 'Лог пуст: свитчер ещё не запускался или событий не было.';
      box.scrollTop = box.scrollHeight;
    } catch (e) {
      box.textContent = 'Не удалось прочитать лог: ' + e;
    } finally {
      if (button) button.disabled = false;
      if (spinner) spinner.hidden = true;
    }
  }

  $id('diagRefreshBtn').onclick = () => loadDiagnosticsTab();
  $id('diagOpenBtn').onclick = async () => {
    try {
      await invoke('open_switcher_log');
    } catch (e) {
      showToast(String(e), 'error');
    }
  };
  $id('diagCopyBtn').onclick = async () => {
    const text = $id('diagLog')?.textContent || '';
    try {
      await navigator.clipboard.writeText(text);
      showToast('Лог скопирован в буфер обмена', 'success');
    } catch {
      showToast('Не удалось скопировать: выделите текст вручную', 'error');
    }
  };

  $id('dnsRefreshBtn').onclick = () => loadNetworkTab();
  $id('dnsInterface').onchange = () => refreshDnsState();
  $id('dnsPreset').onchange = () => syncDnsCustomVisibility();

  $id('dnsApplyBtn').onclick = async () => {
    const interfaceName = $id('dnsInterface').value;
    if (!interfaceName) return showToast('Выберите сетевое подключение', 'error');

    const presetKey = $id('dnsPreset').value;
    const doh = $id('dnsDoh').checked;
    let dns1;
    let dns2;
    let presetName;

    if (presetKey === 'custom') {
      dns1 = ($id('dnsCustom1').value || '').trim();
      dns2 = ($id('dnsCustom2').value || '').trim();
      if (!isValidDnsAddress(dns1)) {
        return showToast('Укажите корректный основной DNS, например 1.1.1.1', 'error');
      }
      if (dns2 && !isValidDnsAddress(dns2)) {
        return showToast('Дополнительный DNS указан неверно', 'error');
      }
      presetName = 'Свой DNS';
    } else {
      const preset = DNS_PRESETS[presetKey] || DNS_PRESETS.cloudflare;
      dns1 = preset.dns1;
      dns2 = preset.dns2;
      presetName = preset.name;
    }

    const button = $id('dnsApplyBtn');
    const spinner = $id('dnsSpinner');
    button.disabled = true;
    spinner.hidden = false;
    try {
      await invoke('set_dns', { interfaceName, dns1, dns2: dns2 || null, doh });
      showToast(`DNS ${presetName} применён${doh ? ' (DoH)' : ''}`, 'success');
      logTo($id('networkLog'), `DNS ${presetName}: ${dns1}${dns2 ? ', ' + dns2 : ''}${doh ? ' + DoH' : ''}`, 'ok');
      await refreshDnsState();
    } catch (e) {
      showToast('Не удалось изменить DNS: ' + e, 'error');
    } finally {
      button.disabled = false;
      spinner.hidden = true;
    }
  };

  $id('dnsResetBtn').onclick = async () => {
    const interfaceName = $id('dnsInterface').value;
    if (!interfaceName) return showToast('Выберите сетевое подключение', 'error');
    try {
      await invoke('reset_dns', { interfaceName });
      showToast('DNS сброшен на автоматический', 'success');
      logTo($id('networkLog'), 'DNS сброшен на DHCP', 'ok');
      await refreshDnsState();
    } catch (e) {
      showToast('Не удалось сбросить DNS: ' + e, 'error');
    }
  };

  $id('quicBlock').onchange = async (event) => {
    const enabled = event.target.checked;
    const input = event.target;
    const label = $id('quicBlockLabel');
    input.disabled = true;
    label.textContent = enabled ? 'Включаем…' : 'Выключаем…';
    try {
      await invoke('toggle_quic_block', { enable: enabled });
      label.textContent = enabled ? 'Включено' : 'Выключено';
      showToast(enabled ? 'QUIC заблокирован (UDP 80/443)' : 'Блокировка QUIC снята', 'success');
      logTo($id('networkLog'), enabled ? 'Правило брандмауэра QUIC добавлено' : 'Правило брандмауэра QUIC удалено', 'ok');
    } catch (e) {
      input.checked = !enabled;
      label.textContent = enabled ? 'Выключено' : 'Включено';
      showToast('Не удалось изменить правило: ' + e, 'error');
    } finally {
      input.disabled = false;
    }
  };

  // ---- Hosts ----
  function hostsEntryLooksValid(entry) {
    return /^\s*(?:#.*|[0-9a-fA-F:.%]+\s+\S+.*)$/.test(entry);
  }

  function renderHosts() {
    const list = $id('hostsList');
    if (!list) return;
    if (!state.hostsEntries.length) {
      list.innerHTML = '<div class="exception-empty">Записей нет</div>';
      return;
    }
    list.innerHTML = state.hostsEntries.map((entry, index) => `
      <div class="exception-row">
        <span>${escapeHtml(entry)}</span>
        <button class="icon-btn" type="button" data-host-remove="${index}" title="Удалить запись">
          <svg class="icon"><use href="#i-x"></use></svg>
        </button>
      </div>`).join('');
  }

  function addHostsEntry() {
    const input = $id('hostsEntry');
    const value = (input.value || '').trim();
    if (!value) return;
    if (!hostsEntryLooksValid(value)) {
      return showToast('Формат: IP и домен, например 1.2.3.4 example.com', 'error');
    }
    if (state.hostsEntries.some(entry => entry.toLowerCase() === value.toLowerCase())) {
      return showToast('Такая запись уже есть', 'info');
    }
    state.hostsEntries.push(value);
    input.value = '';
    renderHosts();
  }

  async function loadHostsTab() {
    try {
      const data = await invoke('read_hosts');
      state.hostsEntries = Array.isArray(data.entries) ? data.entries : [];
      $id('hostsPath').textContent = data.path || '';
      renderHosts();
    } catch (e) {
      showToast('Не удалось прочитать hosts: ' + e, 'error');
    }
  }

  $id('hostsAddBtn').onclick = () => addHostsEntry();
  $id('hostsEntry').onkeydown = (event) => {
    if (event.key === 'Enter') { event.preventDefault(); addHostsEntry(); }
  };
  $id('hostsList').onclick = (event) => {
    const button = event.target.closest('[data-host-remove]');
    if (!button) return;
    const index = Number(button.dataset.hostRemove);
    if (!Number.isInteger(index) || index < 0) return;
    state.hostsEntries.splice(index, 1);
    renderHosts();
  };
  $id('hostsApplyBtn').onclick = async () => {
    const button = $id('hostsApplyBtn');
    if (button.disabled) return;
    const spinner = $id('hostsSpinner');
    const label = $id('hostsApplyLabel');
    button.disabled = true;
    spinner.hidden = false;
    label.textContent = 'Применение…';
    try {
      await invoke('update_hosts', { entries: state.hostsEntries });
      showToast('hosts обновлён', 'success');
      await loadHostsTab();
    } catch (e) {
      showToast('Не удалось обновить hosts: ' + e, 'error');
    } finally {
      button.disabled = false;
      spinner.hidden = true;
      label.textContent = 'Применить';
    }
  };
  $id('hostsReloadBtn').onclick = () => loadHostsTab();
  $id('hostsAutoBtn').onclick = async () => {
    const button = $id('hostsAutoBtn');
    if (button.disabled) return;
    const spinner = $id('hostsAutoSpinner');
    const label = $id('hostsAutoLabel');
    button.disabled = true;
    spinner.hidden = false;
    label.textContent = 'Обновление…';
    try {
      const result = await invoke('auto_update_hosts');
      showToast(typeof result === 'string' && result ? result : 'hosts обновлён из Zapret', 'success');
      await loadHostsTab();
    } catch (e) {
      showToast('Не удалось авто-обновить hosts: ' + e, 'error');
    } finally {
      button.disabled = false;
      spinner.hidden = true;
      label.textContent = 'Авто-обновить из Zapret';
    }
  };

  // ---- Инструменты ----
  function renderTools() {
    const list = $id('toolsList');
    if (!list) return;
    if (!state.tools.length) {
      list.innerHTML = '<div class="exception-empty">В папке сборки нет exe-файлов</div>';
      return;
    }
    list.innerHTML = state.tools.map(name => {
      const running = state.toolProcesses.filter(p => String(p.name).toLowerCase() === name.toLowerCase());
      const badge = running.length
        ? `<span class="vbadge">PID ${running.map(p => p.pid).join(', ')}</span>`
        : '<span class="vbadge muted">не запущен</span>';
      const action = running.length
        ? `<button class="btn btn-danger btn-sm" type="button" data-tool-stop="${running[0].pid}">Остановить</button>`
        : `<button class="btn btn-secondary btn-sm" type="button" data-tool-start="${escapeHtml(name)}">Запустить</button>`;
      return `<div class="exception-row"><span>${escapeHtml(name)} ${badge}</span>${action}</div>`;
    }).join('');
  }

  async function loadToolsTab() {
    const list = $id('toolsList');
    if (!list) return;
    if (!state.activeVersion) {
      list.innerHTML = '<div class="exception-empty">Сначала выберите активную версию во вкладке «Версии»</div>';
      return;
    }
    list.innerHTML = '<div class="exception-empty">Загрузка…</div>';
    try {
      const [tools, running] = await Promise.all([
        invoke('list_tools', { versionPath: state.activeVersion.path }),
        invoke('find_tool_processes', { versionPath: state.activeVersion.path })
      ]);
      state.tools = Array.isArray(tools) ? tools : [];
      state.toolProcesses = Array.isArray(running) ? running : [];
      renderTools();
    } catch (e) {
      list.innerHTML = `<div class="exception-empty error-text">Ошибка: ${escapeHtml(e)}</div>`;
    }
  }

  $id('toolsRefreshBtn').onclick = () => loadToolsTab();
  $id('toolsList').onclick = async (event) => {
    const startButton = event.target.closest('[data-tool-start]');
    if (startButton) {
      if (!state.activeVersion) return;
      const exeName = startButton.dataset.toolStart;
      startButton.disabled = true;
      try {
        const pid = await invoke('start_tool', { versionPath: state.activeVersion.path, exeName });
        showToast(`${exeName} запущен (PID ${pid})`, 'success');
        await loadToolsTab();
      } catch (e) {
        startButton.disabled = false;
        showToast('Не удалось запустить: ' + e, 'error');
      }
      return;
    }

    const stopButton = event.target.closest('[data-tool-stop]');
    if (stopButton) {
      const pid = Number(stopButton.dataset.toolStop);
      if (!Number.isInteger(pid) || pid <= 0) return;
      stopButton.disabled = true;
      try {
        await invoke('stop_tool', { pid });
        showToast('Процесс остановлен', 'success');
      } catch (e) {
        showToast('Не удалось остановить: ' + e, 'error');
      }
      await loadToolsTab();
    }
  };

  // ---- Blockcheck ----
  let blockcheckRunning = false;

  function openBlockcheckModal() {
    if (!state.activeVersion) return showToast('Сначала выберите активную версию', 'error');
    $id('blockcheckModal').classList.add('is-open');
    if (blockcheckRunning) {
      $id('blockcheckForm').hidden = true;
      $id('blockcheckProgress').hidden = false;
      $id('blockcheckStart').disabled = true;
      return;
    }
    $id('blockcheckForm').hidden = false;
    $id('blockcheckProgress').hidden = true;
    $id('blockcheckStart').disabled = false;
    $id('blockcheckDomains').focus();
  }

  function closeBlockcheckModal() {
    $id('blockcheckModal').classList.remove('is-open');
  }

  function renderBlockcheckStrategies(strategies) {
    const log = $id('versionsLog');
    if (!log) return;
    logTo(log, `Найдено стратегий: ${strategies.length}`, 'ok');

    strategies.slice(0, 10).forEach((command, index) => {
      const row = document.createElement('div');
      row.className = 'log-line';
      const preview = command.length > 160 ? command.slice(0, 160) + '…' : command;
      row.innerHTML = `<span class="mono-value">${escapeHtml(preview)}</span> `;

      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'btn btn-secondary btn-sm';
      button.textContent = 'Сохранить .bat';
      button.onclick = async () => {
        try {
          const path = await invoke('save_blockcheck_strategy', {
            versionPath: state.activeVersion.path,
            name: `blockcheck-${index + 1}`,
            command
          });
          showToast('Стратегия сохранена: ' + path, 'success', { timeout: 8000 });
        } catch (e) {
          showToast('Не удалось сохранить стратегию: ' + e, 'error');
        }
      };
      row.appendChild(button);
      log.appendChild(row);
      while (log.childElementCount > LOG_DRAWER_LIMIT) log.firstElementChild.remove();
    });
    syncLogDrawer();
    setLogDrawerOpen(true);
  }

  async function runBlockcheck() {
    if (!state.activeVersion) return showToast('Сначала выберите активную версию', 'error');
    if (blockcheckRunning) return;
    const raw = $id('blockcheckDomains').value || '';
    const domains = raw.split(/[,;\s]+/).map(item => item.trim()).filter(Boolean);
    if (!domains.length) return showToast('Укажите хотя бы один домен', 'error');

    blockcheckRunning = true;
    $id('blockcheckForm').hidden = true;
    $id('blockcheckProgress').hidden = false;
    $id('blockcheckStart').disabled = true;

    logTo($id('versionsLog'), `Blockcheck: проверяем ${domains.join(', ')}…`);
    try {
      const report = await invoke('run_blockcheck', {
        versionPath: state.activeVersion.path,
        domains
      });
      if (!report || typeof report !== 'object') throw new Error('Пустой ответ Blockcheck');

      const lines = String(report.log || '').split(/\r?\n/).filter(Boolean);
      lines.forEach(line => {
        const cls = /blocked|fail|error|ошибка/i.test(line)
          ? 'err'
          : (/\[ok\]|summary|winws/i.test(line) ? 'ok' : '');
        logTo($id('versionsLog'), line, cls);
      });
      if (report.log_path) {
        logTo($id('versionsLog'), `Лог сохранён: ${report.log_path}`, 'info');
      }

      if (Array.isArray(report.strategies) && report.strategies.length) {
        renderBlockcheckStrategies(report.strategies);
        showToast(`Найдено стратегий: ${report.strategies.length}`, 'success');
      } else {
        showToast('Blockcheck завершён. Рабочие стратегии не найдены.', 'info', { timeout: 7000 });
      }
      closeBlockcheckModal();
    } catch (e) {
      showToast('Blockcheck не выполнен: ' + e, 'error');
      logTo($id('versionsLog'), 'Blockcheck: ' + e, 'err');
      $id('blockcheckForm').hidden = false;
      $id('blockcheckProgress').hidden = true;
    } finally {
      blockcheckRunning = false;
      $id('blockcheckStart').disabled = false;
    }
  }

  $id('runZapretTestsBtn').onclick = async () => {
    if (!state.activeVersion) {
      return showToast('Сначала выберите активную версию во вкладке «Версии».', 'info');
    }
    const button = $id('runZapretTestsBtn');
    if (button.disabled) return;
    const proceed = await showConfirm(
      'Тесты Zapret идут до 30 минут: winws останавливается и запускается заново, ' +
      'IPSet на время тестов переключается в режим any. Продолжить?'
    );
    if (!proceed) return;
    const spinner = $id('zapretTestsSpinner');
    const label = $id('zapretTestsLabel');
    const log = $id('versionsLog');
    button.disabled = true;
    spinner.hidden = false;
    label.textContent = 'Тесты выполняются…';
    logTo(log, 'Запуск тестов Zapret…', 'info');
    try {
      const output = await invoke('run_zapret_tests', { versionPath: state.activeVersion.path });
      const lines = String(output || '').split(/\r?\n/).filter(Boolean);
      if (!lines.length) logTo(log, 'Тесты завершены без вывода.', 'info');
      lines.forEach(line => {
        const cls = /blocked|fail|error|ошибка/i.test(line)
          ? 'err'
          : (/\[ok\]|summary|winws|success/i.test(line) ? 'ok' : '');
        logTo(log, line, cls);
      });
      showToast('Тесты Zapret завершены', 'success');
    } catch (e) {
      showToast('Тесты Zapret не выполнены: ' + e, 'error');
      logTo(log, 'Тесты Zapret: ' + e, 'err');
    } finally {
      button.disabled = false;
      spinner.hidden = true;
      label.textContent = 'Запустить тесты Zapret';
    }
  };

  $id('blockcheckBtn').onclick = openBlockcheckModal;
  $id('blockcheckCancel').onclick = closeBlockcheckModal;
  $id('blockcheckClose').onclick = closeBlockcheckModal;
  $id('blockcheckStart').onclick = runBlockcheck;
  $id('blockcheckModal').addEventListener('click', (event) => {
    if (event.target.id === 'blockcheckModal' && $id('blockcheckProgress').hidden) {
      closeBlockcheckModal();
    }
  });

  document.querySelectorAll('.tab').forEach(tab => {
    tab.onclick = () => {
      document.querySelectorAll('.tab').forEach(t => {
        t.classList.remove('active');
        t.setAttribute('aria-selected', 'false');
      });
      document.querySelectorAll('.tab-content').forEach(c => c.classList.remove('active'));
      tab.classList.add('active');
      tab.setAttribute('aria-selected', 'true');
      $id('tab-' + tab.dataset.tab).classList.add('active');
      if (tab.dataset.tab === 'lists') loadListsTab();
      if (tab.dataset.tab === 'trash') loadTrashTab();
      if (tab.dataset.tab === 'network') loadNetworkTab();
      if (tab.dataset.tab === 'hosts') loadHostsTab();
      if (tab.dataset.tab === 'tools') loadToolsTab();
      if (tab.dataset.tab === 'diagnostics') loadDiagnosticsTab();
    };
  });

  window.addEventListener('tauri://drag-drop', async (event) => {
    const paths = event.payload.paths;
    if (!paths || paths.length === 0) return;
    const txtPath = paths.find(p => p.toLowerCase().endsWith('.txt'));
    if (!txtPath) return;
    if (!state.activeVersion || !state.currentListFile) return showToast('Выберите файл в левой панели, куда добавить список.', 'info');

    try {
      const fileContent = await invoke('read_external_file', { path: txtPath });
      const textarea = $id('fileContent');
      let currentLines = textarea.value.split(/\r?\n/).map(s => s.trim()).filter(Boolean);
      const seen = new Set(currentLines.map(s => s.toLowerCase()));
      let added = 0;
      fileContent.split(/\r?\n/).forEach(line => {
        const trimmed = line.trim();
        if (trimmed && !seen.has(trimmed.toLowerCase())) {
          seen.add(trimmed.toLowerCase());
          currentLines.push(trimmed);
          added++;
        }
      });
      currentLines.sort();
      textarea.value = currentLines.join('\n') + '\n';
      updateFileStats();
      updateLineNumbers();
      logTo($id('fileLog'), `Из файла добавлено: ${added}`, 'ok');
    } catch(e) { showToast('Ошибка чтения: ' + e, 'error'); }
  });

  async function loadTrashTab() {
    if (!state.rootPath) return;
    const list = $id('trashList');
    list.innerHTML = '<div class="empty-state">Загрузка</div>';

    try {
      const files = await invoke('get_trash_files', { rootPath: state.rootPath });
      state.trashFiles = files;

      if (files.length === 0) {
        list.innerHTML = '<div class="empty-state">Корзина пуста</div>';
        return;
      }

      list.innerHTML = files.map(f => {
        const date = new Date(f.time * 1000).toLocaleString('ru-RU');
        return `
        <div class="version-card">
          <div class="head">
            <span class="name">${escapeHtml(f.original_name)}</span>
            <span class="trash-meta">${f.deleted_lines.length} строк · ${date}</span>
            <div class="actions">
              <button class="btn btn-secondary btn-sm" data-trash-toggle="${escapeHtml(f.id)}" type="button">
                <svg class="icon"><use href="#i-eye"></use></svg>
                Показать
              </button>
              <button class="btn btn-secondary btn-sm" data-trash-restore="${escapeHtml(f.id)}" type="button">
                <svg class="icon"><use href="#i-rotate"></use></svg>
                Восстановить
              </button>
            </div>
          </div>
          <div id="lines-${f.id}" class="trash-lines"><pre>${f.deleted_lines.map(l => escapeHtml(l)).join('\n')}</pre></div>
        </div>`;
      }).join('');
    } catch(e) { list.innerHTML = `<div class="empty-state error-text">Ошибка: ${escapeHtml(e)}</div>`; }
  }

  window.toggleTrashLines = (id) => {
    const el = $id(`lines-${id}`);
    if (el) el.classList.toggle('show');
  };

  // Обработчики корзины — через data-атрибуты, без inline onclick:
  // CSP без 'unsafe-inline' не должен разрешать код в разметке.
  $id('trashList').addEventListener('click', (event) => {
    const toggle = event.target.closest('[data-trash-toggle]');
    if (toggle) {
      window.toggleTrashLines(toggle.dataset.trashToggle);
      return;
    }
    const restore = event.target.closest('[data-trash-restore]');
    if (restore) window.restoreTrash(restore.dataset.trashRestore);
  });

  function showConfirm(message) {
    return new Promise((resolve) => {
      const modal = $id('customConfirm');
      $id('confirmMessage').textContent = message;
      modal.classList.add('is-open');

      const btnYes = $id('confirmYes');
      const btnNo = $id('confirmNo');

      btnYes.onclick = null;
      btnNo.onclick = null;

      const close = (result) => {
        modal.classList.remove('is-open');
        resolve(result);
      };

      btnYes.onclick = () => close(true);
      btnNo.onclick = () => close(false);
      btnNo.focus();
    });
  }

  window.restoreTrash = async (trashId) => {
    if (!state.activeVersion) return showToast('Сначала выберите активную версию.', 'info');
    const file = state.trashFiles.find(x => x.id === trashId);
    if (!file) return;

    const isConfirmed = await showConfirm(`Восстановить ${file.deleted_lines.length} строк из "${file.original_name}"?`);
    if (!isConfirmed) return;

    try {
      const msg = await invoke('restore_trash_file', {
        rootPath: state.rootPath,
        versionPath: state.activeVersion.path,
        trashId: file.id,
        originalName: file.original_name,
        deletedLines: file.deleted_lines
      });

      logTo($id('trashLog'), msg, 'ok');
      loadTrashTab();

      if (state.currentListFile === file.original_name) {
        selectListFile(file.original_name);
      }
    } catch(e) { logTo($id('trashLog'), 'Ошибка восстановления: ' + e, 'err'); }
  };

  const emptyTrashBtn = $id('emptyTrashBtn');
  if (emptyTrashBtn) {
    emptyTrashBtn.onclick = async (e) => {
      e.preventDefault();
      e.stopPropagation();

      if (!state.rootPath) return;

      const isConfirmed = await showConfirm('Удалить все файлы из корзины? Это действие нельзя отменить.');
      if (!isConfirmed) return;

      try {
        await invoke('empty_trash', { rootPath: state.rootPath });
        loadTrashTab();
        logTo($id('trashLog'), 'Корзина очищена', 'warn');
      } catch(e) {
        showToast('Ошибка при очистке: ' + e, 'error');
      }
    };
  }
