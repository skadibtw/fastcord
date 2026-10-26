//! The network page, its notices and the network layer's errors.

pub const ENTRIES: &[(&str, &str)] = &[
    ("Network", "Сеть"),
    ("Network settings", "Настройки сети"),
    ("API", "API"),
    ("Gateway", "Шлюз"),
    ("QR login", "Вход по QR-коду"),
    ("Login", "Вход"),
    ("CDN", "CDN"),
    ("Uploads", "Загрузки"),
    ("Streams", "Трансляции"),
    ("Stream", "Трансляция"),
    ("Voice calls", "Звонки"),
    ("AutoMod", "AutoMod"),
    ("Services", "Службы"),
    ("Connection recovery", "Восстановление соединения"),
    ("Automatic", "Автоматически"),
    (
        "Automatic retries a blocked secure connection by another route. Certificates are always checked.",
        "В автоматическом режиме заблокированное защищённое соединение повторяется другим путём. Сертификаты проверяются всегда.",
    ),
    ("Start packet helper", "Запустить пакетный помощник"),
    ("Stop packet helper", "Остановить пакетный помощник"),
    ("Check connection", "Проверить соединение"),
    (
        "The packet helper is on for this session.",
        "Пакетный помощник включён на этот сеанс.",
    ),
    (
        "The packet helper asks for administrator rights once per session and selects Automatic.",
        "Пакетный помощник один раз за сеанс запрашивает права администратора и включает автоматический режим.",
    ),
    ("Proxy", "Прокси"),
    ("Host", "Адрес"),
    ("Port", "Порт"),
    ("Username (optional)", "Имя пользователя (необязательно)"),
    ("Password", "Пароль"),
    ("Save and use proxy", "Сохранить и использовать прокси"),
    (
        "A proxy is never bypassed. Voice and video need SOCKS5 with UDP. The password is kept in the system's credential store.",
        "Прокси никогда не обходится. Для голоса и видео нужен SOCKS5 с UDP. Пароль хранится в системном хранилище учётных данных.",
    ),
    (
        "Experimental UDP discovery (unverified)",
        "Экспериментальное обнаружение UDP (не проверено)",
    ),
    (
        "Changes apply to the next connection.",
        "Изменения применятся к следующему подключению.",
    ),
    (
        "Sign out to change routing. Connected sessions are not interrupted.",
        "Чтобы изменить маршрут, выйдите из аккаунта. Подключённые сеансы не прерываются.",
    ),
    ("Hide details", "Скрыть подробности"),
    (
        "Retry pending TLS check",
        "Повторить отложенную проверку TLS",
    ),
    (
        "Active path: direct, no recovery",
        "Путь: напрямую, без восстановления",
    ),
    (
        "Active path: direct, TLS recovery and the packet helper",
        "Путь: напрямую, восстановление TLS и пакетный помощник",
    ),
    (
        "Active path: direct with TLS recovery; no packet helper",
        "Путь: напрямую с восстановлением TLS, без пакетного помощника",
    ),
    (
        "Active path: proxy ({}), never direct",
        "Путь: прокси ({}), без прямого подключения",
    ),
    ("checking, {}/{} probes", "проверка, проб: {}/{}"),
    ("idle", "простаивает"),
    (
        " Budget spent: new checks wait up to 10 minutes.",
        " Лимит исчерпан: новые проверки подождут до 10 минут.",
    ),
    (
        "Recovery: {}; {}/{} dials used in the last 10 minutes.{}",
        "Восстановление: {}; подключений за 10 минут: {}/{}.{}",
    ),
    ("Not checked", "Не проверено"),
    ("Checking TLS routes…", "Проверка маршрутов TLS…"),
    (
        "TLS reachable; service not confirmed",
        "TLS доступен, служба не подтверждена",
    ),
    ("Service answered", "Служба ответила"),
    ("Session connected", "Сеанс подключён"),
    (
        "UDP media did not answer; TLS is separate",
        "Медиа по UDP не отвечает; TLS работает отдельно",
    ),
    (
        "Experimental UDP discovery: variant {}/3",
        "Экспериментальное обнаружение UDP: вариант {}/3",
    ),
    (
        "Auto checks only a required service after an eligible connection failure. TLS reachability does not establish that Discord or media works. Recovery is bounded; it never bypasses certificate verification.",
        "Автоматический режим проверяет только нужную службу и только после подходящего сбоя подключения. Доступность TLS ещё не означает, что работают Discord или медиа. Восстановление ограничено и никогда не отключает проверку сертификатов.",
    ),
    (
        "TLS splitting is best effort. TCP writes do not guarantee packet boundaries. UDP media has a separate connection path.",
        "Разбиение TLS работает по возможности: запись в TCP не гарантирует границ пакетов. У медиа по UDP свой путь подключения.",
    ),
    (
        "Optional Windows helper requires UAC once per session. It handles only the public TLS handshake for fastcord connections. The client and account stay unprivileged. Starting it selects Auto.",
        "Необязательный помощник Windows один раз за сеанс запрашивает UAC. Он обрабатывает только открытое TLS-рукопожатие подключений fastcord; клиент и аккаунт остаются без повышенных прав. Запуск включает автоматический режим.",
    ),
    (
        "Proxy mode never falls back to a direct connection. CONNECT proxies carry TCP only: voice, camera and screen sharing cannot join through them. SOCKS5 UDP requires a UDP-capable proxy and a working UDP route to that proxy. It does not carry media over TCP and does not encrypt the client-to-proxy transport. Passwords are saved in native credential storage.",
        "В режиме прокси прямое подключение не используется никогда. Прокси CONNECT передают только TCP: голос, камера и демонстрация экрана через них не подключатся. Для SOCKS5 UDP нужен прокси с поддержкой UDP и рабочий UDP-маршрут до него; медиа не передаётся поверх TCP, а канал до прокси не шифруется. Пароли хранятся в системном хранилище учётных данных.",
    ),
    (
        "Experimental preludes are disabled in Proxy mode. SOCKS5 UDP uses ordinary discovery.",
        "В режиме прокси экспериментальные прелюдии выключены: SOCKS5 UDP использует обычное обнаружение.",
    ),
    (
        "Separate opt-in for direct media routes. Tries up to three UDP flows before protocol selection. Effectiveness on filtered networks is unverified; proxy routing does not enable it.",
        "Отдельная настройка для прямых медиамаршрутов: до выбора протокола пробуются до трёх UDP-потоков. Действенность в фильтруемых сетях не проверена; через прокси не включается.",
    ),
    // Notices.
    (
        "Packet recovery could not start. UAC may have been cancelled, or the pinned WinDivert driver is unavailable.",
        "Не удалось запустить пакетное восстановление: запрос UAC отменён или закреплённый драйвер WinDivert недоступен.",
    ),
    (
        "Packet recovery is currently available on Windows only.",
        "Пакетное восстановление пока доступно только в Windows.",
    ),
    (
        "Packet recovery worker stopped.",
        "Обработчик пакетного восстановления остановлен.",
    ),
    (
        "Anonymous check: HTTP client could not start.",
        "Анонимная проверка: не удалось запустить HTTP-клиент.",
    ),
    (
        "Discord API HTTP 200: packet overlap 681 restored access. You can try signing in.",
        "API Discord ответил HTTP 200: доступ восстановлен перекрытием пакетов 681. Можно попробовать войти.",
    ),
    (
        "Discord API HTTP 200: packet fake + disorder restored access. You can try signing in.",
        "API Discord ответил HTTP 200: доступ восстановлен ложным пакетом и перестановкой. Можно попробовать войти.",
    ),
    (
        "Anonymous Discord API check passed (HTTP 200). You can try signing in.",
        "Анонимная проверка API Discord пройдена (HTTP 200). Можно попробовать войти.",
    ),
    (
        "Discord returned an HTTP response; the anonymous API check did not return 200.",
        "Discord ответил по HTTP, но анонимная проверка API не вернула 200.",
    ),
    (
        "Anonymous Discord API check failed. Packet recovery has not restored access.",
        "Анонимная проверка API Discord не прошла: пакетное восстановление не вернуло доступ.",
    ),
    (
        "Network settings could not be read. Routing is stopped until you choose a mode.",
        "Не удалось прочитать настройки сети. Подключения остановлены, пока вы не выберете режим.",
    ),
    (
        "The proxy password could not be loaded. Open Network recovery to save it again.",
        "Не удалось загрузить пароль прокси. Откройте настройки сети и сохраните его снова.",
    ),
    (
        "Network settings could not be read; routing is stopped.",
        "Не удалось прочитать настройки сети; подключения остановлены.",
    ),
    (
        "Enter a proxy port from 1 to 65535.",
        "Введите порт прокси от 1 до 65535.",
    ),
    (
        "The proxy host, port or username is invalid.",
        "Неверный адрес, порт или имя пользователя прокси.",
    ),
    ("Enter a proxy first.", "Сначала укажите прокси."),
    (
        "Proxy passwords are unavailable in fake mode.",
        "В тестовом режиме пароли прокси недоступны.",
    ),
    (
        "Native credential storage is unavailable; the proxy was not applied.",
        "Системное хранилище учётных данных недоступно; прокси не применён.",
    ),
    (
        "The proxy password could not be saved.",
        "Не удалось сохранить пароль прокси.",
    ),
    ("Network settings are invalid.", "Настройки сети неверны."),
    (
        "The settings folder is unavailable.",
        "Папка настроек недоступна.",
    ),
    (
        "Network settings could not be saved.",
        "Не удалось сохранить настройки сети.",
    ),
    (
        "This proxy configuration carries TCP only. Voice, camera and screen sharing require a UDP-capable relay; no direct media connection was opened.",
        "Этот прокси передаёт только TCP. Для голоса, камеры и демонстрации экрана нужен ретранслятор с UDP; прямое медиаподключение не открывалось.",
    ),
    (
        "Waiting for Windows administrator approval…",
        "Ожидание подтверждения администратора Windows…",
    ),
    (
        "Packet helper started. Checking the public Discord API…",
        "Пакетный помощник запущен. Проверка открытого API Discord…",
    ),
    (
        "Packet recovery stopped.",
        "Пакетное восстановление остановлено.",
    ),
    (
        "Checking the public Discord API…",
        "Проверка открытого API Discord…",
    ),
    (
        "Sign out before starting packet recovery.",
        "Перед запуском пакетного восстановления выйдите из аккаунта.",
    ),
    (
        "The proxy credentials are invalid.",
        "Неверные учётные данные прокси.",
    ),
    (
        "Proxy applied. Routing failures will be shown; there is no direct fallback.",
        "Прокси применён. Сбои маршрута будут показаны; прямого запасного пути нет.",
    ),
    (
        "Pending check finished. Ordinary reconnect limits still apply.",
        "Отложенная проверка завершена. Обычные ограничения переподключения действуют.",
    ),
    // The network layer's errors.
    (
        "The network destination was refused.",
        "Адрес назначения отклонён.",
    ),
    (
        "The service name could not be resolved.",
        "Не удалось разрешить имя службы.",
    ),
    (
        "There is no route to the service.",
        "Нет маршрута до службы.",
    ),
    (
        "The TCP connection timed out.",
        "Истекло время ожидания TCP-подключения.",
    ),
    ("The TCP connection was reset.", "TCP-подключение сброшено."),
    (
        "The TLS handshake timed out.",
        "Истекло время TLS-рукопожатия.",
    ),
    (
        "The TLS connection was reset during its handshake.",
        "TLS-подключение сброшено во время рукопожатия.",
    ),
    (
        "The service certificate was rejected.",
        "Сертификат службы отклонён.",
    ),
    (
        "The TLS handshake was rejected.",
        "TLS-рукопожатие отклонено.",
    ),
    (
        "The WebSocket upgrade failed.",
        "Не удалось перейти на WebSocket.",
    ),
    (
        "The configured proxy refused the connection.",
        "Настроенный прокси отклонил подключение.",
    ),
    (
        "The proxy configuration is invalid or incomplete.",
        "Настройки прокси неверны или неполны.",
    ),
    (
        "The proxy name could not be resolved.",
        "Не удалось разрешить имя прокси.",
    ),
    (
        "The connection to the proxy timed out.",
        "Истекло время подключения к прокси.",
    ),
    (
        "The HTTPS proxy certificate was rejected.",
        "Сертификат HTTPS-прокси отклонён.",
    ),
    (
        "The proxy rejected its login details.",
        "Прокси отклонил данные входа.",
    ),
    (
        "The proxy returned an invalid response.",
        "Прокси вернул неверный ответ.",
    ),
    (
        "The SOCKS5 UDP relay control connection closed.",
        "Управляющее подключение ретранслятора SOCKS5 UDP закрыто.",
    ),
    (
        "The configured SOCKS5 proxy could not establish a UDP relay.",
        "Настроенный прокси SOCKS5 не смог открыть UDP-ретранслятор.",
    ),
    (
        "The network recovery budget is exhausted.",
        "Лимит восстановления сети исчерпан.",
    ),
    (
        "This network configuration is unsupported.",
        "Такая сетевая конфигурация не поддерживается.",
    ),
    (
        "The private network bridge is unavailable.",
        "Частный сетевой мост недоступен.",
    ),
    (
        "Network recovery was cancelled.",
        "Восстановление сети отменено.",
    ),
    // Settings screens, profile, menus.
    ("Overview", "Обзор"),
    ("MODERATION", "МОДЕРАЦИЯ"),
    ("Type: {}", "Тип: {}"),
    ("Display name", "Отображаемое имя"),
    (
        "Empty shows your username",
        "Если пусто, показывается имя пользователя",
    ),
    ("Connections", "Подключения"),
    (
        "Shown on your profile when Discord allows it.",
        "Показываются в профиле, если Discord это разрешает.",
    ),
    ("Remove friend", "Удалить из друзей"),
    ("Move up", "Переместить вверх"),
    ("Move down", "Переместить вниз"),
    ("Remove from group", "Удалить из группы"),
    (
        "Animated and external emoji may need Nitro.",
        "Для анимированных и внешних эмодзи может понадобиться Nitro.",
    ),
    (
        "Twemoji 17.0.3 · Unicode 16.0",
        "Twemoji 17.0.3 · Unicode 16.0",
    ),
    ("{} ({}/{})", "{} ({}/{})"),
    (
        "Discord did not answer. Open the link again to retry.",
        "Discord не ответил. Откройте ссылку ещё раз, чтобы повторить.",
    ),
];
