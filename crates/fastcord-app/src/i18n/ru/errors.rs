//! Error text from the lower crates, shown as `tr(&error.to_string())`.

pub const ENTRIES: &[(&str, &str)] = &[
    (
        "A rule name has 1 to 100 characters.",
        "Название правила — от 1 до 100 символов.",
    ),
    (
        "This kind of rule is not created here.",
        "Правила этого вида здесь не создаются.",
    ),
    ("At most 1000 keywords.", "Не больше 1000 ключевых слов."),
    (
        "A keyword has at most 60 characters.",
        "Ключевое слово — не больше 60 символов.",
    ),
    (
        "At most 10 regex patterns.",
        "Не больше 10 регулярных выражений.",
    ),
    (
        "A pattern has at most 260 characters.",
        "Шаблон — не больше 260 символов.",
    ),
    (
        "At most 100 allowed phrases (1000 for word lists).",
        "Не больше 100 разрешённых фраз (1000 для списков слов).",
    ),
    (
        "Add at least one keyword or pattern.",
        "Добавьте хотя бы одно ключевое слово или шаблон.",
    ),
    (
        "Choose at least one word list.",
        "Выберите хотя бы один список слов.",
    ),
    (
        "The mention limit is 1 to 50.",
        "Лимит упоминаний — от 1 до 50.",
    ),
    (
        "Choose at least one action.",
        "Выберите хотя бы одно действие.",
    ),
    (
        "That action is not available for this kind of rule.",
        "Это действие недоступно для правил такого вида.",
    ),
    (
        "A timeout is 1 second to 4 weeks.",
        "Тайм-аут — от 1 секунды до 4 недель.",
    ),
    (
        "The message shown to the author has at most 150 characters.",
        "Сообщение для автора — не больше 150 символов.",
    ),
    ("At most 20 exempt roles.", "Не больше 20 ролей-исключений."),
    (
        "At most 50 exempt channels.",
        "Не больше 50 каналов-исключений.",
    ),
    (
        "Discord sent more entries than this client keeps, so saving would delete the rest. Edit this in the official client.",
        "Discord прислал больше записей, чем хранит fastcord, поэтому при сохранении остальные пропали бы. Измените это в официальном клиенте.",
    ),
    (
        "A server has at most 16 rules.",
        "На сервере не больше 16 правил.",
    ),
    (
        "A rule has at most 300 characters.",
        "Правило — не больше 300 символов.",
    ),
    ("That description is too long.", "Описание слишком длинное."),
    ("At most 15 questions.", "Не больше 15 вопросов."),
    (
        "At most 50 answers per question.",
        "Не больше 50 ответов на вопрос.",
    ),
    (
        "Every question, answer and entry needs a title.",
        "У каждого вопроса, ответа и пункта должно быть название.",
    ),
    (
        "A title or description is too long.",
        "Название или описание слишком длинное.",
    ),
    (
        "Every question needs at least one answer.",
        "У каждого вопроса должен быть хотя бы один ответ.",
    ),
    (
        "Use a Unicode emoji (custom emoji are kept, not added).",
        "Используйте эмодзи Unicode (пользовательские эмодзи сохраняются, но не добавляются).",
    ),
    (
        "A chosen role no longer exists.",
        "Выбранной роли больше нет.",
    ),
    (
        "A chosen role is one you cannot give: it is above your highest role, managed, or @everyone.",
        "Выбранную роль нельзя выдавать: она выше вашей высшей роли, управляется интеграцией или это @everyone.",
    ),
    (
        "A chosen channel is not one you can see.",
        "Выбранный канал вам не виден.",
    ),
    (
        "Default channels must be text channels @everyone can view.",
        "Каналы по умолчанию должны быть текстовыми и видимыми для @everyone.",
    ),
    (
        "At most 5 new-member actions.",
        "Не больше 5 действий для новых участников.",
    ),
    (
        "At most 7 resource channels.",
        "Не больше 7 каналов-ресурсов.",
    ),
    (
        "The welcome message has at most 300 characters.",
        "Приветствие — не больше 300 символов.",
    ),
    (
        "Community needs a rules channel and a public updates channel.",
        "Для сообщества нужны канал с правилами и канал публичных обновлений.",
    ),
    (
        "The welcome message needs an author.",
        "У приветствия должен быть автор.",
    ),
    (
        "Not yet verified on a live account. Enable it under Settings → Advanced → Test unverified actions (alt account only).",
        "Ещё не проверено на живой учётной записи. Включите в разделе «Настройки → Дополнительно → Проверка непроверенных действий» (только на дополнительном аккаунте).",
    ),
    (
        "Discord requires account verification; open the official client before reconnecting",
        "Discord требует подтвердить учётную запись: откройте официальный клиент, прежде чем переподключаться",
    ),
    (
        "Notification changes are paused pending validation; use the official Discord client",
        "Изменение уведомлений приостановлено до проверки: используйте официальный клиент Discord",
    ),
    (
        "Discord kept rate limiting this request",
        "Discord продолжал ограничивать частоту этого запроса",
    ),
    (
        "Discord says direct messages are being opened too fast; opening DMs is paused for a while",
        "Discord сообщает, что личные сообщения открываются слишком часто; открытие ЛС приостановлено на время",
    ),
    (
        "Discord is rate limiting changes; writes are paused for a minute",
        "Discord ограничивает частоту изменений; запись приостановлена на минуту",
    ),
    (
        "authentication is required",
        "требуется вход в учётную запись",
    ),
    (
        "permission denied for this resource",
        "недостаточно прав для этого ресурса",
    ),
    (
        "the requested resource is gone",
        "запрошенного ресурса больше нет",
    ),
    (
        "token cannot be used as an authorization header",
        "токен нельзя использовать как заголовок авторизации",
    ),
    (
        "expected an absolute Discord API resource path",
        "ожидался абсолютный путь ресурса Discord API",
    ),
    (
        "invalid REST JSON payload",
        "некорректные данные JSON для REST-запроса",
    ),
    (
        "429 response has no usable retry deadline",
        "в ответе 429 нет пригодного срока повтора",
    ),
    (
        "could not configure the REST client",
        "не удалось настроить REST-клиент",
    ),
    (
        "REST request exceeds the body byte budget",
        "REST-запрос превышает допустимый размер тела",
    ),
    (
        "REST response exceeds the body byte budget",
        "REST-ответ превышает допустимый размер тела",
    ),
    (
        "REST scheduler byte budget exhausted",
        "лимит памяти планировщика REST исчерпан",
    ),
    (
        "not connected to Discord; reconnect and try again",
        "нет подключения к Discord; переподключитесь и попробуйте снова",
    ),
    (
        "a required option is empty",
        "обязательный параметр не заполнен",
    ),
    (
        "an option needs a whole number",
        "параметру нужно целое число",
    ),
    ("an option needs a number", "параметру нужно число"),
    (
        "an option needs yes or no",
        "параметру нужно «да» или «нет»",
    ),
    (
        "an option needs an ID or a mention",
        "параметру нужен ID или упоминание",
    ),
    (
        "A required field is empty.",
        "Обязательное поле не заполнено.",
    ),
    (
        "A field is shorter than the app allows.",
        "Поле короче, чем разрешает приложение.",
    ),
    (
        "A field is longer than the app allows.",
        "Поле длиннее, чем разрешает приложение.",
    ),
    ("Server mute", "Заглушить на сервере"),
    ("Server unmute", "Включить звук на сервере"),
    ("Server deafen", "Выключить звук на сервере"),
    ("Server undeafen", "Вернуть звук на сервере"),
    ("Disconnect from voice", "Отключить от голосового канала"),
    ("Kick from server", "Выгнать с сервера"),
    ("Ban from server", "Забанить на сервере"),
    ("Time out", "Выдать тайм-аут"),
    ("Remove timeout", "Снять тайм-аут"),
    ("Reset nickname", "Сбросить никнейм"),
    ("Move to voice channel", "Переместить в голосовой канал"),
    ("An unknown channel", "Неизвестный канал"),
    (
        "Enter an email or phone and a password.",
        "Введите почту или телефон и пароль.",
    ),
    (
        "The client identity could not be built.",
        "Не удалось создать идентификатор клиента.",
    ),
    (
        "Could not reach Discord. Login stopped; try again.",
        "Не удалось связаться с Discord. Вход прерван, повторите попытку.",
    ),
    (
        "Discord is rate limiting logins. Wait and try again.",
        "Discord ограничивает частоту входов. Подождите и повторите.",
    ),
    (
        "Discord rejected those credentials or that code.",
        "Discord отклонил эти данные или код.",
    ),
    (
        "Discord sent an unexpected login response.",
        "Discord прислал неожиданный ответ при входе.",
    ),
    (
        "; it may require verification in an official client.",
        "; возможно, нужно подтверждение в официальном клиенте.",
    ),
    ("a poll needs a question", "в опросе должен быть вопрос"),
    ("the question is too long", "вопрос слишком длинный"),
    (
        "a poll needs at least two answers",
        "в опросе должно быть хотя бы два ответа",
    ),
    (
        "a poll has at most ten answers",
        "в опросе не больше десяти ответов",
    ),
    ("an answer is too long", "ответ слишком длинный"),
    (
        "the poll's duration is not allowed",
        "такая длительность опроса недопустима",
    ),
    (
        "an emoji needs a name or an ID to react with",
        "чтобы поставить реакцию, нужно имя или ID эмодзи",
    ),
    (
        "this emoji cannot be used as a reaction",
        "это эмодзи нельзя использовать как реакцию",
    ),
    ("a message needs some text", "в сообщении должен быть текст"),
    (
        "the message is too long for Discord",
        "сообщение слишком длинное для Discord",
    ),
    (
        "a voice message cannot carry text or other files",
        "голосовое сообщение не может содержать текст или другие файлы",
    ),
    (
        "Give the post a title of 1 to 100 characters.",
        "Дайте публикации название от 1 до 100 символов.",
    ),
    (
        "Write the first message of the post or attach a file.",
        "Напишите первое сообщение публикации или прикрепите файл.",
    ),
    (
        "The first message is too long.",
        "Первое сообщение слишком длинное.",
    ),
    (
        "A post carries at most 5 tags.",
        "У публикации не больше 5 тегов.",
    ),
    (
        "This forum requires at least one tag.",
        "Этот форум требует хотя бы один тег.",
    ),
    (
        "This channel requires a file in every post.",
        "Этот канал требует файл в каждой публикации.",
    ),
    (
        "A file is missing or cannot be read.",
        "Файл отсутствует или его не удаётся прочитать.",
    ),
    (
        "Only files can be attached, not folders.",
        "Можно прикреплять только файлы, не папки.",
    ),
    (
        "A file is empty; Discord does not accept empty files.",
        "Файл пуст; Discord не принимает пустые файлы.",
    ),
    (
        "A file is larger than fastcord attaches.",
        "Файл больше, чем fastcord может прикрепить.",
    ),
    (
        "Discord allows at most 10 files per message.",
        "Discord разрешает не больше 10 файлов в сообщении.",
    ),
    (
        "A file changed after you chose it. Attach it again.",
        "Файл изменился после выбора. Прикрепите его заново.",
    ),
    (
        "A file name is empty or too long.",
        "Имя файла пустое или слишком длинное.",
    ),
    (
        "Discord did not prepare the upload as expected.",
        "Discord подготовил загрузку не так, как ожидалось.",
    ),
    (
        "Discord named an upload address fastcord will not use.",
        "Discord указал адрес загрузки, которым fastcord не будет пользоваться.",
    ),
    (
        "The upload was refused (the upload address expired or is not allowed).",
        "Загрузка отклонена (адрес загрузки устарел или не разрешён).",
    ),
    (
        "The file is larger than your account or this server allows.",
        "Файл больше, чем разрешено вашей учётной записью или этим сервером.",
    ),
    (
        "The upload service refused the file.",
        "Служба загрузки отклонила файл.",
    ),
    (
        "You cannot attach files in this channel.",
        "В этом канале нельзя прикреплять файлы.",
    ),
    (
        "The files are larger than your account or this server allows.",
        "Файлы больше, чем разрешено вашей учётной записью или этим сервером.",
    ),
    (
        "Discord refused the upload (the files may be too large for your account or this server).",
        "Discord отклонил загрузку (возможно, файлы слишком велики для вашей учётной записи или сервера).",
    ),
    (
        "The upload was interrupted. Check the connection and retry.",
        "Загрузка прервана. Проверьте подключение и повторите.",
    ),
    (
        "Discord sent an event larger than fastcord's 16 MiB safety limit, so the account state could not be loaded safely.",
        "Discord прислал событие больше предела безопасности fastcord в 16 МиБ, поэтому состояние учётной записи не удалось загрузить безопасно.",
    ),
    (
        "Discord's startup data (READY) could not be decoded. Discord may have changed its protocol; update fastcord.",
        "Не удалось разобрать начальные данные Discord (READY). Возможно, Discord изменил протокол; обновите fastcord.",
    ),
    (
        "This account has too many open Discord sessions. Close one and log in again.",
        "У этой учётной записи слишком много открытых сеансов Discord. Закройте один и войдите снова.",
    ),
    (
        "Discord repeatedly refused this connection without a reason. Check the account in the official client; fastcord stops instead of retrying.",
        "Discord несколько раз без объяснения отклонил подключение. Проверьте учётную запись в официальном клиенте; fastcord прекращает попытки.",
    ),
    (
        "Discord returned a Gateway address that is not a Discord Gateway. fastcord refused to send the login token there.",
        "Discord вернул адрес Gateway, который не принадлежит Discord. fastcord не стал отправлять туда токен входа.",
    ),
    (
        "A guild subscription could not be encoded within Discord's 15 KiB Gateway payload limit. Automatic attempts have stopped.",
        "Подписку на сервер не удалось уложить в предел Gateway в 15 КиБ. Автоматические попытки прекращены.",
    ),
    (
        "A Go Live signal could not be encoded within Discord's 15 KiB Gateway payload limit. Automatic attempts have stopped.",
        "Сигнал демонстрации экрана не удалось уложить в предел Gateway в 15 КиБ. Автоматические попытки прекращены.",
    ),
    (
        "Ten connection attempts in a row failed, so fastcord stopped trying. Check the network, then reconnect.",
        "Десять попыток подключения подряд не удались, и fastcord прекратил попытки. Проверьте сеть и подключитесь снова.",
    ),
    (
        "fastcord signed in to Discord 20 times in 24 hours and stopped to avoid hammering it. Reconnect when you are ready.",
        "fastcord входил в Discord 20 раз за 24 часа и остановился, чтобы не перегружать сервис. Подключитесь снова, когда будете готовы.",
    ),
    (
        "Discord (or the network in front of it) refused to name a Gateway address twice in a row. Reconnect later.",
        "Discord (или сеть перед ним) дважды подряд отказался назвать адрес Gateway. Подключитесь позже.",
    ),
    (
        "Required account state exceeds fastcord's 12 MiB metadata safety limit. The connection has stopped rather than silently dropping guild identity or permission data.",
        "Необходимое состояние учётной записи превышает предел безопасности fastcord в 12 МиБ для метаданных. Подключение остановлено, чтобы не терять сведения о серверах и правах.",
    ),
    (
        "the Gateway stream-signal queue is full or closed",
        "очередь сигналов трансляции Gateway переполнена или закрыта",
    ),
    (
        "Could not connect to Discord's QR login service. Check your network and try again.",
        "Не удалось подключиться к службе входа по QR-коду. Проверьте сеть и повторите попытку.",
    ),
    (
        "Could not generate this login attempt's encryption key.",
        "Не удалось создать ключ шифрования для этой попытки входа.",
    ),
    (
        "Discord's QR login service sent an unexpected response. Login stopped; use token login or try again.",
        "Служба входа по QR-коду прислала неожиданный ответ. Вход прерван; войдите по токену или повторите попытку.",
    ),
    (
        "Discord's QR login response could not be decrypted. Login stopped; try again.",
        "Не удалось расшифровать ответ службы входа по QR-коду. Вход прерван, повторите попытку.",
    ),
    (
        "Discord's QR login service did not confirm this attempt's key. Login stopped for safety; try again.",
        "Служба входа по QR-коду не подтвердила ключ этой попытки. Вход прерван из соображений безопасности, повторите попытку.",
    ),
    ("This QR code expired.", "Срок действия QR-кода истёк."),
    (
        "Login was cancelled on the phone.",
        "Вход отменён на телефоне.",
    ),
    ("QR login was cancelled.", "Вход по QR-коду отменён."),
    (
        "The connection to Discord's QR login service stopped responding.",
        "Служба входа по QR-коду перестала отвечать.",
    ),
    (
        "Discord's QR login service closed the connection.",
        "Служба входа по QR-коду закрыла соединение.",
    ),
    (
        "Discord requires a CAPTCHA to finish this QR login. fastcord cannot solve CAPTCHAs and does not bypass them. Use token login (advanced) or log in with the official Discord client.",
        "Для завершения входа по QR-коду Discord требует капчу. fastcord не решает капчи и не обходит их. Войдите по токену (для опытных) или через официальный клиент Discord.",
    ),
    (
        "Discord is rate limiting QR logins. Wait a few minutes before trying again.",
        "Discord ограничивает частоту входов по QR-коду. Подождите несколько минут и повторите.",
    ),
    (
        "Could not reach Discord to finish the QR login. Try again.",
        "Не удалось связаться с Discord для завершения входа по QR-коду. Повторите попытку.",
    ),
    (
        "The name needs 1 to 100 characters.",
        "Название — от 1 до 100 символов.",
    ),
    (
        "The description is limited to 1000 characters.",
        "Описание — не больше 1000 символов.",
    ),
    (
        "Choose the voice or Stage channel.",
        "Выберите голосовой канал или трибуну.",
    ),
    (
        "An external event needs a location of up to 100 characters.",
        "Для внешнего события нужно место — до 100 символов.",
    ),
    (
        "An external event needs an end time.",
        "Для внешнего события нужно время окончания.",
    ),
    (
        "The start time must be in the future.",
        "Время начала должно быть в будущем.",
    ),
    (
        "The end time must be after the start.",
        "Время окончания должно быть позже начала.",
    ),
    (
        "Starting at sign-in is not supported on this operating system.",
        "Запуск при входе в систему не поддерживается в этой операционной системе.",
    ),
    (
        "The operating system refused the start-at-sign-in change.",
        "Операционная система отклонила изменение автозапуска.",
    ),
    (
        "capture supports even output sizes up to 1280x720 at 1-30 frames per second",
        "захват поддерживает чётные размеры до 1280x720 и 1–30 кадров в секунду",
    ),
    (
        "no screen or window was chosen",
        "экран или окно не выбраны",
    ),
    (
        "Windows refused to capture this source (a privacy setting, a policy, or a protected window)",
        "Windows отказала в захвате этого источника (из-за настройки конфиденциальности, политики или защищённого окна)",
    ),
    (
        "macOS refused to capture the screen: allow fastcord under System Settings, Privacy & Security, Screen Recording, then restart it",
        "macOS отказала в захвате экрана: разрешите fastcord в «Системные настройки → Конфиденциальность и безопасность → Запись экрана» и перезапустите приложение",
    ),
    (
        "the system refused to capture this source",
        "система отказала в захвате этого источника",
    ),
    (
        "the captured window, display or camera closed",
        "захватываемое окно, экран или камера закрылись",
    ),
    (
        "the selected camera is not connected",
        "выбранная камера не подключена",
    ),
    (
        "The attachment could not be opened by the operating system.",
        "Операционная система не смогла открыть вложение.",
    ),
    (
        "The downloaded file could not be marked as coming from the Internet.",
        "Не удалось пометить скачанный файл как полученный из интернета.",
    ),
    (
        "The native credential store is locked or access was denied.",
        "Хранилище учётных данных системы заблокировано или доступ запрещён.",
    ),
    (
        "The native credential store is unavailable.",
        "Хранилище учётных данных системы недоступно.",
    ),
    (
        "The saved credential has an invalid format.",
        "Сохранённые учётные данные имеют неверный формат.",
    ),
    (
        "Multiple credentials match this account in the native store.",
        "В хранилище системы для этой учётной записи найдено несколько записей.",
    ),
    (
        "the application to capture audio from is gone",
        "приложение, звук которого нужно захватить, закрылось",
    ),
    (
        "there is no audio output device to capture",
        "нет устройства вывода звука для захвата",
    ),
    (
        "the system refused to capture audio (a privacy setting or policy)",
        "система отказала в захвате звука (из-за настройки конфиденциальности или политики)",
    ),
    (
        "Desktop notifications are not supported on this platform.",
        "Уведомления на рабочем столе на этой платформе не поддерживаются.",
    ),
    (
        "The operating system refused the notification.",
        "Операционная система отклонила уведомление.",
    ),
    (
        "The operating system refused the global shortcut listener.",
        "Операционная система отклонила отслеживание глобальных сочетаний клавиш.",
    ),
    ("Too many shortcuts.", "Слишком много сочетаний клавиш."),
    (
        "Speech synthesis is not supported on this platform.",
        "Синтез речи на этой платформе не поддерживается.",
    ),
    (
        "The operating system's speech engine is not available.",
        "Система синтеза речи операционной системы недоступна.",
    ),
    (
        "Too many messages are waiting to be read aloud.",
        "Слишком много сообщений ждёт озвучивания.",
    ),
    (
        "The tray icon is not supported on this operating system.",
        "Значок в области уведомлений не поддерживается в этой операционной системе.",
    ),
    (
        "This desktop shows no tray icons (on GNOME, the AppIndicator extension adds them).",
        "Этот рабочий стол не показывает значки в трее (в GNOME их добавляет расширение AppIndicator).",
    ),
    (
        "The operating system refused the tray icon.",
        "Операционная система отклонила значок в трее.",
    ),
    ("Open fastcord", "Открыть fastcord"),
    ("Unmute microphone", "Включить микрофон"),
    ("Mute microphone", "Выключить микрофон"),
    ("Disconnect", "Отключиться"),
    ("Quit", "Выйти"),
    (
        "fastcord - 1 unread mention",
        "fastcord — 1 непрочитанное упоминание",
    ),
    (
        "the capture thread could not be started",
        "не удалось запустить поток захвата",
    ),
    (
        "the portal runtime could not be started",
        "не удалось запустить среду портала",
    ),
    (
        "PipeWire refused the screen-share stream",
        "PipeWire отклонил поток демонстрации экрана",
    ),
    (
        "the desktop portal refused the screen share",
        "портал рабочего стола отклонил демонстрацию экрана",
    ),
    (
        "the camera is only implemented for Windows so far",
        "камера пока реализована только для Windows",
    ),
    (
        "screen capture is only implemented for Windows and Linux so far",
        "захват экрана пока реализован только для Windows и Linux",
    ),
    (
        "Media Foundation is not available, so cameras cannot be used",
        "Media Foundation недоступна, поэтому камеры использовать нельзя",
    ),
    (
        "could not create the camera listing thread",
        "не удалось создать поток списка камер",
    ),
    (
        "could not create the capture thread",
        "не удалось создать поток захвата",
    ),
    (
        "the capture thread ended abnormally",
        "поток захвата завершился аварийно",
    ),
    (
        "the capture thread panicked",
        "поток захвата аварийно остановился",
    ),
    (
        "could not create the capture probe thread",
        "не удалось создать поток проверки захвата",
    ),
    (
        "the capture probe panicked",
        "проверка захвата аварийно остановилась",
    ),
    (
        "ScreenCaptureKit did not answer",
        "ScreenCaptureKit не ответил",
    ),
    (
        "sharing audio needs a running PipeWire daemon",
        "для передачи звука нужна работающая служба PipeWire",
    ),
    (
        "PipeWire did not list its nodes",
        "PipeWire не вернул список узлов",
    ),
    (
        "the audio capture thread could not be started",
        "не удалось запустить поток захвата звука",
    ),
    (
        "PipeWire refused the audio stream",
        "PipeWire отклонил аудиопоток",
    ),
    (
        "macOS always leaves fastcord out of shared audio",
        "macOS всегда исключает fastcord из передаваемого звука",
    ),
    (
        "could not start the audio capture thread",
        "не удалось запустить поток захвата звука",
    ),
    (
        "audio capture did not start in time",
        "захват звука не запустился вовремя",
    ),
    (
        "audio activation did not complete",
        "активация звука не завершилась",
    ),
    (
        "audio activation returned no client",
        "активация звука не вернула клиент",
    ),
    (
        "the hardware codec stopped responding",
        "аппаратный кодек перестал отвечать",
    ),
    ("empty Opus packet", "пустой пакет Opus"),
    ("Opus packet too large", "пакет Opus слишком велик"),
    (
        "invalid Opus frame duration",
        "недопустимая длительность кадра Opus",
    ),
    ("the device is disconnected", "устройство отключено"),
    (
        "the device is in use by another application",
        "устройство занято другим приложением",
    ),
    (
        "access to the device was denied",
        "доступ к устройству запрещён",
    ),
    (
        "the system audio service is unavailable",
        "системная звуковая служба недоступна",
    ),
    (
        "the device configuration changed",
        "настройки устройства изменились",
    ),
    (
        "the device format is not supported",
        "формат устройства не поддерживается",
    ),
    ("the audio device failed", "сбой аудиоустройства"),
    (
        "the device was not found. Plug it in again or choose another device.",
        "устройство не найдено. Подключите его снова или выберите другое.",
    ),
    (
        "another application is using the device exclusively. Close it, or turn off exclusive mode for this device in the system sound settings.",
        "другое приложение использует устройство в монопольном режиме. Закройте его или отключите монопольный режим для этого устройства в настройках звука системы.",
    ),
    (
        "access was denied. Allow microphone access for desktop applications in the system privacy settings (Windows: Settings > Privacy & security > Microphone; macOS: System Settings > Privacy & Security > Microphone).",
        "доступ запрещён. Разрешите приложениям доступ к микрофону в настройках конфиденциальности системы (Windows: «Параметры → Конфиденциальность и защита → Микрофон»; macOS: «Системные настройки → Конфиденциальность и безопасность → Микрофон»).",
    ),
    (
        "the system audio service is not running. Restart it or restart the computer.",
        "системная звуковая служба не запущена. Перезапустите её или компьютер.",
    ),
    (
        "the device changed its format while in use. fastcord reopens it on its own; if it stays silent, select it again.",
        "устройство изменило формат во время работы. fastcord переоткроет его сам; если звука нет, выберите его снова.",
    ),
    (
        "the device offers no format fastcord can use. Choose another device, or another sample rate in the system sound settings.",
        "устройство не предлагает формат, который может использовать fastcord. Выберите другое устройство или другую частоту дискретизации в настройках звука системы.",
    ),
    (
        "the device failed. Select it again or choose another device.",
        "сбой устройства. Выберите его снова или выберите другое.",
    ),
    (
        "no default input device",
        "нет устройство ввода по умолчанию",
    ),
    (
        "the selected input device is not connected",
        "выбранное устройство ввода не подключено",
    ),
    (
        "input device: the device is disconnected",
        "Устройство ввода: устройство отключено",
    ),
    (
        "input device: the device is in use by another application",
        "Устройство ввода: устройство занято другим приложением",
    ),
    (
        "input device: access to the device was denied",
        "Устройство ввода: доступ к устройству запрещён",
    ),
    (
        "input device: the system audio service is unavailable",
        "Устройство ввода: системная звуковая служба недоступна",
    ),
    (
        "input device: the device configuration changed",
        "Устройство ввода: настройки устройства изменились",
    ),
    (
        "input device: the device format is not supported",
        "Устройство ввода: формат устройства не поддерживается",
    ),
    (
        "input device: the audio device failed",
        "Устройство ввода: сбой аудиоустройства",
    ),
    (
        "no default output device",
        "нет устройство вывода по умолчанию",
    ),
    (
        "the selected output device is not connected",
        "выбранное устройство вывода не подключено",
    ),
    (
        "output device: the device is disconnected",
        "Устройство вывода: устройство отключено",
    ),
    (
        "output device: the device is in use by another application",
        "Устройство вывода: устройство занято другим приложением",
    ),
    (
        "output device: access to the device was denied",
        "Устройство вывода: доступ к устройству запрещён",
    ),
    (
        "output device: the system audio service is unavailable",
        "Устройство вывода: системная звуковая служба недоступна",
    ),
    (
        "output device: the device configuration changed",
        "Устройство вывода: настройки устройства изменились",
    ),
    (
        "output device: the device format is not supported",
        "Устройство вывода: формат устройства не поддерживается",
    ),
    (
        "output device: the audio device failed",
        "Устройство вывода: сбой аудиоустройства",
    ),
    (
        "Microphone: the device was not found. Plug it in again or choose another device.",
        "Микрофон: устройство не найдено. Подключите его снова или выберите другое.",
    ),
    (
        "Microphone: another application is using the device exclusively. Close it, or turn off exclusive mode for this device in the system sound settings.",
        "Микрофон: другое приложение использует устройство в монопольном режиме. Закройте его или отключите монопольный режим для этого устройства в настройках звука системы.",
    ),
    (
        "Microphone: access was denied. Allow microphone access for desktop applications in the system privacy settings (Windows: Settings > Privacy & security > Microphone; macOS: System Settings > Privacy & Security > Microphone).",
        "Микрофон: доступ запрещён. Разрешите приложениям доступ к микрофону в настройках конфиденциальности системы (Windows: «Параметры → Конфиденциальность и защита → Микрофон»; macOS: «Системные настройки → Конфиденциальность и безопасность → Микрофон»).",
    ),
    (
        "Microphone: the system audio service is not running. Restart it or restart the computer.",
        "Микрофон: системная звуковая служба не запущена. Перезапустите её или компьютер.",
    ),
    (
        "Microphone: the device changed its format while in use. fastcord reopens it on its own; if it stays silent, select it again.",
        "Микрофон: устройство изменило формат во время работы. fastcord переоткроет его сам; если звука нет, выберите его снова.",
    ),
    (
        "Microphone: the device offers no format fastcord can use. Choose another device, or another sample rate in the system sound settings.",
        "Микрофон: устройство не предлагает формат, который может использовать fastcord. Выберите другое устройство или другую частоту дискретизации в настройках звука системы.",
    ),
    (
        "Microphone: the device failed. Select it again or choose another device.",
        "Микрофон: сбой устройства. Выберите его снова или выберите другое.",
    ),
    (
        "Playback: the device was not found. Plug it in again or choose another device.",
        "Воспроизведение: устройство не найдено. Подключите его снова или выберите другое.",
    ),
    (
        "Playback: another application is using the device exclusively. Close it, or turn off exclusive mode for this device in the system sound settings.",
        "Воспроизведение: другое приложение использует устройство в монопольном режиме. Закройте его или отключите монопольный режим для этого устройства в настройках звука системы.",
    ),
    (
        "Playback: access was denied. Allow microphone access for desktop applications in the system privacy settings (Windows: Settings > Privacy & security > Microphone; macOS: System Settings > Privacy & Security > Microphone).",
        "Воспроизведение: доступ запрещён. Разрешите приложениям доступ к микрофону в настройках конфиденциальности системы (Windows: «Параметры → Конфиденциальность и защита → Микрофон»; macOS: «Системные настройки → Конфиденциальность и безопасность → Микрофон»).",
    ),
    (
        "Playback: the system audio service is not running. Restart it or restart the computer.",
        "Воспроизведение: системная звуковая служба не запущена. Перезапустите её или компьютер.",
    ),
    (
        "Playback: the device changed its format while in use. fastcord reopens it on its own; if it stays silent, select it again.",
        "Воспроизведение: устройство изменило формат во время работы. fastcord переоткроет его сам; если звука нет, выберите его снова.",
    ),
    (
        "Playback: the device offers no format fastcord can use. Choose another device, or another sample rate in the system sound settings.",
        "Воспроизведение: устройство не предлагает формат, который может использовать fastcord. Выберите другое устройство или другую частоту дискретизации в настройках звука системы.",
    ),
    (
        "Playback: the device failed. Select it again or choose another device.",
        "Воспроизведение: сбой устройства. Выберите его снова или выберите другое.",
    ),
    (
        "no audio input or output requested",
        "не запрошен ни ввод, ни вывод звука",
    ),
    (
        "the audio engine thread failed",
        "поток звукового движка завершился с ошибкой",
    ),
    (
        "The microphone is open but no sound arrived. Check that it is not muted in the system sound settings or by a switch on the device, that it is plugged in, and that the right input is selected.",
        "Микрофон открыт, но звука нет. Проверьте, что он не отключён в настройках звука системы или переключателем на устройстве, что он подключён и что выбран нужный вход.",
    ),
    (
        "The microphone test failed.",
        "Проверка микрофона не удалась.",
    ),
    (
        "the audio file could not be read",
        "не удалось прочитать аудиофайл",
    ),
    ("the audio file is damaged", "аудиофайл повреждён"),
    (
        "this Ogg audio format is not supported",
        "этот формат аудио Ogg не поддерживается",
    ),
    (
        "Microphone access was denied. Allow it in the system privacy settings.",
        "Доступ к микрофону запрещён. Разрешите его в настройках конфиденциальности системы.",
    ),
    (
        "The microphone is unavailable or stopped. Check the input device.",
        "Микрофон недоступен или остановился. Проверьте устройство ввода.",
    ),
    ("The recording is too short.", "Запись слишком короткая."),
    ("The recording failed.", "Запись не удалась."),
    ("the sound file is too large", "файл звука слишком велик"),
    ("the sound is too long", "звук слишком длинный"),
    (
        "the audio format of this sound is not supported",
        "аудиоформат этого звука не поддерживается",
    ),
    ("the sound file is damaged", "файл звука повреждён"),
    ("Light (default)", "Лёгкое (по умолчанию)"),
    (
        "High quality (DeepFilterNet 3)",
        "Высокое качество (DeepFilterNet 3)",
    ),
    (
        "Could not initialize DeepFilterNet 3. Select another mode or reselect High quality to retry.",
        "Не удалось запустить DeepFilterNet 3. Выберите другой режим или заново выберите «Высокое качество».",
    ),
    (
        "The bundled denoiser model does not support 48 kHz mono / 480-sample blocks.",
        "Встроенная модель шумоподавления не поддерживает моно 48 кГц и блоки по 480 отсчётов.",
    ),
    (
        "High quality denoising failed. Reselect High quality to retry.",
        "Шумоподавление высокого качества дало сбой. Заново выберите «Высокое качество».",
    ),
    (
        "The High quality denoiser stopped unexpectedly. Reselect High quality to retry.",
        "Шумоподавление высокого качества неожиданно остановилось. Заново выберите «Высокое качество».",
    ),
    (
        "High quality could not keep up with real time on this computer. Reselect High quality to retry.",
        "На этом компьютере высокое качество не успевает работать в реальном времени. Заново выберите «Высокое качество».",
    ),
    ("hardware", "аппаратный"),
    ("software", "программный"),
    ("the file could not be read", "не удалось прочитать файл"),
    ("the media file is damaged", "медиафайл повреждён"),
    (
        "the media file has too many or too large samples",
        "в медиафайле слишком много отсчётов или они слишком велики",
    ),
    (
        "fragmented MP4 is not supported",
        "фрагментированный MP4 не поддерживается",
    ),
    (
        "the video codec is not supported",
        "видеокодек не поддерживается",
    ),
    (
        "the file has no playable track",
        "в файле нет воспроизводимой дорожки",
    ),
    (
        "the file is not an MP4 or WebM",
        "файл не является MP4 или WebM",
    ),
    ("the image has no pixels", "в изображении нет пикселей"),
    (
        "an NV12 stride is shorter than its row",
        "шаг строки NV12 короче самой строки",
    ),
    (
        "an NV12 plane is shorter than its rows",
        "плоскость NV12 короче своих строк",
    ),
    (
        "no hardware video codec is available",
        "нет доступного аппаратного видеокодека",
    ),
    (
        "no software video codec is available",
        "нет доступного программного видеокодека",
    ),
    ("no video codec is available", "нет доступного видеокодека"),
    (
        "every video codec failed to open",
        "ни один видеокодек не удалось открыть",
    ),
    (
        "decoded frame dimensions exceed playback limits",
        "размеры декодированного кадра превышают пределы воспроизведения",
    ),
    (
        "decoded frame pool capacity limit exceeded",
        "превышен предел пула декодированных кадров",
    ),
    (
        "all decoded frame buffers are leased",
        "все буферы декодированных кадров заняты",
    ),
    (
        "decoded frame buffer allocation failed",
        "не удалось выделить буфер декодированного кадра",
    ),
    ("sharing stopped", "демонстрация остановлена"),
    (
        "the stream connection closed",
        "подключение трансляции закрыто",
    ),
    ("Dusk", "Сумерки"),
    ("Mint", "Мята"),
    ("Slate", "Графит"),
    ("Studio", "Студия"),
    ("No effect", "Без эффекта"),
    ("Blur background", "Размытие фона"),
    ("Background: Dusk", "Фон: сумерки"),
    ("Background: Mint", "Фон: мята"),
    ("Background: Slate", "Фон: графит"),
    ("Background: Studio", "Фон: студия"),
    ("left voice", "вышли из голосового канала"),
    ("disconnected from voice", "отключено от голосового канала"),
    ("voice session expired", "голосовой сеанс истёк"),
    (
        "voice server rejected the session",
        "голосовой сервер отклонил сеанс",
    ),
    (
        "this call requires end-to-end encryption (DAVE)",
        "для этого звонка нужно сквозное шифрование (DAVE)",
    ),
    (
        "voice could not establish DAVE encryption",
        "не удалось установить шифрование DAVE для голоса",
    ),
    (
        "voice server rate limit reached",
        "достигнут предел частоты запросов голосового сервера",
    ),
    ("voice server not found", "голосовой сервер не найден"),
    (
        "UDP to the voice server is blocked",
        "UDP до голосового сервера заблокирован",
    ),
    (
        "no supported voice encryption mode",
        "нет поддерживаемого режима шифрования голоса",
    ),
    (
        "voice encryption key exhausted",
        "ключ шифрования голоса исчерпан",
    ),
    (
        "invalid voice server endpoint",
        "некорректный адрес голосового сервера",
    ),
    ("voice connection lost", "голосовое подключение потеряно"),
    ("voice protocol error", "ошибка голосового протокола"),
    (
        "voice media queue is full",
        "очередь голосовых данных переполнена",
    ),
    (
        "not a valid audio or video frame",
        "недопустимый аудио- или видеокадр",
    ),
    (
        "voice connection is closed",
        "голосовое подключение закрыто",
    ),
    (
        "unsupported or inconsistent video negotiation",
        "неподдерживаемое или противоречивое согласование видео",
    ),
    (
        "malformed codec RTP payload",
        "некорректные данные кодека в RTP",
    ),
    (
        "video frame exceeds its byte bound",
        "видеокадр превышает допустимый размер",
    ),
    ("incomplete video frame", "неполный видеокадр"),
    (
        "video pacing queue reached its time or byte bound",
        "очередь отправки видео достигла предела по времени или размеру",
    ),
    (
        "transport key is not 32 bytes",
        "транспортный ключ не равен 32 байтам",
    ),
    (
        "transport nonce space exhausted; a new key is required",
        "пространство одноразовых значений исчерпано; нужен новый ключ",
    ),
    (
        "encrypted packet is too short",
        "зашифрованный пакет слишком короткий",
    ),
    (
        "packet failed transport authentication",
        "пакет не прошёл проверку подлинности",
    ),
    (
        "unsupported DAVE protocol version",
        "неподдерживаемая версия протокола DAVE",
    ),
    ("invalid DAVE message", "некорректное сообщение DAVE"),
    ("DAVE operation failed", "сбой операции DAVE"),
    ("unexpected DAVE epoch", "неожиданная эпоха DAVE"),
    ("unexpected DAVE transition", "неожиданный переход DAVE"),
    ("DAVE media keys are not ready", "медиаключи DAVE не готовы"),
    (
        "local user is not in the DAVE group",
        "локального пользователя нет в группе DAVE",
    ),
    (
        "DAVE refused an unencrypted media frame",
        "DAVE отклонил незашифрованный медиакадр",
    ),
    (
        "RTCP packet shorter than its header",
        "пакет RTCP короче своего заголовка",
    ),
    (
        "RTCP report count exceeds the packet",
        "число отчётов RTCP превышает размер пакета",
    ),
    ("invalid RTCP padding", "некорректное заполнение RTCP"),
    (
        "malformed RTCP feedback packet",
        "повреждённый пакет обратной связи RTCP",
    ),
    (
        "RTP packet shorter than its header",
        "пакет RTP короче своего заголовка",
    ),
    (
        "RTP header extension exceeds the packet",
        "расширение заголовка RTP превышает размер пакета",
    ),
    ("invalid RTP padding", "некорректное заполнение RTP"),
    (
        "malformed RTP header extension element",
        "повреждённый элемент расширения заголовка RTP",
    ),
    (
        "malformed IP discovery response",
        "повреждённый ответ определения IP",
    ),
    (
        "IP discovery response for another SSRC",
        "ответ определения IP для другого SSRC",
    ),
    (
        "IP discovery response with an invalid address",
        "ответ определения IP с некорректным адресом",
    ),
    (
        "snowflake must be a non-negative decimal integer",
        "snowflake должен быть неотрицательным десятичным целым числом",
    ),
    ("a list or null", "список или null"),
    (
        "a non-negative integer or decimal string",
        "неотрицательное целое число или десятичная строка",
    ),
    ("a message nonce", "одноразовое значение сообщения"),
];
