//! Voice, video, audio settings, shortcuts, desktop integration and connection state.

pub const ENTRIES: &[(&str, &str)] = &[
    // Connection state (gateway.rs).
    ("Connecting to Discord…", "Подключение к Discord…"),
    (
        "Connecting to Discord… (attempt {})",
        "Подключение к Discord… (попытка {})",
    ),
    ("Connected; signing in…", "Соединение установлено, вход…"),
    (
        "Connected; resuming your session…",
        "Соединение установлено, восстановление сеанса…",
    ),
    (
        "Connected to Discord: {} servers, {} direct messages.",
        "Подключено к Discord: серверов — {}, личных сообщений — {}.",
    ),
    ("{} servers are unavailable.", "Недоступно серверов: {}."),
    (
        "Connection lost ({}). Reconnecting in {} (attempt {}).",
        "Соединение потеряно ({}). Повторное подключение через {} (попытка {}).",
    ),
    (
        "Discord no longer accepts this login. Log in again.",
        "Discord больше не принимает этот вход. Войдите снова.",
    ),
    ("Disconnected: {}", "Отключено: {}"),
    (
        "Discord asked to reconnect",
        "Discord попросил переподключиться",
    ),
    ("Discord stopped responding", "Discord перестал отвечать"),
    (
        "Discord did not greet the connection",
        "Discord не ответил на подключение",
    ),
    ("unreadable data", "нечитаемые данные"),
    ("session expired", "сеанс истёк"),
    ("closed by Discord", "закрыто Discord"),
    ("network error", "ошибка сети"),
    ("could not connect", "не удалось подключиться"),
    ("could not reach Discord", "не удалось связаться с Discord"),
    ("a moment", "несколько мгновений"),
    ("1 second", "1 секунду"),
    ("Someone", "Кто-то"),
    (
        "That is not a stream this client can watch.",
        "Эту трансляцию нельзя смотреть в этом клиенте.",
    ),
    (
        "You are already watching this stream.",
        "Вы уже смотрите эту трансляцию.",
    ),
    (
        "You are watching as many streams as fastcord allows at once. Stop watching one first.",
        "Вы смотрите максимум трансляций, который позволяет fastcord. Сначала закройте одну из них.",
    ),
    (
        "Could not queue Watch; retry.",
        "Не удалось начать просмотр. Повторите попытку.",
    ),
    ("Starting capture of “{}”…", "Запуск захвата «{}»…"),
    (
        "Could not start sharing; try again.",
        "Не удалось начать демонстрацию. Повторите попытку.",
    ),
    (
        "Could not refresh direct conversations: {}",
        "Не удалось обновить личные сообщения: {}",
    ),
    (
        "Could not open direct conversation: {}",
        "Не удалось открыть личную переписку: {}",
    ),
    (
        "The stream's RTC identity or endpoint is invalid.",
        "Недопустимый RTC-идентификатор или адрес трансляции.",
    ),
    // Camera (gateway/camera.rs).
    ("Camera off.", "Камера выключена."),
    (
        "Video encoding failed: {}.",
        "Ошибка кодирования видео: {}.",
    ),
    (
        "The call connection closed, so the camera was turned off.",
        "Соединение со звонком закрыто, поэтому камера выключена.",
    ),
    (
        "The background effect could not run ({}), so the camera was turned off instead of showing your room. Choose No effect to use the camera plainly.",
        "Не удалось применить эффект фона ({}), поэтому камера выключена, чтобы не показывать вашу комнату. Выберите «Без эффекта», чтобы использовать камеру как есть.",
    ),
    (
        "Windows blocked camera access. Allow desktop apps to use the camera in Settings, Privacy and security, Camera.",
        "Windows заблокировала доступ к камере. Разрешите классическим приложениям использовать камеру: Параметры, Конфиденциальность и защита, Камера.",
    ),
    (
        "The selected camera is not connected. Pick another camera or plug it back in.",
        "Выбранная камера не подключена. Выберите другую камеру или подключите её снова.",
    ),
    (
        "The camera was unplugged or taken by another application, so it was turned off.",
        "Камеру отключили или заняло другое приложение, поэтому она выключена.",
    ),
    ("The camera stopped: {}.", "Камера остановилась: {}."),
    ("Starting your camera…", "Включение камеры…"),
    ("Camera on{}", "Камера включена{}"),
    ("Camera on · {}{}", "Камера включена · {}{}"),
    (
        "mirrored; others see it unmirrored",
        "зеркально; другие видят без отражения",
    ),
    ("as others see it", "как видят другие"),
    ("You · preview ({}){}", "Вы · предпросмотр ({}){}"),
    (
        "The camera can only be used on Windows so far.",
        "Пока камера работает только в Windows.",
    ),
    (
        "Could not start the camera: {}.",
        "Не удалось включить камеру: {}.",
    ),
    (
        "waiting for the call to accept H.264 video from fastcord",
        "ожидание, пока звонок примет видео H.264 от fastcord",
    ),
    (" (not connected)", " (не подключена)"),
    ("First camera", "Первая камера"),
    ("Saved camera", "Сохранённая камера"),
    (
        "Camera listing stopped unexpectedly.",
        "Список камер неожиданно прервался.",
    ),
    // Screen share (gateway/stream_share.rs, share_audio.rs).
    (
        "Sharing audio is not available on this system yet.",
        "Передача звука пока недоступна в этой системе.",
    ),
    (
        "A window shares that application's sound; a screen shares all system sound except fastcord, so your call is not sent. Your microphone stays separate.",
        "Окно передаёт звук своего приложения; экран передаёт весь системный звук, кроме fastcord, так что ваш звонок не отправляется. Микрофон остаётся отдельным.",
    ),
    (
        "Broad capture: this Windows version can only capture everything your speakers play, including your call, so viewers may hear the people in your call. Turn this off to share the picture only.",
        "Общий захват: эта версия Windows может записывать только всё, что воспроизводят ваши динамики, включая звонок, поэтому зрители могут услышать участников звонка. Выключите, чтобы передавать только изображение.",
    ),
    (
        "Choose what viewers hear. An output shares everything it plays, including your call if fastcord plays there; to share one application only, route it to a separate output (for example with pavucontrol) and choose that one. Your microphone stays separate.",
        "Выберите, что услышат зрители. Устройство вывода передаёт всё, что на нём воспроизводится, в том числе звонок, если fastcord играет на нём; чтобы передать звук одного приложения, направьте его на отдельное устройство (например, через pavucontrol) и выберите его. Микрофон остаётся отдельным.",
    ),
    ("app audio", "звук приложения"),
    (
        "system audio without fastcord",
        "системный звук без fastcord",
    ),
    (
        "all system audio (viewers may hear your call)",
        "весь системный звук (зрители могут услышать ваш звонок)",
    ),
    (
        "the chosen output's audio (viewers may hear your call)",
        "звук выбранного устройства вывода (зрители могут услышать ваш звонок)",
    ),
    (
        "the chosen input's audio",
        "звук выбранного устройства ввода",
    ),
    (
        "Could not capture audio: {}. Turn off “Share audio” to share only the picture.",
        "Не удалось захватить звук: {}. Выключите «Передавать звук», чтобы передавать только изображение.",
    ),
    (
        "Could not start the audio encoder: {}.",
        "Не удалось запустить кодирование звука: {}.",
    ),
    ("Sharing stopped.", "Демонстрация остановлена."),
    (
        "The shared window or screen was closed, so sharing stopped.",
        "Окно или экран закрыт, поэтому демонстрация остановлена.",
    ),
    (
        "Windows refused to capture this source (a privacy setting, a policy, or a protected window). Choose another window or screen.",
        "Windows отказала в захвате этого источника (из-за настроек конфиденциальности, политики или защищённого окна). Выберите другое окно или экран.",
    ),
    (
        "No screen or window was chosen, so nothing was shared.",
        "Экран или окно не выбраны, поэтому ничего не передаётся.",
    ),
    (
        "Screen capture stopped: {}.",
        "Захват экрана остановлен: {}.",
    ),
    (
        "The stream connection closed.",
        "Соединение трансляции закрыто.",
    ),
    (
        "The picture effect failed: {}.",
        "Ошибка эффекта изображения: {}.",
    ),
    (
        "Discord's stream server is unavailable; sharing stopped.",
        "Сервер трансляций Discord недоступен; демонстрация остановлена.",
    ),
    ("Discord ended the stream.", "Discord завершил трансляцию."),
    (
        "Discord says the stream is full.",
        "Discord сообщает, что в трансляции нет свободных мест.",
    ),
    (
        "Discord refused the stream: you may not stream in this channel.",
        "Discord отклонил трансляцию: вам нельзя вести трансляции в этом канале.",
    ),
    (
        "Discord rate limited streaming in this server. Try again later.",
        "Discord ограничил частоту трансляций на этом сервере. Повторите попытку позже.",
    ),
    (
        "Discord rejected the stream request.",
        "Discord отклонил запрос трансляции.",
    ),
    ("Sharing “{}”", "Демонстрация «{}»"),
    (" · copy path: {}", " · путь с копированием: {}"),
    (
        " · lowered to {} fps, encoder busy",
        " · снижено до {} кадр/с, кодировщик занят",
    ),
    (
        " · {} fps · audio {} packets",
        " · {} кадр/с · пакетов звука: {}",
    ),
    (
        " · {} frames refused",
        " · {} кадр отклонён| · {} кадра отклонено| · {} кадров отклонено",
    ),
    (
        "Starting your stream of “{}”…",
        "Запуск вашей трансляции «{}»…",
    ),
    (
        "Could not start sharing: {}.",
        "Не удалось начать демонстрацию: {}.",
    ),
    ("The stream session ended.", "Сеанс трансляции завершён."),
    (
        "The stream connection failed: {}.",
        "Сбой соединения трансляции: {}.",
    ),
    (
        "Could not start the stream session: {}.",
        "Не удалось начать сеанс трансляции: {}.",
    ),
    (
        "Discord did not accept H.264 video from fastcord for this stream.",
        "Discord не принял видео H.264 от fastcord для этой трансляции.",
    ),
    (
        "The stream session stopped unexpectedly.",
        "Сеанс трансляции неожиданно прервался.",
    ),
    (
        "Still connecting to Discord; try again in a moment.",
        "Подключение к Discord ещё идёт; повторите попытку через несколько секунд.",
    ),
    (
        "This voice channel cannot be shared to.",
        "В этот голосовой канал нельзя вести демонстрацию.",
    ),
    // Watching a stream (gateway/stream_audio.rs, stream_speaker.rs, stream_video.rs, playback.rs).
    (
        "Could not start stream session: {}",
        "Не удалось начать сеанс трансляции: {}",
    ),
    ("Stream session failed: {}", "Сбой сеанса трансляции: {}"),
    (
        "No video is arriving. Retry watching; your call and screen share are still connected.",
        "Видео не поступает. Повторите просмотр; звонок и ваша демонстрация экрана остаются подключёнными.",
    ),
    (
        "Stream session stopped unexpectedly",
        "Сеанс трансляции неожиданно прервался",
    ),
    (
        "Stream audio output failed: {}",
        "Ошибка вывода звука трансляции: {}",
    ),
    (
        "Stream audio output did not open",
        "Не удалось открыть вывод звука трансляции",
    ),
    (
        "Stream audio output was lost; retrying in 5 seconds",
        "Вывод звука трансляции потерян; повтор через 5 секунд",
    ),
    (
        "Stream audio muted by deafen",
        "Звук трансляции отключён вместе со звуком звонка",
    ),
    ("Stream audio muted", "Звук трансляции выключен"),
    ("Receiving stream audio", "Получение звука трансляции"),
    (
        "Could not start video decoding worker: {}",
        "Не удалось запустить декодирование видео: {}",
    ),
    (
        "Video decoding worker stopped unexpectedly",
        "Декодирование видео неожиданно остановилось",
    ),
    (
        "Could not open {} decoder: {}",
        "Не удалось открыть декодер {}: {}",
    ),
    ("{} decode failed: {}", "Ошибка декодирования {}: {}"),
    ("Connecting to the stream…", "Подключение к трансляции…"),
    (
        "Waiting for a video keyframe…",
        "Ожидание ключевого кадра видео…",
    ),
    ("Watching · {}", "Просмотр · {}"),
    ("Video stopped: {}", "Видео остановлено: {}"),
    ("The stream ended.", "Трансляция завершена."),
    ("Waiting for stream audio", "Ожидание звука трансляции"),
    ("Video frame unavailable: {}", "Кадр видео недоступен: {}"),
    (
        " · Frame pool busy; newest pictures are being dropped.",
        " · Буфер кадров занят; новые кадры отбрасываются.",
    ),
    // Call layout.
    ("Grid", "Сетка"),
    ("Focus", "Фокус"),
    ("Speaker", "Докладчик"),
    (
        "{} more videos are not shown: at most {} are decoded and drawn at once. Pin one, or stop watching a stream, to choose.",
        "Не показано ещё видео: {}. Одновременно показывается не более {}. Закрепите одно из них или перестаньте смотреть трансляцию, чтобы выбрать.|Не показано ещё видео: {}. Одновременно показывается не более {}. Закрепите одно из них или перестаньте смотреть трансляцию, чтобы выбрать.|Не показано ещё видео: {}. Одновременно показывается не более {}. Закрепите одно из них или перестаньте смотреть трансляцию, чтобы выбрать.",
    ),
    // Soundboard.
    ("Soundboard of this server", "Звуковая панель сервера"),
    ("Received volume {}%", "Громкость чужих звуков: {}%"),
    ("Soundboard muted", "Звуковая панель выключена"),
    ("Mute soundboard", "Выключить звуковую панель"),
    ("Search sounds", "Поиск звуков"),
    ("Favorites only", "Только избранные"),
    ("All sounds", "Все звуки"),
    (" (unavailable)", " (недоступен)"),
    (
        "Discord did not let this account list the sounds here.",
        "Discord не разрешил этой учётной записи просматривать звуки здесь.",
    ),
    (
        "This server's sounds are no longer available.",
        "Звуки этого сервера больше недоступны.",
    ),
    (
        "The sounds could not be loaded. Try again.",
        "Не удалось загрузить звуки. Повторите попытку.",
    ),
    (
        "Discord did not let this account play that sound here (permission, or the call has ended).",
        "Discord не разрешил этой учётной записи воспроизвести этот звук (нет прав или звонок завершён).",
    ),
    (
        "That sound or voice channel no longer exists.",
        "Этого звука или голосового канала больше нет.",
    ),
    (
        "Discord is rate limiting sounds. Wait a moment, then click again.",
        "Discord ограничивает частоту звуков. Подождите немного и нажмите снова.",
    ),
    (
        "The sound may have played. It is not sent again automatically.",
        "Возможно, звук воспроизведён. Автоматически он не отправляется повторно.",
    ),
    (
        "That sound is too long to play here.",
        "Этот звук слишком длинный для воспроизведения здесь.",
    ),
    (
        "The audio format of that sound is not supported.",
        "Формат этого звука не поддерживается.",
    ),
    (
        "That sound could not be downloaded.",
        "Не удалось скачать этот звук.",
    ),
    (
        "That sound could not be read.",
        "Не удалось прочитать этот звук.",
    ),
    (
        "The output device could not play the sound.",
        "Устройство вывода не смогло воспроизвести звук.",
    ),
    (
        "Undeafen to hear sounds.",
        "Включите звук, чтобы слышать звуки.",
    ),
    ("Playing…", "Воспроизведение…"),
    (
        "You may preview sounds, but not play them in this channel.",
        "Звуки можно прослушать, но нельзя воспроизвести в этом канале.",
    ),
    ("This server has no sounds.", "На этом сервере нет звуков."),
    (
        "Preview is only for you. Play is heard by everyone in the call.",
        "Прослушивание слышите только вы. Воспроизведение слышат все в звонке.",
    ),
    // Stage.
    (
        "That is not allowed right now. The Stage or your role may have changed.",
        "Сейчас это запрещено. Возможно, трибуна или ваша роль изменились.",
    ),
    (
        "The Stage session no longer exists.",
        "Сеанса трибуны больше нет.",
    ),
    ("You are a speaker", "Вы докладчик"),
    (
        "Hand raised: waiting for a moderator",
        "Рука поднята: ожидание модератора",
    ),
    ("Become speaker", "Стать докладчиком"),
    ("You are in the audience", "Вы среди слушателей"),
    (
        "You cannot ask to speak in this Stage.",
        "На этой трибуне нельзя просить слово.",
    ),
    ("No live Stage session", "Нет активного сеанса трибуны"),
    ("Edit topic", "Изменить тему"),
    ("End Stage", "Завершить трибуну"),
    ("Start Stage", "Начать трибуну"),
    ("Start", "Начать"),
    ("{}/{} characters", "{}/{} симв."),
    ("RAISED HANDS ({})", "ПОДНЯТЫЕ РУКИ ({})"),
    ("Approve", "Одобрить"),
    ("+{} more", "и ещё {}"),
    // Voice input and shortcuts.
    ("Voice activity", "Активация по голосу"),
    ("Push to talk", "Режим рации"),
    ("Push to mute", "Выключение микрофона по клавише"),
    ("Toggle mute", "Включить или выключить микрофон"),
    ("Toggle deafen", "Включить или выключить звук"),
    (
        "Only while this window is focused",
        "Только когда это окно в фокусе",
    ),
    (
        "System-wide (works while another app is focused)",
        "В системе (работает, когда в фокусе другое приложение)",
    ),
    (
        "Push to talk is selected but has no key, so you will not transmit.",
        "Выбран режим рации, но клавиша не назначена, поэтому вас не будет слышно.",
    ),
    ("Input mode", "Режим ввода"),
    (
        "This channel does not allow voice activity, so push to talk is used here.",
        "Этот канал не допускает активацию по голосу, поэтому здесь используется режим рации.",
    ),
    (
        "Automatic sensitivity (transmit everything the noise suppression lets through)",
        "Автоматическая чувствительность (передавать всё, что пропускает шумоподавление)",
    ),
    (
        "Speech quieter than -{} dBFS is not sent",
        "Речь тише -{} dBFS не передаётся",
    ),
    (
        "Manual sensitivity is off.",
        "Ручная чувствительность выключена.",
    ),
    (
        "Keep transmitting {} ms after release",
        "Продолжать передачу {} мс после отпускания",
    ),
    ("Shortcuts", "Горячие клавиши"),
    (
        "Press a key or combination… (Esc cancels)",
        "Нажмите клавишу или сочетание… (Esc — отмена)",
    ),
    ("Not set", "Не назначено"),
    ("Set key", "Назначить"),
    (
        "Shortcuts are heard only while you are in a call, and no other key is read or stored. The pressed key is still delivered to the focused application, so prefer a function key, a mouse-free key such as Right Ctrl, or a combination.",
        "Горячие клавиши работают только во время звонка, другие нажатия не читаются и не сохраняются. Нажатая клавиша всё равно попадает в приложение в фокусе, поэтому лучше выбрать функциональную клавишу, редко используемую клавишу вроде правого Ctrl или сочетание.",
    ),
    (
        "In-window shortcuts work on every system. Pressing a key in another app is not seen, and leaving this window releases every held key.",
        "Клавиши в окне работают в любой системе. Нажатия в других приложениях не видны, а при выходе из окна все удерживаемые клавиши отпускаются.",
    ),
    (
        "System-wide shortcuts are inactive: {}",
        "Системные горячие клавиши не работают: {}",
    ),
    (
        "System-wide listener running for this call.",
        "Системное прослушивание клавиш работает в этом звонке.",
    ),
    (
        "The system-wide listener starts when you join a call.",
        "Системное прослушивание клавиш запустится при входе в звонок.",
    ),
    // Microphone panel (mic_panel.rs).
    ("No level yet.", "Уровня пока нет."),
    (
        "Level {} dBFS, peak {} dBFS{}",
        "Уровень {} dBFS, пик {} dBFS{}",
    ),
    (
        " (your microphone in this call)",
        " (ваш микрофон в этом звонке)",
    ),
    (
        "Echo cancellation: found an echo path of about {} ms, microphone reduced by {} dB while others speak ({}).",
        "Эхоподавление: найден путь эха около {} мс, микрофон ослабляется на {} дБ, пока говорят другие ({}).",
    ),
    ("adapted", "настроено"),
    ("still adapting", "ещё настраивается"),
    (
        "Echo cancellation: running, no echo path found yet (it needs a few seconds of others speaking through your speakers).",
        "Эхоподавление: работает, путь эха пока не найден (нужно несколько секунд, пока другие говорят через ваши динамики).",
    ),
    (
        "Echo cancellation: off or waiting for the first audio.",
        "Эхоподавление: выключено или ждёт первый звук.",
    ),
    (
        "Echo cancellation statistics appear during a call.",
        "Статистика эхоподавления появится во время звонка.",
    ),
    ("Playing back what you recorded…", "Воспроизведение записи…"),
    ("Recording: speak now.", "Запись: говорите."),
    (
        "Listening. The microphone stays open until you stop or close this window.",
        "Прослушивание. Микрофон остаётся включённым, пока вы не остановите проверку или не закроете окно.",
    ),
    (
        "Leave the call to run the checks; the meter shows your call microphone.",
        "Выйдите из звонка, чтобы запустить проверку; индикатор показывает микрофон в звонке.",
    ),
    ("Microphone", "Микрофон"),
    ("Input gain: {} dB", "Усиление входа: {} дБ"),
    ("Automatic gain", "Автоматическое усиление"),
    (
        "Gain is applied after noise suppression and before the voice-activity threshold, with a limiter so loud speech does not clip. Automatic gain steers speech toward a steady level and leaves silence alone.",
        "Усиление применяется после шумоподавления и до порога активации по голосу; ограничитель не даёт громкой речи искажаться. Автоматическое усиление выравнивает громкость речи и не трогает тишину.",
    ),
    (
        "{} frames reached full scale or the limiter ceiling. Lower the gain or move the microphone back.",
        "Кадров с максимальным уровнем или у предела ограничителя: {}. Уменьшите усиление или отодвиньте микрофон.|Кадров с максимальным уровнем или у предела ограничителя: {}. Уменьшите усиление или отодвиньте микрофон.|Кадров с максимальным уровнем или у предела ограничителя: {}. Уменьшите усиление или отодвиньте микрофон.",
    ),
    (
        "Voice-activity threshold -{} dB: your voice is currently {} it.",
        "Порог активации по голосу -{} дБ: ваш голос сейчас {} порога.",
    ),
    ("above", "выше"),
    ("below", "ниже"),
    (
        "Sending your voice at {} kbps (the channel's bitrate).",
        "Голос передаётся со скоростью {} кбит/с (битрейт канала).",
    ),
    ("Check level", "Проверить уровень"),
    ("Record and play back", "Записать и воспроизвести"),
    (
        "These checks never reach a call: a recording stays in memory (5 seconds, about 20 KB) and is played only to your selected output.",
        "Эти проверки не попадают в звонок: запись хранится в памяти (5 секунд, около 20 КБ) и воспроизводится только на выбранное устройство вывода.",
    ),
    // Desktop integration and blocking bypass (desktop.rs).
    (
        "Automatic: try each strategy until Discord answers",
        "Автоматически: пробовать стратегии, пока Discord не ответит",
    ),
    ("Split + TLS records", "Разделение + записи TLS"),
    ("Disorder", "Перестановка пакетов"),
    ("Out-of-band byte", "Внеполосный байт"),
    ("Split at the server name", "Разделение по имени сервера"),
    ("Fake packet", "Поддельный пакет"),
    (
        "Only my options (Advanced)",
        "Только мои параметры (дополнительно)",
    ),
    (
        "Restart fastcord to apply.",
        "Перезапустите fastcord, чтобы применить.",
    ),
    ("Desktop", "Рабочий стол"),
    (
        "Closing the window minimizes fastcord to the tray instead of quitting",
        "Закрытие окна сворачивает fastcord в трей, а не завершает работу",
    ),
    (
        "Quit in the tray menu always exits fastcord and leaves any call.",
        "«Выход» в меню трея всегда закрывает fastcord и завершает звонок.",
    ),
    (
        "Tray icon unavailable: {} Closing the window quits.",
        "Значок в трее недоступен: {} Закрытие окна завершает работу.",
    ),
    (
        "Start fastcord when I sign in",
        "Запускать fastcord при входе в систему",
    ),
    (
        "Compact call window (small; Wayland does not let it stay on top)",
        "Компактное окно звонка (маленькое; Wayland не позволяет держать его поверх окон)",
    ),
    (
        "Compact call window (small, always on top)",
        "Компактное окно звонка (маленькое, поверх всех окон)",
    ),
    (
        "Microphone muted. Click to unmute",
        "Микрофон выключен. Нажмите, чтобы включить",
    ),
    (
        "Microphone on. Click to mute",
        "Микрофон включён. Нажмите, чтобы выключить",
    ),
    (
        "Deafened. Click to hear again",
        "Звук выключен. Нажмите, чтобы включить",
    ),
    (
        "Hearing. Click to deafen",
        "Звук включён. Нажмите, чтобы выключить",
    ),
    ("Back to the full window", "Вернуться в полное окно"),
    ("In call", "В звонке"),
];
