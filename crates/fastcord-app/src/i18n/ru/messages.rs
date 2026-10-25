//! Message rows: timeline, rich text, reactions, polls, components, slash commands, pins, search and the emoji picker.

pub const ENTRIES: &[(&str, &str)] = &[
    // rich
    ("@unknown-user", "@неизвестный-пользователь"),
    ("@unknown-role", "@неизвестная-роль"),
    ("#unknown-channel", "#неизвестный-канал"),
    ("Image", "Изображение"),
    ("Thumbnail", "Миниатюра"),
    ("{}: {}", "{}: {}"),
    // reactions
    (
        "That reaction is still being saved.",
        "Эта реакция ещё сохраняется.",
    ),
    (
        "Too many reactions are still being saved. Try again in a moment.",
        "Слишком много реакций ещё сохраняется. Повторите чуть позже.",
    ),
    (
        "Discord did not allow this reaction. You may lack permission to add reactions here.",
        "Discord не разрешил эту реакцию. Возможно, у вас нет прав добавлять реакции здесь.",
    ),
    (
        "Discord did not allow removing this reaction.",
        "Discord не разрешил удалить эту реакцию.",
    ),
    (
        "That message or emoji is no longer available.",
        "Это сообщение или эмодзи больше недоступны.",
    ),
    (
        "Discord rejected the reaction. The message may already have the most different reactions it allows.",
        "Discord отклонил реакцию. Возможно, на сообщении уже максимальное число разных реакций.",
    ),
    (
        "Discord rejected the reaction.",
        "Discord отклонил реакцию.",
    ),
    (
        "Discord rejected removing the reaction.",
        "Discord отклонил удаление реакции.",
    ),
    (
        "Could not reach Discord; the reaction was not changed.",
        "Не удалось связаться с Discord; реакция не изменена.",
    ),
    (
        "Discord did not confirm the reaction change; it may have been applied.",
        "Discord не подтвердил изменение реакции; оно могло примениться.",
    ),
    (
        "The reaction could not be changed.",
        "Не удалось изменить реакцию.",
    ),
    (
        "Discord did not allow listing who reacted.",
        "Discord не разрешил показать, кто отреагировал.",
    ),
    (
        "That reaction or message is no longer available.",
        "Эта реакция или сообщение больше недоступны.",
    ),
    (
        "Could not reach Discord to list who reacted.",
        "Не удалось связаться с Discord, чтобы показать, кто отреагировал.",
    ),
    (
        "Who reacted could not be loaded.",
        "Не удалось загрузить список отреагировавших.",
    ),
    // polls
    (
        "That poll is still being updated.",
        "Этот опрос ещё обновляется.",
    ),
    (
        "Too many poll changes are still being saved. Try again in a moment.",
        "Слишком много изменений опросов ещё сохраняется. Повторите чуть позже.",
    ),
    ("This poll has ended.", "Этот опрос завершён."),
    ("That message has no poll.", "В этом сообщении нет опроса."),
    (
        "Only the person who made a poll can end it.",
        "Завершить опрос может только его автор.",
    ),
    (
        "Discord did not allow ending this poll.",
        "Discord не разрешил завершить этот опрос.",
    ),
    (
        "Discord did not allow voting in this poll.",
        "Discord не разрешил голосовать в этом опросе.",
    ),
    (
        "That poll is no longer available.",
        "Этот опрос больше недоступен.",
    ),
    (
        "Discord rejected the request. The poll may have ended.",
        "Discord отклонил запрос. Возможно, опрос уже завершён.",
    ),
    (
        "Discord rejected ending the poll.",
        "Discord отклонил завершение опроса.",
    ),
    ("Discord rejected the vote.", "Discord отклонил голос."),
    (
        "Could not reach Discord; the poll was not changed.",
        "Не удалось связаться с Discord; опрос не изменён.",
    ),
    (
        "Discord did not confirm the change; it may have been applied. Check the poll before trying again.",
        "Discord не подтвердил изменение; оно могло примениться. Проверьте опрос, прежде чем повторять.",
    ),
    (
        "The poll could not be changed.",
        "Не удалось изменить опрос.",
    ),
    (
        "Discord did not allow listing the voters.",
        "Discord не разрешил показать проголосовавших.",
    ),
    (
        "Could not reach Discord to list the voters.",
        "Не удалось связаться с Discord, чтобы показать проголосовавших.",
    ),
    (
        "The voters could not be loaded.",
        "Не удалось загрузить список проголосовавших.",
    ),
    // poll_create
    ("1 hour", "1 час"),
    ("4 hours", "4 часа"),
    ("8 hours", "8 часов"),
    ("24 hours", "24 часа"),
    ("3 days", "3 дня"),
    ("1 week", "1 неделя"),
    ("2 weeks", "2 недели"),
    (
        "The poll may have been created. Check the conversation; pressing Create again will not make a second copy of the same poll.",
        "Опрос мог быть создан. Проверьте беседу; повторное нажатие «Создать» не создаст второй копии этого опроса.",
    ),
    (
        "Discord did not create the poll. You may lack the permission to create polls here.",
        "Discord не создал опрос. Возможно, у вас нет прав создавать опросы здесь.",
    ),
    ("Create a poll", "Создать опрос"),
    ("Question", "Вопрос"),
    ("Answer {}", "Ответ {}"),
    ("Open for", "Открыт на"),
    ("Add answer", "Добавить ответ"),
    ("Allow several answers", "Несколько ответов"),
    ("Sending…", "Отправка…"),
    // component_view
    ("Unsupported component", "Неподдерживаемый компонент"),
    ("Button", "Кнопка"),
    (
        "{} (not available in this client)",
        "{} (недоступно в этом клиенте)",
    ),
    ("[link] {}", "[ссылка] {}"),
    ("Select", "Выбор"),
    ("needs several values", "нужно несколько значений"),
    ("users", "пользователи"),
    ("roles", "роли"),
    ("users and roles", "пользователи и роли"),
    ("channels", "каналы"),
    ("unknown", "неизвестно"),
    (
        "{} ({}: not available in this client)",
        "{} ({}: недоступно в этом клиенте)",
    ),
    // interactions
    (
        "The application did not respond.",
        "Приложение не ответило.",
    ),
    (
        "The application could not handle that.",
        "Приложение не смогло это обработать.",
    ),
    (
        "That is still waiting for the application.",
        "Приложение ещё не ответило на это.",
    ),
    (
        "A command is still waiting for the application in this channel.",
        "В этом канале команда всё ещё ждёт ответа приложения.",
    ),
    (
        "Too many requests are waiting for applications. Try again in a moment.",
        "Слишком много запросов ждут ответа приложений. Повторите чуть позже.",
    ),
    (
        "Not connected yet; try again in a moment.",
        "Соединение ещё не установлено; повторите чуть позже.",
    ),
    (
        "You cannot use applications in this channel.",
        "Вы не можете использовать приложения в этом канале.",
    ),
    (
        "That control is no longer on the message.",
        "Этого элемента больше нет в сообщении.",
    ),
    ("That control is disabled.", "Этот элемент отключён."),
    (
        "Not available in this client.",
        "Недоступно в этом клиенте.",
    ),
    (
        "That choice is not offered by the select.",
        "Такого варианта нет в списке.",
    ),
    (
        "That form is no longer open.",
        "Эта форма больше не открыта.",
    ),
    (
        "Only messages meant for you can be dismissed.",
        "Скрыть можно только сообщения, видимые лишь вам.",
    ),
    (
        "Discord did not allow that. You may lack the permission.",
        "Discord этого не разрешил. Возможно, у вас нет прав.",
    ),
    (
        "That message, command or application is no longer available.",
        "Это сообщение, команда или приложение больше недоступны.",
    ),
    (
        "Discord rejected the request. The control may be out of date.",
        "Discord отклонил запрос. Возможно, элемент устарел.",
    ),
    ("Discord rejected the request.", "Discord отклонил запрос."),
    (
        "Could not reach Discord; nothing was sent.",
        "Не удалось связаться с Discord; ничего не отправлено.",
    ),
    (
        "Discord did not confirm the request; it may have been received. Check the conversation before trying again.",
        "Discord не подтвердил запрос; он мог быть получен. Проверьте беседу, прежде чем повторять.",
    ),
    (
        "The request could not be sent.",
        "Не удалось отправить запрос.",
    ),
    (
        "A required option is empty.",
        "Обязательный параметр не заполнен.",
    ),
    (
        "An option needs a whole number.",
        "Параметр должен быть целым числом.",
    ),
    ("An option needs a number.", "Параметр должен быть числом."),
    (
        "An option needs yes or no.",
        "Параметр должен быть «да» или «нет».",
    ),
    (
        "A user, channel or role option needs an ID or a mention.",
        "Параметр пользователя, канала или роли требует ID или упоминания.",
    ),
    // slash
    (
        "The command list could not be loaded.",
        "Не удалось загрузить список команд.",
    ),
    (
        "Not sent: the connection is busy. Try again.",
        "Не отправлено: соединение занято. Повторите.",
    ),
    (
        "Fill in every option marked with *.",
        "Заполните все параметры, отмеченные *.",
    ),
    ("Choose a subcommand first.", "Сначала выберите подкоманду."),
    (
        "That command is no longer available here.",
        "Эта команда здесь больше недоступна.",
    ),
    ("Waiting for /{}…", "Ожидание ответа на /{}…"),
    (
        "Commands (pick one, or send the text as it is)",
        "Команды (выберите или отправьте текст как есть)",
    ),
    ("Loading commands…", "Загрузка команд…"),
    ("No matching command.", "Подходящих команд нет."),
    ("ID or mention", "ID или упоминание"),
    ("Run", "Выполнить"),
    ("Submit", "Отправить"),
    // pins
    (
        "You cannot see or change pins here.",
        "Вы не можете просматривать или менять закреплённые сообщения здесь.",
    ),
    (
        "That message or conversation is gone.",
        "Этого сообщения или беседы больше нет.",
    ),
    (
        "Pinned messages could not be read.",
        "Не удалось прочитать закреплённые сообщения.",
    ),
    (
        "Loading pinned messages…",
        "Загрузка закреплённых сообщений…",
    ),
    ("No pinned messages.", "Закреплённых сообщений нет."),
    ("{} pinned", "Закреплено: {}"),
    (
        " [{} attachments]",
        " [{} вложение]| [{} вложения]| [{} вложений]",
    ),
    ("Pinned messages", "Закреплённые сообщения"),
    // search
    ("Any content", "Любое содержимое"),
    ("Has image", "С изображением"),
    ("Has file", "С файлом"),
    ("Has link", "Со ссылкой"),
    ("Has other", "С другим содержимым"),
    ("You cannot search here.", "Вы не можете искать здесь."),
    (
        "This conversation is no longer available.",
        "Эта беседа больше недоступна.",
    ),
    (
        "Discord is rate limiting searches. Try again shortly.",
        "Discord ограничивает частоту поиска. Повторите чуть позже.",
    ),
    (
        "Search failed on Discord's side. Try again.",
        "Поиск не удался на стороне Discord. Повторите.",
    ),
    (
        "Search results could not be read.",
        "Не удалось прочитать результаты поиска.",
    ),
    ("Type something to search for.", "Введите, что искать."),
    (
        "Discord is still indexing this conversation.",
        "Discord всё ещё индексирует эту беседу.",
    ),
    ("Search messages", "Поиск по сообщениям"),
    ("From me", "От меня"),
    ("Whole server", "Весь сервер"),
    ("Clear", "Очистить"),
    ("Close search", "Закрыть поиск"),
    ("Searching…", "Поиск…"),
    (
        "Discord is indexing this conversation; retrying ({}/{})…",
        "Discord индексирует эту беседу; повтор ({}/{})…",
    ),
    ("No messages found.", "Сообщения не найдены."),
    ("{} of {} results", "Результаты: {} из {}"),
    // sticker
    (
        "Discord did not let this account list stickers here.",
        "Discord не позволил этой учётной записи просматривать стикеры здесь.",
    ),
    (
        "This server's stickers are no longer available.",
        "Стикеры этого сервера больше недоступны.",
    ),
    (
        "Discord is rate limiting this request. Try again shortly.",
        "Discord ограничивает частоту запросов. Повторите чуть позже.",
    ),
    (
        "Stickers could not be loaded. Try again.",
        "Не удалось загрузить стикеры. Повторите.",
    ),
    (
        "The sticker list could not be read.",
        "Не удалось прочитать список стикеров.",
    ),
    (
        "The sticker may have been delivered. Check the conversation before sending again.",
        "Стикер мог быть доставлен. Проверьте беседу, прежде чем отправлять снова.",
    ),
    (
        "Discord did not send the sticker: it may be unavailable, or you may lack permission here.",
        "Discord не отправил стикер: он может быть недоступен, или у вас нет прав здесь.",
    ),
    ("Stickers of this server", "Стикеры этого сервера"),
    (
        "Stickers from other servers and packs need Nitro; open a server channel.",
        "Стикеры с других серверов и наборы требуют Nitro; откройте канал сервера.",
    ),
    (
        "This server has no stickers.",
        "На этом сервере нет стикеров.",
    ),
    (
        "Pick a sticker to send it.",
        "Выберите стикер, чтобы отправить его.",
    ),
    ("(not shown)", "(не показан)"),
    ("{} (unavailable)", "{} (недоступен)"),
    // gif
    (
        "Discord refused the GIF search.",
        "Discord отклонил поиск GIF.",
    ),
    (
        "Discord has no GIF search for this account.",
        "Для этой учётной записи в Discord нет поиска GIF.",
    ),
    (
        "Discord is rate limiting GIF searches. Try again shortly.",
        "Discord ограничивает частоту поиска GIF. Повторите чуть позже.",
    ),
    (
        "GIF search failed. Try again.",
        "Поиск GIF не удался. Повторите.",
    ),
    (
        "GIF results could not be read.",
        "Не удалось прочитать результаты поиска GIF.",
    ),
    (
        "Search GIFs (empty for trending)",
        "Поиск GIF (пусто — популярные)",
    ),
    ("No GIFs found.", "GIF не найдены."),
    (
        "Pick a GIF to add its link to your message.",
        "Выберите GIF, чтобы добавить ссылку на него в сообщение.",
    ),
    // emoji
    ("Emoji is unavailable.", "Эмодзи недоступно."),
    (
        "Custom emoji could not be loaded; its original token is preserved.",
        "Не удалось загрузить эмодзи сервера; исходный текст сохранён.",
    ),
    (
        "Emoji could not be decoded; original text is preserved.",
        "Не удалось декодировать эмодзи; исходный текст сохранён.",
    ),
    // emoji_picker
    ("Smileys & Emotion", "Смайлики и эмоции"),
    ("People & Body", "Люди и тело"),
    ("Animals & Nature", "Животные и природа"),
    ("Food & Drink", "Еда и напитки"),
    ("Travel & Places", "Путешествия и места"),
    ("Activities", "Занятия"),
    ("Objects", "Предметы"),
    ("Symbols", "Символы"),
    ("Flags", "Флаги"),
    ("All emoji", "Все эмодзи"),
    ("Recent", "Недавние"),
    ("Server emoji", "Эмодзи сервера"),
    ("Unicode variants", "Варианты Unicode"),
    ("Skin tones", "Оттенки кожи"),
    (
        "Hover an emoji for its name.",
        "Наведите на эмодзи, чтобы увидеть название.",
    ),
    ("React with emoji", "Отреагировать эмодзи"),
    ("Emoji", "Эмодзи"),
    (
        "Search names, aliases or emoji",
        "Поиск по названиям, псевдонимам или эмодзи",
    ),
    (
        "No available emoji match this search.",
        "Подходящих доступных эмодзи нет.",
    ),
    (
        "Animated and external emoji may require Nitro. Account entitlements are not assumed; a reaction Discord rejects is reported above the message box.",
        "Анимированные и внешние эмодзи могут требовать Nitro. Наличие подписки не предполагается; реакцию, отклонённую Discord, покажет сообщение над полем ввода.",
    ),
    (
        "Animated and external emoji may require Nitro. Account entitlements are not assumed; Discord rejections appear beside unsent messages.",
        "Анимированные и внешние эмодзи могут требовать Nitro. Наличие подписки не предполагается; отказы Discord показываются рядом с неотправленными сообщениями.",
    ),
    (
        "Twemoji 17.0.3 · Unicode 16.0 · Attribution (hover)",
        "Twemoji 17.0.3 · Unicode 16.0 · Авторство (наведите)",
    ),
    // localtime
    ("{} {} {}", "{} {} {} г."),
    ("January", "января"),
    ("February", "февраля"),
    ("March", "марта"),
    ("April", "апреля"),
    ("May", "мая"),
    ("June", "июня"),
    ("July", "июля"),
    ("August", "августа"),
    ("September", "сентября"),
    ("October", "октября"),
    ("November", "ноября"),
    ("December", "декабря"),
    ("Monday", "понедельник"),
    ("Tuesday", "вторник"),
    ("Wednesday", "среда"),
    ("Thursday", "четверг"),
    ("Friday", "пятница"),
    ("Saturday", "суббота"),
    ("Sunday", "воскресенье"),
    ("a few seconds", "несколько секунд"),
    ("a minute", "минуту"),
    ("{} minutes", "{} минуту|{} минуты|{} минут"),
    ("an hour", "час"),
    ("{} hours", "{} час|{} часа|{} часов"),
    ("a day", "день"),
    ("{} days", "{} день|{} дня|{} дней"),
    ("a month", "месяц"),
    ("{} months", "{} месяц|{} месяца|{} месяцев"),
    ("a year", "год"),
    ("{} years", "{} год|{} года|{} лет"),
    ("in {}", "через {}"),
    ("{} ago", "{} назад"),
    // timeline
    ("Loading messages...", "Загрузка сообщений…"),
    (
        "There are no messages here yet.",
        "Здесь пока нет сообщений.",
    ),
    (
        "Select a channel to read its messages.",
        "Выберите канал, чтобы читать сообщения.",
    ),
    ("Load older messages", "Загрузить более ранние сообщения"),
    ("Deleting…", "Удаление…"),
    (
        "Delete {} messages? This cannot be undone.",
        "Удалить {} сообщение? Это нельзя отменить.|Удалить {} сообщения? Это нельзя отменить.|Удалить {} сообщений? Это нельзя отменить.",
    ),
    (
        "{} selected (at most 100, under 14 days old)",
        "Выбрано: {} (не более 100, не старше 14 дней)",
    ),
    ("Jump to latest", "К последним сообщениям"),
    ("Download", "Скачать"),
    ("Load preview", "Загрузить превью"),
    ("Open thread", "Открыть ветку"),
    ("Dismiss", "Скрыть"),
    ("View", "Просмотреть"),
    ("Retry preview", "Повторить загрузку превью"),
    ("End poll", "Завершить опрос"),
    ("Publish", "Опубликовать"),
    (
        "Clear all of this emoji",
        "Удалить все реакции с этим эмодзи",
    ),
    ("React", "Отреагировать"),
    ("Pin", "Закрепить"),
    ("Unpin", "Открепить"),
    ("Forward", "Переслать"),
    ("Thread", "Ветка"),
    ("Copy text", "Копировать текст"),
    ("Copy link", "Копировать ссылку"),
    ("Show embeds", "Показать вставки"),
    ("Hide embeds", "Скрыть вставки"),
    ("Report", "Пожаловаться"),
    ("Read aloud", "Прочитать вслух"),
    ("Stop reading", "Остановить чтение"),
    (
        "Click to add or remove your reaction. Right-click to see who reacted.",
        "Нажмите, чтобы добавить или убрать свою реакцию. Правая кнопка — кто отреагировал.",
    ),
    ("Add reaction", "Добавить реакцию"),
    ("emoji", "эмодзи"),
    ("a deleted emoji", "удалённое эмодзи"),
    ("an unknown emoji", "неизвестное эмодзи"),
    ("Loading voters…", "Загрузка проголосовавших…"),
    ("Loading who reacted…", "Загрузка отреагировавших…"),
    ("and others", "и другие"),
    ("Waiting for Discord…", "Ожидание Discord…"),
    (
        "Remove {}'s reaction? This cannot be undone.",
        "Удалить реакцию пользователя {}? Это нельзя отменить.",
    ),
    (
        "Remove this user's reaction? This cannot be undone.",
        "Удалить реакцию этого пользователя? Это нельзя отменить.",
    ),
    (
        "Remove every reaction? This cannot be undone.",
        "Удалить все реакции? Это нельзя отменить.",
    ),
    ("No one is listed.", "Список пуст."),
    ("Voted for {}", "Проголосовали за «{}»"),
    ("Reacted with {}", "Отреагировали: {}"),
    (
        "Message from an ignored user hidden.",
        "Сообщение игнорируемого пользователя скрыто.",
    ),
    (
        "Message from a blocked user hidden.",
        "Сообщение заблокированного пользователя скрыто.",
    ),
    ("Reply target", "Цель ответа"),
    ("Jumped here", "Переход сюда"),
    ("(edited)", "(изменено)"),
    ("Published", "Опубликовано"),
    ("Replying to {}: {}", "Ответ {}: {}"),
    (
        "Original message was deleted.",
        "Исходное сообщение удалено.",
    ),
    (
        "Loading the original message…",
        "Загрузка исходного сообщения…",
    ),
    (
        "Original message unavailable.",
        "Исходное сообщение недоступно.",
    ),
    (
        "Replying to a hidden message.",
        "Ответ на скрытое сообщение.",
    ),
    ("Published from {}", "Опубликовано из: {}"),
    ("Only you can see this", "Это видите только вы"),
    ("Saving your edit…", "Сохранение изменений…"),
    (
        "Editing in the message box below. Enter saves, Escape cancels.",
        "Изменение в поле ввода ниже. Enter — сохранить, Esc — отменить.",
    ),
    (
        "Delete this message? This cannot be undone.",
        "Удалить это сообщение? Это нельзя отменить.",
    ),
    (
        "Publish to every server following this channel?",
        "Опубликовать на всех серверах, подписанных на этот канал?",
    ),
    ("Final results", "Итоги"),
    (
        "Select one or more answers",
        "Выберите один или несколько ответов",
    ),
    ("Select one answer", "Выберите один ответ"),
    ("Voters", "Голоса"),
    ("Poll ended", "Опрос завершён"),
    ("Ends {}", "Завершится {}"),
    ("No end time", "Без срока"),
    ("{} · {}", "{} · {}"),
    ("{} votes", "{} голос|{} голоса|{} голосов"),
    ("Saving…", "Сохранение…"),
    ("Forwarded", "Переслано"),
    (
        "The forwarded message has no content to show.",
        "В пересланном сообщении нет содержимого.",
    ),
    (
        "Sticker: {} (this sticker format is not shown)",
        "Стикер: {} (этот формат стикеров не показывается)",
    ),
    ("Sticker: {}", "Стикер: {}"),
    ("Loading image preview…", "Загрузка превью изображения…"),
    ("No text content.", "Нет текста."),
    (
        "A member was added to the conversation.",
        "Участник добавлен в беседу.",
    ),
    (
        "A member left the conversation.",
        "Участник покинул беседу.",
    ),
    (
        "The conversation name was changed.",
        "Название беседы изменено.",
    ),
    (
        "The conversation icon was changed.",
        "Значок беседы изменён.",
    ),
    ("A message was pinned.", "Сообщение закреплено."),
    ("A member joined.", "Участник присоединился."),
    ("The server was boosted.", "Сервер получил буст."),
    ("A channel was followed.", "Оформлена подписка на канал."),
    ("A thread was created.", "Создана ветка."),
    (
        "The message this thread was started from.",
        "Сообщение, с которого началась ветка.",
    ),
    (
        "An invite reminder was posted.",
        "Опубликовано напоминание о приглашении.",
    ),
    ("A poll has ended.", "Опрос завершён."),
    (
        "Message type {} is not displayed by this client.",
        "Сообщение типа {} не отображается в этом клиенте.",
    ),
    (
        "AutoMod flagged a message from {} (rule: {}).",
        "AutoMod пометил сообщение от {} (правило: {}).",
    ),
    (
        "AutoMod flagged a message from {}.",
        "AutoMod пометил сообщение от {}.",
    ),
    ("{} started a call.", "{} начал(а) звонок."),
    ("Missed call from {}.", "Пропущенный звонок от {}."),
    (
        "{} called. The call has ended.",
        "{} звонил(а). Звонок завершён.",
    ),
    ("Attachment: {} ({})", "Вложение: {} ({})"),
    ("{} B", "{} Б"),
    ("KB", "КБ"),
    ("MB", "МБ"),
    ("GB", "ГБ"),
    ("TB", "ТБ"),
];
