//! Account, profile, privacy, friends, reports, status and notifications.

pub const ENTRIES: &[(&str, &str)] = &[
    // account_panel, accounts
    (
        "Discord wants multi-factor or password re-authentication for this (code {}: {}). fastcord does not answer it. Confirm in the official client.",
        "Discord требует многофакторное подтверждение или повторный ввод пароля (код {}: {}). fastcord на это не отвечает. Подтвердите в официальном клиенте.",
    ),
    (
        "Discord did not confirm. The session may have been logged out; reload the list before trying again.",
        "Discord не подтвердил действие. Сеанс мог быть завершён; обновите список, прежде чем повторять.",
    ),
    (
        "Discord refused this. Confirm in the official client.",
        "Discord отклонил запрос. Подтвердите в официальном клиенте.",
    ),
    (
        "Discord did not find that session list or session. Reload the list.",
        "Discord не нашёл список сеансов или сеанс. Обновите список.",
    ),
    ("That session was logged out.", "Сеанс завершён."),
    ("unknown", "неизвестно"),
    ("on", "включена"),
    ("off", "выключена"),
    ("verified", "подтверждена"),
    ("not verified", "не подтверждена"),
    ("added", "добавлен"),
    ("none", "нет"),
    ("Unknown client", "Неизвестный клиент"),
    ("last used {}", "последний вход {}"),
    ("Saved accounts", "Сохранённые аккаунты"),
    ("Active", "Активна"),
    ("Log out of fastcord", "Выйти из fastcord"),
    ("Switch", "Переключиться"),
    (
        "Switching logs this account out of fastcord first; only one account is ever connected.",
        "При переключении fastcord сначала выходит из этой учётной записи: подключена всегда только одна.",
    ),
    (
        "This session is memory-only; switching would lose it, so Switch is off.",
        "Этот сеанс хранится только в памяти: при переключении он будет потерян, поэтому переключение отключено.",
    ),
    ("Security", "Безопасность"),
    (
        "Two-factor authentication: {}",
        "Двухфакторная проверка: {}",
    ),
    ("Email: {}", "Электронная почта: {}"),
    ("Phone number: {}", "Номер телефона: {}"),
    (
        "Changing the password, two-factor authentication, passkeys and account recovery is done in the official client or Discord's website, not here.",
        "Пароль, двухфакторная проверка, ключи доступа и восстановление учётной записи меняются в официальном клиенте или на сайте Discord, но не здесь.",
    ),
    (
        "Open account settings in the browser",
        "Открыть настройки учётной записи в браузере",
    ),
    ("Authorized apps", "Авторизованные приложения"),
    ("Sessions", "Сеансы"),
    ("Reload", "Обновить"),
    ("Log out", "Выйти"),
    (
        "Discord listed no sessions.",
        "Discord не показал ни одного сеанса.",
    ),
    (
        "One of these is probably fastcord; logging it out ends this login.",
        "Один из них, вероятно, fastcord: если завершить его, этот вход закончится.",
    ),
    ("Log that session out?", "Завершить этот сеанс?"),
    (
        "Forget the saved login for {}? Its token is deleted from this computer; the account stays logged in elsewhere.",
        "Забыть сохранённый вход для {}? Токен будет удалён с этого компьютера; на других устройствах учётная запись останется в сети.",
    ),
    ("Accounts and security", "Аккаунты и безопасность"),
    ("Account {}", "Аккаунт {}"),
    // profile_edit
    (
        "That text is longer than Discord allows.",
        "Текст длиннее, чем позволяет Discord.",
    ),
    (
        "Choose a PNG or JPEG image (animated avatars need Nitro).",
        "Выберите изображение PNG или JPEG (анимированные аватары доступны только с Nitro).",
    ),
    (
        "The image is larger than 512 KiB.",
        "Изображение больше 512 КиБ.",
    ),
    (
        "That connection cannot be changed from here.",
        "Это подключение нельзя изменить здесь.",
    ),
    (
        "Your session has expired. Log in again.",
        "Сеанс истёк. Войдите снова.",
    ),
    (
        "Discord refused: you may not be allowed to change your nickname in this server.",
        "Discord отклонил запрос: возможно, вам нельзя менять ник на этом сервере.",
    ),
    (
        "Discord refused this change for this account.",
        "Discord отклонил это изменение для данной учётной записи.",
    ),
    (
        "Discord could not find that server or connection.",
        "Discord не нашёл этот сервер или подключение.",
    ),
    (
        "Discord is rate limiting requests. Wait a moment and try again.",
        "Discord ограничивает частоту запросов. Подождите немного и повторите.",
    ),
    (
        "Discord rejected the value. Names are limited in how often they change, and some changes need verification (password, two-factor or CAPTCHA) that only the official Discord client can complete. What you typed is kept.",
        "Discord отклонил значение. Имена можно менять не чаще определённого срока, а для некоторых изменений нужна проверка (пароль, двухфакторная проверка или капча), которую пройти можно только в официальном клиенте Discord. Введённый текст сохранён.",
    ),
    (
        "That image is too large to send.",
        "Изображение слишком велико для отправки.",
    ),
    (
        "Discord answered with something unexpected. The change may have been applied; reopen this panel to check.",
        "Discord ответил что-то неожиданное. Изменение могло примениться; откройте эту панель заново, чтобы проверить.",
    ),
    (
        "Discord did not confirm. The change may have been applied; reopen this panel to check before trying again. What you typed is kept.",
        "Discord не подтвердил действие. Изменение могло примениться; откройте эту панель заново и проверьте, прежде чем повторять. Введённый текст сохранён.",
    ),
    ("The request failed: {}.", "Запрос не выполнен: {}."),
    (
        "Wait for the previous change to finish.",
        "Дождитесь завершения предыдущего изменения.",
    ),
    (
        "The display name is unchanged.",
        "Отображаемое имя не изменилось.",
    ),
    (
        "A display name has at most {} characters.",
        "Отображаемое имя может содержать не более {} символов.",
    ),
    ("Avatar", "Аватар"),
    ("Image", "Изображение"),
    (
        "The image could not be read.",
        "Не удалось прочитать изображение.",
    ),
    (
        "Your current bio could not be read, so it is not edited.",
        "Не удалось прочитать текущее «О себе», поэтому оно не изменяется.",
    ),
    (
        "Bio and pronouns are unchanged.",
        "«О себе» и местоимения не изменились.",
    ),
    ("Select a server first.", "Сначала выберите сервер."),
    ("The nickname is unchanged.", "Ник не изменился."),
    ("Saved.", "Сохранено."),
    ("No connections.", "Нет подключений."),
    (
        "Sent. It shows here when Discord confirms the change.",
        "Отправлено. Изменение появится здесь, когда Discord его подтвердит.",
    ),
    ("Your profile", "Ваш профиль"),
    ("Username: @{}", "Имя пользователя: @{}"),
    (
        "Changing your username needs your password (and two-factor code), so do it in the official Discord client.",
        "Для смены имени пользователя нужны пароль (и код двухфакторной проверки), поэтому сделайте это в официальном клиенте Discord.",
    ),
    (
        "Display name (empty shows your username)",
        "Отображаемое имя (если пусто, показывается имя пользователя)",
    ),
    ("Avatar: set", "Аватар: задан"),
    ("Avatar: none", "Аватар: нет"),
    ("Choose avatar", "Выбрать аватар"),
    ("Remove avatar", "Убрать аватар"),
    ("Pronouns", "Местоимения"),
    ("About me", "О себе"),
    (
        "Your current bio and pronouns could not be read; close and reopen to try again.",
        "Не удалось прочитать «О себе» и местоимения; закройте панель и откройте заново.",
    ),
    ("Nickname in {}", "Ник на сервере {}"),
    (
        "Nickname (empty removes it)",
        "Ник (если пусто, будет убран)",
    ),
    (
        "Open a server to change your nickname there.",
        "Откройте сервер, чтобы изменить там свой ник.",
    ),
    ("Connections on your profile", "Подключения в вашем профиле"),
    ("Load", "Загрузить"),
    ("Shown", "Показано"),
    ("Hidden", "Скрыто"),
    ("Activity shown", "Активность показана"),
    ("Activity hidden", "Активность скрыта"),
    ("Waiting for Discord…", "Ожидание Discord…"),
    // safety
    (
        "That change is not one this client can send.",
        "Этот клиент не может отправить такое изменение.",
    ),
    (
        "Discord refused this for this account.",
        "Discord отклонил это действие для данной учётной записи.",
    ),
    (
        "That request is already gone; the list follows Discord.",
        "Этого запроса уже нет; список обновится по данным Discord.",
    ),
    ("Discord could not find that.", "Discord не нашёл это."),
    (
        "Discord rejected the change, or it needs verification only the official client can complete.",
        "Discord отклонил изменение, либо нужна проверка, которую можно пройти только в официальном клиенте.",
    ),
    (
        "Discord answered with something unexpected, so these settings cannot be edited here.",
        "Discord ответил что-то неожиданное, поэтому эти настройки нельзя изменить здесь.",
    ),
    (
        "Discord answered with something unexpected. The change may have been applied; reload to check.",
        "Discord ответил что-то неожиданное. Изменение могло примениться; обновите, чтобы проверить.",
    ),
    (
        "Could not reach Discord. Try Load again.",
        "Не удалось связаться с Discord. Нажмите «Загрузить» ещё раз.",
    ),
    (
        "Discord did not confirm. The change may have been applied; reload or check the list before trying again.",
        "Discord не подтвердил действие. Изменение могло примениться; обновите или проверьте список, прежде чем повторять.",
    ),
    ("Hidden message", "Скрытое сообщение"),
    ("(no text)", "(без текста)"),
    ("(attachment)", "(вложение)"),
    (
        "That request is no longer pending.",
        "Этот запрос больше не ожидает ответа.",
    ),
    ("Nothing changed.", "Ничего не изменилось."),
    (
        "Accepted. The conversation moves to your messages when Discord confirms.",
        "Принято. Переписка появится в ваших сообщениях, когда Discord подтвердит.",
    ),
    (
        "Declined. It leaves the list when Discord confirms.",
        "Отклонено. Запрос исчезнет из списка, когда Discord подтвердит.",
    ),
    (
        "Sent. The list changes when Discord confirms.",
        "Отправлено. Список изменится, когда Discord подтвердит.",
    ),
    ("User {}", "Пользователь {}"),
    ("Safety", "Безопасность"),
    ("Requests", "Запросы"),
    ("Ignored", "Игнорируемые"),
    ("Privacy", "Конфиденциальность"),
    (
        "Previewing a request does not accept it or mark it read. Only Accept lets this person message you normally.",
        "Просмотр запроса не принимает его и не отмечает прочитанным. Только «Принять» разрешает этому человеку писать вам как обычно.",
    ),
    ("No message requests.", "Нет запросов на переписку."),
    ("{} (spam)", "{} (спам)"),
    ("Show preview", "Показать просмотр"),
    ("Accept", "Принять"),
    ("Yes, decline", "Да, отклонить"),
    ("Keep", "Оставить"),
    ("Decline", "Отклонить"),
    ("Hide preview", "Скрыть просмотр"),
    ("Loading preview…", "Загрузка просмотра…"),
    ("No messages.", "Нет сообщений."),
    (" ({} not listed)", " (не показано: {})"),
    (
        "Page {} of {}, {} requests{}",
        "Страница {} из {}, запросов: {}{}",
    ),
    ("Previous", "Назад"),
    ("Next", "Далее"),
    ("Stop ignoring", "Перестать игнорировать"),
    (
        "You are not ignoring anyone. Use Ignore on a profile card. Ignoring hides messages, notifications and calls without blocking.",
        "Вы никого не игнорируете. Используйте «Игнорировать» в карточке профиля. Игнорирование скрывает сообщения, уведомления и звонки, но не блокирует человека.",
    ),
    (
        "{} ignored users. Showing {}.",
        "Игнорируемых пользователей: {}. Показано: {}.",
    ),
    ("{} ignored.", "Игнорируется: {}."),
    (
        "Settings are read only when you ask.",
        "Настройки считываются только по вашему запросу.",
    ),
    (
        "Not available for this account.",
        "Недоступно для этой учётной записи.",
    ),
    (
        "Who can send you a friend request",
        "Кто может отправлять вам запросы в друзья",
    ),
    ("Everyone", "Все"),
    ("Friends of friends", "Друзья друзей"),
    ("Server members", "Участники серверов"),
    (
        "Direct messages from server members",
        "Личные сообщения от участников серверов",
    ),
    ("Allowed in new servers", "Разрешены на новых серверах"),
    ("Allowed from {}", "Разрешены с сервера {}"),
    (
        "Open a server to change its override.",
        "Откройте сервер, чтобы изменить его настройку.",
    ),
    (
        "Scan direct messages for explicit images",
        "Проверять личные сообщения на откровенные изображения",
    ),
    ("From non-friends", "От не друзей"),
    ("From everyone", "От всех"),
    ("Save changes", "Сохранить изменения"),
    (
        "Only the fields you changed are sent.",
        "Отправляются только изменённые поля.",
    ),
    // report
    (
        "That report could not be built from Discord's menu. Nothing was sent.",
        "Не удалось составить жалобу из меню Discord. Ничего не отправлено.",
    ),
    ("Nothing was submitted.", "Ничего не отправлено."),
    (
        "Discord refused this report for this account; it may need a verification or age check only the official client can complete.",
        "Discord отклонил эту жалобу для данной учётной записи: возможно, нужна проверка или подтверждение возраста, которые можно пройти только в официальном клиенте.",
    ),
    (
        "Discord can no longer find it; it was deleted or you can no longer see it.",
        "Discord больше не находит объект: он удалён или вам больше не виден.",
    ),
    (
        "Discord is rate limiting requests. Try again later.",
        "Discord ограничивает частоту запросов. Повторите позже.",
    ),
    (
        "Discord rejected the report, or asks for a challenge or verification only the official client can complete.",
        "Discord отклонил жалобу либо требует проверку, которую можно пройти только в официальном клиенте.",
    ),
    (
        "Discord's report menu has a shape this client does not understand.",
        "Меню жалоб Discord имеет вид, который этот клиент не понимает.",
    ),
    (
        "The report could not be built.",
        "Не удалось составить жалобу.",
    ),
    (
        "Could not reach Discord to load the report menu. Nothing was submitted.",
        "Не удалось связаться с Discord, чтобы загрузить меню жалоб. Ничего не отправлено.",
    ),
    (
        "The report menu could not be loaded. Nothing was submitted.",
        "Не удалось загрузить меню жалоб. Ничего не отправлено.",
    ),
    (
        "Discord did not confirm. The report may have been submitted. It is not sent again automatically; check with Discord before reporting again.",
        "Discord не подтвердил отправку. Жалоба могла быть отправлена. Автоматически она не повторяется; прежде чем жаловаться снова, проверьте это в Discord.",
    ),
    (
        "This category asks for details this client cannot collect. Use Discord's web form.",
        "Для этой категории нужны сведения, которые этот клиент не может собрать. Используйте веб-форму Discord.",
    ),
    (
        "Requested. The result appears under Safety (Ignored) or Friends (Blocked).",
        "Запрос отправлен. Результат появится в разделе «Безопасность» (игнорируемые) или «Друзья» (заблокированные).",
    ),
    ("message", "сообщение"),
    ("voice message", "голосовое сообщение"),
    ("user", "пользователь"),
    ("server", "сервер"),
    ("Report {}", "Жалоба: {}"),
    (
        "Reports go to Discord's Trust & Safety. Nothing is sent until you press Submit on the review step.",
        "Жалобы попадают в службу доверия и безопасности Discord. Ничего не отправляется, пока вы не нажмёте «Отправить» на шаге проверки.",
    ),
    (
        "Loading Discord's report menu…",
        "Загрузка меню жалоб Discord…",
    ),
    ("{} (web form)", "{} (веб-форма)"),
    ("Review before sending", "Проверьте перед отправкой"),
    ("Reporting: {} by/of {}", "Жалоба на: {} — {}"),
    ("Reason: {}", "Причина: {}"),
    (
        "Discord receives the {}'s identity, your chosen reason and your account. Discord attaches the {} itself; it is not copied from this window. The report is sent once and never retried automatically.",
        "Discord получает сведения об объекте жалобы ({}), выбранную причину и вашу учётную запись. Само содержимое ({}) Discord берёт у себя, из этого окна оно не копируется. Жалоба отправляется один раз и автоматически не повторяется.",
    ),
    ("Submit report", "Отправить жалобу"),
    (
        "Sending… waiting for Discord.",
        "Отправка… ожидание ответа Discord.",
    ),
    (
        "Discord received the report (reference {}).",
        "Discord получил жалобу (номер {}).",
    ),
    ("Discord received the report.", "Discord получил жалобу."),
    (
        "You can also stop seeing {}. These are separate requests.",
        "Вы также можете перестать видеть {}. Это отдельные запросы.",
    ),
    ("Ignore", "Игнорировать"),
    ("Block", "Заблокировать"),
    ("Open Discord's web form", "Открыть веб-форму Discord"),
    ("Copy message link", "Копировать ссылку на сообщение"),
    (
        "That message is no longer there (deleted, or no longer loaded). Nothing was submitted. Open the report again from the message if it is still visible.",
        "Этого сообщения больше нет (оно удалено или не загружено). Ничего не отправлено. Если сообщение ещё видно, откройте жалобу на него заново.",
    ),
    // friends
    (
        "Discord requires an account check. Requests are paused. Open the official Discord client, complete its verification, then reconnect fastcord.",
        "Discord требует проверку учётной записи. Запросы приостановлены. Откройте официальный клиент Discord, пройдите проверку и снова подключите fastcord.",
    ),
    ("{}. Try again later.", "{}. Повторите позже."),
    (
        "Discord refused ({}{}): {}",
        "Discord отклонил запрос ({}{}): {}",
    ),
    (", code {}", ", код {}"),
    (
        "Discord refused: the user may have blocked you or may not accept friend requests.",
        "Discord отклонил запрос: пользователь мог заблокировать вас или не принимает запросы в друзья.",
    ),
    (
        "Discord could not find that user.",
        "Discord не нашёл этого пользователя.",
    ),
    (
        "Discord rejected the request. Check the username and try again.",
        "Discord отклонил запрос. Проверьте имя пользователя и повторите.",
    ),
    (
        "Could not reach Discord. Check the list before trying again.",
        "Не удалось связаться с Discord. Проверьте список, прежде чем повторять.",
    ),
    (
        "Discord did not answer. Check the list before trying again.",
        "Discord не ответил. Проверьте список, прежде чем повторять.",
    ),
    (
        "Enter a username to send a friend request.",
        "Введите имя пользователя, чтобы отправить запрос в друзья.",
    ),
    (
        "That is not a valid username (2 to 32 characters, no spaces).",
        "Недопустимое имя пользователя (от 2 до 32 символов, без пробелов).",
    ),
    (
        "Too many requests are still being sent. Wait a moment.",
        "Слишком много запросов ещё отправляется. Подождите немного.",
    ),
    ("Friend request sent.", "Запрос в друзья отправлен."),
    ("Confirm acceptance", "Подтвердить принятие"),
    (
        "Discord considers this person a stranger. Only confirm if you recognize them and want to accept their friend request.",
        "Discord считает этого пользователя незнакомцем. Подтверждайте только если вы его узнаёте и хотите принять запрос в друзья.",
    ),
    (
        "Discord requires confirmation to accept this friend request. Open Pending and choose Accept to review it.",
        "Discord требует подтверждения запроса в друзья. Откройте «Ожидают» и нажмите «Принять», чтобы проверить запрос.",
    ),
    (
        "This friend request is no longer pending.",
        "Этот запрос в друзья больше не ожидает ответа.",
    ),
    (
        "This person already sent you a friend request. Choose Accept below to review it.",
        "Этот пользователь уже отправил вам запрос в друзья. Нажмите «Принять» ниже, чтобы проверить его.",
    ),
    (
        "Too many friend requests are waiting for confirmation. Confirm or cancel one before trying again.",
        "Слишком много запросов в друзья ожидают подтверждения. Подтвердите или отмените один из них перед повторной попыткой.",
    ),
    ("Message", "Сообщение"),
    ("Cancel request", "Отменить запрос"),
    ("Unblock", "Разблокировать"),
    ("Add friend", "Добавить в друзья"),
    ("Friend", "Друг"),
    ("Blocked", "Заблокирован"),
    ("Incoming friend request", "Входящий запрос в друзья"),
    ("Outgoing friend request", "Исходящий запрос в друзья"),
    ("Working…", "Выполняется…"),
    ("Friends", "Друзья"),
    ("All", "Все"),
    ("Pending", "Ожидают"),
    (
        "Add a friend by username",
        "Добавить друга по имени пользователя",
    ),
    ("Send friend request", "Отправить запрос в друзья"),
    ("Nobody is online right now.", "Сейчас никого нет в сети."),
    (
        "No friends yet. Add one by username above.",
        "Друзей пока нет. Добавьте кого-нибудь по имени пользователя выше.",
    ),
    (
        "No pending friend requests.",
        "Нет ожидающих запросов в друзья.",
    ),
    (
        "You have not blocked anyone.",
        "Вы никого не заблокировали.",
    ),
    ("{} more are not shown.", "Ещё не показано: {}."),
    ("Bot", "Бот"),
    (
        "Ignored: their messages, notifications and calls are hidden.",
        "Игнорируется: сообщения, уведомления и звонки скрыты.",
    ),
    ("Nickname in {}: {}", "Ник на сервере {}: {}"),
    (" and {} more", " и ещё {}"),
    ("Roles in {}: {}", "Роли на сервере {}: {}"),
    ("Loading profile…", "Загрузка профиля…"),
    (
        "{} mutual servers",
        "{} общий сервер|{} общих сервера|{} общих серверов",
    ),
    (
        "{} mutual friends",
        "{} общий друг|{} общих друга|{} общих друзей",
    ),
    // group_dm
    (
        "Discord refused: you may lack permission, or the person cannot be added (for example because of blocking or privacy settings).",
        "Discord отклонил запрос: возможно, у вас нет прав или человека нельзя добавить (например, из-за блокировки или настроек конфиденциальности).",
    ),
    (
        "That conversation or person no longer exists.",
        "Этого чата или человека больше нет.",
    ),
    (
        "Discord rejected the request (the group may be full, or the value is not allowed).",
        "Discord отклонил запрос (возможно, группа заполнена или значение недопустимо).",
    ),
    (
        "A group name has at most {} characters.",
        "Название группы может содержать не более {} символов.",
    ),
    ("The name is unchanged.", "Название не изменилось."),
    ("Group icon", "Значок группы"),
    (
        "Choose a PNG, JPEG or GIF image.",
        "Выберите изображение PNG, JPEG или GIF.",
    ),
    ("The name is too long.", "Название слишком длинное."),
    (
        "Only a group can have people added.",
        "Добавлять людей можно только в группу.",
    ),
    (
        "Pick a friend who is not in the group yet.",
        "Выберите друга, которого ещё нет в группе.",
    ),
    (
        "That person is no longer in the group.",
        "Этого человека больше нет в группе.",
    ),
    ("Done.", "Готово."),
    (
        "Sent. The panel updates when Discord confirms.",
        "Отправлено. Панель обновится, когда Discord подтвердит.",
    ),
    ("Group", "Группа"),
    ("Conversation", "Переписка"),
    (
        "Group name (empty restores the default)",
        "Название группы (если пусто, вернётся стандартное)",
    ),
    ("Rename", "Переименовать"),
    ("Icon: set", "Значок: задан"),
    ("Icon: none", "Значок: нет"),
    ("Choose icon", "Выбрать значок"),
    ("Remove icon", "Убрать значок"),
    ("{} (owner)", "{} (владелец)"),
    ("Make owner", "Сделать владельцем"),
    ("Members ({})", "Участники ({})"),
    ("You", "Вы"),
    ("Add {}", "Добавить: {}"),
    ("Add a friend", "Добавить друга"),
    ("Leave group", "Покинуть группу"),
    ("Close conversation", "Закрыть переписку"),
    (
        "Leave this group? You will not get back in unless someone adds you.",
        "Покинуть эту группу? Вернуться можно будет, только если вас снова добавят.",
    ),
    ("Remove {} from the group?", "Убрать {} из группы?"),
    (
        "Make {} the owner? You will no longer be able to remove people.",
        "Сделать владельцем: {}? Вы больше не сможете убирать участников.",
    ),
    ("this person", "этого человека"),
    // status
    ("Custom status", "Свой статус"),
    ("Set", "Задать"),
    ("Clear", "Очистить"),
    ("Status", "Статус"),
    // notify_settings
    (
        "Notification changes are paused pending validation. Use the official Discord client to change them. Opening Alerts sends no request.",
        "Изменение уведомлений приостановлено до проверки. Меняйте их в официальном клиенте Discord. Открытие раздела не отправляет запросов.",
    ),
    ("Default", "По умолчанию"),
    ("All messages", "Все сообщения"),
    ("Only @mentions", "Только @упоминания"),
    ("Nothing", "Ничего"),
    ("For 15 minutes", "На 15 минут"),
    ("For 1 hour", "На 1 час"),
    ("For 3 hours", "На 3 часа"),
    ("For 8 hours", "На 8 часов"),
    ("For 24 hours", "На 24 часа"),
    ("Until I turn it back on", "Пока я не включу снова"),
    (
        "Discord requires an account check. All account requests have stopped; open the official Discord client.",
        "Discord требует проверку учётной записи. Все запросы остановлены; откройте официальный клиент Discord.",
    ),
    (
        "That server or conversation is gone.",
        "Этого сервера или чата больше нет.",
    ),
    (
        "Discord did not answer; the change may have been applied.",
        "Discord не ответил; изменение могло примениться.",
    ),
    (
        "The notification change failed.",
        "Не удалось изменить уведомления.",
    ),
    ("Notifications", "Уведомления"),
    ("Notify for: {}", "Уведомлять: {}"),
    ("Muted", "Заглушён"),
    ("Not muted", "Не заглушён"),
    ("Muted: change…", "Заглушён: изменить…"),
    ("Mute…", "Заглушить…"),
    ("Notify for", "Уведомлять"),
    ("This server", "Этот сервер"),
    ("This channel", "Этот канал"),
    ("This conversation", "Эта переписка"),
    (
        "Suppress @everyone and @here",
        "Подавлять @everyone и @here",
    ),
    (
        "Suppress all role @mentions",
        "Подавлять все @упоминания ролей",
    ),
    (
        "Hide muted channels in this server (this device only)",
        "Скрывать заглушённые каналы на этом сервере (только на этом устройстве)",
    ),
    (
        "Open a server or conversation first.",
        "Сначала откройте сервер или чат.",
    ),
    (
        "\"Default\" follows the server's setting. Muting stops badges and notifications; mentions still show in muted places.",
        "«По умолчанию» следует настройке сервера. Заглушение отключает значки и уведомления; упоминания в заглушённых местах всё равно видны.",
    ),
    // notify
    ("New messages", "Новые сообщения"),
    ("Sent an attachment", "Отправил(а) вложение"),
    ("New message", "Новое сообщение"),
    // captcha
    (
        "Discord asked for a CAPTCHA, which this platform cannot show. Use QR or token login.",
        "Discord запросил капчу, а на этой платформе её показать нельзя. Войдите по QR-коду или токену.",
    ),
    (
        "CAPTCHA closed before it was completed. Log in again.",
        "Капча закрыта до завершения. Войдите снова.",
    ),
    (
        "The CAPTCHA did not load. This window connects directly and does not use fastcord's network recovery: if discord.com or js.hcaptcha.com is blocked here, use QR or token login.",
        "Капча не загрузилась. Это окно подключается напрямую и не использует восстановление сети fastcord: если discord.com или js.hcaptcha.com здесь недоступны, войдите по QR-коду или токену.",
    ),
    (
        "fastcord - complete the CAPTCHA",
        "fastcord — пройдите капчу",
    ),
    // qr
    (
        "about {} minutes",
        "около {} минуты|около {} минут|около {} минут",
    ),
    (
        "about {} seconds",
        "около {} секунды|около {} секунд|около {} секунд",
    ),
    // login
    (
        "This network may be blocking Discord. Open Network recovery and start the packet helper (Windows, administrator approval), or use a proxy or VPN.",
        "Похоже, эта сеть блокирует Discord. Откройте «Сетевое подключение» и запустите пакетный помощник (Windows, нужно разрешение администратора) или используйте прокси или VPN.",
    ),
    (
        "Discord rejected this token. Login stopped; paste a valid alt-account token.",
        "Discord отклонил этот токен. Вход остановлен; вставьте действительный токен дополнительного аккаунта.",
    ),
    (
        "Could not reach Discord or Discord is temporarily unavailable. Login stopped; try again.",
        "Не удалось связаться с Discord, либо он временно недоступен. Вход остановлен; повторите попытку.",
    ),
    ("Login stopped: {}.", "Вход остановлен: {}."),
    (
        "{} Unlock it to restore your saved login, or explicitly choose a memory-only session.",
        "{} Разблокируйте хранилище, чтобы восстановить сохранённый вход, или явно выберите сеанс только в памяти.",
    ),
    (
        "The saved token is no longer in the native credential store.",
        "Сохранённого токена больше нет в системном хранилище учётных данных.",
    ),
    (
        "The saved token belongs to a different account. Login stopped; forget this saved login.",
        "Сохранённый токен принадлежит другой учётной записи. Вход остановлен; забудьте этот сохранённый вход.",
    ),
    (
        "Paste a nonempty token of at most 4096 bytes. No passwords are accepted.",
        "Вставьте непустой токен размером не более 4096 байт. Пароли не принимаются.",
    ),
    (
        "The login worker stopped. No login was established; try again.",
        "Процесс входа остановился. Вход не выполнен; повторите попытку.",
    ),
    // unverified
    ("Allow all", "Разрешить всё"),
    ("Turn all off", "Отключить всё"),
    (
        "Every group at once; still this session only.",
        "Все группы сразу; по-прежнему только на текущую сессию.",
    ),
    (
        "Hide: Test unverified actions (alt account only)",
        "Скрыть: проверка непроверенных действий (только дополнительный аккаунт)",
    ),
    (
        "Show: Test unverified actions (alt account only)",
        "Показать: проверка непроверенных действий (только дополнительный аккаунт)",
    ),
    (
        "Writes that have not been checked on a live account are off, so nothing reaches Discord by accident. Turn a group on only on the alt account, one at a time, and report what happens. This resets when you restart, log out or switch accounts.",
        "Запросы на изменение, не проверенные на реальной учётной записи, отключены, чтобы ничего не попало в Discord случайно. Включайте группу только на дополнительном аккаунте, по одной, и сообщайте о результате. Настройка сбрасывается при перезапуске, выходе и смене учётной записи.",
    ),
    (
        "Sign in to change these.",
        "Войдите, чтобы изменить эти настройки.",
    ),
    ("Basic messages", "Обычные сообщения"),
    ("Own reactions", "Свои реакции"),
    ("Typing", "Индикатор набора"),
    ("Read acknowledgement", "Отметка о прочтении"),
    ("Attachment URL refresh", "Обновление ссылок на вложения"),
    ("Message extras", "Дополнения к сообщениям"),
    ("File uploads", "Загрузка файлов"),
    ("Pins", "Закрепления"),
    ("Poll votes", "Голоса в опросах"),
    ("Application interactions", "Взаимодействия с приложениями"),
    ("Ring a call", "Вызов по звонку"),
    ("Soundboard play", "Воспроизведение звуковой панели"),
    ("Open a DM by typed ID", "Открытие чата по введённому ID"),
    ("Group DM management", "Управление групповыми чатами"),
    (
        "Relationships, ignore and message requests",
        "Отношения, игнорирование и запросы на переписку",
    ),
    (
        "Friend request by username",
        "Запрос в друзья по имени пользователя",
    ),
    ("Join or leave a server", "Вход на сервер и выход с него"),
    ("Create and revoke invites", "Создание и отзыв приглашений"),
    ("Profile and connections", "Профиль и подключения"),
    (
        "Status and privacy settings",
        "Статус и настройки конфиденциальности",
    ),
    ("Log out sessions", "Завершение сеансов"),
    ("Deauthorize an app", "Отзыв доступа приложения"),
    ("Reports", "Жалобы"),
    ("Server administration", "Администрирование сервера"),
    ("Channels", "Каналы"),
    (
        "Roles and permission overwrites",
        "Роли и переопределения прав",
    ),
    ("Moderation", "Модерация"),
    (
        "Community and onboarding setup",
        "Настройка сообщества и знакомства с сервером",
    ),
    (
        "Accept rules and onboarding answers",
        "Принятие правил и ответы при знакомстве",
    ),
    ("Scheduled events", "События"),
    ("Announcements", "Объявления"),
    ("Emoji, stickers and sounds", "Эмодзи, стикеры и звуки"),
    ("Webhooks and integrations", "Вебхуки и интеграции"),
    (
        "Stage voice states and instances",
        "Состояния и экземпляры трибун",
    ),
    ("Threads and forum posts", "Ветки и публикации форума"),
    (
        "Other, unclassified writes",
        "Прочие неклассифицированные запросы",
    ),
    // speech
    ("Never", "Никогда"),
    (
        "For the selected channel only",
        "Только для выбранного канала",
    ),
    ("All channels", "Все каналы"),
];
