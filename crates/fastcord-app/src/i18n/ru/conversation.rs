//! History, sending, outbox, threads, forums, forwarding, attachments, media players and voice messages.

pub const ENTRIES: &[(&str, &str)] = &[
    // history.rs
    (
        "A message is too large to display and was left out.",
        "Сообщение слишком большое для показа и пропущено.",
    ),
    (
        "That message's channel is no longer open.",
        "Канал этого сообщения больше не открыт.",
    ),
    (
        "That message is no longer loaded; it may have been deleted.",
        "Сообщение больше не загружено: возможно, оно удалено.",
    ),
    (
        "Only your own messages can be edited or deleted.",
        "Изменять и удалять можно только свои сообщения.",
    ),
    (
        "A message needs some text. Delete it instead.",
        "В сообщении должен быть текст. Удалите его вместо этого.",
    ),
    (
        "The message is too long for Discord.",
        "Сообщение слишком длинное для Discord.",
    ),
    (
        "That message was deleted before your edit was saved.",
        "Сообщение удалили до сохранения вашей правки.",
    ),
    (
        "A change to a message that is no longer shown failed.",
        "Не удалось изменить сообщение, которое уже не показано.",
    ),
    (
        "That message was deleted and cannot be opened.",
        "Сообщение удалено, открыть его нельзя.",
    ),
    (
        "That message is unavailable and cannot be opened.",
        "Сообщение недоступно, открыть его нельзя.",
    ),
    ("There are no newer messages.", "Новых сообщений нет."),
    (
        "You cannot add reactions in this channel.",
        "В этом канале нельзя добавлять реакции.",
    ),
    (
        "That emoji cannot be used as a reaction.",
        "Это эмодзи нельзя использовать как реакцию.",
    ),
    (
        "The channel changed while loading. Retry to load a current page.",
        "Канал изменился во время загрузки. Повторите, чтобы загрузить актуальную страницу.",
    ),
    (
        "Discord denied access to this history. Check your permissions here.",
        "Discord закрыл доступ к этой истории. Проверьте свои права здесь.",
    ),
    (
        "This channel is no longer available.",
        "Этот канал больше недоступен.",
    ),
    (
        "Discord sent a history page fastcord could not accept.",
        "Discord прислал страницу истории, которую fastcord не смог принять.",
    ),
    (
        "Could not load messages. Retry when the connection is back.",
        "Не удалось загрузить сообщения. Повторите, когда появится соединение.",
    ),
    // history/embeds.rs
    (
        "That message's link previews are still being changed.",
        "Предпросмотр ссылок этого сообщения ещё меняется.",
    ),
    (
        "Too many link-preview changes are still being saved. Try again shortly.",
        "Слишком много изменений предпросмотра ссылок ещё сохраняется. Повторите чуть позже.",
    ),
    (
        "You can only change link previews of your own messages here.",
        "Здесь можно менять предпросмотр ссылок только в своих сообщениях.",
    ),
    (
        "Discord did not allow changing this message's link previews.",
        "Discord не разрешил менять предпросмотр ссылок этого сообщения.",
    ),
    (
        "That message is no longer available.",
        "Это сообщение больше недоступно.",
    ),
    (
        "Discord rejected the change to the link previews.",
        "Discord отклонил изменение предпросмотра ссылок.",
    ),
    (
        "Could not reach Discord; the link previews were not changed.",
        "Не удалось связаться с Discord: предпросмотр ссылок не изменён.",
    ),
    (
        "Discord did not confirm the change; it may have been applied. Trying again is safe.",
        "Discord не подтвердил изменение, возможно, оно применено. Повторить безопасно.",
    ),
    (
        "The link previews could not be changed.",
        "Не удалось изменить предпросмотр ссылок.",
    ),
    // history/moderation.rs
    (
        "You need Manage Messages in this channel.",
        "В этом канале нужно право «Управлять сообщениями».",
    ),
    (
        "At most 100 messages can be deleted at once.",
        "За раз можно удалить не больше 100 сообщений.",
    ),
    (
        "Messages 14 days old or older cannot be deleted in bulk.",
        "Сообщения старше 14 дней нельзя удалять массово.",
    ),
    (
        "Select at least 2 messages.",
        "Выберите не меньше двух сообщений.",
    ),
    (
        "A moderation request is still waiting for Discord.",
        "Запрос модерации ещё ждёт ответа Discord.",
    ),
    (
        "Discord refused: you need Manage Messages here.",
        "Discord отказал: здесь нужно право «Управлять сообщениями».",
    ),
    ("That message is already gone.", "Этого сообщения уже нет."),
    (
        "Discord did not confirm. It may have been applied; check the messages before trying again.",
        "Discord не подтвердил. Возможно, действие выполнено: проверьте сообщения, прежде чем повторять.",
    ),
    (
        "Discord asks for two-factor verification. Complete it in the official client; nothing was changed here.",
        "Discord требует двухфакторную проверку. Пройдите её в официальном клиенте; здесь ничего не изменено.",
    ),
    (
        "Discord requires an account check. Requests are paused.",
        "Discord требует проверки учётной записи. Запросы приостановлены.",
    ),
    (
        "Discord did not accept the request.",
        "Discord не принял запрос.",
    ),
    ("Messages deleted.", "Сообщения удалены."),
    ("Reaction removed.", "Реакция убрана."),
    // history/poll.rs
    (
        "That answer is not part of the poll.",
        "Этого варианта нет в опросе.",
    ),
    // outbox.rs
    (
        "Upload cancelled. Retry to start it again.",
        "Загрузка отменена. Повторите, чтобы начать заново.",
    ),
    (
        "The channel was closed, or you cannot send messages in it.",
        "Канал закрыт, или вы не можете отправлять в нём сообщения.",
    ),
    (
        "Discord did not allow this message or emoji here. Check channel permissions and animated/external emoji entitlement.",
        "Discord не разрешил здесь это сообщение или эмодзи. Проверьте права канала и доступ к анимированным и внешним эмодзи.",
    ),
    (
        "Discord rejected the reply; the message it answers may be deleted, unavailable, not replyable, or the text may be too long.",
        "Discord отклонил ответ: сообщение, на которое вы отвечаете, могло быть удалено, стать недоступным или не допускать ответов, либо текст слишком длинный.",
    ),
    (
        "Discord rejected the message; it may be too long or empty, or a custom emoji may be unavailable or not permitted for this account.",
        "Discord отклонил сообщение: оно может быть слишком длинным или пустым, либо пользовательское эмодзи недоступно этой учётной записи.",
    ),
    (
        "Discord rejected the message.",
        "Discord отклонил сообщение.",
    ),
    (
        "Could not reach Discord; the message was not sent.",
        "Не удалось связаться с Discord: сообщение не отправлено.",
    ),
    (
        "The message could not be sent.",
        "Не удалось отправить сообщение.",
    ),
    (
        "Discord reported a server error; the message may have been delivered.",
        "Discord сообщил об ошибке сервера; сообщение могло быть доставлено.",
    ),
    (
        "Discord did not answer in time; the message may have been delivered.",
        "Discord не ответил вовремя; сообщение могло быть доставлено.",
    ),
    (
        "The connection failed while sending; the message may have been delivered.",
        "Соединение прервалось при отправке; сообщение могло быть доставлено.",
    ),
    (
        "Discord's answer could not be read; the message may have been delivered.",
        "Не удалось прочитать ответ Discord; сообщение могло быть доставлено.",
    ),
    (
        "Discord says you lack permission to send here (50013). Check your role and the channel's permissions.",
        "Discord сообщает, что у вас нет прав писать здесь (50013). Проверьте свою роль и права канала.",
    ),
    (
        "Discord says you cannot access this channel (50001).",
        "Discord сообщает, что у вас нет доступа к этому каналу (50001).",
    ),
    (
        "Discord says this user cannot receive messages from you (50007).",
        "Discord сообщает, что этот пользователь не может получать ваши сообщения (50007).",
    ),
    (
        "Discord requires you to verify your account before sending (40002). Verify your e-mail or phone in an official client.",
        "Discord требует подтвердить учётную запись перед отправкой (40002). Подтвердите почту или телефон в официальном клиенте.",
    ),
    (
        "You are opening direct messages too fast (40003). Wait a while and try again.",
        "Вы слишком быстро открываете личные сообщения (40003). Подождите и повторите.",
    ),
    (
        "Discord has temporarily disabled sending messages for this account (40004).",
        "Discord временно отключил отправку сообщений для этой учётной записи (40004).",
    ),
    (
        "Slowmode is on in this channel (20016). Wait a moment and try again.",
        "В этом канале включён медленный режим (20016). Подождите немного и повторите.",
    ),
    (
        "AutoMod blocked this message",
        "AutoMod заблокировал это сообщение",
    ),
    (
        "AutoMod blocked this message: {}",
        "AutoMod заблокировал это сообщение: {}",
    ),
    (
        "{} ({}). Nothing was sent; change the text, then retry.",
        "{} ({}). Ничего не отправлено: измените текст и повторите.",
    ),
    (
        "Discord refused the send ({}).",
        "Discord отказал в отправке ({}).",
    ),
    (
        "Discord refused the send: {} ({}).",
        "Discord отказал в отправке: {} ({}).",
    ),
    (
        "A message needs some text.",
        "В сообщении должен быть текст.",
    ),
    (
        "A voice message cannot carry text or other files.",
        "К голосовому сообщению нельзя добавить текст или другие файлы.",
    ),
    // changes.rs
    (
        "A change to that message is still being saved.",
        "Изменение этого сообщения ещё сохраняется.",
    ),
    (
        "Too many unsaved changes. Retry or dismiss some first.",
        "Слишком много несохранённых изменений. Сначала повторите или отклоните некоторые.",
    ),
    (
        "Discord did not allow this edit.",
        "Discord не разрешил это изменение.",
    ),
    (
        "Discord rejected the edit; it may be too long.",
        "Discord отклонил правку: возможно, текст слишком длинный.",
    ),
    ("Discord rejected the edit.", "Discord отклонил правку."),
    (
        "Could not reach Discord; the edit was not saved.",
        "Не удалось связаться с Discord: правка не сохранена.",
    ),
    (
        "Discord did not confirm the edit; it may have been saved. Retrying is safe.",
        "Discord не подтвердил правку, возможно, она сохранена. Повторить безопасно.",
    ),
    (
        "The edit could not be saved.",
        "Не удалось сохранить правку.",
    ),
    (
        "Discord did not allow deleting this message.",
        "Discord не разрешил удалить это сообщение.",
    ),
    (
        "Discord refused to delete the message.",
        "Discord отказался удалять сообщение.",
    ),
    (
        "Could not reach Discord; the message was not deleted.",
        "Не удалось связаться с Discord: сообщение не удалено.",
    ),
    (
        "Discord did not confirm the deletion; it may already be gone. Retrying is safe.",
        "Discord не подтвердил удаление, возможно, сообщения уже нет. Повторить безопасно.",
    ),
    (
        "The message could not be deleted.",
        "Не удалось удалить сообщение.",
    ),
    // threads.rs
    (
        "You are not allowed to do that with this thread.",
        "Вам нельзя делать это с этой веткой.",
    ),
    (
        "That thread or message is gone.",
        "Этой ветки или сообщения больше нет.",
    ),
    ("Discord did not accept that.", "Discord не принял это."),
    (
        "Discord did not confirm. The change may have been applied; check the thread before trying again.",
        "Discord не подтвердил. Возможно, изменение применено: проверьте ветку, прежде чем повторять.",
    ),
    (
        "Discord refused: you need Manage Threads here.",
        "Discord отказал: здесь нужно право «Управлять ветками».",
    ),
    ("That thread is gone.", "Этой ветки больше нет."),
    (
        "This thread cannot be changed that way any more.",
        "Эту ветку больше нельзя так изменить.",
    ),
    ("Give the thread a name.", "Введите название ветки."),
    (
        "Discord answered with something unexpected.",
        "Discord ответил неожиданным образом.",
    ),
    ("Thread name", "Название ветки"),
    (
        "Start a thread from this message",
        "Создать ветку из этого сообщения",
    ),
    ("Loading archived threads…", "Загрузка архивных веток…"),
    ("No archived threads.", "Архивных веток нет."),
    ("{} archived", "В архиве: {}"),
    ("Unnamed thread", "Ветка без названия"),
    (" (locked)", " (закрыта)"),
    ("Archived threads", "Архивные ветки"),
    // forum.rs
    (
        "Discord did not confirm the post. It may exist: check the list before posting again.",
        "Discord не подтвердил публикацию. Возможно, она создана: проверьте список, прежде чем публиковать снова.",
    ),
    (
        "You cannot post in this forum: a permission is missing.",
        "Вы не можете публиковать на этом форуме: не хватает прав.",
    ),
    ("This forum no longer exists.", "Этого форума больше нет."),
    ("The post is too large.", "Публикация слишком большая."),
    (
        "Discord rejected the title, the tags or the message. Check them and try again.",
        "Discord отклонил название, теги или сообщение. Проверьте их и повторите.",
    ),
    (
        "Discord refused the post: {} ({})",
        "Discord отказал в публикации: {} ({})",
    ),
    (
        "Discord refused the post ({}): {}",
        "Discord отказал в публикации ({}): {}",
    ),
    (
        "Discord did not accept the post. Try again.",
        "Discord не принял публикацию. Повторите попытку.",
    ),
    ("Attach files to the post", "Прикрепить файлы к публикации"),
    ("{} replies", "{} ответ|{} ответа|{} ответов"),
    ("pinned ", "закреплена "),
    ("locked ", "закрыта "),
    ("{} \u{b7} active {}{}", "{} \u{b7} активность {}{}"),
    ("{} \u{b7} active {}", "{} \u{b7} активность {}"),
    ("Sort", "Сортировка"),
    ("Latest activity", "По активности"),
    ("Date posted", "По дате создания"),
    ("All", "Все"),
    ("No active posts.", "Активных публикаций нет."),
    (
        "No active post has this tag.",
        "Нет активных публикаций с этим тегом.",
    ),
    (
        "Showing the {} most recent of {} active posts.",
        "Показано последних публикаций: {} из {} активных.",
    ),
    ("Archived posts", "Архивные публикации"),
    ("Hide", "Скрыть"),
    ("Load archived", "Загрузить архивные"),
    ("Untitled post", "Публикация без названия"),
    ("archived locked", "в архиве, закрыта"),
    ("archived", "в архиве"),
    (
        "Loading archived posts\u{2026}",
        "Загрузка архивных публикаций\u{2026}",
    ),
    ("No archived posts.", "Архивных публикаций нет."),
    ("Post title", "Название публикации"),
    (
        "Describe the post (a file is required)",
        "Опишите публикацию (нужен файл)",
    ),
    ("First message of the post", "Первое сообщение публикации"),
    ("New post", "Новая публикация"),
    ("Tags (required)", "Теги (обязательно)"),
    ("Tags", "Теги"),
    ("Attach files", "Прикреплять файлы"),
    (
        "Posting\u{2026} uploaded {}%",
        "Публикация\u{2026} загружено {}%",
    ),
    ("Posting\u{2026}", "Публикация\u{2026}"),
    (
        "A post with this title from you is in the list.",
        "Ваша публикация с таким названием уже есть в списке.",
    ),
    ("Open it", "Открыть её"),
    ("Discard draft", "Удалить черновик"),
    ("Post again", "Опубликовать снова"),
    ("Post", "Опубликовать"),
    // forward.rs
    (
        "Discord did not allow this: you may lack permission there, or the message cannot be forwarded.",
        "Discord не разрешил это: возможно, у вас нет прав там или сообщение нельзя переслать.",
    ),
    (
        "The destination or the original message is no longer available.",
        "Место назначения или исходное сообщение больше недоступно.",
    ),
    (
        "Sign-in is required again.",
        "Нужно снова войти в учётную запись.",
    ),
    (
        "Discord did not forward the message.",
        "Discord не переслал сообщение.",
    ),
    (
        "Already forwarded there; it will not be sent twice.",
        "Туда уже переслано; повторно отправлено не будет.",
    ),
    ("You cannot post there.", "Вы не можете писать туда."),
    (
        "Forward to at most five places at once.",
        "Можно переслать не больше чем в пять мест за раз.",
    ),
    ("The note is too long.", "Заметка слишком длинная."),
    (
        "Nothing to send: pick a place that has not received it.",
        "Нечего отправлять: выберите место, куда оно ещё не пересылалось.",
    ),
    (
        "Forwarded, but the note may not have been sent.",
        "Переслано, но заметка, возможно, не отправлена.",
    ),
    (
        "Forwarded, but the note was not sent.",
        "Переслано, но заметка не отправлена.",
    ),
    ("Forward message", "Переслать сообщение"),
    ("(no text)", "(без текста)"),
    (
        "Add a message (optional)",
        "Добавьте сообщение (необязательно)",
    ),
    (" - sending", " — отправка"),
    (" - forwarded", " — переслано"),
    (" - not sent: {}", " — не отправлено: {}"),
    (
        " - may have been delivered; check before sending again",
        " — возможно, доставлено; проверьте, прежде чем отправлять снова",
    ),
    ("Forward", "Переслать"),
    ("{} of {} places", "Выбрано мест: {} из {}"),
    // attachment_ui.rs
    (
        "This image is too large to preview here.",
        "Это изображение слишком большое для предпросмотра.",
    ),
    (
        "This image could not be decoded.",
        "Не удалось декодировать это изображение.",
    ),
    (
        "The image could not be fetched.",
        "Не удалось загрузить изображение.",
    ),
    (
        "The local attachment cache is unavailable.",
        "Локальный кэш вложений недоступен.",
    ),
    (
        "The file could not be saved there.",
        "Не удалось сохранить файл в это место.",
    ),
    (
        "The image preview was cancelled.",
        "Предпросмотр изображения отменён.",
    ),
    (
        "This file is larger than the 64 MiB limit for Open. Use Download to save it.",
        "Файл больше 64 МиБ — предела для открытия. Скачайте его, чтобы сохранить.",
    ),
    (
        "The attachment action was cancelled.",
        "Действие с вложением отменено.",
    ),
    (
        "The attachment could not be fetched.",
        "Не удалось загрузить вложение.",
    ),
    (
        "Attachment work is busy; try again shortly.",
        "Работа с вложениями занята; повторите чуть позже.",
    ),
    (
        "Two downloads are already running; try again when one finishes.",
        "Уже идут две загрузки; повторите, когда одна из них закончится.",
    ),
    (
        "This attachment action is already in progress.",
        "Это действие с вложением уже выполняется.",
    ),
    (
        "Only images, audio, video, PDF and text files can be opened from here. Use Download.",
        "Отсюда можно открыть только изображения, аудио, видео, PDF и текстовые файлы. Используйте «Скачать».",
    ),
    ("Attachment saved.", "Вложение сохранено."),
    ("Opened attachment.", "Вложение открыто."),
    ("Download", "Скачать"),
    ("Loading image…", "Загрузка изображения…"),
    // media_ui.rs
    (
        "The audio output stopped. Check the device and try again.",
        "Вывод звука остановился. Проверьте устройство и повторите.",
    ),
    (
        "This audio file is damaged. Use Download.",
        "Этот аудиофайл повреждён. Используйте «Скачать».",
    ),
    (
        "This audio format cannot be played here. Use Download or Open.",
        "Этот аудиоформат нельзя воспроизвести здесь. Используйте «Скачать» или «Открыть».",
    ),
    (
        "This file is damaged. Use Download.",
        "Этот файл повреждён. Используйте «Скачать».",
    ),
    (
        "This video format cannot be played here (only H.264 or AV1 in MP4 or WebM). Use Download or Open.",
        "Этот видеоформат нельзя воспроизвести здесь (только H.264 или AV1 в MP4 или WebM). Используйте «Скачать» или «Открыть».",
    ),
    (
        "This video is stored in a layout that cannot be played here. Use Download or Open.",
        "Это видео хранится в структуре, которую нельзя воспроизвести здесь. Используйте «Скачать» или «Открыть».",
    ),
    (
        "This media file is too large or complex to play here. Use Download.",
        "Этот медиафайл слишком большой или сложный для воспроизведения здесь. Используйте «Скачать».",
    ),
    (
        "The audio output could not be opened. Check the device in audio settings.",
        "Не удалось открыть вывод звука. Проверьте устройство в настройках звука.",
    ),
    (
        "No video decoder is available for this file on this device. Use Download or Open.",
        "На этом устройстве нет видеодекодера для этого файла. Используйте «Скачать» или «Открыть».",
    ),
    (
        "This file is larger than the 64 MiB playback limit. Use Download.",
        "Файл больше 64 МиБ — предела воспроизведения. Используйте «Скачать».",
    ),
    ("Playback was cancelled.", "Воспроизведение отменено."),
    (
        "The file could not be fetched. Try again.",
        "Не удалось загрузить файл. Повторите попытку.",
    ),
    ("Voice message", "Голосовое сообщение"),
    ("Pause", "Пауза"),
    ("Replay", "Повторить"),
    ("Play", "Воспроизвести"),
    (
        "The video could not be decoded; any sound continues.",
        "Не удалось декодировать видео; звук продолжает воспроизводиться.",
    ),
    // av_player.rs
    (
        "This AAC audio cannot be decoded here; playing without sound.",
        "Этот звук AAC нельзя декодировать здесь; воспроизведение без звука.",
    ),
    (
        "This audio format is not supported; playing without sound.",
        "Этот аудиоформат не поддерживается; воспроизведение без звука.",
    ),
    (
        "this video could not be decoded",
        "не удалось декодировать это видео",
    ),
    // voice_record.rs
    (
        "The preview could not be played. Check the output device.",
        "Не удалось воспроизвести запись. Проверьте устройство вывода.",
    ),
    (
        "The recording could not be saved.",
        "Не удалось сохранить запись.",
    ),
    (
        "Experimental: Discord documents voice-message recording for mobile only. Whether it is accepted from this client is unverified.",
        "Экспериментально: Discord описывает запись голосовых сообщений только для мобильных устройств. Примет ли он их от этого клиента, не проверено.",
    ),
    ("Record voice message", "Записать голосовое сообщение"),
    ("Opening the microphone…", "Включение микрофона…"),
    ("Recording", "Запись"),
    ("Stop", "Остановить"),
    ("Saving the recording…", "Сохранение записи…"),
    ("Voice message {}", "Голосовое сообщение {}"),
    ("Stop preview", "Остановить прослушивание"),
    ("Preview", "Прослушать"),
    ("Discard", "Сбросить"),
    // paste.rs
    (
        "The clipboard image could not be read.",
        "Не удалось прочитать изображение из буфера обмена.",
    ),
    (
        "Pasting an image is not supported on this platform yet.",
        "Вставка изображения на этой платформе пока не поддерживается.",
    ),
    (
        "The clipboard does not hold an image.",
        "В буфере обмена нет изображения.",
    ),
    (
        "The clipboard is in use by another program. Try again.",
        "Буфер обмена занят другой программой. Повторите попытку.",
    ),
    (
        "The clipboard image is too large to attach.",
        "Изображение из буфера обмена слишком большое для вложения.",
    ),
    (
        "The pasted image could not be saved.",
        "Не удалось сохранить вставленное изображение.",
    ),
    // switcher.rs
    ("opens chat", "откроет чат"),
    ("draft", "черновик"),
    ("No matching conversation.", "Подходящих бесед нет."),
    (
        "Jump to a conversation, or paste a Discord link",
        "Перейти к беседе или вставить ссылку Discord",
    ),
    (
        "Enter opens, Up/Down chooses, Escape closes. A pasted Discord link opens too.",
        "Enter открывает, стрелки вверх и вниз выбирают, Escape закрывает. Вставленная ссылка Discord тоже откроется.",
    ),
    ("Quick switcher", "Быстрый переход"),
    (
        "Previous / next channel or DM",
        "Предыдущий / следующий канал или личный чат",
    ),
    (
        "Previous / next unread conversation",
        "Предыдущая / следующая непрочитанная беседа",
    ),
    ("Previous / next server", "Предыдущий / следующий сервер"),
    (
        "Close overlay, cancel edit or reply",
        "Закрыть окно, отменить правку или ответ",
    ),
    ("This list", "Этот список"),
    ("Send / new line", "Отправить / новая строка"),
    ("Keyboard shortcuts", "Горячие клавиши"),
    (
        "Ctrl is Cmd on macOS. Voice shortcuts are set in the audio settings.",
        "В macOS вместо Ctrl используется Cmd. Голосовые сочетания задаются в настройках звука.",
    ),
    // attachments.rs
    ("Attachment URL is invalid.", "Неверная ссылка на вложение."),
    (
        "Attachment could not be fetched.",
        "Не удалось загрузить вложение.",
    ),
    (
        "Attachment server rejected the request.",
        "Сервер вложений отклонил запрос.",
    ),
    (
        "Attachment exceeds the local image or open limit.",
        "Вложение превышает локальный предел для изображений или открытия.",
    ),
    (
        "This attachment is not a supported image.",
        "Это вложение не является поддерживаемым изображением.",
    ),
    (
        "The image data could not be decoded.",
        "Не удалось декодировать данные изображения.",
    ),
    (
        "Attachment work was cancelled.",
        "Работа с вложением отменена.",
    ),
    (
        "The attachment URL could not be refreshed.",
        "Не удалось обновить ссылку на вложение.",
    ),
];
