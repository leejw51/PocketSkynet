//! The translation layer.
//!
//! # Why the strings are compiled in
//!
//! Eight locales of JSON fetched at runtime would be eight more round trips before
//! the first paint, on a client whose whole premise is cold-start speed — and
//! the room list is already painting from cache by then. These are `&'static
//! str` in the binary: no fetch, no parse, no flash of untranslated text, and
//! the compiler is what guarantees a key exists.
//!
//! # Why one table rather than a file per language
//!
//! [`strings!`] takes a key and *all eight* translations on one line. A
//! translator sees the sentence and its seven siblings together, which is how
//! you notice that "Delete" is a verb in one column and a noun in another; and
//! a key added without its Czech simply does not compile. The usual layout —
//! `en.json` beside `ko.json` — makes a missing key a runtime fallback, which
//! is to say an English word appearing in the middle of a Korean sentence for
//! however long it takes someone to notice.
//!
//! # Adding a string
//!
//! Add a line to [`strings!`]. The `Key` variant, the eight lookups and the
//! completeness test all follow from it. Order the columns as they are ordered
//! everywhere else in this module: en, ko, ja, yue, cs, es, zh, de.

use crate::session::backend;

const KEY_LANG: &str = "ps-lang";

/// The languages the interface speaks.
///
/// Cantonese is written in traditional characters and tagged `yue`, not
/// `zh-HK`: it is a language here, not a regional flavour of Mandarin, and the
/// vocabulary differs enough that treating it as one would produce text a
/// Cantonese reader recognises as Mandarin with the wrong words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    Ko,
    Ja,
    Yue,
    /// Simplified Chinese (Mandarin), tagged `zh-Hans`.
    Zh,
    Cs,
    /// German.
    De,
    Es,
}

impl Lang {
    /// Every language, in the order they appear in a picker. English first as
    /// the source text; the rest alphabetical by their own name, so no reader
    /// has to know the English name of their language to find it.
    pub const ALL: [Lang; 8] = [
        Lang::En,
        Lang::Ko,
        Lang::Ja,
        Lang::Yue,
        Lang::Zh,
        Lang::Cs,
        Lang::De,
        Lang::Es,
    ];

    /// The BCP-47 tag, which is also what goes in `<html lang>` and what is
    /// persisted.
    pub fn tag(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ko => "ko",
            Lang::Ja => "ja",
            Lang::Yue => "yue",
            Lang::Zh => "zh-Hans",
            Lang::Cs => "cs",
            Lang::De => "de",
            Lang::Es => "es",
        }
    }

    /// The language's name *in that language*. A picker that lists "Korean"
    /// in English is only useful to someone who already reads English, which
    /// is exactly the person who does not need it.
    pub fn endonym(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ko => "한국어",
            Lang::Ja => "日本語",
            Lang::Yue => "廣東話",
            Lang::Zh => "简体中文",
            Lang::Cs => "Čeština",
            Lang::De => "Deutsch",
            Lang::Es => "Español",
        }
    }

    /// Parse a tag, tolerating a region suffix (`ko-KR`, `es-419`) and case.
    ///
    /// `zh-HK` and `zh-Hant-HK` map to Cantonese: a browser set to Hong Kong
    /// Chinese is the closest signal we get, and Cantonese is the only Chinese
    /// this interface speaks. Plain `zh` is *not* claimed — that is far more
    /// likely to be a Mandarin reader, who is better served by English than by
    /// text that looks almost right.
    pub fn parse(tag: &str) -> Option<Lang> {
        let lower = tag.to_ascii_lowercase();
        let primary = lower.split(['-', '_']).next().unwrap_or("");
        match primary {
            "en" => Some(Lang::En),
            "ko" => Some(Lang::Ko),
            "ja" => Some(Lang::Ja),
            "yue" => Some(Lang::Yue),
            "cs" => Some(Lang::Cs),
            "de" => Some(Lang::De),
            "es" => Some(Lang::Es),
            // Hong Kong Chinese is Cantonese; every other Chinese tag now
            // has a home in Simplified Chinese.
            "zh" if lower.contains("hk") || lower.contains("hant") => Some(Lang::Yue),
            "zh" => Some(Lang::Zh),
            _ => None,
        }
    }

    /// The stored choice, or the browser's preference, or English.
    ///
    /// Read before authentication so the sign-in screen is already in the
    /// right language — the one screen where a user who cannot read the
    /// interface has no way to get to Settings and change it.
    pub fn load() -> Lang {
        if let Some(saved) = backend::get::<String>(KEY_LANG).and_then(|t| Lang::parse(&t)) {
            return saved;
        }
        Self::from_browser().unwrap_or_default()
    }

    #[cfg(target_arch = "wasm32")]
    fn from_browser() -> Option<Lang> {
        let nav = web_sys::window()?.navigator();
        // `languages` is the ordered preference list; the first one this
        // interface actually speaks wins, rather than the first one at all.
        let list = nav.languages();
        for value in list.iter() {
            if let Some(tag) = value.as_string() {
                if let Some(lang) = Lang::parse(&tag) {
                    return Some(lang);
                }
            }
        }
        nav.language().and_then(|t| Lang::parse(&t))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn from_browser() -> Option<Lang> {
        None
    }

    pub fn save(self) {
        backend::set(KEY_LANG, &self.tag());
        self.apply();
    }

    /// Put the tag on `<html>`, which is what tells a screen reader which
    /// voice to use and the browser which hyphenation and quotation rules
    /// apply. Getting this wrong is not cosmetic: an English voice reading
    /// Korean is unintelligible.
    pub fn apply(self) {
        #[cfg(target_arch = "wasm32")]
        if let Some(root) = backend::root_element() {
            let _ = root.set_attribute("lang", self.tag());
        }
    }
}

/// Generates [`Key`] and the lookup.
///
/// Each line is one string in all eight languages. The macro exists so that
/// shape is *enforced*: a key with five translations is a compile error, not a
/// gap discovered in production.
macro_rules! strings {
    ($(($key:ident, $en:expr, $ko:expr, $ja:expr, $yue:expr, $cs:expr, $es:expr, $zh:expr, $de:expr),)*) => {
        /// Every translatable string in the interface.
        ///
        /// `dead_code` is allowed: a key can be authored ahead of the screen
        /// that will use it, and the completeness test in this module — not
        /// the usage analyser — is what guards the table.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[allow(non_camel_case_types, dead_code)]
        pub enum Key { $($key,)* }

        impl Key {
            /// Every key, for the completeness test.
            #[cfg(test)]
            pub const ALL: &'static [Key] = &[$(Key::$key,)*];
        }

        /// Look one string up. Total by construction — there is no missing-key
        /// path and therefore no fallback that could leak English.
        pub fn t(lang: Lang, key: Key) -> &'static str {
            match key {
                $(Key::$key => match lang {
                    Lang::En => $en,
                    Lang::Ko => $ko,
                    Lang::Ja => $ja,
                    Lang::Yue => $yue,
                    Lang::Cs => $cs,
                    Lang::Es => $es,
                    Lang::Zh => $zh,
                    Lang::De => $de,
                },)*
            }
        }
    };
}

strings! {
    // --- Navigation and the shell -----------------------------------------
    (nav_rooms, "Rooms", "채팅방", "ルーム", "聊天室", "Místnosti", "Salas", "聊天室", "Räume"),
    (nav_chat, "Chat", "대화", "チャット", "對話", "Chat", "Chat", "聊天", "Chat"),
    (nav_members, "Members", "멤버", "メンバー", "成員", "Členové", "Miembros", "成员", "Mitglieder"),
    (nav_invites, "Invites", "초대", "招待", "邀請", "Pozvánky", "Invitaciones", "邀请", "Einladungen"),
    (nav_settings, "Settings", "설정", "設定", "設定", "Nastavení", "Ajustes", "设置", "Einstellungen"),
    (nav_sections, "Sections", "섹션", "セクション", "區段", "Sekce", "Secciones", "栏目", "Bereiche"),
    // The bottom nav's fifth tab, and the title of the sheet behind it.
    (nav_more, "More", "더보기", "その他", "更多", "Více", "Más", "更多", "Mehr"),
    (more_tools, "Tools", "도구", "ツール", "工具", "Nástroje", "Herramientas", "工具", "Werkzeuge"),
    // Where this server is and which transport is carrying this session. The
    // dialog's own contents are English throughout — they are addresses, ALPN
    // names and port numbers — but the way in is a menu row like any other.
    (server_info, "Server info", "서버 정보", "サーバー情報", "伺服器資訊", "Info o serveru", "Información del servidor", "服务器信息", "Serverinfo"),
    (more_hint, "Everything that does not fit on the bar.", "바에 다 담기지 않는 나머지입니다.", "バーに収まらないものはすべてここにあります。", "擺唔落條 bar 嘅嘢全部喺呢度。", "Vše, co se nevejde na lištu.", "Todo lo que no cabe en la barra.", "栏上放不下的都在这里。", "Alles, was nicht auf die Leiste passt."),
    (nav_account, "Account", "계정", "アカウント", "帳戶", "Účet", "Cuenta", "账户", "Konto"),
    (skip_to_messages, "Skip to messages", "메시지로 건너뛰기", "メッセージへスキップ", "跳至訊息", "Přejít na zprávy", "Ir a los mensajes", "跳转到消息", "Zu den Nachrichten springen"),

    // --- Top bar ------------------------------------------------------------
    (wallet, "Wallet", "지갑", "ウォレット", "錢包", "Peněženka", "Cartera", "钱包", "Wallet"),
    (appearance, "Appearance", "화면 모드", "外観", "外觀", "Vzhled", "Apariencia", "外观", "Erscheinungsbild"),
    (invitations, "Invitations", "초대", "招待", "邀請", "Pozvánky", "Invitaciones", "邀请", "Einladungen"),
    (sign_out, "Sign out", "로그아웃", "サインアウト", "登出", "Odhlásit se", "Cerrar sesión", "退出登录", "Abmelden"),
    (layout, "Layout", "레이아웃", "レイアウト", "版面", "Rozvržení", "Disposición", "布局", "Layout"),
    (layout_side_by_side, "Side by side", "나란히", "左右に並べる", "並排", "Vedle sebe", "Lado a lado", "并排", "Nebeneinander"),
    (layout_stacked, "Stacked", "위아래로", "上下に重ねる", "上下排列", "Nad sebou", "Apilado", "上下排列", "Übereinander"),

    // --- Common actions -----------------------------------------------------
    (cancel, "Cancel", "취소", "キャンセル", "取消", "Zrušit", "Cancelar", "取消", "Abbrechen"),
    (close, "Close", "닫기", "閉じる", "關閉", "Zavřít", "Cerrar", "关闭", "Schließen"),
    (delete, "Delete", "삭제", "削除", "刪除", "Smazat", "Eliminar", "删除", "Löschen"),
    (edit, "Edit", "편집", "編集", "編輯", "Upravit", "Editar", "编辑", "Bearbeiten"),
    (manage, "Manage", "관리", "管理", "管理", "Spravovat", "Gestionar", "管理", "Verwalten"),
    (send, "Send", "보내기", "送信", "傳送", "Odeslat", "Enviar", "发送", "Senden"),
    (retry, "Retry", "다시 시도", "再試行", "重試", "Zkusit znovu", "Reintentar", "重试", "Erneut versuchen"),
    (copy_text, "Copy text", "텍스트 복사", "テキストをコピー", "複製文字", "Kopírovat text", "Copiar texto", "复制文本", "Text kopieren"),
    (copy_link, "Copy link", "링크 복사", "リンクをコピー", "複製連結", "Kopírovat odkaz", "Copiar enlace", "复制链接", "Link kopieren"),
    (copy_hash, "Copy hash", "해시 복사", "ハッシュをコピー", "複製雜湊", "Kopírovat hash", "Copiar hash", "复制哈希", "Hash kopieren"),

    // --- Rooms --------------------------------------------------------------
    (search_rooms, "Search rooms", "채팅방 검색", "ルームを検索", "搜尋聊天室", "Hledat místnosti", "Buscar salas", "搜索聊天室", "Räume durchsuchen"),
    (no_rooms_yet, "No rooms yet", "아직 채팅방이 없습니다", "ルームがまだありません", "尚未有聊天室", "Zatím žádné místnosti", "Aún no hay salas", "还没有聊天室", "Noch keine Räume"),
    (no_rooms_body, "Create one and invite someone by wallet address.", "채팅방을 만들고 지갑 주소로 초대하세요.", "ルームを作成し、ウォレットアドレスで招待しましょう。", "建立一個聊天室，用錢包地址邀請其他人。", "Vytvořte místnost a pozvěte někoho podle adresy peněženky.", "Crea una y invita a alguien con su dirección de cartera.", "创建一个，然后按钱包地址邀请别人。", "Erstelle einen und lade jemanden per Wallet-Adresse ein."),
    (fast_create_room, "Fast create room", "빠른 채팅방 만들기", "ルームをすぐ作成", "快速建立聊天室", "Rychle vytvořit místnost", "Crear sala rápida", "快速创建聊天室", "Raum schnell erstellen"),
    (create_room_setup, "Set it up yourself…", "직접 설정하기…", "自分で設定する…", "自行設定…", "Nastavit ručně…", "Configurarla tú mismo…", "自己设置…", "Selbst einrichten…"),
    (pick_a_room, "Pick a room", "채팅방을 선택하세요", "ルームを選んでください", "選擇聊天室", "Vyberte místnost", "Elige una sala", "选择一个聊天室", "Wähle einen Raum"),
    (pick_a_room_body, "Choose a conversation on the left, or create one.", "왼쪽에서 대화를 선택하거나 새로 만드세요.", "左から会話を選ぶか、新しく作成してください。", "喺左邊揀一個對話，或者建立新嘅。", "Vyberte konverzaci vlevo, nebo založte novou.", "Elige una conversación a la izquierda, o crea una.", "在左侧选择一个会话，或新建一个。", "Wähle links eine Unterhaltung oder erstelle eine neue."),
    (encrypted_message, "Encrypted message", "암호화된 메시지", "暗号化されたメッセージ", "加密訊息", "Šifrovaná zpráva", "Mensaje cifrado", "加密消息", "Verschlüsselte Nachricht"),
    (admin, "Admin", "관리자", "管理者", "管理員", "Správce", "Admin", "管理员", "Admin"),

    // --- Chat ---------------------------------------------------------------
    (today, "Today", "오늘", "今日", "今日", "Dnes", "Hoy", "今天", "Heute"),
    (yesterday, "Yesterday", "어제", "昨日", "尋日", "Včera", "Ayer", "昨天", "Gestern"),
    (message_room, "Message", "메시지 보내기", "メッセージを送る", "傳送訊息", "Napsat zprávu", "Mensaje", "消息", "Nachricht"),
    (enter_to_send, "Enter to send · Shift+Enter for a new line", "Enter로 전송 · Shift+Enter로 줄바꿈", "Enterで送信 · Shift+Enterで改行", "Enter 傳送 · Shift+Enter 換行", "Enter odešle · Shift+Enter nový řádek", "Enter para enviar · Shift+Enter para salto de línea", "Enter 发送 · Shift+Enter 换行", "Enter zum Senden · Shift+Enter für neue Zeile"),
    (sync_now, "Sync now", "지금 동기화", "今すぐ同期", "立即同步", "Synchronizovat", "Sincronizar ahora", "立即同步", "Jetzt synchronisieren"),
    (room_actions, "Room actions", "채팅방 메뉴", "ルーム操作", "聊天室選項", "Akce místnosti", "Acciones de la sala", "聊天室操作", "Raumaktionen"),
    (live, "Live", "실시간", "ライブ", "即時", "Živě", "En vivo", "实时", "Live"),

    // --- Settings -----------------------------------------------------------
    (preferences, "PREFERENCES", "환경설정", "環境設定", "偏好設定", "PŘEDVOLBY", "PREFERENCIAS", "偏好设置", "EINSTELLUNGEN"),
    (account_section, "ACCOUNT", "계정", "アカウント", "帳戶", "ÚČET", "CUENTA", "账户", "KONTO"),
    (theme_light, "Light", "밝게", "ライト", "淺色", "Světlý", "Claro", "浅色", "Hell"),
    (theme_dark, "Dark", "어둡게", "ダーク", "深色", "Tmavý", "Oscuro", "深色", "Dunkel"),
    (theme_system, "System", "시스템", "システム", "系統", "Systém", "Sistema", "跟随系统", "System"),
    (connection, "Connection", "연결", "接続", "連線", "Připojení", "Conexión", "连接", "Verbindung"),
    (conn_live, "Live", "실시간", "ライブ", "即時", "Živě", "En vivo", "实时", "Live"),
    (conn_events, "Events", "이벤트", "イベント", "事件", "Události", "Eventos", "事件流", "Events"),
    (conn_polling, "Polling", "폴링", "ポーリング", "輪詢", "Dotazování", "Sondeo", "轮询", "Polling"),
    (blocked_people, "Blocked people", "차단한 사용자", "ブロックした人", "已封鎖的人", "Blokovaní lidé", "Personas bloqueadas", "已屏蔽的人", "Blockierte Personen"),
    (hidden_rooms, "Hidden rooms", "숨긴 채팅방", "非表示のルーム", "隱藏的聊天室", "Skryté místnosti", "Salas ocultas", "已隐藏的聊天室", "Ausgeblendete Räume"),
    (language, "Language", "언어", "言語", "語言", "Jazyk", "Idioma", "语言", "Sprache"),
    (erase_local_data, "Erase local data", "로컬 데이터 삭제", "ローカルデータを消去", "清除本機資料", "Vymazat místní data", "Borrar datos locales", "清除本地数据", "Lokale Daten löschen"),

    // --- Login --------------------------------------------------------------
    (sign_in_tagline, "Sign in with your wallet. No password.", "지갑으로 로그인하세요. 비밀번호가 없습니다.", "ウォレットでサインイン。パスワードは不要です。", "用錢包登入，唔使密碼。", "Přihlaste se peněženkou. Žádné heslo.", "Inicia sesión con tu cartera. Sin contraseña.", "用钱包登录，无需密码。", "Melde dich mit deiner Wallet an. Kein Passwort."),
    (sign_in, "Sign in", "로그인", "サインイン", "登入", "Přihlásit se", "Iniciar sesión", "登录", "Anmelden"),
    (create_wallet_and_sign_in, "Create a wallet and sign in", "지갑을 만들고 로그인", "ウォレットを作成してサインイン", "建立錢包並登入", "Vytvořit peněženku a přihlásit se", "Crear una cartera e iniciar sesión", "创建钱包并登录", "Wallet erstellen und anmelden"),
    (or_sign_in_with, "OR SIGN IN WITH", "또는 다음으로 로그인", "または次でサインイン", "或以下列方式登入", "NEBO SE PŘIHLASTE POMOCÍ", "O INICIA SESIÓN CON", "或使用以下方式登录", "ODER ANMELDEN MIT"),
    (recovery_phrase, "Recovery phrase", "복구 문구", "リカバリーフレーズ", "復原字詞", "Obnovovací fráze", "Frase de recuperación", "恢复助记词", "Wiederherstellungsphrase"),
    (private_key, "Private key", "개인 키", "秘密鍵", "私密金鑰", "Soukromý klíč", "Clave privada", "私钥", "Privater Schlüssel"),
    (username, "Username", "사용자 이름", "ユーザー名", "使用者名稱", "Uživatelské jméno", "Nombre de usuario", "用户名", "Benutzername"),
    (wallet_address_is_account, "Your wallet address is your account.", "지갑 주소가 곧 계정입니다.", "ウォレットアドレスがそのままアカウントです。", "你嘅錢包地址就係你嘅帳戶。", "Vaše adresa peněženky je váš účet.", "Tu dirección de cartera es tu cuenta.", "你的钱包地址就是你的账户。", "Deine Wallet-Adresse ist dein Konto."),

    // --- Room list ----------------------------------------------------------
    (create_room_dots, "Create a room…", "채팅방 만들기…", "ルームを作成…", "建立聊天室…", "Vytvořit místnost…", "Crear una sala…", "创建聊天室…", "Raum erstellen…"),
    (fast_create_hint, "Fast create room — encrypted, named for you, opened", "빠른 채팅방 만들기 — 암호화, 자동 이름, 바로 열기", "すぐ作成 — 暗号化・自動命名・そのまま開く", "快速建立 — 加密、自動命名、即刻開啟", "Rychlá místnost — šifrovaná, pojmenovaná, otevřená", "Sala rápida: cifrada, con nombre y abierta", "快速创建聊天室 — 已加密、自动命名并打开", "Raum schnell erstellen — verschlüsselt, benannt und geöffnet"),
    (couldnt_load_rooms, "Couldn't load rooms", "채팅방을 불러오지 못했습니다", "ルームを読み込めませんでした", "無法載入聊天室", "Místnosti se nepodařilo načíst", "No se pudieron cargar las salas", "无法加载聊天室", "Räume konnten nicht geladen werden"),
    (try_again, "Try again", "다시 시도", "もう一度試す", "再試一次", "Zkusit znovu", "Intentar de nuevo", "重试", "Erneut versuchen"),
    (offline, "Offline", "오프라인", "オフライン", "離線", "Offline", "Sin conexión", "离线", "Offline"),

    // --- Chat ---------------------------------------------------------------
    (back_to_rooms, "Back to rooms", "채팅방 목록으로", "ルーム一覧へ戻る", "返回聊天室", "Zpět na místnosti", "Volver a las salas", "返回聊天室列表", "Zurück zu den Räumen"),
    (load_earlier, "Load earlier messages", "이전 메시지 불러오기", "以前のメッセージを読み込む", "載入較早的訊息", "Načíst starší zprávy", "Cargar mensajes anteriores", "加载更早的消息", "Frühere Nachrichten laden"),
    (loading, "Loading…", "불러오는 중…", "読み込み中…", "載入中…", "Načítání…", "Cargando…", "加载中…", "Lädt…"),
    (opening_room, "Opening room…", "채팅방 여는 중…", "ルームを開いています…", "開啟聊天室中…", "Otevírání místnosti…", "Abriendo la sala…", "正在打开聊天室…", "Raum wird geöffnet…"),
    (sending, "Sending…", "보내는 중…", "送信中…", "傳送中…", "Odesílání…", "Enviando…", "发送中…", "Wird gesendet…"),
    (not_sent, "Not sent", "전송 실패", "未送信", "未傳送", "Neodesláno", "No enviado", "未发送", "Nicht gesendet"),
    (rotating_keys, "Rotating keys…", "키 교체 중…", "鍵を更新中…", "更換金鑰中…", "Výměna klíčů…", "Rotando claves…", "正在轮换密钥…", "Schlüssel werden rotiert…"),
    (view_members, "View members", "멤버 보기", "メンバーを見る", "查看成員", "Zobrazit členy", "Ver miembros", "查看成员", "Mitglieder anzeigen"),
    (sync_this_room, "Sync this room now", "이 채팅방 지금 동기화", "このルームを今すぐ同期", "立即同步此聊天室", "Synchronizovat tuto místnost", "Sincronizar esta sala ahora", "立即同步此聊天室", "Diesen Raum jetzt synchronisieren"),
    (couldnt_load_room, "Couldn't load this room", "채팅방을 불러오지 못했습니다", "このルームを読み込めませんでした", "無法載入此聊天室", "Místnost se nepodařilo načíst", "No se pudo cargar esta sala", "无法加载此聊天室", "Dieser Raum konnte nicht geladen werden"),
    (no_messages_yet, "No messages yet", "아직 메시지가 없습니다", "メッセージがまだありません", "尚未有訊息", "Zatím žádné zprávy", "Aún no hay mensajes", "还没有消息", "Noch keine Nachrichten"),
    (room_unavailable, "Room unavailable", "채팅방을 사용할 수 없습니다", "ルームを利用できません", "聊天室無法使用", "Místnost není dostupná", "Sala no disponible", "聊天室不可用", "Raum nicht verfügbar"),

    // --- Message actions ----------------------------------------------------
    (copy_tx_hash, "Copy transaction hash", "트랜잭션 해시 복사", "取引ハッシュをコピー", "複製交易雜湊", "Kopírovat hash transakce", "Copiar hash de la transacción", "复制交易哈希", "Transaktionshash kopieren"),
    (not_encrypted, "Not encrypted", "암호화되지 않음", "暗号化されていません", "未加密", "Nešifrováno", "Sin cifrar", "未加密", "Nicht verschlüsselt"),
    (edit_message, "Edit message", "메시지 편집", "メッセージを編集", "編輯訊息", "Upravit zprávu", "Editar mensaje", "编辑消息", "Nachricht bearbeiten"),
    (save_edit, "Save edit", "편집 저장", "編集を保存", "儲存編輯", "Uložit úpravu", "Guardar edición", "保存编辑", "Änderung speichern"),
    (cancel_edit, "Cancel edit", "편집 취소", "編集をキャンセル", "取消編輯", "Zrušit úpravu", "Cancelar edición", "取消编辑", "Bearbeitung abbrechen"),
    (image_alt, "Image", "이미지", "画像", "圖片", "Obrázek", "Imagen", "图片", "Bild"),
    (image_failed, "Image failed to load", "이미지를 불러오지 못했습니다", "画像を読み込めませんでした", "圖片載入失敗", "Obrázek se nepodařilo načíst", "No se pudo cargar la imagen", "图片加载失败", "Bild konnte nicht geladen werden"),
    (image_zoom, "View full screen", "전체 화면으로 보기", "全画面で表示", "全螢幕檢視", "Zobrazit na celou obrazovku", "Ver a pantalla completa", "全屏查看", "Im Vollbild ansehen"),
    (video_alt, "Video", "동영상", "動画", "影片", "Video", "Vídeo", "视频", "Video"),
    (video_failed, "Video failed to load", "동영상을 불러오지 못했습니다", "動画を読み込めませんでした", "影片載入失敗", "Video se nepodařilo načíst", "No se pudo cargar el vídeo", "视频加载失败", "Video konnte nicht geladen werden"),
    (youtube_video, "YouTube video", "YouTube 동영상", "YouTube動画", "YouTube影片", "Video YouTube", "Vídeo de YouTube", "YouTube 视频", "YouTube-Video"),

    // --- Composer -----------------------------------------------------------
    (offline_queue_note, "Offline — messages send when you reconnect.", "오프라인 — 다시 연결되면 전송됩니다.", "オフライン — 再接続時に送信されます。", "離線 — 重新連線後會傳送。", "Offline — zprávy se odešlou po připojení.", "Sin conexión: se enviarán al reconectar.", "已离线 — 重新连接后将发送消息。", "Offline — Nachrichten werden nach der Wiederverbindung gesendet."),
    (insert_emoticon, "Insert an emoticon", "이모티콘 넣기", "絵文字を挿入", "插入表情符號", "Vložit emotikon", "Insertar un emoticono", "插入表情", "Emoticon einfügen"),
    (emoticon_categories, "Emoticon categories", "이모티콘 분류", "絵文字カテゴリ", "表情符號分類", "Kategorie emotikonů", "Categorías de emoticonos", "表情分类", "Emoticon-Kategorien"),
    (open_ai_assistant, "Open the AI assistant", "AI 어시스턴트 열기", "AIアシスタントを開く", "開啟 AI 助手", "Otevřít AI asistenta", "Abrir el asistente de IA", "打开 AI 助手", "KI-Assistenten öffnen"),
    (ai_assistant, "AI assistant", "AI 어시스턴트", "AIアシスタント", "AI 助手", "AI asistent", "Asistente de IA", "AI 助手", "KI-Assistent"),
    // --- browser wallet sign-in ---
    (wallet_signin, "Continue with MetaMask", "MetaMask로 계속하기", "MetaMaskで続ける", "用 MetaMask 繼續", "Pokračovat s MetaMask", "Continuar con MetaMask", "使用 MetaMask 继续", "Mit MetaMask fortfahren"),
    (wallet_signin_hint, "Three signatures: sign in, encryption key, key binding", "서명 3회: 로그인, 암호화 키, 키 바인딩", "署名3回: ログイン、暗号鍵、鍵バインディング", "三次簽署：登入、加密金鑰、金鑰綁定", "Tři podpisy: přihlášení, šifrovací klíč, vazba klíče", "Tres firmas: inicio, clave de cifrado, vinculación", "三次签名：登录、加密密钥、密钥绑定", "Drei Signaturen: Anmeldung, Schlüssel, Bindung"),
    (wallet_connecting, "Check your wallet…", "지갑을 확인하세요…", "ウォレットを確認してください…", "請查看你的錢包…", "Zkontrolujte peněženku…", "Revisa tu monedero…", "请查看你的钱包…", "Prüfe deine Wallet…"),
    (wallet_not_found, "No browser wallet found. Install MetaMask, or sign in with a recovery phrase.", "브라우저 지갑을 찾을 수 없습니다. MetaMask를 설치하거나 복구 문구로 로그인하세요.", "ブラウザウォレットが見つかりません。MetaMaskを入れるか、リカバリーフレーズでログインしてください。", "找不到瀏覽器錢包。請安裝 MetaMask，或用復原詞登入。", "Peněženka v prohlížeči nenalezena. Nainstalujte MetaMask nebo se přihlaste frází.", "No se encontró un monedero. Instala MetaMask o entra con tu frase de recuperación.", "未找到浏览器钱包。请安装 MetaMask，或用助记词登录。", "Keine Browser-Wallet gefunden. Installiere MetaMask oder melde dich mit der Wiederherstellungsphrase an."),
    (wallet_rejected, "You cancelled the wallet request.", "지갑 요청을 취소했습니다.", "ウォレットの要求をキャンセルしました。", "你取消了錢包請求。", "Požadavek peněženky jste zrušili.", "Cancelaste la solicitud del monedero.", "你取消了钱包请求。", "Du hast die Wallet-Anfrage abgebrochen."),
    (wallet_no_account, "Your wallet returned no account. Unlock it and try again.", "지갑이 계정을 반환하지 않았습니다. 잠금을 해제하고 다시 시도하세요.", "ウォレットがアカウントを返しませんでした。ロックを解除して再試行してください。", "錢包沒有回傳帳戶。請解鎖後再試。", "Peněženka nevrátila žádný účet. Odemkněte ji a zkuste to znovu.", "Tu monedero no devolvió ninguna cuenta. Desbloquéalo e inténtalo de nuevo.", "钱包未返回账户。请解锁后重试。", "Deine Wallet hat kein Konto zurückgegeben. Entsperre sie und versuche es erneut."),
    (wallet_bad_address, "Your wallet returned an address this app cannot use.", "지갑이 이 앱에서 사용할 수 없는 주소를 반환했습니다.", "ウォレットがこのアプリで使えないアドレスを返しました。", "錢包回傳了此應用無法使用的位址。", "Peněženka vrátila adresu, kterou tato aplikace neumí použít.", "Tu monedero devolvió una dirección que esta app no puede usar.", "钱包返回了此应用无法使用的地址。", "Deine Wallet hat eine unbrauchbare Adresse zurückgegeben."),
    (wallet_failed, "The wallet request failed. Try again.", "지갑 요청이 실패했습니다. 다시 시도하세요.", "ウォレットの要求が失敗しました。再試行してください。", "錢包請求失敗，請再試。", "Požadavek peněženky selhal. Zkuste to znovu.", "La solicitud del monedero falló. Inténtalo de nuevo.", "钱包请求失败，请重试。", "Die Wallet-Anfrage ist fehlgeschlagen. Versuche es erneut."),
    (wallet_no_local_key, "This session signed in with a browser wallet, so it has no key on this device. Sending funds needs a recovery-phrase sign-in.", "이 세션은 브라우저 지갑으로 로그인했으므로 이 기기에 키가 없습니다. 송금은 복구 문구 로그인이 필요합니다.", "このセッションはブラウザウォレットでログインしたため、この端末に鍵がありません。送金にはリカバリーフレーズでのログインが必要です。", "此工作階段以瀏覽器錢包登入，本機沒有金鑰。轉帳需要用復原詞登入。", "Tato session se přihlásila peněženkou v prohlížeči, takže na tomto zařízení není klíč. Posílání prostředků vyžaduje přihlášení frází.", "Esta sesión entró con un monedero del navegador, así que no hay clave en este dispositivo. Enviar fondos requiere entrar con la frase.", "此会话使用浏览器钱包登录，本设备没有密钥。转账需要用助记词登录。", "Diese Sitzung nutzt eine Browser-Wallet, daher liegt hier kein Schlüssel. Für Überweisungen ist eine Anmeldung mit der Phrase nötig."),
    (privy_signin, "Continue with email", "이메일로 계속하기", "メールで続ける", "用電子郵件繼續", "Pokračovat e-mailem", "Continuar con correo", "使用邮箱继续", "Mit E-Mail fortfahren"),
    (privy_signin_hint, "Privy makes a wallet for you — no extension needed", "Privy가 지갑을 만들어 줍니다 — 확장 프로그램 불필요", "Privyがウォレットを作ります — 拡張機能は不要", "Privy 會替你建立錢包 — 無需擴充功能", "Privy vám vytvoří peněženku — bez rozšíření", "Privy crea un monedero para ti — sin extensión", "Privy 会为你创建钱包 — 无需扩展程序", "Privy erstellt eine Wallet für dich — keine Erweiterung nötig"),
    (privy_loading, "Loading Privy…", "Privy 불러오는 중…", "Privyを読み込み中…", "正在載入 Privy…", "Načítání Privy…", "Cargando Privy…", "正在加载 Privy…", "Privy wird geladen…"),
    (wallet_open_in_metamask, "Open in MetaMask", "MetaMask에서 열기", "MetaMaskで開く", "在 MetaMask 中開啟", "Otevřít v MetaMask", "Abrir en MetaMask", "在 MetaMask 中打开", "In MetaMask öffnen"),
    (wallet_ios_hint, "On a phone, MetaMask can only sign inside its own browser", "휴대폰에서는 MetaMask 자체 브라우저에서만 서명할 수 있습니다", "スマホではMetaMask内のブラウザでのみ署名できます", "在手機上，MetaMask 只能在其自帶瀏覽器中簽署", "Na telefonu může MetaMask podepisovat jen ve svém prohlížeči", "En el móvil, MetaMask solo puede firmar en su propio navegador", "在手机上，MetaMask 只能在其自带浏览器中签名", "Auf dem Handy kann MetaMask nur im eigenen Browser signieren"),
    (trust_server, "Trust this server", "이 서버 신뢰하기", "このサーバーを信頼", "信任此伺服器", "Důvěřovat tomuto serveru", "Confiar en este servidor", "信任此服务器", "Diesem Server vertrauen"),
    (trust_server_why, "Install this server's certificate to remove the browser warning — and to let MetaMask's in-app browser open the app at all.", "브라우저 경고를 없애고 MetaMask 내장 브라우저에서 앱을 열려면 이 서버의 인증서를 설치하세요.", "ブラウザの警告を消し、MetaMask内蔵ブラウザでアプリを開くには、このサーバーの証明書をインストールしてください。", "安裝此伺服器的憑證以移除瀏覽器警告，並讓 MetaMask 內建瀏覽器能開啟應用程式。", "Nainstalujte certifikát tohoto serveru, aby zmizelo varování prohlížeče a aby aplikaci otevřel i prohlížeč v MetaMask.", "Instala el certificado de este servidor para quitar el aviso del navegador y permitir que el navegador de MetaMask abra la app.", "安装此服务器的证书以消除浏览器警告，并让 MetaMask 内置浏览器能打开应用。", "Installiere das Zertifikat dieses Servers, um die Browserwarnung zu entfernen und MetaMasks internen Browser die App öffnen zu lassen."),
    (trust_server_ios, "On iPhone: open the file, then Settings → General → VPN & Device Management to install it, then Settings → General → About → Certificate Trust Settings to switch it on.", "아이폰: 파일을 연 뒤 설정 → 일반 → VPN 및 기기 관리에서 설치하고, 설정 → 일반 → 정보 → 인증서 신뢰 설정에서 켜세요.", "iPhone: ファイルを開き、設定 → 一般 → VPNとデバイス管理でインストール、設定 → 一般 → 情報 → 証明書信頼設定でオンにします。", "iPhone：開啟檔案後，設定 → 一般 → VPN 與裝置管理安裝，再到設定 → 一般 → 關於本機 → 憑證信任設定開啟。", "iPhone: otevřete soubor, pak Nastavení → Obecné → VPN a správa zařízení pro instalaci a Nastavení → Obecné → Informace → Nastavení důvěry certifikátů pro zapnutí.", "En iPhone: abre el archivo, luego Ajustes → General → VPN y gestión de dispositivos para instalarlo, y Ajustes → General → Información → Ajustes de confianza de certificados para activarlo.", "iPhone：打开文件，然后设置 → 通用 → VPN与设备管理进行安装，再到设置 → 通用 → 关于本机 → 证书信任设置中开启。", "iPhone: Datei öffnen, dann Einstellungen → Allgemein → VPN & Geräteverwaltung zum Installieren und Einstellungen → Allgemein → Info → Zertifikatsvertrauenseinstellungen zum Aktivieren."),
    (trust_server_get, "1. Download and install the certificate", "1. 인증서를 다운로드해 설치하세요", "1. 証明書をダウンロードしてインストール", "1. 下載並安裝憑證", "1. Stáhněte a nainstalujte certifikát", "1. Descarga e instala el certificado", "1. 下载并安装证书", "1. Zertifikat herunterladen und installieren"),
    (trust_server_refresh, "2. Then refresh this page", "2. 그런 다음 이 페이지를 새로고침하세요", "2. その後、このページを再読み込み", "2. 然後重新整理此頁面", "2. Poté obnovte tuto stránku", "2. Luego actualiza esta página", "2. 然后刷新此页面", "2. Danach diese Seite neu laden"),
    // --- attachments (docs/API.md §14) ---
    (attach_file, "Attach a file — anything typed becomes its caption and #hashtags", "파일 첨부 — 입력한 내용이 설명과 #해시태그가 됩니다", "ファイルを添付 — 入力した内容が説明と #ハッシュタグになります", "附加檔案 — 已輸入的文字會成為說明與 #標籤", "Přiložit soubor — napsaný text se stane popisem a #hashtagy", "Adjuntar un archivo — lo escrito será su descripción y #etiquetas", "附加文件 — 已输入的文字将成为说明和 #标签", "Datei anhängen — Getipptes wird Beschreibung und #Hashtags"),
    (files_title, "Files", "파일", "ファイル", "檔案", "Soubory", "Archivos", "文件", "Dateien"),
    (files_desc, "Everything attached to this room. Tag a file to find it later.", "이 방에 첨부된 모든 파일입니다. 태그를 달면 나중에 찾기 쉽습니다.", "このルームに添付されたすべてのファイル。タグを付けると後で見つけやすくなります。", "此聊天室的所有附件。加上標籤方便日後尋找。", "Vše, co je přiloženo k této místnosti. Označte soubor, abyste ho později našli.", "Todo lo adjuntado a esta sala. Etiqueta un archivo para encontrarlo después.", "此房间的所有附件。为文件加标签便于以后查找。", "Alles, was an diesen Raum angehängt ist. Markiere eine Datei, um sie später zu finden."),
    (files_empty, "No files yet", "아직 파일이 없습니다", "まだファイルはありません", "尚無檔案", "Zatím žádné soubory", "Aún no hay archivos", "还没有文件", "Noch keine Dateien"),
    (files_empty_desc, "Attach one from the composer, and give it a #hashtag so it can be found.", "작성창에서 파일을 첨부하고 #해시태그를 달아 찾을 수 있게 하세요.", "入力欄から添付し、#ハッシュタグを付けて見つけられるようにしましょう。", "從輸入框附加檔案，並加上 #標籤以便尋找。", "Přiložte soubor z editoru a dejte mu #hashtag, aby se dal najít.", "Adjunta uno desde el editor y ponle una #etiqueta para poder encontrarlo.", "从输入框附加文件，并加上 #标签以便查找。", "Hänge eine über das Eingabefeld an und gib ihr einen #Hashtag, damit sie gefunden wird."),
    (files_none_tagged, "No files with that tag", "해당 태그의 파일이 없습니다", "そのタグのファイルはありません", "沒有該標籤的檔案", "Žádné soubory s tímto tagem", "No hay archivos con esa etiqueta", "没有该标签的文件", "Keine Dateien mit diesem Hashtag"),
    (open_files, "Open files", "파일 열기", "ファイルを開く", "開啟檔案", "Otevřít soubory", "Abrir archivos", "打开文件", "Dateien öffnen"),
    (attach_caption_label, "Caption and #hashtags", "설명과 #해시태그", "説明と #ハッシュタグ", "說明與 #標籤", "Popis a #hashtagy", "Descripción y #etiquetas", "说明和 #标签", "Beschreibung und #Hashtags"),
    (attach_caption_help, "Hashtags make this searchable. The filename is indexed too.", "해시태그로 검색할 수 있습니다. 파일 이름도 색인됩니다.", "ハッシュタグで検索できます。ファイル名も索引されます。", "標籤讓它可被搜尋。檔名也會被索引。", "Hashtagy umožní vyhledávání. Indexuje se i název souboru.", "Las etiquetas lo hacen buscable. El nombre del archivo también se indexa.", "标签让它可被搜索。文件名也会被索引。", "Hashtags machen sie durchsuchbar. Der Dateiname wird ebenfalls indexiert."),
    (attach_send, "Attach", "첨부", "添付", "附加", "Přiložit", "Adjuntar", "附加", "Anhängen"),
    (attach_uploading, "Uploading…", "업로드 중…", "アップロード中…", "上傳中…", "Nahrávání…", "Subiendo…", "上传中…", "Wird hochgeladen…"),
    (attach_too_large, "That file is larger than 4 GB.", "파일이 4GB보다 큽니다.", "そのファイルは4GBを超えています。", "該檔案超過 4 GB。", "Tento soubor je větší než 4 GB.", "Ese archivo supera los 4 GB.", "该文件大于 4 GB。", "Diese Datei ist größer als 4 GB."),
    (attach_download_started, "Downloading {name} — check your browser's downloads.", "{name} 다운로드 중 — 브라우저의 다운로드를 확인하세요.", "{name} をダウンロード中 — ブラウザのダウンロードを確認してください。", "正在下載 {name} — 請查看瀏覽器的下載項目。", "Stahuje se {name} — podívejte se do stahování v prohlížeči.", "Descargando {name}: revisa las descargas del navegador.", "正在下载 {name} — 请查看浏览器的下载内容。", "{name} wird heruntergeladen – siehe Downloads im Browser."),
    (video_play, "Play this video", "이 동영상 재생", "この動画を再生", "播放此影片", "Přehrát video", "Reproducir este vídeo", "播放此视频", "Dieses Video abspielen"),
    (attach_downloaded_ok, "{name} saved — checksum verified.", "{name} 저장됨 — 체크섬 확인 완료.", "{name} を保存しました — チェックサム確認済み。", "已儲存 {name} — 總和檢查碼已驗證。", "{name} uloženo — kontrolní součet ověřen.", "{name} guardado: suma de verificación verificada.", "已保存 {name} — 校验和已验证。", "{name} gespeichert – Prüfsumme bestätigt."),
    (attach_verify, "Verify a downloaded copy", "다운로드한 사본 검증", "ダウンロードしたファイルを検証", "驗證已下載的副本", "Ověřit staženou kopii", "Verificar una copia descargada", "验证已下载的副本", "Heruntergeladene Kopie prüfen"),
    (attach_verify_ok, "Checksum matches — the file is intact.", "체크섬 일치 — 파일이 온전합니다.", "チェックサム一致 — ファイルは無傷です。", "總和檢查碼相符 — 檔案完整。", "Kontrolní součet souhlasí — soubor je v pořádku.", "La suma de verificación coincide: el archivo está intacto.", "校验和匹配 — 文件完整。", "Prüfsumme stimmt – die Datei ist unversehrt."),
    (attach_verify_failed, "Checksum does NOT match. That copy is damaged — download it again.", "체크섬이 일치하지 않습니다. 사본이 손상되었습니다 — 다시 다운로드하세요.", "チェックサムが一致しません。そのファイルは破損しています — 再度ダウンロードしてください。", "總和檢查碼不符。該副本已損毀 — 請重新下載。", "Kontrolní součet NEsouhlasí. Kopie je poškozená — stáhněte ji znovu.", "La suma de verificación NO coincide. Esa copia está dañada: descárgala de nuevo.", "校验和不匹配。该副本已损坏 — 请重新下载。", "Prüfsumme stimmt NICHT. Diese Kopie ist beschädigt – bitte erneut herunterladen."),
    (transfer_downloading, "Downloading", "다운로드 중", "ダウンロード中", "下載中", "Stahování", "Descargando", "下载中", "Herunterladen"),
    (transfer_cancel, "Cancel this transfer", "이 전송 취소", "この転送をキャンセル", "取消此傳輸", "Zrušit tento přenos", "Cancelar esta transferencia", "取消此传输", "Diese Übertragung abbrechen"),
    (transfer_stalled, "Stalled", "멈춤", "停止中", "已停滯", "Zastaveno", "Detenido", "已停滞", "Angehalten"),
    (transfer_stalled_hint, "— cancel and attach it again to resume", "— 취소 후 다시 첨부하면 이어서 진행됩니다", "— キャンセルして再度添付すると再開します", "— 取消後再次附加即可繼續", "— zrušte a přiložte znovu pro pokračování", "— cancela y adjúntalo de nuevo para reanudar", "— 取消后重新附加即可继续", "— abbrechen und erneut anhängen, um fortzusetzen"),
    (transfer_checksum, "Checking", "확인 중", "確認中", "檢查中", "Kontrola", "Comprobando", "检查中", "Prüfen"),
    (transfer_uploading, "Uploading", "업로드 중", "アップロード中", "上傳中", "Nahrávání", "Subiendo", "上传中", "Hochladen"),
    (transfer_verifying, "Verifying", "검증 중", "検証中", "驗證中", "Ověřování", "Verificando", "验证中", "Überprüfen"),
    (attach_read_failed, "Could not read that file.", "파일을 읽을 수 없습니다.", "そのファイルを読み込めませんでした。", "無法讀取該檔案。", "Soubor se nepodařilo přečíst.", "No se pudo leer ese archivo.", "无法读取该文件。", "Diese Datei konnte nicht gelesen werden."),
    (attach_uploaded, "{name} attached", "{name} 첨부됨", "{name} を添付しました", "已附加 {name}", "{name} přiloženo", "{name} adjuntado", "已附加 {name}", "{name} angehängt"),
    (file_download, "Save", "저장", "保存", "儲存", "Uložit", "Guardar", "保存", "Speichern"),
    (file_delete, "Delete attachment", "첨부 파일 삭제", "添付を削除", "刪除附件", "Smazat přílohu", "Eliminar el adjunto", "删除附件", "Anhang löschen"),
    (file_deleted, "Attachment deleted", "첨부 파일이 삭제되었습니다", "添付を削除しました", "已刪除附件", "Příloha smazána", "Adjunto eliminado", "附件已删除", "Anhang gelöscht"),
    (file_delete_confirm, "Delete {name}? This cannot be undone.", "{name}을(를) 삭제할까요? 되돌릴 수 없습니다.", "{name} を削除しますか？取り消せません。", "要刪除 {name} 嗎？無法復原。", "Smazat {name}? Tuto akci nelze vzít zpět.", "¿Eliminar {name}? No se puede deshacer.", "删除 {name}？此操作无法撤销。", "{name} löschen? Das kann nicht rückgängig gemacht werden."),
    (open_in_new_window, "Open in a new window", "새 창에서 열기", "新しいウィンドウで開く", "在新視窗開啟", "Otevřít v novém okně", "Abrir en una ventana nueva", "在新窗口打开", "In einem neuen Fenster öffnen"),
    (attachment_verifying, "Checking the file…", "파일 검사 중…", "ファイルを確認中…", "檢查緊個檔案…", "Kontroluji soubor…", "Comprobando el archivo…", "正在检查文件…", "Datei wird geprüft…"),
    (attachment_failed, "This attachment could not be loaded.", "이 첨부 파일을 불러올 수 없습니다.", "この添付を読み込めませんでした。", "無法載入此附件。", "Tuto přílohu nelze načíst.", "No se pudo cargar este adjunto.", "无法加载此附件。", "Dieser Anhang konnte nicht geladen werden."),
    (file_filter_all, "All", "전체", "すべて", "全部", "Vše", "Todos", "全部", "Alle"),

    // --- Members ------------------------------------------------------------
    (invite_people, "Invite people", "사람 초대", "メンバーを招待", "邀請其他人", "Pozvat lidi", "Invitar personas", "邀请成员", "Personen einladen"),
    (blocked, "Blocked", "차단됨", "ブロック中", "已封鎖", "Blokován", "Bloqueado", "已屏蔽", "Blockiert"),
    (you, "You", "나", "あなた", "你", "Vy", "Tú", "你", "Du"),
    (couldnt_load_members, "Couldn't load members", "멤버를 불러오지 못했습니다", "メンバーを読み込めませんでした", "無法載入成員", "Členy se nepodařilo načíst", "No se pudieron cargar los miembros", "无法加载成员", "Mitglieder konnten nicht geladen werden"),

    // --- Invitations --------------------------------------------------------
    (decline, "Decline", "거절", "辞退", "拒絕", "Odmítnout", "Rechazar", "拒绝", "Ablehnen"),
    (accept, "Accept", "수락", "承諾", "接受", "Přijmout", "Aceptar", "接受", "Annehmen"),
    (invite_key_note, "You'll receive the room key when you accept.", "수락하면 채팅방 키를 받습니다.", "承諾するとルームの鍵を受け取ります。", "你接受之後就會收到聊天室金鑰。", "Po přijetí obdržíte klíč místnosti.", "Recibirás la clave de la sala al aceptar.", "接受后你将收到聊天室密钥。", "Du erhältst den Raumschlüssel, sobald du annimmst."),
    (couldnt_load_invitations, "Couldn't load invitations", "초대를 불러오지 못했습니다", "招待を読み込めませんでした", "無法載入邀請", "Pozvánky se nepodařilo načíst", "No se pudieron cargar las invitaciones", "无法加载邀请", "Einladungen konnten nicht geladen werden"),
    (no_invitations, "No invitations", "초대가 없습니다", "招待はありません", "沒有邀請", "Žádné pozvánky", "Sin invitaciones", "没有邀请", "Keine Einladungen"),

    // --- Login --------------------------------------------------------------
    (new_phrase_tagline, "New recovery phrase, save it, done", "새 복구 문구를 저장하면 끝", "新しいリカバリーフレーズを保存すれば完了", "產生新復原字詞，儲存好就完成", "Nová obnovovací fráze, uložit, hotovo", "Nueva frase de recuperación, guárdala y listo", "新的恢复助记词，保存好即可", "Neue Wiederherstellungsphrase, speichern, fertig"),
    (sign_in_method, "Sign-in method", "로그인 방법", "サインイン方法", "登入方式", "Způsob přihlášení", "Método de inicio de sesión", "登录方式", "Anmeldemethode"),
    (generate_new_phrase, "Generate a new phrase", "새 문구 생성", "新しいフレーズを生成", "產生新字詞", "Vygenerovat novou frázi", "Generar una frase nueva", "生成新助记词", "Neue Phrase generieren"),
    (show, "Show", "표시", "表示", "顯示", "Zobrazit", "Mostrar", "显示", "Anzeigen"),
    (hide, "Hide", "숨기기", "非表示", "隱藏", "Skrýt", "Ocultar", "隐藏", "Ausblenden"),
    (show_phrase, "Show recovery phrase", "복구 문구 표시", "リカバリーフレーズを表示", "顯示復原字詞", "Zobrazit obnovovací frázi", "Mostrar la frase de recuperación", "显示恢复助记词", "Wiederherstellungsphrase anzeigen"),
    (hide_phrase, "Hide recovery phrase", "복구 문구 숨기기", "リカバリーフレーズを非表示", "隱藏復原字詞", "Skrýt obnovovací frázi", "Ocultar la frase de recuperación", "隐藏恢复助记词", "Wiederherstellungsphrase ausblenden"),
    (clear_phrase, "Clear recovery phrase", "복구 문구 지우기", "リカバリーフレーズを消去", "清除復原字詞", "Vymazat obnovovací frázi", "Borrar la frase de recuperación", "清除恢复助记词", "Wiederherstellungsphrase löschen"),
    (show_private_key, "Show private key", "개인 키 표시", "秘密鍵を表示", "顯示私密金鑰", "Zobrazit soukromý klíč", "Mostrar la clave privada", "显示私钥", "Privaten Schlüssel anzeigen"),
    (hide_private_key, "Hide private key", "개인 키 숨기기", "秘密鍵を非表示", "隱藏私密金鑰", "Skrýt soukromý klíč", "Ocultar la clave privada", "隐藏私钥", "Privaten Schlüssel ausblenden"),
    (clear_private_key, "Clear private key", "개인 키 지우기", "秘密鍵を消去", "清除私密金鑰", "Vymazat soukromý klíč", "Borrar la clave privada", "清除私钥", "Privaten Schlüssel löschen"),
    (wallet_index, "Wallet index", "지갑 인덱스", "ウォレットのインデックス", "錢包索引", "Index peněženky", "Índice de la cartera", "钱包索引", "Wallet-Index"),
    (prev_wallet_index, "Previous wallet index", "이전 지갑 인덱스", "前のインデックス", "上一個錢包索引", "Předchozí index", "Índice anterior", "上一个钱包索引", "Vorheriger Wallet-Index"),
    (next_wallet_index, "Next wallet index", "다음 지갑 인덱스", "次のインデックス", "下一個錢包索引", "Další index", "Índice siguiente", "下一个钱包索引", "Nächster Wallet-Index"),
    (stay_signed_in, "Stay signed in on this device", "이 기기에서 로그인 유지", "この端末でサインインしたままにする", "喺呢部裝置保持登入", "Zůstat přihlášen na tomto zařízení", "Mantener la sesión en este dispositivo", "在此设备上保持登录", "Auf diesem Gerät angemeldet bleiben"),
    (save_phrase_now, "Save this phrase now", "지금 이 문구를 저장하세요", "今すぐこのフレーズを保存", "而家就儲存呢串字詞", "Uložte si tuto frázi hned", "Guarda esta frase ahora", "立即保存此助记词", "Speichere diese Phrase jetzt"),
    (phrase_only_way_back, "It is the only way back into this account.", "이 계정으로 돌아올 수 있는 유일한 방법입니다.", "このアカウントに戻る唯一の方法です。", "呢個係返呢個帳戶嘅唯一方法。", "Je to jediná cesta zpět k tomuto účtu.", "Es la única forma de volver a esta cuenta.", "这是找回此账户的唯一方式。", "Sie ist der einzige Weg zurück zu diesem Konto."),
    (phrase_nobody_recovers, "Nobody — including this server — can recover it for you.", "이 서버를 포함해 누구도 대신 복구해 줄 수 없습니다.", "このサーバーを含め、誰も復元できません。", "冇人可以幫你復原，包括呢個伺服器。", "Nikdo — ani tento server — vám ji neobnoví.", "Nadie, ni este servidor, puede recuperarla por ti.", "没有人 — 包括此服务器 — 能替你找回它。", "Niemand — auch dieser Server nicht — kann sie für dich wiederherstellen."),
    (phrase_anyone_reads, "Anyone who has it can read every message you can.", "이 문구를 가진 사람은 당신이 읽는 모든 메시지를 읽을 수 있습니다.", "これを持つ人は、あなたと同じすべてのメッセージを読めます。", "任何人攞到都可以睇晒你睇到嘅訊息。", "Kdokoli ji má, přečte všechny vaše zprávy.", "Quien la tenga podrá leer todos tus mensajes.", "任何拿到它的人都能读到你的全部消息。", "Wer sie besitzt, kann alle deine Nachrichten lesen."),
    (unlock, "Unlock", "잠금 해제", "ロック解除", "解鎖", "Odemknout", "Desbloquear", "解锁", "Entsperren"),
    (sign_in_as_someone_else, "Sign in as someone else", "다른 계정으로 로그인", "別のアカウントでサインイン", "用另一個帳戶登入", "Přihlásit se jako někdo jiný", "Iniciar sesión con otra cuenta", "以其他身份登录", "Als jemand anderes anmelden"),

    // --- Settings -----------------------------------------------------------
    (profile, "Profile", "프로필", "プロフィール", "個人檔案", "Profil", "Perfil", "个人资料", "Profil"),
    (profile_image, "Profile image", "프로필 이미지", "プロフィール画像", "個人頭像", "Profilový obrázek", "Imagen de perfil", "头像", "Profilbild"),
    (avatar_pick, "Choose a face", "얼굴 선택", "顔を選ぶ", "揀一個面孔", "Vybrat tvář", "Elegir un rostro", "选择面孔", "Gesicht wählen"),
    (avatar_make_ai, "Make one with AI", "AI로 만들기", "AIで作る", "用 AI 整一個", "Vytvořit pomocí AI", "Crear con IA", "用 AI 生成", "Mit KI erstellen"),
    (avatar_upload, "Upload", "업로드", "アップロード", "上載", "Nahrát", "Subir", "上传", "Hochladen"),
    (avatar_default, "Use default", "기본값 사용", "デフォルトに戻す", "用返預設", "Použít výchozí", "Usar predeterminado", "使用默认", "Standard verwenden"),
    (avatar_updated, "Profile image updated", "프로필 이미지가 변경되었습니다", "プロフィール画像を更新しました", "個人頭像已更新", "Profilový obrázek aktualizován", "Imagen de perfil actualizada", "头像已更新", "Profilbild aktualisiert"),
    (avatar_update_failed, "Couldn't update the profile image", "프로필 이미지를 변경하지 못했습니다", "プロフィール画像を更新できませんでした", "無法更新個人頭像", "Profilový obrázek se nepodařilo změnit", "No se pudo actualizar la imagen de perfil", "无法更新头像", "Profilbild konnte nicht aktualisiert werden"),
    (avatar_ai_hint, "Describe the human half — the machine half is already Skynet's.", "인간 쪽 절반을 설명하세요 — 기계 쪽 절반은 이미 스카이넷의 것입니다.", "人間側の半分を説明してください — 機械側はすでにスカイネットのものです。", "描述人類嗰半 — 機器嗰半已經係天網嘅。", "Popište lidskou polovinu — strojová už patří Skynetu.", "Describe la mitad humana — la mitad máquina ya es de Skynet.", "描述人类的那一半 — 机器的那一半已经属于天网。", "Beschreibe die menschliche Hälfte — die Maschinenhälfte gehört schon Skynet."),
    (avatar_ai_placeholder, "e.g. a violinist with silver hair", "예: 은발의 바이올리니스트", "例: 銀髪のバイオリニスト", "例如：銀髮小提琴手", "např. houslistka se stříbrnými vlasy", "p. ej., una violinista de pelo plateado", "例如：银发小提琴家", "z. B. eine Geigerin mit silbernem Haar"),
    (avatar_need_ai_key, "Add an AI key with image support first — see the AI assistant section below.", "이미지를 지원하는 AI 키를 먼저 추가하세요 — 아래 AI 어시스턴트 섹션을 확인하세요.", "まず画像対応のAIキーを追加してください — 下のAIアシスタント欄をご覧ください。", "請先加一個支援圖像嘅 AI 密鑰 — 睇下面嘅 AI 助手部分。", "Nejprve přidejte AI klíč s podporou obrázků — viz sekce AI asistent níže.", "Añade primero una clave de IA con soporte de imágenes — mira la sección del asistente de IA más abajo.", "请先添加支持图像的 AI 密钥 — 见下方 AI 助手部分。", "Füge zuerst einen KI-Schlüssel mit Bildunterstützung hinzu — siehe KI-Assistent unten."),
    (connection_mode, "Connection mode", "연결 방식", "接続モード", "連線模式", "Režim připojení", "Modo de conexión", "连接方式", "Verbindungsmodus"),
    (pane_layout, "Pane layout", "화면 배치", "ペイン配置", "版面配置", "Rozvržení panelů", "Disposición de paneles", "窗格布局", "Fensteranordnung"),
    (recovery_phrase_on_device, "Recovery phrase on this device", "이 기기의 복구 문구", "この端末のリカバリーフレーズ", "呢部裝置嘅復原字詞", "Obnovovací fráze v tomto zařízení", "Frase de recuperación en este dispositivo", "此设备上的恢复助记词", "Wiederherstellungsphrase auf diesem Gerät"),
    (forget, "Forget", "삭제", "削除", "忘記", "Zapomenout", "Olvidar", "忘记", "Vergessen"),
    (erase, "Erase", "삭제", "消去", "清除", "Vymazat", "Borrar", "清除", "Löschen"),
    (page_not_found, "Page not found", "페이지를 찾을 수 없습니다", "ページが見つかりません", "找唔到呢一頁", "Stránka nenalezena", "Página no encontrada", "页面不存在", "Seite nicht gefunden"),
    (page_not_found_body, "That address doesn't point anywhere.", "이 주소는 어디로도 연결되지 않습니다.", "そのアドレスはどこにもつながっていません。", "呢個網址冇指向任何地方。", "Tato adresa nikam nevede.", "Esa dirección no lleva a ninguna parte.", "这个地址没有指向任何页面。", "Diese Adresse führt nirgendwohin."),
    (go_to_your_rooms, "Go to your rooms", "내 채팅방으로", "自分のルームへ", "去我嘅聊天室", "Přejít na místnosti", "Ir a tus salas", "前往你的聊天室", "Zu deinen Räumen"),
    (go_to_sign_in, "Go to sign in", "로그인으로", "サインインへ", "去登入", "Přejít na přihlášení", "Ir a iniciar sesión", "前往登录", "Zur Anmeldung"),

    // --- Dialogs: shared ----------------------------------------------------
    (done, "Done", "완료", "完了", "完成", "Hotovo", "Listo", "完成", "Fertig"),
    (save, "Save", "저장", "保存", "儲存", "Uložit", "Guardar", "保存", "Speichern"),
    (room_name, "Room name", "채팅방 이름", "ルーム名", "聊天室名稱", "Název místnosti", "Nombre de la sala", "聊天室名称", "Raumname"),
    (rename_room, "Rename room", "채팅방 이름 변경", "ルーム名を変更", "重新命名聊天室", "Přejmenovat místnost", "Cambiar nombre de la sala", "重命名聊天室", "Raum umbenennen"),
    (wallet_address_label, "Wallet address", "지갑 주소", "ウォレットアドレス", "錢包地址", "Adresa peněženky", "Dirección de cartera", "钱包地址", "Wallet-Adresse"),
    (block, "Block", "차단", "ブロック", "封鎖", "Blokovat", "Bloquear", "屏蔽", "Blockieren"),
    (unblock, "Unblock", "차단 해제", "ブロック解除", "解除封鎖", "Odblokovat", "Desbloquear", "取消屏蔽", "Blockierung aufheben"),
    (no_one_blocked, "No one blocked", "차단한 사용자가 없습니다", "ブロック中の人はいません", "冇封鎖任何人", "Nikdo není blokován", "Nadie bloqueado", "没有屏蔽任何人", "Niemand blockiert"),
    (unhide, "Unhide", "다시 표시", "再表示", "取消隱藏", "Zrušit skrytí", "Mostrar de nuevo", "取消隐藏", "Wieder einblenden"),
    (no_hidden_rooms, "No hidden rooms", "숨긴 채팅방이 없습니다", "非表示のルームはありません", "冇隱藏嘅聊天室", "Žádné skryté místnosti", "No hay salas ocultas", "没有隐藏的聊天室", "Keine ausgeblendeten Räume"),
    (couldnt_load_hidden, "Couldn't load hidden rooms", "숨긴 채팅방을 불러오지 못했습니다", "非表示のルームを読み込めませんでした", "無法載入隱藏嘅聊天室", "Skryté místnosti se nepodařilo načíst", "No se pudieron cargar las salas ocultas", "无法加载隐藏的聊天室", "Ausgeblendete Räume konnten nicht geladen werden"),

    // --- Dialogs: admins ----------------------------------------------------
    (manage_admins, "Manage admins", "관리자 관리", "管理者を管理", "管理管理員", "Spravovat správce", "Gestionar administradores", "管理管理员", "Admins verwalten"),
    (current_admins, "Current admins", "현재 관리자", "現在の管理者", "現時管理員", "Současní správci", "Administradores actuales", "当前管理员", "Aktuelle Admins"),
    (add_an_admin, "Add an admin", "관리자 추가", "管理者を追加", "新增管理員", "Přidat správce", "Añadir administrador", "添加管理员", "Admin hinzufügen"),
    (make_admin, "Make admin", "관리자로 지정", "管理者にする", "設為管理員", "Nastavit správcem", "Hacer administrador", "设为管理员", "Zum Admin machen"),
    (remove, "Remove", "해제", "解除", "移除", "Odebrat", "Quitar", "移除", "Entfernen"),
    (need_one_admin, "A room needs at least one admin.", "채팅방에는 관리자가 최소 한 명 필요합니다.", "ルームには管理者が少なくとも1人必要です。", "聊天室最少要有一個管理員。", "Místnost potřebuje alespoň jednoho správce.", "Una sala necesita al menos un administrador.", "聊天室至少需要一名管理员。", "Ein Raum braucht mindestens einen Admin."),
    (admin_limit_reached, "Admin limit reached. Remove an admin to add another.", "관리자 수가 한도에 도달했습니다. 한 명을 해제해야 추가할 수 있습니다.", "管理者数が上限です。追加するには誰かを解除してください。", "管理員數量已滿，要移除一個先可以再加。", "Dosažen limit správců. Nejprve někoho odeberte.", "Límite de administradores alcanzado. Quita uno para añadir otro.", "已达到管理员上限。移除一名后才能再添加。", "Admin-Limit erreicht. Entferne einen Admin, um einen weiteren hinzuzufügen."),
    (everyone_is_admin, "Every other member is already an admin.", "다른 멤버는 모두 이미 관리자입니다.", "他のメンバーは全員すでに管理者です。", "其他成員全部都已經係管理員。", "Všichni ostatní členové už jsou správci.", "Todos los demás miembros ya son administradores.", "其他成员都已是管理员。", "Alle anderen Mitglieder sind bereits Admins."),

    // --- Dialogs: invite ----------------------------------------------------
    (invite, "Invite", "초대", "招待", "邀請", "Pozvat", "Invitar", "邀请", "Einladen"),
    (invited, "Invited", "초대함", "招待済み", "已邀請", "Pozván", "Invitado", "已邀请", "Eingeladen"),
    (already_a_member, "Already a member", "이미 멤버입니다", "すでにメンバーです", "已經係成員", "Už je členem", "Ya es miembro", "已是成员", "Bereits Mitglied"),
    (search_for_someone, "Search for someone", "사람 검색", "ユーザーを検索", "搜尋用戶", "Vyhledat člověka", "Buscar a alguien", "搜索用户", "Jemanden suchen"),
    (search_to_invite, "Search for someone to invite", "초대할 사람 검색", "招待するユーザーを検索", "搜尋想邀請嘅人", "Vyhledat koho pozvat", "Buscar a quién invitar", "搜索要邀请的人", "Jemanden zum Einladen suchen"),
    (username_or_address, "Username or 0x address", "사용자 이름 또는 0x 주소", "ユーザー名または0xアドレス", "使用者名稱或 0x 地址", "Jméno nebo adresa 0x", "Nombre de usuario o dirección 0x", "用户名或 0x 地址", "Benutzername oder 0x-Adresse"),
    (search_failed, "Search failed", "검색에 실패했습니다", "検索に失敗しました", "搜尋失敗", "Hledání selhalo", "La búsqueda falló", "搜索失败", "Suche fehlgeschlagen"),

    // --- Dialogs: create room ------------------------------------------------
    (create_a_room, "Create a room", "채팅방 만들기", "ルームを作成", "建立聊天室", "Vytvořit místnost", "Crear una sala", "创建聊天室", "Raum erstellen"),
    (create, "Create", "만들기", "作成", "建立", "Vytvořit", "Crear", "创建", "Erstellen"),
    (description_optional, "Description (optional)", "설명 (선택)", "説明（任意）", "描述（選填）", "Popis (nepovinné)", "Descripción (opcional)", "描述（可选）", "Beschreibung (optional)"),
    (or_set_it_up_yourself, "OR SET IT UP YOURSELF", "또는 직접 설정하기", "または自分で設定する", "或者自行設定", "NEBO NASTAVIT RUČNĚ", "O CONFIGÚRALA TÚ MISMO", "或自己设置", "ODER SELBST EINRICHTEN"),
    (fast_room_tagline, "Named for you, encrypted, and opened — one click", "이름 자동 생성, 암호화, 바로 열기 — 한 번의 클릭", "自動命名・暗号化・そのまま開く — ワンクリック", "自動命名、加密、即刻開啟 — 一撳就得", "Pojmenovaná, šifrovaná a otevřená — jedním kliknutím", "Con nombre, cifrada y abierta: un clic", "自动命名、已加密并打开 — 一键完成", "Benannt, verschlüsselt und geöffnet — mit einem Klick"),
    (unlock_to_encrypt, "Unlock your wallet to create an encrypted room.", "암호화된 채팅방을 만들려면 지갑을 잠금 해제하세요.", "暗号化ルームを作るにはウォレットのロックを解除してください。", "要解鎖錢包先可以建立加密聊天室。", "Pro šifrovanou místnost odemkněte peněženku.", "Desbloquea tu cartera para crear una sala cifrada.", "解锁钱包以创建加密聊天室。", "Entsperre deine Wallet, um einen verschlüsselten Raum zu erstellen."),

    // --- Wallet -------------------------------------------------------------
    (network, "Network", "네트워크", "ネットワーク", "網絡", "Síť", "Red", "网络", "Netzwerk"),
    (asset, "Asset", "자산", "資産", "資產", "Aktivum", "Activo", "资产", "Asset"),
    (recipient, "Recipient", "받는 주소", "送信先", "收款人", "Příjemce", "Destinatario", "收款地址", "Empfänger"),
    (amount, "Amount", "금액", "金額", "金額", "Částka", "Cantidad", "金额", "Betrag"),
    (advanced_settings, "Advanced settings", "고급 설정", "詳細設定", "進階設定", "Pokročilé nastavení", "Ajustes avanzados", "高级设置", "Erweiterte Einstellungen"),
    (gas_price_gwei, "Gas price (gwei)", "가스 가격 (gwei)", "ガス価格（gwei）", "Gas 價格（gwei）", "Cena plynu (gwei)", "Precio del gas (gwei)", "Gas 价格（gwei）", "Gaspreis (Gwei)"),
    (gas_limit, "Gas limit", "가스 한도", "ガス上限", "Gas 上限", "Limit plynu", "Límite de gas", "Gas 上限", "Gaslimit"),
    (gas, "Gas", "가스", "ガス", "Gas 費", "Plyn", "Gas", "Gas", "Gas"),
    (gas_used, "Gas used", "사용한 가스", "使用ガス量", "已用 Gas", "Spotřebovaný plyn", "Gas usado", "已用 Gas", "Verbrauchtes Gas"),
    (data_optional, "Data (text or 0x hex, optional)", "데이터 (텍스트 또는 0x 16진수, 선택)", "データ（テキストまたは0x16進数、任意）", "資料（文字或 0x 十六進位，選填）", "Data (text nebo 0x hex, nepovinné)", "Datos (texto o hex 0x, opcional)", "数据（文本或 0x 十六进制，可选）", "Daten (Text oder 0x-Hex, optional)"),
    (estimate, "Estimate", "예상 계산", "見積もり", "估算", "Odhadnout", "Estimar", "估算", "Schätzen"),
    (review_send, "Review send", "전송 검토", "送信内容を確認", "檢查後傳送", "Zkontrolovat odeslání", "Revisar el envío", "检查并发送", "Senden prüfen"),
    (back, "Back", "뒤로", "戻る", "返回", "Zpět", "Atrás", "返回", "Zurück"),
    (balance_before, "Balance before", "이전 잔액", "送信前の残高", "傳送前餘額", "Zůstatek před", "Saldo anterior", "发送前余额", "Saldo vorher"),
    (balance_after, "Balance after", "이후 잔액", "送信後の残高", "傳送後餘額", "Zůstatek po", "Saldo posterior", "发送后余额", "Saldo nachher"),
    (refresh_balances, "Refresh balances", "잔액 새로고침", "残高を更新", "重新整理餘額", "Obnovit zůstatky", "Actualizar saldos", "刷新余额", "Salden aktualisieren"),
    (broadcasting, "Broadcasting to the network…", "네트워크에 전송 중…", "ネットワークに送信中…", "廣播到網絡中…", "Odesílání do sítě…", "Difundiendo a la red…", "正在向网络广播…", "Wird ans Netzwerk übertragen…"),
    (large_amount_warning, "Large amount — double-check the recipient.", "큰 금액입니다 — 받는 주소를 다시 확인하세요.", "高額です — 送信先をもう一度確認してください。", "金額唔細 — 再確認收款人。", "Vysoká částka — zkontrolujte příjemce.", "Cantidad grande: verifica el destinatario.", "金额较大 — 请再次核对收款地址。", "Großer Betrag — prüfe den Empfänger noch einmal."),
    (very_large_amount, "Very large amount. Retype it exactly to arm the send button.", "매우 큰 금액입니다. 전송 버튼을 활성화하려면 금액을 정확히 다시 입력하세요.", "非常に高額です。送信ボタンを有効にするには金額を正確に再入力してください。", "金額非常大。要再打多次一模一樣先可以撳傳送。", "Velmi vysoká částka. Pro odeslání ji přesně přepište.", "Cantidad muy grande. Vuelve a escribirla exactamente para habilitar el envío.", "金额非常大。请原样重新输入以启用发送按钮。", "Sehr großer Betrag. Tippe ihn exakt erneut ein, um den Senden-Knopf zu aktivieren."),
    (retype_the_amount, "Retype the amount", "금액 다시 입력", "金額を再入力", "再輸入一次金額", "Přepište částku", "Vuelve a escribir la cantidad", "重新输入金额", "Betrag erneut eingeben"),
    (registry_not_loaded, "The network registry hasn't loaded yet. Try again in a moment.", "네트워크 정보를 아직 불러오지 못했습니다. 잠시 후 다시 시도하세요.", "ネットワーク情報がまだ読み込まれていません。少し待って再試行してください。", "網絡資料仲未載入，等陣再試。", "Registr sítí se ještě nenačetl. Zkuste to za chvíli.", "El registro de redes aún no ha cargado. Inténtalo en un momento.", "网络注册表尚未加载。请稍后重试。", "Die Netzwerkliste ist noch nicht geladen. Versuche es gleich noch einmal."),
    (testnet_badge, "TESTNET", "테스트넷", "テストネット", "測試網", "TESTNET", "RED DE PRUEBAS", "测试网", "TESTNETZ"),
    // --- the Vault Warden (wallet dialog avatar) ---
    // One duty-status line per dialog phase, spoken in the machine's own
    // clipped report tone — statements, not chatter.
    (warden_name, "Vault Warden", "금고 수호자", "金庫の番人", "金庫守衛", "Strážce trezoru", "Guardián de la bóveda", "金库守卫", "Tresorwächter"),
    (warden_idle, "Vault online. Assets secured.", "금고 온라인. 자산이 안전합니다.", "金庫オンライン。資産は安全です。", "金庫上線。資產穩妥。", "Trezor online. Aktiva zajištěna.", "Bóveda en línea. Activos asegurados.", "金库在线。资产安全。", "Tresor online. Vermögen gesichert."),
    (warden_scan, "Scanning the chain…", "체인 스캔 중…", "チェーンをスキャン中…", "掃描緊鏈上資料…", "Skenuji řetězec…", "Escaneando la cadena…", "正在扫描链上数据…", "Scanne die Chain…"),
    (warden_target, "Designate recipient and amount.", "받는 주소와 금액을 지정하세요.", "宛先と金額を指定してください。", "指定收款地址同金額。", "Zadejte příjemce a částku.", "Indica destinatario y cantidad.", "指定收款地址和金额。", "Empfänger und Betrag festlegen."),
    (warden_arm, "Final authorization required.", "최종 승인이 필요합니다.", "最終承認が必要です。", "需要最終授權。", "Je vyžadováno konečné schválení.", "Se requiere autorización final.", "需要最终授权。", "Letzte Freigabe erforderlich."),
    (warden_relay, "Relaying to the network…", "네트워크로 중계 중…", "ネットワークへ中継中…", "傳送緊去網絡…", "Přenáším do sítě…", "Retransmitiendo a la red…", "正在中继到网络…", "Übertrage ins Netzwerk…"),
    (warden_ok, "Mission complete. Funds delivered.", "임무 완료. 자금이 전달되었습니다.", "任務完了。資金を送金しました。", "任務完成。資金已送到。", "Mise splněna. Prostředky doručeny.", "Misión cumplida. Fondos entregados.", "任务完成。资金已送达。", "Mission erfüllt. Mittel zugestellt."),
    (warden_fail, "Transfer failed. Regroup and retry.", "전송 실패. 다시 시도하세요.", "送金失敗。再試行してください。", "轉帳失敗。請再試。", "Převod selhal. Zkuste to znovu.", "Transferencia fallida. Inténtalo de nuevo.", "转账失败。请重试。", "Überweisung fehlgeschlagen. Bitte erneut versuchen."),

    // --- AI assistant --------------------------------------------------------
    (assistant, "Assistant", "어시스턴트", "アシスタント", "助手", "Asistent", "Asistente", "助手", "Assistent"),
    (generate, "Generate", "생성", "生成", "產生", "Generovat", "Generar", "生成", "Generieren"),
    (generating, "Generating…", "생성 중…", "生成中…", "產生中…", "Generuji…", "Generando…", "生成中…", "Generiert…"),
    (generate_image, "Generate image", "이미지 생성", "画像を生成", "產生圖片", "Vygenerovat obrázek", "Generar imagen", "生成图片", "Bild generieren"),
    (post_to_room, "Post to room", "채팅방에 올리기", "ルームに投稿", "貼去聊天室", "Odeslat do místnosti", "Publicar en la sala", "发送到聊天室", "In den Raum posten"),
    (copy, "Copy", "복사", "コピー", "複製", "Kopírovat", "Copiar", "复制", "Kopieren"),
    (test, "Test", "테스트", "テスト", "測試", "Otestovat", "Probar", "测试", "Testen"),
    (anthropic_text_only, "Anthropic is text-only; image generation uses Grok, OpenAI or Gemini.", "Anthropic은 텍스트 전용입니다. 이미지 생성은 Grok, OpenAI 또는 Gemini를 사용합니다.", "Anthropicはテキスト専用です。画像生成にはGrok、OpenAI、Geminiを使います。", "Anthropic 淨係支援文字；產生圖片要用 Grok、OpenAI 或者 Gemini。", "Anthropic zvládá jen text; obrázky generuje Grok, OpenAI nebo Gemini.", "Anthropic es solo texto; la generación de imágenes usa Grok, OpenAI o Gemini.", "Anthropic 仅支持文本；图片生成使用 Grok、OpenAI 或 Gemini。", "Anthropic ist nur Text; Bilder erzeugen Grok, OpenAI oder Gemini."),

    // --- Boot ---------------------------------------------------------------
    (skip, "SKIP", "건너뛰기", "スキップ", "略過", "PŘESKOČIT", "OMITIR", "跳过", "ÜBERSPRINGEN"),

    // --- Empty-state and dialog descriptions --------------------------------
    (message_actions, "Message actions", "메시지 메뉴", "メッセージ操作", "訊息選項", "Akce zprávy", "Acciones del mensaje", "消息操作", "Nachrichtenaktionen"),
    (choose_emoticon, "Choose an emoticon", "이모티콘 선택", "絵文字を選ぶ", "揀個表情符號", "Vyberte emotikon", "Elige un emoticono", "选择表情", "Emoticon auswählen"),
    (no_search_results, "Try a different word, or clear the search.", "다른 단어로 검색하거나 검색어를 지우세요.", "別の言葉で検索するか、検索を消してください。", "換個字再搵，或者清除搜尋。", "Zkuste jiné slovo, nebo hledání zrušte.", "Prueba otra palabra, o borra la búsqueda.", "换个词试试，或清除搜索。", "Versuche ein anderes Wort oder lösche die Suche."),
    (nothing_found, "Nothing found", "결과가 없습니다", "見つかりません", "搵唔到", "Nic nenalezeno", "Sin resultados", "未找到任何内容", "Nichts gefunden"),
    (invitations_empty_body, "When someone invites you to a room, it shows up here.", "누군가 채팅방에 초대하면 여기에 표시됩니다.", "誰かがルームに招待すると、ここに表示されます。", "有人邀請你入聊天室嘅時候就會喺呢度出現。", "Až vás někdo pozve do místnosti, objeví se to zde.", "Cuando alguien te invite a una sala, aparecerá aquí.", "有人邀请你加入聊天室时，会显示在这里。", "Wenn dich jemand in einen Raum einlädt, erscheint es hier."),
    (keys_stay_in_browser, "Keys stay in this browser. Prompts go straight to the provider.", "키는 이 브라우저에 남습니다. 프롬프트는 제공자에게 직접 전송됩니다.", "キーはこのブラウザに保存されます。プロンプトは提供元へ直接送信されます。", "金鑰只留喺呢個瀏覽器。提示會直接送去供應商。", "Klíče zůstávají v tomto prohlížeči. Dotazy jdou přímo poskytovateli.", "Las claves se quedan en este navegador. Los mensajes van directo al proveedor.", "密钥只保存在此浏览器中。提示词直接发给提供商。", "Schlüssel bleiben in diesem Browser. Prompts gehen direkt an den Anbieter."),
    (hiding_a_room_note, "Hiding a room removes it from your list but keeps you a member.", "채팅방을 숨기면 목록에서 사라지지만 멤버 자격은 유지됩니다.", "ルームを非表示にすると一覧から消えますが、メンバーのままです。", "隱藏聊天室會喺清單度消失，但你仍然係成員。", "Skrytí místnosti ji odebere ze seznamu, členem zůstáváte.", "Ocultar una sala la quita de tu lista, pero sigues siendo miembro.", "隐藏聊天室只是从列表中移除，你仍是成员。", "Einen Raum auszublenden entfernt ihn aus deiner Liste, du bleibst aber Mitglied."),
    (delete_message_title, "Delete message?", "메시지를 삭제할까요?", "メッセージを削除しますか？", "刪除呢條訊息？", "Smazat zprávu?", "¿Eliminar el mensaje?", "删除这条消息？", "Nachricht löschen?"),
    (delete_message_body, "This removes it for everyone. It can't be undone.", "모든 사람에게서 삭제되며 되돌릴 수 없습니다.", "全員から削除され、元に戻せません。", "會為所有人刪除，冇得復原。", "Odstraní se všem. Nelze vzít zpět.", "Se elimina para todos. No se puede deshacer.", "将对所有人删除，且无法撤销。", "Sie wird für alle entfernt. Das lässt sich nicht rückgängig machen."),
    (join_once_accept, "They join once they accept.", "상대가 수락하면 참여합니다.", "相手が承諾すると参加します。", "佢哋接受咗就會加入。", "Připojí se, jakmile pozvánku přijmou.", "Se unirán en cuanto acepten.", "对方接受后即加入。", "Sie treten bei, sobald sie annehmen."),
    (search_by_username, "Search by username, or paste a wallet address.", "사용자 이름으로 검색하거나 지갑 주소를 붙여넣으세요.", "ユーザー名で検索するか、ウォレットアドレスを貼り付けてください。", "用使用者名稱搵，或者貼上錢包地址。", "Hledejte podle jména, nebo vložte adresu peněženky.", "Busca por nombre de usuario, o pega una dirección de cartera.", "按用户名搜索，或粘贴钱包地址。", "Suche per Benutzername oder füge eine Wallet-Adresse ein."),
    (rooms_are_private, "Rooms are private. Invite people by wallet.", "채팅방은 비공개입니다. 지갑 주소로 초대하세요.", "ルームは非公開です。ウォレットで招待します。", "聊天室係私人嘅，用錢包地址邀請人。", "Místnosti jsou soukromé. Zvěte lidi přes peněženku.", "Las salas son privadas. Invita con la cartera.", "聊天室是私密的。请按钱包地址邀请。", "Räume sind privat. Lade per Wallet ein."),
    (admins_can_note, "Admins can invite, rename, remove members and delete the room.", "관리자는 초대, 이름 변경, 멤버 제거, 채팅방 삭제를 할 수 있습니다.", "管理者は招待・名前変更・メンバー削除・ルーム削除ができます。", "管理員可以邀請、改名、移除成員同刪除聊天室。", "Správci mohou zvát, přejmenovat, odebírat členy a smazat místnost.", "Los administradores pueden invitar, renombrar, quitar miembros y eliminar la sala.", "管理员可以邀请、重命名、移除成员和删除聊天室。", "Admins können einladen, umbenennen, Mitglieder entfernen und den Raum löschen."),
    (blocked_note, "Blocked people can't invite you to rooms, and you won't see their messages.", "차단한 사람은 당신을 초대할 수 없고, 그들의 메시지도 보이지 않습니다.", "ブロックした人はあなたを招待できず、メッセージも表示されません。", "被封鎖嘅人唔可以邀請你，你亦唔會見到佢哋嘅訊息。", "Blokovaní vás nemohou zvát a jejich zprávy neuvidíte.", "Las personas bloqueadas no pueden invitarte, y no verás sus mensajes.", "被屏蔽的人不能邀请你，你也看不到他们的消息。", "Blockierte Personen können dich nicht einladen, und du siehst ihre Nachrichten nicht."),
    (block_from_row, "Block someone from their row in a room, or paste an address above.", "채팅방의 멤버 행에서 차단하거나, 위에 주소를 붙여넣으세요.", "ルームのメンバー行からブロックするか、上にアドレスを貼り付けてください。", "喺聊天室嘅成員列封鎖，或者喺上面貼地址。", "Blokujte z řádku člena v místnosti, nebo vložte adresu výše.", "Bloquea desde la fila de un miembro, o pega una dirección arriba.", "在聊天室成员行中屏蔽某人，或在上方粘贴地址。", "Blockiere jemanden über seine Zeile im Raum oder füge oben eine Adresse ein."),

    // --- Toasts and action results ------------------------------------------
    //
    // Placeholders live *inside* the translation (`{name}`, `{short}`) rather
    // than being concatenated in Rust. Word order is not a constant across
    // languages — Korean and Japanese put the verb last, so "Joined {name}"
    // has to become "{name}에 참여했습니다" — and a sentence assembled from
    // fragments in English order cannot express that. Substituted with
    // `.replace()` at the call site.
    (address_copied, "Address copied", "주소를 복사했습니다", "アドレスをコピーしました", "已複製地址", "Adresa zkopírována", "Dirección copiada", "地址已复制", "Adresse kopiert"),
    (copied, "Copied", "복사했습니다", "コピーしました", "已複製", "Zkopírováno", "Copiado", "已复制", "Kopiert"),
    (phrase_copied, "Recovery phrase copied", "복구 문구를 복사했습니다", "リカバリーフレーズをコピーしました", "已複製復原字詞", "Obnovovací fráze zkopírována", "Frase de recuperación copiada", "恢复助记词已复制", "Wiederherstellungsphrase kopiert"),
    (phrase_forgotten, "Recovery phrase forgotten", "복구 문구를 삭제했습니다", "リカバリーフレーズを削除しました", "已忘記復原字詞", "Obnovovací fráze zapomenuta", "Frase de recuperación olvidada", "恢复助记词已忘记", "Wiederherstellungsphrase vergessen"),
    (backup_downloaded, "Backup file downloaded", "백업 파일을 내려받았습니다", "バックアップファイルを保存しました", "已下載備份檔", "Záložní soubor stažen", "Archivo de respaldo descargado", "备份文件已下载", "Sicherungsdatei heruntergeladen"),
    (back_online, "Back online", "다시 연결되었습니다", "オンラインに戻りました", "重新連線", "Zpět online", "De nuevo en línea", "已恢复在线", "Wieder online"),
    (signed_in_as, "Signed in as {name}", "{name}(으)로 로그인했습니다", "{name} としてサインインしました", "已用 {name} 登入", "Přihlášen jako {name}", "Sesión iniciada como {name}", "已登录为 {name}", "Angemeldet als {name}"),
    (couldnt_sign_in, "Couldn't sign in", "로그인하지 못했습니다", "サインインできませんでした", "無法登入", "Přihlášení se nezdařilo", "No se pudo iniciar sesión", "登录失败", "Anmeldung fehlgeschlagen"),
    (room_created, "Room created", "채팅방을 만들었습니다", "ルームを作成しました", "已建立聊天室", "Místnost vytvořena", "Sala creada", "聊天室已创建", "Raum erstellt"),
    (room_created_named, "Room created — {name}", "채팅방을 만들었습니다 — {name}", "ルームを作成しました — {name}", "已建立聊天室 — {name}", "Místnost vytvořena — {name}", "Sala creada: {name}", "聊天室已创建 — {name}", "Raum erstellt — {name}"),
    (couldnt_create_room, "Couldn't create a room", "채팅방을 만들지 못했습니다", "ルームを作成できませんでした", "無法建立聊天室", "Místnost se nepodařilo vytvořit", "No se pudo crear la sala", "无法创建聊天室", "Raum konnte nicht erstellt werden"),
    (joined_room, "Joined {name}", "{name}에 참여했습니다", "{name} に参加しました", "已加入 {name}", "Připojeno k {name}", "Te uniste a {name}", "已加入 {name}", "{name} beigetreten"),
    (invitation_gone, "That invitation is no longer available", "그 초대는 더 이상 유효하지 않습니다", "その招待はもう利用できません", "嗰個邀請已經冇效", "Tato pozvánka už není dostupná", "Esa invitación ya no está disponible", "该邀请已失效", "Diese Einladung ist nicht mehr verfügbar"),
    (new_invitation, "New invitation", "새 초대", "新しい招待", "新邀請", "Nová pozvánka", "Nueva invitación", "新邀请", "Neue Einladung"),
    (invite_sent, "{name} joins once they accept", "{name}이(가) 수락하면 참여합니다", "{name} が承諾すると参加します", "{name} 接受咗就會加入", "{name} se připojí po přijetí", "{name} se unirá al aceptar", "{name} 接受后即加入", "{name} tritt bei, sobald angenommen"),
    (blocked_someone, "Blocked {short}", "{short}을(를) 차단했습니다", "{short} をブロックしました", "已封鎖 {short}", "{short} blokován", "{short} bloqueado", "已屏蔽 {short}", "{short} blockiert"),
    (unblocked_someone, "Unblocked {short}", "{short}의 차단을 해제했습니다", "{short} のブロックを解除しました", "已解除封鎖 {short}", "{short} odblokován", "{short} desbloqueado", "已取消屏蔽 {short}", "Blockierung von {short} aufgehoben"),
    (room_unhidden, "Room unhidden", "채팅방을 다시 표시했습니다", "ルームを再表示しました", "已取消隱藏聊天室", "Místnost znovu zobrazena", "Sala visible de nuevo", "聊天室已取消隐藏", "Raum wieder eingeblendet"),
    (room_key_rotated, "Room key rotated", "채팅방 키를 교체했습니다", "ルームの鍵を更新しました", "已更換聊天室金鑰", "Klíč místnosti vyměněn", "Clave de la sala rotada", "聊天室密钥已轮换", "Raumschlüssel rotiert"),
    (transaction_confirmed, "Transaction confirmed", "트랜잭션이 확인되었습니다", "取引が確定しました", "交易已確認", "Transakce potvrzena", "Transacción confirmada", "交易已确认", "Transaktion bestätigt"),
    (cant_edit, "Can't edit", "편집할 수 없습니다", "編集できません", "無法編輯", "Nelze upravit", "No se puede editar", "无法编辑", "Bearbeiten nicht möglich"),
    (couldnt_save_edit, "Couldn't save the edit", "편집을 저장하지 못했습니다", "編集を保存できませんでした", "無法儲存編輯", "Úpravu se nepodařilo uložit", "No se pudo guardar la edición", "无法保存编辑", "Änderung konnte nicht gespeichert werden"),
    (couldnt_copy, "Couldn't copy", "복사하지 못했습니다", "コピーできませんでした", "無法複製", "Nepodařilo se zkopírovat", "No se pudo copiar", "无法复制", "Kopieren fehlgeschlagen"),
    (clipboard_blocked, "Your browser blocked clipboard access.", "브라우저가 클립보드 접근을 차단했습니다.", "ブラウザがクリップボードへのアクセスをブロックしました。", "瀏覽器封鎖咗剪貼簿存取。", "Prohlížeč zablokoval přístup ke schránce.", "Tu navegador bloqueó el acceso al portapapeles.", "浏览器阻止了剪贴板访问。", "Dein Browser hat den Zugriff auf die Zwischenablage blockiert."),

    // --- Encryption and network errors ---------------------------------------
    (no_room_key_yet, "This room is encrypted and you don't have its key yet.", "이 채팅방은 암호화되어 있으며 아직 키가 없습니다.", "このルームは暗号化されていますが、まだ鍵がありません。", "呢個聊天室加咗密，但你仲未有金鑰。", "Tato místnost je šifrovaná a zatím nemáte její klíč.", "Esta sala está cifrada y aún no tienes su clave.", "此聊天室已加密，你还没有它的密钥。", "Dieser Raum ist verschlüsselt und du hast seinen Schlüssel noch nicht."),
    (no_current_key, "You don't have the room's current key. Try reopening the room.", "채팅방의 현재 키가 없습니다. 채팅방을 다시 열어 보세요.", "ルームの現在の鍵がありません。ルームを開き直してください。", "你冇聊天室嘅現行金鑰，試下重新開返個聊天室。", "Nemáte aktuální klíč místnosti. Zkuste ji otevřít znovu.", "No tienes la clave actual de la sala. Prueba a abrirla de nuevo.", "你没有聊天室的当前密钥。请尝试重新打开。", "Du hast den aktuellen Raumschlüssel nicht. Öffne den Raum erneut."),
    (someone_left_rotate, "Someone left this room. Rotate the key before posting.", "누군가 채팅방을 떠났습니다. 글을 올리기 전에 키를 교체하세요.", "誰かがこのルームを退出しました。投稿する前に鍵を更新してください。", "有人離開咗呢個聊天室，發言之前要先換金鑰。", "Někdo místnost opustil. Před psaním vyměňte klíč.", "Alguien salió de la sala. Rota la clave antes de publicar.", "有人离开了此聊天室。发消息前请先轮换密钥。", "Jemand hat diesen Raum verlassen. Rotiere den Schlüssel, bevor du postest."),
    (unlock_before_rotate, "Unlock your wallet before rotating the room key.", "채팅방 키를 교체하려면 먼저 지갑을 잠금 해제하세요.", "ルームの鍵を更新する前にウォレットのロックを解除してください。", "換金鑰之前要先解鎖錢包。", "Před výměnou klíče odemkněte peněženku.", "Desbloquea tu cartera antes de rotar la clave.", "轮换聊天室密钥前请先解锁钱包。", "Entsperre deine Wallet, bevor du den Raumschlüssel rotierst."),
    (couldnt_read_members, "Couldn't read the room's members.", "채팅방 멤버를 읽지 못했습니다.", "ルームのメンバーを読み込めませんでした。", "讀唔到聊天室嘅成員。", "Nepodařilo se načíst členy místnosti.", "No se pudieron leer los miembros de la sala.", "无法读取聊天室成员。", "Die Mitglieder des Raums konnten nicht gelesen werden."),
    (someone_rotated_first, "Someone else rotated the key first — try posting again.", "다른 사람이 먼저 키를 교체했습니다 — 다시 시도하세요.", "他の誰かが先に鍵を更新しました。もう一度投稿してください。", "有第二個人早咗換金鑰 — 再試多次。", "Klíč vyměnil někdo jiný — zkuste odeslat znovu.", "Otra persona rotó la clave primero: inténtalo de nuevo.", "其他人先轮换了密钥 — 请重新发送。", "Jemand anderes hat den Schlüssel zuerst rotiert — poste erneut."),
    (unlock_wallet_first, "Unlock your wallet first", "먼저 지갑을 잠금 해제하세요", "先にウォレットのロックを解除してください", "要先解鎖錢包", "Nejprve odemkněte peněženku", "Desbloquea tu cartera primero", "请先解锁钱包", "Entsperre zuerst deine Wallet"),
    (fast_room_needs_phrase, "A fast room is always encrypted, and encryption needs your recovery phrase.", "빠른 채팅방은 항상 암호화되며, 암호화에는 복구 문구가 필요합니다.", "すぐ作成するルームは常に暗号化され、暗号化にはリカバリーフレーズが必要です。", "快速聊天室一定會加密，而加密需要你嘅復原字詞。", "Rychlá místnost je vždy šifrovaná a šifrování vyžaduje obnovovací frázi.", "Una sala rápida siempre está cifrada, y el cifrado necesita tu frase de recuperación.", "快速聊天室始终加密，而加密需要你的恢复助记词。", "Ein Schnellraum ist immer verschlüsselt, und Verschlüsselung braucht deine Wiederherstellungsphrase."),
    (cant_reach_server, "Can't reach the server. Check your connection and try again.", "서버에 연결할 수 없습니다. 연결 상태를 확인하고 다시 시도하세요.", "サーバーに接続できません。接続を確認してもう一度お試しください。", "連唔到伺服器，檢查下連線再試。", "Nelze se spojit se serverem. Zkontrolujte připojení.", "No se puede conectar con el servidor. Revisa tu conexión.", "无法连接服务器。请检查网络后重试。", "Server nicht erreichbar. Prüfe deine Verbindung und versuche es erneut."),
    (save_phrase_to_continue, "Save the phrase to continue", "계속하려면 문구를 저장하세요", "続けるにはフレーズを保存してください", "要繼續就要先儲存字詞", "Pokračujte uložením fráze", "Guarda la frase para continuar", "保存助记词以继续", "Speichere die Phrase, um fortzufahren"),
    (syncing, "Syncing", "동기화 중", "同期中", "同步中", "Synchronizace", "Sincronizando", "同步中", "Synchronisiert"),
    (conn_live_aria, "Connection: Live (WebSocket). Switch to polling.", "연결: 실시간(WebSocket). 폴링으로 전환합니다.", "接続: ライブ（WebSocket）。ポーリングに切り替えます。", "連線：即時（WebSocket）。切換去輪詢。", "Připojení: živě (WebSocket). Přepnout na dotazování.", "Conexión: en vivo (WebSocket). Cambiar a sondeo.", "连接：实时（WebSocket）。切换到轮询。", "Verbindung: Live (WebSocket). Zu Polling wechseln."),
    (conn_events_aria, "Connection: Events (server-sent events). Switch to polling.", "연결: 이벤트(server-sent events). 폴링으로 전환합니다.", "接続: イベント（server-sent events）。ポーリングに切り替えます。", "連線：事件（server-sent events）。切換去輪詢。", "Připojení: události (SSE). Přepnout na dotazování.", "Conexión: eventos (SSE). Cambiar a sondeo.", "连接：事件流（SSE）。切换到轮询。", "Verbindung: Events (Server-Sent Events). Zu Polling wechseln."),
    (conn_polling_aria, "Connection: Polling every 10 seconds. Switch to live.", "연결: 10초마다 폴링. 실시간으로 전환합니다.", "接続: 10秒ごとのポーリング。ライブに切り替えます。", "連線：每 10 秒輪詢。切換去即時。", "Připojení: dotazování každých 10 s. Přepnout na živé.", "Conexión: sondeo cada 10 segundos. Cambiar a en vivo.", "连接：每 10 秒轮询一次。切换到实时。", "Verbindung: Polling alle 10 Sekunden. Zu Live wechseln."),
    (conn_syncing_aria, "Connection: syncing.", "연결: 동기화 중.", "接続: 同期中。", "連線：同步中。", "Připojení: synchronizace.", "Conexión: sincronizando.", "连接：同步中。", "Verbindung: synchronisiert."),
    (conn_offline_aria, "Connection: offline. Retry.", "연결: 오프라인. 다시 시도합니다.", "接続: オフライン。再試行します。", "連線：離線。重試。", "Připojení: offline. Zkusit znovu.", "Conexión: sin conexión. Reintentar.", "连接：离线。重试。", "Verbindung: offline. Erneut versuchen."),
    (tab_write, "Write", "작성", "作成", "撰寫", "Napsat", "Escribir", "撰写", "Schreiben"),
    (tab_reply, "Reply", "답장", "返信", "回覆", "Odpovědět", "Responder", "回复", "Antworten"),
    (tab_image, "Image", "이미지", "画像", "圖片", "Obrázek", "Imagen", "图片", "Bild"),
    (tab_video, "Video", "동영상", "動画", "影片", "Video", "Vídeo", "视频", "Video"),
    (tab_keys, "Keys", "키", "キー", "金鑰", "Klíče", "Claves", "密钥", "Schlüssel"),
    (generate_video, "Generate video", "동영상 생성", "動画を生成", "產生影片", "Vygenerovat video", "Generar vídeo", "生成视频", "Video generieren"),
    (video_needs_grok, "Video needs a Grok (xAI) key — add one in the Keys tab.", "동영상에는 Grok(xAI) 키가 필요합니다 — 키 탭에서 추가하세요.", "動画には Grok（xAI）のキーが必要です — キータブで追加してください。", "影片需要 Grok（xAI）金鑰 — 喺金鑰分頁加入。", "Video vyžaduje klíč Grok (xAI) — přidejte jej na kartě Klíče.", "El vídeo necesita una clave de Grok (xAI): añádela en la pestaña Claves.", "生成视频需要 Grok（xAI）密钥 — 请在密钥标签页中添加。", "Video braucht einen Grok-(xAI-)Schlüssel — füge ihn im Tab „Schlüssel“ hinzu."),
    (ai_writing, "Writing…", "작성 중…", "作成中…", "撰寫中…", "Píšu…", "Escribiendo…", "撰写中…", "Schreibt…"),
    (ai_drawing, "Generating the image…", "이미지를 생성하는 중…", "画像を生成中…", "產生緊圖片…", "Generuji obrázek…", "Generando la imagen…", "正在生成图片…", "Bild wird generiert…"),
    (ai_filming, "Rendering the clip — this can take a few minutes…", "영상을 만드는 중 — 몇 분 걸릴 수 있습니다…", "動画を生成中 — 数分かかることがあります…", "製作緊影片 — 可能要幾分鐘…", "Vytvářím video — může to trvat několik minut…", "Renderizando el clip: puede tardar unos minutos…", "正在渲染视频 — 可能需要几分钟…", "Clip wird gerendert — das kann einige Minuten dauern…"),
    (ai_saving_here, "Saving to this server…", "이 서버에 저장하는 중…", "このサーバーに保存中…", "儲存去呢部伺服器…", "Ukládám na tento server…", "Guardando en este servidor…", "正在保存到本服务器…", "Wird auf diesem Server gespeichert…"),
    (ai_video_timeout, "The provider is still rendering after ten minutes. Try a shorter or simpler prompt.", "10분이 지나도 생성이 끝나지 않았습니다. 더 짧거나 단순한 프롬프트로 시도해 보세요.", "10 分たっても生成が終わりませんでした。より短く簡単なプロンプトで試してください。", "過咗十分鐘都仲未完成。試下短啲或簡單啲嘅提示。", "Poskytovatel po deseti minutách stále generuje. Zkuste kratší nebo jednodušší zadání.", "El proveedor sigue renderizando tras diez minutos. Prueba con una indicación más corta o simple.", "十分钟后仍未生成完成。请尝试更短或更简单的提示词。", "Der Anbieter rendert nach zehn Minuten immer noch. Versuche eine kürzere oder einfachere Eingabe."),
    (media_saved_here, "Saved on this server, so this link keeps working — the provider's own link expires within a day.", "이 서버에 저장되어 링크가 계속 유효합니다 — 제공자의 링크는 하루 안에 만료됩니다.", "このサーバーに保存されるのでリンクは有効なままです — プロバイダー側のリンクは 1 日ほどで失効します。", "已經存喺呢部伺服器，所以呢條連結會一直有效 — 供應商嗰條一日之內就會失效。", "Uloženo na tomto serveru, takže odkaz zůstane funkční — odkaz poskytovatele vyprší do jednoho dne.", "Guardado en este servidor, así que este enlace sigue funcionando: el del proveedor caduca en un día.", "已保存在本服务器，因此该链接会长期有效 — 提供方的链接一天内就会失效。", "Auf diesem Server gespeichert, damit dieser Link weiter funktioniert — der Link des Anbieters verfällt binnen eines Tages."),
    (media_link, "Link to this media", "이 미디어 링크", "このメディアのリンク", "呢個媒體嘅連結", "Odkaz na toto médium", "Enlace a este contenido", "此媒体的链接", "Link zu dieser Datei"),
    (login_vertical, "Vertical", "세로", "縦", "垂直", "Svisle", "Vertical", "竖排", "Vertikal"),
    (login_horizontal, "Horizontal", "가로", "横", "水平", "Vodorovně", "Horizontal", "横排", "Horizontal"),
    (artwork_above, "Artwork above, form below", "위에 아트워크, 아래에 입력란", "上に画像、下に入力欄", "上面圖像，下面表格", "Obrázek nahoře, formulář dole", "Ilustración arriba, formulario abajo", "插图在上，表单在下", "Bild oben, Formular unten"),
    (form_beside, "Form beside the artwork", "아트워크 옆에 입력란", "画像の横に入力欄", "表格喺圖像側邊", "Formulář vedle obrázku", "Formulario junto a la ilustración", "表单在插图旁", "Formular neben dem Bild"),
    (username_blank_hint, "Leave blank if you've signed in before, or to be named automatically from your wallet address.", "이전에 로그인한 적이 있거나 지갑 주소에서 이름을 자동으로 만들려면 비워 두세요.", "以前サインインしたことがある場合、またはウォレットアドレスから自動で名前を付ける場合は空欄のままにしてください。", "如果之前登入過，或者想用錢包地址自動改名，就留空。", "Nechte prázdné, pokud jste se už přihlásili, nebo pro jméno odvozené z adresy peněženky.", "Déjalo vacío si ya iniciaste sesión antes, o para nombrarte automáticamente desde tu dirección de cartera.", "如果你之前登录过，或想根据钱包地址自动命名，请留空。", "Leer lassen, wenn du dich schon einmal angemeldet hast oder automatisch nach deiner Wallet-Adresse benannt werden willst."),
    (stay_signed_in_hint, "Your username and recovery phrase are kept in this browser so reloading doesn't ask again. Anyone who can use this browser profile can then read your messages — leave it off on a shared computer.", "사용자 이름과 복구 문구가 이 브라우저에 저장되어 새로 고쳐도 다시 묻지 않습니다. 이 브라우저 프로필을 사용할 수 있는 사람은 메시지를 읽을 수 있으니, 공용 컴퓨터에서는 꺼 두세요.", "ユーザー名とリカバリーフレーズがこのブラウザに保存され、再読み込みしても再入力を求められません。このブラウザのプロフィールを使える人はメッセージを読めるため、共用のパソコンではオフにしてください。", "使用者名稱同復原字詞會存喺呢個瀏覽器，重新載入就唔使再輸入。任何可以用呢個瀏覽器設定檔嘅人都可以睇你嘅訊息 — 公用電腦就唔好開。", "Jméno a obnovovací fráze zůstanou v tomto prohlížeči, takže se po načtení znovu neptá. Kdokoli s přístupem k tomuto profilu si pak přečte vaše zprávy — na sdíleném počítači nechte vypnuté.", "Tu nombre de usuario y frase de recuperación se guardan en este navegador para no pedírtelos otra vez. Cualquiera que use este perfil podrá leer tus mensajes: déjalo desactivado en un ordenador compartido.", "用户名和恢复助记词会保存在此浏览器中，刷新后无需再次输入。任何能使用此浏览器配置文件的人都能读到你的消息 — 共用电脑请勿开启。", "Benutzername und Wiederherstellungsphrase bleiben in diesem Browser, damit ein Neuladen nicht erneut fragt. Wer dieses Browserprofil nutzen kann, kann dann deine Nachrichten lesen — auf einem geteilten Rechner ausgeschaltet lassen."),
    (private_key_hint, "64 hex characters. The 0x prefix is optional. This never leaves your browser.", "16진수 64자입니다. 0x 접두사는 선택 사항이며, 이 값은 브라우저를 벗어나지 않습니다.", "16進数64文字です。0xの接頭辞は任意で、この値はブラウザの外に出ません。", "64 個十六進位字元。0x 前綴可有可無，呢個值唔會離開你嘅瀏覽器。", "64 hexadecimálních znaků. Předpona 0x je nepovinná. Nikdy neopustí váš prohlížeč.", "64 caracteres hexadecimales. El prefijo 0x es opcional. Nunca sale de tu navegador.", "64 个十六进制字符。0x 前缀可选。绝不会离开你的浏览器。", "64 Hex-Zeichen. Das 0x-Präfix ist optional. Verlässt nie deinen Browser."),
    (offline_banner, "You're offline. Messages will send when you reconnect.", "오프라인입니다. 다시 연결되면 메시지가 전송됩니다.", "オフラインです。再接続するとメッセージが送信されます。", "你而家離線。重新連線之後訊息就會傳送。", "Jste offline. Zprávy se odešlou po připojení.", "Estás sin conexión. Los mensajes se enviarán al reconectar.", "你已离线。重新连接后将发送消息。", "Du bist offline. Nachrichten werden nach der Wiederverbindung gesendet."),
    // --- Send ---------------------------------------------------------------
    (send_button, "Send", "보내기", "送信", "傳送", "Odeslat", "Enviar", "发送", "Senden"),
    (new_room_fallback, "New room", "새 채팅방", "新しいルーム", "新聊天室", "Nová místnost", "Sala nueva", "新聊天室", "Neuer Raum"),

    // --- Fast-room descriptions ---------------------------------------------
    //
    // Four of them so a workspace full of one-click rooms does not read as the
    // same sentence repeated. Every one says the two things that are true of
    // the room: it is encrypted, and you get in by invitation.
    //
    // **No apostrophes, quotes or `<>{};"\` in any language.** The server
    // rejects that set in a room description, so a Czech or Spanish
    // translation reaching for a quotation mark would turn the one-click
    // button into a validation error. Pinned by
    // `fast_room_text_never_contains_markup_the_server_rejects`.
    (room_desc_0, "Created in one click. End-to-end encrypted — invite people by wallet address.", "한 번의 클릭으로 만들었습니다. 종단간 암호화 — 지갑 주소로 초대하세요.", "ワンクリックで作成。エンドツーエンド暗号化 — ウォレットアドレスで招待します。", "一撳就建立。端對端加密 — 用錢包地址邀請人。", "Vytvořeno jedním kliknutím. Koncové šifrování — zvěte lidi adresou peněženky.", "Creada con un clic. Cifrada de extremo a extremo — invita con la dirección de cartera.", "一键创建。端到端加密 — 按钱包地址邀请成员。", "Mit einem Klick erstellt. Ende-zu-Ende-verschlüsselt — lade per Wallet-Adresse ein."),
    (room_desc_1, "A quick encrypted room. Only members can read the messages posted here.", "빠르게 만든 암호화 채팅방입니다. 멤버만 여기 올린 메시지를 읽을 수 있습니다.", "手早く作った暗号化ルームです。ここの投稿はメンバーだけが読めます。", "快速建立嘅加密聊天室。只有成員睇得到呢度嘅訊息。", "Rychlá šifrovaná místnost. Zprávy zde přečtou jen členové.", "Una sala cifrada rápida. Solo los miembros pueden leer lo que se publica aquí.", "一个快速加密聊天室。只有成员能读取这里的消息。", "Ein schneller verschlüsselter Raum. Nur Mitglieder können die Nachrichten hier lesen."),
    (room_desc_2, "Encrypted from the first message. Add people by wallet address.", "첫 메시지부터 암호화됩니다. 지갑 주소로 사람을 추가하세요.", "最初のメッセージから暗号化されます。ウォレットアドレスで招待できます。", "由第一條訊息開始就加密。用錢包地址加人。", "Šifrováno od první zprávy. Lidi přidávejte adresou peněženky.", "Cifrada desde el primer mensaje. Añade personas por dirección de cartera.", "从第一条消息起就加密。按钱包地址添加成员。", "Verschlüsselt ab der ersten Nachricht. Füge Personen per Wallet-Adresse hinzu."),
    (room_desc_3, "One-click room, encrypted. Messages are readable only by the people invited to it.", "한 번의 클릭으로 만든 암호화 채팅방입니다. 초대받은 사람만 메시지를 읽을 수 있습니다.", "ワンクリックで作る暗号化ルーム。招待された人だけがメッセージを読めます。", "一撳就得嘅加密聊天室。只有受邀嘅人睇得到訊息。", "Místnost na jedno kliknutí, šifrovaná. Zprávy přečtou jen zvaní lidé.", "Sala de un clic, cifrada. Solo quienes reciban invitación pueden leer los mensajes.", "一键聊天室，已加密。只有受邀的人能读取消息。", "Ein-Klick-Raum, verschlüsselt. Nachrichten sind nur für Eingeladene lesbar."),

    // --- Fast-room greetings -------------------------------------------------
    (greeting_0, "Hello, world! 👋🌍", "안녕, 세상! 👋🌍", "こんにちは、世界！ 👋🌍", "哈囉，世界！ 👋🌍", "Ahoj, světe! 👋🌍", "¡Hola, mundo! 👋🌍", "你好，世界！👋🌍", "Hallo, Welt! 👋🌍"),
    (greeting_1, "First light in a brand-new room 🌅✨", "새 채팅방의 첫 빛 🌅✨", "できたてのルームに差す最初の光 🌅✨", "新聊天室嘅第一道光 🌅✨", "První světlo v nové místnosti 🌅✨", "Primera luz en una sala recién creada 🌅✨", "新聊天室的第一缕光 🌅✨", "Erstes Licht in einem brandneuen Raum 🌅✨"),
    (greeting_2, "Systems online. Hello, world! 🤖📡", "시스템 가동. 안녕, 세상! 🤖📡", "システム起動。こんにちは、世界！ 🤖📡", "系統上線。哈囉，世界！ 🤖📡", "Systémy online. Ahoj, světe! 🤖📡", "Sistemas en línea. ¡Hola, mundo! 🤖📡", "系统上线。你好，世界！🤖📡", "Systeme online. Hallo, Welt! 🤖📡"),
    (greeting_3, "Hello, world — encrypted and ready 🔐💬", "안녕, 세상 — 암호화 완료 🔐💬", "こんにちは、世界 — 暗号化して準備完了 🔐💬", "哈囉，世界 — 已加密，準備好 🔐💬", "Ahoj, světe — šifrováno a připraveno 🔐💬", "Hola, mundo — cifrado y listo 🔐💬", "你好，世界 — 已加密，准备就绪 🔐💬", "Hallo, Welt — verschlüsselt und bereit 🔐💬"),
    (greeting_4, "A new channel crackles to life ⚡🛰️", "새 채널이 살아납니다 ⚡🛰️", "新しいチャンネルが動きだす ⚡🛰️", "新頻道啪一聲活起身 ⚡🛰️", "Nový kanál se probouzí k životu ⚡🛰️", "Un canal nuevo cobra vida ⚡🛰️", "新的频道啪一声苏醒 ⚡🛰️", "Ein neuer Kanal knistert zum Leben ⚡🛰️"),
    (greeting_5, "Hello from the very first message 🚀🌌", "첫 메시지에서 인사드립니다 🚀🌌", "いちばん最初のメッセージからこんにちは 🚀🌌", "由第一條訊息嚟嘅問候 🚀🌌", "Zdravím z vůbec první zprávy 🚀🌌", "Un saludo desde el primerísimo mensaje 🚀🌌", "来自第一条消息的问候 🚀🌌", "Ein Gruß aus der allerersten Nachricht 🚀🌌"),
    (edited, "edited", "수정됨", "編集済み", "已編輯", "upraveno", "editado", "已编辑", "bearbeitet"),
    (gas_auto, "auto", "자동", "自動", "自動", "automaticky", "automático", "自动", "auto"),
    // "{n} blocked" as one string: the count sits before the noun in English
    // and after it in nothing here, but Korean and Japanese want a counter word
    // and Spanish wants agreement — so the whole sentence is translated, not
    // assembled from a number and a noun.
    (blocked_count_one, "{n} person blocked", "{n}명 차단됨", "{n}人をブロック中", "封鎖咗 {n} 個人", "{n} zablokovaná osoba", "{n} persona bloqueada", "已屏蔽 {n} 人", "{n} Person blockiert"),
    (blocked_count_many, "{n} people blocked", "{n}명 차단됨", "{n}人をブロック中", "封鎖咗 {n} 個人", "{n} zablokovaných osob", "{n} personas bloqueadas", "已屏蔽 {n} 人", "{n} Personen blockiert"),
    (server_unreadable, "The server sent something this client couldn't read.", "서버가 이 클라이언트가 읽을 수 없는 응답을 보냈습니다.", "サーバーがこのクライアントで読めないデータを返しました。", "伺服器傳咗啲呢個客戶端睇唔明嘅嘢。", "Server poslal něco, co klient nepřečetl.", "El servidor envió algo que este cliente no pudo leer.", "服务器返回了此客户端无法读取的内容。", "Der Server hat etwas gesendet, das dieser Client nicht lesen konnte."),

    // --- Knowledge (docs/SEARCH.md) ------------------------------------------
    // --- Operator (the game layer) ------------------------------------------
    (nav_operator, "Operator", "오퍼레이터", "オペレーター", "操作員", "Operátor", "Operador", "操作员", "Operator"),
    (op_dossier, "Dossier", "인사 기록", "個人ファイル", "檔案", "Složka", "Expediente", "档案", "Dossier"),
    (op_synaptic_load, "Synaptic load", "시냅스 부하", "シナプス負荷", "突觸負載", "Synaptická zátěž", "Carga sináptica", "突触负载", "Synaptische Last"),
    (op_streak, "Streak", "연속 접속", "連続日数", "連續日數", "Série", "Racha", "连续天数", "Serie"),
    (op_orders, "Orders", "지령", "任務", "指令", "Rozkazy", "Órdenes", "指令", "Befehle"),
    (op_trophies, "Trophies", "트로피", "トロフィー", "獎章", "Trofeje", "Trofeos", "奖杯", "Trophäen"),
    (op_standing_orders, "Standing orders", "상시 지령", "常設任務", "常設指令", "Trvalé rozkazy", "Órdenes permanentes", "常设指令", "Daueraufträge"),
    (op_reissued, "Reissued at midnight.", "자정에 다시 발령됩니다.", "深夜0時に再発行されます。", "午夜重新下達。", "Znovu vydáno o půlnoci.", "Se reemiten a medianoche.", "午夜重新下发。", "Wird um Mitternacht neu erteilt."),
    (op_file, "File", "기록", "ファイル", "檔案", "Spis", "Archivo", "档案", "Akte"),
    (op_classified, "Classified", "기밀", "機密", "機密", "Utajeno", "Clasificado", "机密", "Geheim"),
    (op_ladder, "Ladder", "사다리", "ラダー", "階梯", "Žebříček", "Escalafón", "阶梯", "Rangliste"),
    (op_this_server, "This server", "이 서버", "このサーバー", "呢個伺服器", "Tento server", "Este servidor", "本服务器", "Dieser Server"),
    (op_ladder_note, "Reported by each device. The server ranks what it is told — it has no way to check, and does not pretend to.", "각 기기가 보고합니다. 서버는 전달받은 값으로 순위를 매길 뿐이며, 검증할 방법이 없고 그런 척도 하지 않습니다.", "各デバイスからの自己申告です。サーバーは伝えられた値で順位を付けるだけで、検証する手段はなく、あるふりもしません。", "由每部裝置自行申報。伺服器只係按收到嘅數字排名 — 佢冇辦法查證，亦唔會扮有。", "Hlásí každé zařízení samo. Server řadí to, co mu řeknou — nemá jak to ověřit a netváří se, že má.", "Lo informa cada dispositivo. El servidor ordena lo que le dicen: no puede comprobarlo, y no finge lo contrario.", "由每台设备自行上报。服务器只按收到的数字排名——它无法核实，也不假装可以。", "Von jedem Gerät selbst gemeldet. Der Server sortiert, was ihm gesagt wird — er kann es nicht prüfen und tut auch nicht so."),
    (op_clearance_raised, "Clearance raised", "권한 상승", "クリアランス上昇", "權限提升", "Zvýšeno oprávnění", "Autorización elevada", "权限提升", "Freigabe erhöht"),
    (op_rank, "Rank", "등급", "ランク", "等級", "Hodnost", "Rango", "等级", "Rang"),
    (op_order_complete, "Order complete", "지령 완료", "任務完了", "指令完成", "Rozkaz splněn", "Orden completada", "指令完成", "Auftrag erfüllt"),
    (op_no_report, "No operator has reported to this server yet.", "아직 이 서버에 보고한 오퍼레이터가 없습니다.", "このサーバーにはまだ誰も報告していません。", "重未有操作員向呢個伺服器報告。", "Tomuto serveru se zatím nikdo nehlásil.", "Todavía no se ha reportado ningún operador a este servidor.", "还没有操作员向本服务器上报。", "Bei diesem Server hat sich noch niemand gemeldet."),
    (nav_knowledge, "Knowledge", "지식", "ナレッジ", "知識", "Znalosti", "Conocimiento", "知识", "Wissen"),
    (knowledge_tagline, "Everything written here is findable. Search it, or teach it something new.", "여기에 쓴 모든 것은 검색할 수 있습니다. 찾아보거나, 새로 가르쳐 주세요.", "ここに書いたことはすべて検索できます。探すか、新しく教えてください。", "喺度寫低嘅嘢全部搵得返。搜尋佢，或者教佢啲新嘢。", "Vše, co je zde napsáno, lze najít. Hledejte, nebo naučte něco nového.", "Todo lo escrito aquí se puede encontrar. Búscalo o enséñale algo nuevo.", "这里写下的一切都能被找到。搜索它，或教它点新东西。", "Alles, was hier geschrieben wird, ist auffindbar. Durchsuche es oder bringe ihm etwas Neues bei."),
    (mode_search, "Search", "검색", "検索", "搜尋", "Hledat", "Buscar", "搜索", "Suchen"),
    (mode_teach, "Teach", "가르치기", "教える", "教識佢", "Naučit", "Enseñar", "教学", "Beibringen"),
    (search_everything, "Search everything — messages, knowledge, #tags", "모든 것을 검색 — 메시지, 지식, #태그", "すべてを検索 — メッセージ、ナレッジ、#タグ", "搜尋所有嘢 — 訊息、知識、#標籤", "Hledat vše — zprávy, znalosti, #tagy", "Buscar todo: mensajes, conocimiento, #etiquetas", "搜索一切 — 消息、知识、#标签", "Alles durchsuchen — Nachrichten, Wissen, #Tags"),
    (teach_placeholder, "Write something worth remembering… #tags become filters", "기억해 둘 만한 것을 적어 주세요… #태그는 필터가 됩니다", "覚えておきたいことを書いてください… #タグはフィルタになります", "寫低值得記住嘅嘢… #標籤會變成篩選", "Napište něco, co stojí za zapamatování… #tagy se stanou filtry", "Escribe algo que valga la pena recordar… las #etiquetas se vuelven filtros", "写下值得记住的内容… #标签会成为筛选条件", "Schreibe etwas Merkenswertes… #Tags werden zu Filtern"),
    (teach_hint, "Saved on this server and searchable by everyone on it. Don't teach it secrets.", "이 서버에 저장되어 서버의 모든 사용자가 검색할 수 있습니다. 비밀은 가르치지 마세요.", "このサーバーに保存され、全員が検索できます。秘密は教えないでください。", "會存喺呢個伺服器，人人都搵到。唔好教佢秘密。", "Uloženo na tomto serveru a prohledávatelné všemi jeho uživateli. Neučte ho tajemství.", "Se guarda en este servidor y todos sus usuarios pueden buscarlo. No le enseñes secretos.", "保存在此服务器上，服务器上的所有人都能搜索到。不要教它秘密。", "Wird auf diesem Server gespeichert und ist für alle darauf durchsuchbar. Bringe ihm keine Geheimnisse bei."),
    (taught_ok, "Learned. It's searchable now.", "배웠습니다. 이제 검색할 수 있습니다.", "覚えました。検索できます。", "學識咗。而家搵得返喇。", "Naučeno. Nyní to lze vyhledat.", "Aprendido. Ya se puede buscar.", "已学会。现在可以搜索到了。", "Gelernt. Es ist jetzt auffindbar."),
    (teach_failed, "Couldn't save that", "저장하지 못했습니다", "保存できませんでした", "儲存唔到", "Nepodařilo se to uložit", "No se pudo guardar", "保存失败", "Konnte nicht gespeichert werden"),
    (forget_note, "Forget", "잊기", "忘れる", "唔記得佢", "Zapomenout", "Olvidar", "忘记", "Vergessen"),
    (note_forgotten, "Forgotten", "잊었습니다", "忘れました", "唔記得咗喇", "Zapomenuto", "Olvidado", "已忘记", "Vergessen"),
    (forget_failed, "Couldn't forget that", "잊지 못했습니다", "忘れられませんでした", "唔記得唔到", "Nepodařilo se zapomenout", "No se pudo olvidar", "无法忘记", "Konnte nicht vergessen werden"),
    (no_results, "Nothing found", "검색 결과가 없습니다", "見つかりませんでした", "搵唔到", "Nic nenalezeno", "No se encontró nada", "未找到任何内容", "Nichts gefunden"),
    (no_results_hint, "Try fewer words, or a #tag.", "단어를 줄이거나 #태그로 검색해 보세요.", "語数を減らすか、#タグで試してください。", "試下少啲字，或者用#標籤。", "Zkuste méně slov nebo #tag.", "Prueba con menos palabras o una #etiqueta.", "试试更少的词，或用 #标签。", "Versuche weniger Wörter oder einen #Tag."),
    (knowledge_empty, "Nothing taught yet", "아직 가르친 것이 없습니다", "まだ何も教えていません", "仲未教過嘢", "Zatím nic naučeno", "Aún no se ha enseñado nada", "还没有教过任何内容", "Noch nichts beigebracht"),
    (knowledge_intro, "Chat is remembered automatically. This page is for the rest — facts, how-tos, anything worth keeping.", "대화는 자동으로 기억됩니다. 이 페이지는 나머지를 위한 곳 — 사실, 방법, 남길 가치가 있는 모든 것.", "チャットは自動的に記憶されます。ここはそれ以外のため — 事実、手順、残す価値のあるすべて。", "傾偈嘅嘢會自動記住。呢頁係畀其他嘢 — 事實、做法、值得留低嘅一切。", "Chat se pamatuje automaticky. Tato stránka je pro zbytek — fakta, návody, vše, co stojí za uchování.", "El chat se recuerda automáticamente. Esta página es para el resto: datos, guías, todo lo que valga la pena conservar.", "聊天内容会自动记住。这个页面用来记其余的 — 事实、方法，任何值得保留的东西。", "Der Chat wird automatisch gemerkt. Diese Seite ist für den Rest — Fakten, Anleitungen, alles Aufbewahrenswerte."),
    (searching, "Searching…", "검색 중…", "検索中…", "搜尋緊…", "Hledám…", "Buscando…", "搜索中…", "Sucht…"),
    (browse_by_tag, "Browse by tag", "태그로 둘러보기", "タグで見る", "按標籤睇", "Procházet podle tagů", "Explorar por etiqueta", "按标签浏览", "Nach Tag stöbern"),
    (open_in_room, "Open in room", "채팅방에서 열기", "ルームで開く", "喺聊天室開", "Otevřít v místnosti", "Abrir en la sala", "在聊天室中打开", "Im Raum öffnen"),
    (teach_from_message, "Teach from this message", "이 메시지로 가르치기", "このメッセージから教える", "用呢條訊息教佢", "Naučit z této zprávy", "Enseñar desde este mensaje", "用这条消息教学", "Aus dieser Nachricht beibringen"),
    (ask_ai_with_results, "Ask {provider} with these results", "{provider}에게 이 결과로 질문하기", "{provider}にこの結果で質問する", "用呢啲結果問{provider}", "Zeptat se {provider} z těchto výsledků", "Preguntar a {provider} con estos resultados", "用这些结果询问 {provider}", "{provider} mit diesen Ergebnissen fragen"),
    (ask_consent_title, "Send to cloud AI?", "클라우드 AI로 보낼까요?", "クラウドAIに送信しますか？", "送去雲端AI？", "Odeslat do cloudové AI?", "¿Enviar a la IA en la nube?", "发送到云端 AI？", "An Cloud-KI senden?"),
    (ask_consent_body, "Your question and the {n} results below will leave this server and go to {provider}. Nothing else is sent.", "질문과 아래 결과 {n}개가 이 서버를 떠나 {provider}(으)로 전송됩니다. 그 외에는 아무것도 전송되지 않습니다.", "質問と以下の{n}件の結果がこのサーバーを離れ、{provider}に送信されます。それ以外は送信されません。", "你嘅問題同下面{n}個結果會離開呢個伺服器，送去{provider}。其他嘢一律唔會送。", "Váš dotaz a {n} výsledků níže opustí tento server a půjdou do {provider}. Nic jiného se neodesílá.", "Tu pregunta y los {n} resultados de abajo saldrán de este servidor hacia {provider}. No se envía nada más.", "你的问题和下方 {n} 条结果将离开此服务器，发送给 {provider}。不会发送其他任何内容。", "Deine Frage und die {n} Ergebnisse unten verlassen diesen Server und gehen an {provider}. Sonst wird nichts gesendet."),
    (ask_send, "Send and ask", "보내고 질문하기", "送信して質問", "送出並提問", "Odeslat a zeptat se", "Enviar y preguntar", "发送并提问", "Senden und fragen"),
    (ai_answered_from, "{provider}, answering from {n} results:", "{provider}이(가) 결과 {n}개로 답변:", "{provider}が{n}件の結果から回答:", "{provider}用{n}個結果答:", "{provider} odpovídá z {n} výsledků:", "{provider}, respondiendo a partir de {n} resultados:", "{provider} 基于 {n} 条结果回答：", "{provider}, antwortet aus {n} Ergebnissen:"),
    (ai_ask_failed, "The AI request failed", "AI 요청이 실패했습니다", "AIリクエストに失敗しました", "AI請求失敗", "Požadavek na AI selhal", "La solicitud de IA falló", "AI 请求失败", "Die KI-Anfrage ist fehlgeschlagen"),
    (asking_ai, "Asking…", "질문 중…", "問い合わせ中…", "問緊…", "Ptám se…", "Preguntando…", "询问中…", "Fragt…"),
    // --- Destructive confirmations -------------------------------------------
    // Every one of these is a title/body pair for `Modal::Confirm`. They are
    // grouped rather than filed next to their screens because they share a
    // voice: the title asks, the body states the irreversible fact, and
    // neither apologises (DESIGN.md §15). `{name}` is substituted by the
    // caller — the whole sentence is translated, so a language that puts the
    // name elsewhere, or needs a particle after it, still reads correctly.
    (sign_out_title, "Sign out?", "로그아웃할까요?", "サインアウトしますか？", "登出？", "Odhlásit se?", "¿Cerrar sesión?", "退出登录？", "Abmelden?"),
    (sign_out_body, "You'll need your recovery phrase to sign back in. The saved sign-in and cached messages on this device are removed; a downloaded wallet backup is not.", "다시 로그인하려면 복구 문구가 필요합니다. 이 기기에 저장된 로그인 정보와 캐시된 메시지는 삭제되지만, 내려받은 지갑 백업 파일은 그대로 남습니다.", "再度サインインするにはリカバリーフレーズが必要です。この端末に保存されたサインイン情報とキャッシュされたメッセージは削除されますが、ダウンロード済みのウォレットバックアップは残ります。", "要再登入就需要復原字詞。呢部裝置儲低嘅登入資料同快取訊息會刪走，但你下載咗嘅錢包備份唔會郁。", "K opětovnému přihlášení budete potřebovat obnovovací frázi. Uložené přihlášení a zprávy v mezipaměti na tomto zařízení se odstraní; stažená záloha peněženky nikoli.", "Necesitarás tu frase de recuperación para volver a entrar. Se eliminan el inicio de sesión guardado y los mensajes en caché de este dispositivo; la copia de seguridad descargada de la cartera, no.", "重新登录需要恢复助记词。此设备上保存的登录信息和缓存消息会被删除；已下载的钱包备份不受影响。", "Zum erneuten Anmelden brauchst du deine Wiederherstellungsphrase. Die gespeicherte Anmeldung und zwischengespeicherte Nachrichten auf diesem Gerät werden entfernt; eine heruntergeladene Wallet-Sicherung nicht."),
    (forget_phrase_title, "Forget the recovery phrase?", "복구 문구를 삭제할까요?", "リカバリーフレーズを削除しますか？", "刪走復原字詞？", "Zapomenout obnovovací frázi?", "¿Olvidar la frase de recuperación?", "忘记恢复助记词？", "Wiederherstellungsphrase vergessen?"),
    (forget_phrase_body, "This device stops holding it. You stay signed in now, but the next reload will ask for it again — make sure you still have it written down.", "이 기기가 문구를 더 이상 보관하지 않습니다. 지금은 로그인 상태가 유지되지만 다음에 새로 고치면 다시 물어봅니다 — 어딘가에 적어 두었는지 확인하세요.", "この端末では保持しなくなります。今はサインインしたままですが、次に再読み込みすると再び入力を求められます — 控えが手元にあるか確認してください。", "呢部裝置唔會再存住佢。而家你仲係登入緊，但下次重新載入就會再問你 — 確保你仲有抄低。", "Toto zařízení ji přestane uchovávat. Nyní zůstáváte přihlášeni, ale při dalším načtení se na ni zeptá znovu — ujistěte se, že ji máte zapsanou.", "Este dispositivo deja de guardarla. Sigues con la sesión iniciada, pero la próxima recarga volverá a pedírtela: asegúrate de tenerla anotada.", "此设备将不再保存它。你目前仍保持登录，但下次刷新会再次询问 — 请确认你已把它抄下来。", "Dieses Gerät behält sie nicht mehr. Du bleibst jetzt angemeldet, aber das nächste Neuladen fragt wieder danach — stelle sicher, dass du sie notiert hast."),
    (erase_local_title, "Erase local data?", "로컬 데이터를 삭제할까요?", "ローカルデータを消去しますか？", "清除本機資料？", "Vymazat místní data?", "¿Borrar los datos locales?", "清除本地数据？", "Lokale Daten löschen?"),
    (erase_local_body, "Removes cached messages, room keys and settings from this device. Your wallet is not affected, and any backup file you downloaded is untouched.", "이 기기에서 캐시된 메시지, 채팅방 키, 설정을 삭제합니다. 지갑에는 영향이 없고, 내려받은 백업 파일도 그대로입니다.", "この端末からキャッシュされたメッセージ、ルームキー、設定を削除します。ウォレットには影響せず、ダウンロード済みのバックアップファイルもそのままです。", "會由呢部裝置刪走快取訊息、聊天室金鑰同設定。你個錢包唔受影響，下載咗嘅備份檔亦都唔會郁。", "Odstraní z tohoto zařízení zprávy v mezipaměti, klíče místností a nastavení. Peněženky se to netýká a stažený záložní soubor zůstane.", "Elimina de este dispositivo los mensajes en caché, las claves de las salas y los ajustes. Tu cartera no se ve afectada y el archivo de copia de seguridad descargado queda intacto.", "从此设备删除缓存消息、聊天室密钥和设置。你的钱包不受影响，已下载的备份文件也保持不变。", "Entfernt zwischengespeicherte Nachrichten, Raumschlüssel und Einstellungen von diesem Gerät. Deine Wallet ist nicht betroffen, und eine heruntergeladene Sicherungsdatei bleibt unangetastet."),
    (erase_local_help, "Removes cached messages, room keys and settings from this device. Your wallet is not affected.", "이 기기에서 캐시된 메시지, 채팅방 키, 설정을 삭제합니다. 지갑에는 영향이 없습니다.", "この端末からキャッシュされたメッセージ、ルームキー、設定を削除します。ウォレットには影響しません。", "會由呢部裝置刪走快取訊息、聊天室金鑰同設定。你個錢包唔受影響。", "Odstraní z tohoto zařízení zprávy v mezipaměti, klíče místností a nastavení. Peněženky se to netýká.", "Elimina de este dispositivo los mensajes en caché, las claves de las salas y los ajustes. Tu cartera no se ve afectada.", "从此设备删除缓存消息、聊天室密钥和设置。你的钱包不受影响。", "Entfernt zwischengespeicherte Nachrichten, Raumschlüssel und Einstellungen von diesem Gerät. Deine Wallet ist nicht betroffen."),
    (encryption_locked, "Encryption is locked on this device. Sign out and back in with your recovery phrase to read encrypted rooms.", "이 기기에서 암호화가 잠겨 있습니다. 암호화된 채팅방을 읽으려면 로그아웃한 뒤 복구 문구로 다시 로그인하세요.", "この端末では暗号化がロックされています。暗号化されたルームを読むには、サインアウトしてリカバリーフレーズで再度サインインしてください。", "呢部裝置嘅加密已鎖上。要睇加密聊天室，請登出再用復原字詞重新登入。", "Šifrování je na tomto zařízení uzamčeno. Odhlaste se a přihlaste znovu obnovovací frází, abyste mohli číst šifrované místnosti.", "El cifrado está bloqueado en este dispositivo. Cierra sesión y vuelve a entrar con tu frase de recuperación para leer las salas cifradas.", "此设备上的加密已锁定。请退出后用恢复助记词重新登录，以读取加密聊天室。", "Die Verschlüsselung ist auf diesem Gerät gesperrt. Melde dich ab und mit deiner Wiederherstellungsphrase wieder an, um verschlüsselte Räume zu lesen."),
    (block_title, "Block {name}?", "{name}님을 차단할까요?", "{name}をブロックしますか？", "封鎖 {name}？", "Blokovat {name}?", "¿Bloquear a {name}?", "屏蔽 {name}？", "{name} blockieren?"),
    (block_body, "You won't see their messages or reactions, and they can't invite you to rooms. They stay in this room.", "이 사람의 메시지와 반응이 보이지 않고, 채팅방에 초대할 수도 없게 됩니다. 이 채팅방에는 그대로 남습니다.", "相手のメッセージやリアクションは表示されなくなり、ルームに招待もできなくなります。このルームには残ります。", "你唔會再見到佢嘅訊息同反應，佢亦都唔可以邀請你入聊天室。佢仲會留喺呢個聊天室。", "Neuvidíte jejich zprávy ani reakce a nebudou vás moci zvát do místností. V této místnosti zůstávají.", "No verás sus mensajes ni sus reacciones, y no podrá invitarte a salas. Sigue en esta sala.", "你将看不到对方的消息和回应，对方也不能邀请你加入聊天室。对方仍留在此聊天室。", "Du siehst ihre Nachrichten und Reaktionen nicht mehr, und sie können dich nicht in Räume einladen. Sie bleiben in diesem Raum."),
    (unblock_title, "Unblock {name}?", "{name}님의 차단을 해제할까요?", "{name}のブロックを解除しますか？", "解除封鎖 {name}？", "Odblokovat {name}?", "¿Desbloquear a {name}?", "取消屏蔽 {name}？", "Blockierung von {name} aufheben?"),
    (unblock_body, "You'll see their messages again, and they'll be able to invite you to rooms.", "이 사람의 메시지가 다시 보이고, 채팅방에 초대할 수 있게 됩니다.", "相手のメッセージが再び表示され、ルームに招待できるようになります。", "你會再見到佢嘅訊息，佢亦都可以邀請你入聊天室。", "Znovu uvidíte jejich zprávy a budou vás moci zvát do místností.", "Volverás a ver sus mensajes y podrá invitarte a salas.", "你将重新看到对方的消息，对方也能邀请你加入聊天室。", "Du siehst ihre Nachrichten wieder, und sie können dich in Räume einladen."),
    (remove_member_title, "Remove {name}?", "{name}님을 내보낼까요?", "{name}を退出させますか？", "移除 {name}？", "Odebrat {name}?", "¿Quitar a {name}?", "移除 {name}？", "{name} entfernen?"),
    (remove_member_body, "They lose access to this room and its keys. The room key will need rotating before anyone can post again.", "이 사람은 채팅방과 키에 접근할 수 없게 됩니다. 다시 메시지를 보내려면 채팅방 키를 교체해야 합니다.", "このルームとその鍵へのアクセスを失います。誰かが再び投稿するには、ルームキーの更新が必要です。", "佢會失去呢個聊天室同金鑰嘅存取權。要有人再出訊息，就要先換聊天室金鑰。", "Ztratí přístup k této místnosti i jejím klíčům. Než bude moci kdokoli znovu psát, bude potřeba obměnit klíč místnosti.", "Pierde el acceso a esta sala y a sus claves. Habrá que rotar la clave de la sala antes de que alguien pueda volver a publicar.", "对方将失去此聊天室及其密钥的访问权限。需要轮换聊天室密钥后才能再发消息。", "Sie verlieren den Zugang zu diesem Raum und seinen Schlüsseln. Der Raumschlüssel muss rotiert werden, bevor wieder jemand posten kann."),
    (give_up_admin, "Give up admin", "관리자 권한 포기", "管理者権限を手放す", "放棄管理員", "Vzdát se správcovství", "Renunciar a administrador", "放弃管理员", "Admin-Rechte abgeben"),
    (give_up_admin_title, "Give up admin?", "관리자 권한을 포기할까요?", "管理者権限を手放しますか？", "放棄管理員？", "Vzdát se správcovství?", "¿Renunciar a administrador?", "放弃管理员权限？", "Admin-Rechte abgeben?"),
    (give_up_admin_body, "You won't be able to manage this room, invite people or rename it. Another admin would have to promote you again.", "이 채팅방을 관리하거나, 사람을 초대하거나, 이름을 바꿀 수 없게 됩니다. 다른 관리자가 다시 권한을 줘야 합니다.", "このルームの管理も、招待も、名前の変更もできなくなります。再び権限を得るには別の管理者による昇格が必要です。", "你將唔可以管理呢個聊天室、邀請人或者改名。要另一個管理員再次升你做管理員先得。", "Nebudete moci spravovat tuto místnost, zvát lidi ani ji přejmenovat. Musel by vás znovu povýšit jiný správce.", "No podrás gestionar esta sala, invitar personas ni cambiarle el nombre. Otro administrador tendría que volver a ascenderte.", "你将无法管理此聊天室、邀请成员或重命名。需要其他管理员再次提升你。", "Du kannst diesen Raum nicht mehr verwalten, niemanden einladen und ihn nicht umbenennen. Ein anderer Admin müsste dich erneut befördern."),
    (leave, "Leave", "나가기", "退出", "離開", "Opustit", "Salir", "离开", "Verlassen"),
    (leave_room, "Leave room", "채팅방 나가기", "ルームを退出", "離開聊天室", "Opustit místnost", "Salir de la sala", "离开聊天室", "Raum verlassen"),
    (leave_room_title, "Leave {name}?", "{name}에서 나갈까요?", "{name}を退出しますか？", "離開 {name}？", "Opustit {name}?", "¿Salir de {name}?", "离开 {name}？", "{name} verlassen?"),
    (leave_room_body, "You'll stop receiving its messages, and the room key will need rotating before anyone can post again.", "이 채팅방의 메시지를 더 이상 받지 않게 되고, 다시 메시지를 보내려면 채팅방 키를 교체해야 합니다.", "このルームのメッセージは届かなくなり、誰かが再び投稿するにはルームキーの更新が必要です。", "你唔會再收到呢個聊天室嘅訊息，而且要有人再出訊息就要先換聊天室金鑰。", "Přestanete dostávat její zprávy a než bude moci kdokoli znovu psát, bude potřeba obměnit klíč místnosti.", "Dejarás de recibir sus mensajes, y habrá que rotar la clave de la sala antes de que alguien pueda volver a publicar.", "你将不再接收它的消息，且需要轮换聊天室密钥后才能再发消息。", "Du erhältst seine Nachrichten nicht mehr, und der Raumschlüssel muss rotiert werden, bevor wieder jemand posten kann."),
    (hide_room, "Hide room", "채팅방 숨기기", "ルームを非表示", "隱藏聊天室", "Skrýt místnost", "Ocultar la sala", "隐藏聊天室", "Raum ausblenden"),
    (hide_room_title, "Hide {name}?", "{name}을(를) 숨길까요?", "{name}を非表示にしますか？", "隱藏 {name}？", "Skrýt {name}?", "¿Ocultar {name}?", "隐藏 {name}？", "{name} ausblenden?"),
    (hide_room_body, "It disappears from your list but you stay a member and keep receiving messages. You can unhide it from Settings.", "목록에서는 사라지지만 멤버 자격은 유지되고 메시지도 계속 받습니다. 설정에서 다시 표시할 수 있습니다.", "一覧からは消えますが、メンバーのままでメッセージも届き続けます。設定から再表示できます。", "佢會喺你個清單度消失，但你仲係成員，一樣會收到訊息。可以喺設定度再顯示返。", "Zmizí z vašeho seznamu, ale zůstáváte členem a zprávy dostáváte dál. Zobrazit ji můžete zpět v nastavení.", "Desaparece de tu lista, pero sigues siendo miembro y recibiendo mensajes. Puedes volver a mostrarla desde Ajustes.", "它会从你的列表中消失，但你仍是成员并继续接收消息。可在设置中取消隐藏。", "Er verschwindet aus deiner Liste, aber du bleibst Mitglied und erhältst weiter Nachrichten. In den Einstellungen kannst du ihn wieder einblenden."),
    (tap_to_copy_address, "Tap to copy the wallet address", "탭하면 지갑 주소를 복사합니다", "タップでウォレットアドレスをコピー", "㩒一下就複製錢包地址", "Klepnutím zkopírujete adresu peněženky", "Toca para copiar la dirección de cartera", "点击复制钱包地址", "Tippen, um die Wallet-Adresse zu kopieren"),
    (swipe_actions_for, "Actions for {name}", "{name} 관련 작업", "{name} の操作", "{name} 嘅操作", "Akce pro {name}", "Acciones para {name}", "{name} 的操作", "Aktionen für {name}"),
    (swipe_hint, "Swipe a room left to hide or leave it", "채팅방을 왼쪽으로 밀면 숨기거나 나갈 수 있습니다", "ルームを左にスワイプすると非表示や退出ができます", "向左掃聊天室就可以隱藏或者離開", "Přejetím místnosti doleva ji skryjete nebo opustíte", "Desliza una sala a la izquierda para ocultarla o salir", "向左滑动聊天室即可隐藏或离开", "Wische einen Raum nach links, um ihn auszublenden oder zu verlassen"),
    (swipe_shortcut_ready, "Shortcut unlocked", "단축 동작이 열렸습니다", "ショートカットが使えます", "解鎖咗捷徑", "Zkratka odemčena", "Atajo desbloqueado", "已解锁快捷手势", "Kurzbefehl freigeschaltet"),
    (swipe_shortcut_ready_body, "Swipe a room all the way across and it goes straight to the confirmation — no need to stop at the buttons.", "이제 채팅방을 끝까지 밀면 버튼을 거치지 않고 바로 확인 창으로 넘어갑니다.", "ルームを端までスワイプすると、ボタンを経由せず確認画面に直接進みます。", "而家掃到底就會直接去確認，唔使停喺掣度。", "Přejeďte místnost až na konec a přejdete rovnou k potvrzení — u tlačítek už zastavovat nemusíte.", "Desliza una sala hasta el final y pasarás directo a la confirmación, sin detenerte en los botones.", "把聊天室一路滑到底，就会直接进入确认，不必停在按钮上。", "Wische einen Raum ganz durch und du landest direkt bei der Bestätigung — ohne Halt bei den Schaltflächen."),
    (room_hidden_toast, "{name} hidden", "{name}을(를) 숨겼습니다", "{name} を非表示にしました", "已隱藏 {name}", "{name} skryta", "{name} está oculta", "已隐藏 {name}", "{name} ausgeblendet"),
    (room_hidden_toast_body, "Bring it back from Settings → Hidden rooms.", "설정 → 숨긴 채팅방에서 되돌릴 수 있습니다.", "設定 → 非表示のルーム から戻せます。", "可以喺 設定 → 隱藏的聊天室 度攞返。", "Vrátíte ji zpět v Nastavení → Skryté místnosti.", "Recupérala en Ajustes → Salas ocultas.", "可在 设置 → 已隐藏的聊天室 中恢复。", "Hol ihn unter Einstellungen → Ausgeblendete Räume zurück."),
    (room_left_toast, "You left {name}", "{name}에서 나왔습니다", "{name} を退出しました", "你離開咗 {name}", "Opustili jste {name}", "Has salido de {name}", "你已离开 {name}", "Du hast {name} verlassen"),
    (room_deleted_toast, "{name} deleted", "{name}을(를) 삭제했습니다", "{name} を削除しました", "已刪除 {name}", "{name} smazána", "{name} eliminada", "已删除 {name}", "{name} gelöscht"),
    (delete_all_messages, "Delete all messages", "모든 메시지 삭제", "すべてのメッセージを削除", "刪除所有訊息", "Smazat všechny zprávy", "Eliminar todos los mensajes", "删除所有消息", "Alle Nachrichten löschen"),
    (delete_all_title, "Delete every message in {name}?", "{name}의 모든 메시지를 삭제할까요?", "{name}のすべてのメッセージを削除しますか？", "刪除 {name} 入面所有訊息？", "Smazat všechny zprávy v {name}?", "¿Eliminar todos los mensajes de {name}?", "删除 {name} 中的所有消息？", "Jede Nachricht in {name} löschen?"),
    (delete_all_body, "This removes the entire history for everyone. It can't be undone.", "모든 사람에게서 전체 기록이 삭제됩니다. 되돌릴 수 없습니다.", "全員から履歴がすべて削除されます。取り消せません。", "會為所有人刪走成個記錄。無法還原。", "Odstraní celou historii všem. Nelze vzít zpět.", "Elimina todo el historial para todos. No se puede deshacer.", "将为所有人删除全部历史记录，且无法撤销。", "Das entfernt den gesamten Verlauf für alle. Es lässt sich nicht rückgängig machen."),
    (delete_all, "Delete all", "모두 삭제", "すべて削除", "全部刪除", "Smazat vše", "Eliminar todo", "全部删除", "Alle löschen"),
    (delete_room, "Delete room", "채팅방 삭제", "ルームを削除", "刪除聊天室", "Smazat místnost", "Eliminar la sala", "删除聊天室", "Raum löschen"),
    (delete_room_title, "Delete {name}?", "{name}을(를) 삭제할까요?", "{name}を削除しますか？", "刪除 {name}？", "Smazat {name}?", "¿Eliminar {name}?", "删除 {name}？", "{name} löschen?"),
    (delete_room_body, "The room, its messages and its keys are removed for every member. It can't be undone.", "채팅방과 메시지, 키가 모든 멤버에게서 삭제됩니다. 되돌릴 수 없습니다.", "ルーム、そのメッセージ、鍵がすべてのメンバーから削除されます。取り消せません。", "聊天室、佢嘅訊息同金鑰會為所有成員刪走。無法還原。", "Místnost, její zprávy i klíče se odstraní všem členům. Nelze vzít zpět.", "La sala, sus mensajes y sus claves se eliminan para todos los miembros. No se puede deshacer.", "聊天室及其消息和密钥将为所有成员删除，且无法撤销。", "Der Raum, seine Nachrichten und Schlüssel werden für alle Mitglieder entfernt. Es lässt sich nicht rückgängig machen."),

    // --- Direct messages, threads and mentions -------------------------------
    (section_channels, "Channels", "채널", "チャンネル", "頻道", "Kanály", "Canales", "频道", "Kanäle"),
    (section_direct_messages, "Direct messages", "다이렉트 메시지", "ダイレクトメッセージ", "私訊", "Přímé zprávy", "Mensajes directos", "私信", "Direktnachrichten"),
    (new_direct_message, "New message", "새 메시지", "新規メッセージ", "新訊息", "Nová zpráva", "Mensaje nuevo", "新消息", "Neue Nachricht"),
    (new_direct_message_hint, "Pick who to message. Choosing yourself opens a private notebook.", "메시지를 보낼 사람을 고르세요. 자신을 선택하면 개인 메모장이 열립니다.", "メッセージを送る相手を選んでください。自分を選ぶと自分専用のメモになります。", "揀你想傾偈嘅人。揀返自己就會開一個私人筆記。", "Vyberte, komu napsat. Když zvolíte sebe, otevře se soukromý zápisník.", "Elige a quién escribir. Si te eliges a ti, se abre un cuaderno privado.", "选择要发消息的人。选择自己会打开一个私人记事本。", "Wähle, wem du schreiben willst. Dich selbst zu wählen öffnet ein privates Notizbuch."),
    (note_to_self, "You", "나", "自分", "自己", "Vy", "Tú", "自己", "Du"),
    (reply_in_thread, "Reply in thread", "스레드로 답장", "スレッドで返信", "喺主題度回覆", "Odpovědět ve vlákně", "Responder en el hilo", "在话题中回复", "Im Thread antworten"),
    (thread, "Thread", "스레드", "スレッド", "主題", "Vlákno", "Hilo", "话题", "Thread"),
    (thread_reply_one, "{n} reply", "답장 {n}개", "返信{n}件", "{n} 個回覆", "{n} odpověď", "{n} respuesta", "{n} 条回复", "{n} Antwort"),
    (thread_reply_many, "{n} replies", "답장 {n}개", "返信{n}件", "{n} 個回覆", "{n} odpovědí", "{n} respuestas", "{n} 条回复", "{n} Antworten"),
    (thread_empty, "No replies yet — start the thread.", "아직 답장이 없습니다. 스레드를 시작해 보세요.", "まだ返信はありません。スレッドを始めましょう。", "仲未有回覆 — 開個主題啦。", "Zatím žádné odpovědi — začněte vlákno.", "Aún no hay respuestas: empieza el hilo.", "还没有回复 —— 来开个话题吧。", "Noch keine Antworten — starte den Thread."),
    (thread_deleted_root, "The first message was deleted.", "첫 메시지가 삭제되었습니다.", "最初のメッセージは削除されました。", "第一則訊息已刪除。", "První zpráva byla smazána.", "El primer mensaje fue eliminado.", "第一条消息已被删除。", "Die erste Nachricht wurde gelöscht."),
    (mentions, "Mentions", "멘션", "メンション", "提及", "Zmínky", "Menciones", "提及", "Erwähnungen"),
    (mentions_empty, "Nothing has mentioned you yet.", "아직 회원님을 멘션한 메시지가 없습니다.", "まだメンションはありません。", "仲未有人提過你。", "Zatím vás nikdo nezmínil.", "Todavía nadie te ha mencionado.", "还没有人提到你。", "Dich hat noch nichts erwähnt."),
    (mentions_empty_hint, "When someone writes @your name, it lands here.", "누군가 @이름을 적으면 여기에 표시됩니다.", "誰かが @あなたの名前 と書くと、ここに届きます。", "有人打 @你個名 嘅時候就會出現喺呢度。", "Když někdo napíše @vaše jméno, objeví se to tady.", "Cuando alguien escriba @tu nombre, aparecerá aquí.", "当有人写下 @你的名字 时，会出现在这里。", "Wenn jemand @deinen Namen schreibt, landet es hier."),
    (message_verb, "Message", "메시지", "メッセージ", "傾偈", "Napsat", "Escribir", "发消息", "Schreiben"),
    (opening, "Opening…", "여는 중…", "開いています…", "開緊…", "Otevírám…", "Abriendo…", "正在打开…", "Wird geöffnet…"),
    (open_conversation, "Open", "열기", "開く", "開啟", "Otevřít", "Abrir", "打开", "Öffnen"),
    (new_message_one, "{n} new message", "새 메시지 {n}개", "新着メッセージ{n}件", "{n} 則新訊息", "{n} nová zpráva", "{n} mensaje nuevo", "{n} 条新消息", "{n} neue Nachricht"),
    (new_message_many, "{n} new messages", "새 메시지 {n}개", "新着メッセージ{n}件", "{n} 則新訊息", "{n} nových zpráv", "{n} mensajes nuevos", "{n} 条新消息", "{n} neue Nachrichten"),
    (mention_suggestions, "People you can mention", "멘션할 수 있는 사람", "メンションできる人", "可以提及嘅人", "Lidé, které můžete zmínit", "Personas a las que puedes mencionar", "可提及的人", "Personen, die du erwähnen kannst"),

    // --- Server administration -----------------------------------------------
    (admin_console, "Server admin", "서버 관리", "サーバー管理", "伺服器管理", "Správa serveru", "Administración del servidor", "服务器管理", "Serververwaltung"),
    (admin_people, "People", "사용자", "ユーザー", "使用者", "Lidé", "Personas", "用户", "Personen"),
    (admin_rooms, "Rooms", "채팅방", "ルーム", "聊天室", "Místnosti", "Salas", "聊天室", "Räume"),
    (admin_suspend, "Suspend", "정지", "利用停止", "停用", "Pozastavit", "Suspender", "停用", "Sperren"),
    (admin_reinstate, "Reinstate", "정지 해제", "停止解除", "恢復", "Obnovit", "Restablecer", "恢复", "Entsperren"),
    (admin_remove, "Remove from server", "서버에서 제거", "サーバーから削除", "喺伺服器移除", "Odebrat ze serveru", "Quitar del servidor", "从服务器移除", "Vom Server entfernen"),
    (admin_suspended, "Suspended", "정지됨", "利用停止中", "已停用", "Pozastaveno", "Suspendido", "已停用", "Gesperrt"),
    (admin_is_admin, "Admin", "관리자", "管理者", "管理員", "Správce", "Administrador", "管理员", "Admin"),
    (admin_suspend_title, "Suspend {name}?", "{name}을(를) 정지할까요?", "{name} を利用停止にしますか？", "停用 {name}？", "Pozastavit {name}?", "¿Suspender a {name}?", "停用 {name}？", "{name} sperren?"),
    (admin_suspend_body, "Their existing sign-in stops working immediately and they cannot sign in again. Their rooms and messages are untouched, and you can reinstate them at any time.", "기존 로그인이 즉시 무효화되고 다시 로그인할 수 없습니다. 채팅방과 메시지는 그대로이며 언제든 정지를 해제할 수 있습니다.", "現在のログインは直ちに無効になり、再ログインもできなくなります。ルームとメッセージはそのままで、いつでも解除できます。", "佢而家嘅登入即刻失效，亦都唔可以再登入。聊天室同訊息唔會郁到，你隨時可以恢復。", "Jejich stávající přihlášení okamžitě přestane fungovat a znovu se nepřihlásí. Místnosti a zprávy zůstanou nedotčené a kdykoli je můžete obnovit.", "Su sesión actual deja de funcionar de inmediato y no podrá volver a entrar. Sus salas y mensajes quedan intactos, y puedes restablecerlo cuando quieras.", "其现有登录会立即失效，且无法再次登录。其聊天室和消息不受影响，你可以随时恢复。", "Die bestehende Anmeldung wird sofort ungültig und eine erneute Anmeldung ist nicht möglich. Räume und Nachrichten bleiben unberührt, und du kannst jederzeit entsperren."),
    (admin_remove_title, "Remove {name} from this server?", "{name}을(를) 이 서버에서 제거할까요?", "{name} をこのサーバーから削除しますか？", "喺呢個伺服器移除 {name}？", "Odebrat {name} z tohoto serveru?", "¿Quitar a {name} de este servidor?", "把 {name} 从此服务器移除？", "{name} von diesem Server entfernen?"),
    (admin_remove_body, "They leave every room, lose their room keys, and are suspended. Every room they were in will need re-keying. Their messages stay where they are, still attributed to them.", "모든 채팅방에서 나가고 채팅방 키를 잃으며 정지됩니다. 그가 있던 모든 채팅방은 키를 교체해야 합니다. 메시지는 작성자 표시와 함께 그대로 남습니다.", "すべてのルームから退出し、ルームキーを失い、利用停止になります。在籍していた各ルームは鍵の更新が必要です。メッセージは投稿者名とともにそのまま残ります。", "佢會離開所有聊天室、失去聊天室金鑰，同埋被停用。佢去過嘅聊天室都要換金鑰。佢嘅訊息會照留低，仍然顯示佢個名。", "Opustí všechny místnosti, přijdou o klíče a budou pozastaveni. Každá místnost, kde byli, bude potřebovat obměnu klíče. Jejich zprávy zůstanou, stále s uvedením autora.", "Sale de todas las salas, pierde sus claves y queda suspendido. Cada sala en la que estuvo necesitará rotar la clave. Sus mensajes permanecen, aún atribuidos a esa persona.", "其将退出所有聊天室、失去聊天室密钥，并被停用。其待过的每个聊天室都需要轮换密钥。其消息将保留，且仍标注为其所发。", "Die Person verlässt jeden Raum, verliert ihre Raumschlüssel und wird gesperrt. Jeder Raum, in dem sie war, muss neu verschlüsselt werden. Ihre Nachrichten bleiben erhalten, weiterhin ihr zugeordnet."),
    (admin_delete_room_body, "The room, its messages and its keys are removed for every member. Use this for a room whose last admin has gone. It can't be undone.", "채팅방과 메시지, 키가 모든 멤버에게서 삭제됩니다. 마지막 관리자가 떠난 채팅방에 사용하세요. 되돌릴 수 없습니다.", "ルーム、そのメッセージ、鍵がすべてのメンバーから削除されます。最後の管理者がいなくなったルームに使ってください。取り消せません。", "聊天室、佢嘅訊息同金鑰會為所有成員刪走。用喺最後一個管理員都走咗嘅聊天室。無法還原。", "Místnost, její zprávy i klíče se odstraní všem členům. Použijte pro místnost, jejíž poslední správce odešel. Nelze vzít zpět.", "La sala, sus mensajes y sus claves se eliminan para todos los miembros. Úsalo con una sala cuyo último administrador se ha ido. No se puede deshacer.", "聊天室及其消息和密钥将为所有成员删除。用于最后一位管理员已离开的聊天室。无法撤销。", "Der Raum, seine Nachrichten und Schlüssel werden für alle Mitglieder entfernt. Für einen Raum, dessen letzter Admin weg ist. Es lässt sich nicht rückgängig machen."),
    (admin_configured_by, "Administrators come from VITE_FRUITNATION_ADMIN on the server. Nothing here can grant or revoke the role.", "관리자는 서버의 VITE_FRUITNATION_ADMIN 설정에서 정해집니다. 여기서는 권한을 주거나 뺏을 수 없습니다.", "管理者はサーバーの VITE_FRUITNATION_ADMIN で決まります。ここから権限を付与・剥奪することはできません。", "管理員係由伺服器嘅 VITE_FRUITNATION_ADMIN 決定。呢度改唔到權限。", "Správci pocházejí z VITE_FRUITNATION_ADMIN na serveru. Odsud roli udělit ani odebrat nelze.", "Los administradores salen de VITE_FRUITNATION_ADMIN en el servidor. Desde aquí no se puede conceder ni revocar el rol.", "管理员来自服务器上的 VITE_FRUITNATION_ADMIN。此处无法授予或撤销该角色。", "Administratoren stammen aus VITE_FRUITNATION_ADMIN auf dem Server. Hier lässt sich die Rolle weder vergeben noch entziehen."),
    (admin_room_one, "{n} room", "채팅방 {n}개", "ルーム{n}件", "{n} 個聊天室", "{n} místnost", "{n} sala", "{n} 个聊天室", "{n} Raum"),
    (admin_room_many, "{n} rooms", "채팅방 {n}개", "ルーム{n}件", "{n} 個聊天室", "{n} místností", "{n} salas", "{n} 个聊天室", "{n} Räume"),
    (admin_message_one, "{n} message", "메시지 {n}개", "メッセージ{n}件", "{n} 則訊息", "{n} zpráva", "{n} mensaje", "{n} 条消息", "{n} Nachricht"),
    (admin_message_many, "{n} messages", "메시지 {n}개", "メッセージ{n}件", "{n} 則訊息", "{n} zpráv", "{n} mensajes", "{n} 条消息", "{n} Nachrichten"),
    (admin_totals, "{users} people · {channels} channels · {dms} direct · {messages} messages", "사용자 {users}명 · 채널 {channels}개 · 다이렉트 {dms}개 · 메시지 {messages}개", "ユーザー{users}人 · チャンネル{channels}個 · ダイレクト{dms}個 · メッセージ{messages}件", "{users} 個用戶 · {channels} 個頻道 · {dms} 個私訊 · {messages} 則訊息", "{users} lidí · {channels} kanálů · {dms} přímých · {messages} zpráv", "{users} personas · {channels} canales · {dms} directos · {messages} mensajes", "{users} 位用户 · {channels} 个频道 · {dms} 个私信 · {messages} 条消息", "{users} Personen · {channels} Kanäle · {dms} direkt · {messages} Nachrichten"),

    (filter_notes, "Filter taught notes…", "가르친 내용 필터…", "教えた内容をフィルタ…", "篩選教過嘅嘢…", "Filtrovat naučené poznámky…", "Filtrar lo enseñado…", "筛选已教内容…", "Beigebrachte Notizen filtern…"),
    (no_matching_notes, "No notes match that filter", "필터와 일치하는 내용이 없습니다", "フィルタに一致するものがありません", "冇符合篩選嘅嘢", "Žádné poznámky neodpovídají filtru", "Ninguna nota coincide con ese filtro", "没有符合筛选的内容", "Keine Notizen passen zu diesem Filter"),
    (couldnt_load_notes, "Couldn't fetch the taught notes", "가르친 내용을 불러오지 못했습니다", "教えた内容を取得できませんでした", "攞唔到教過嘅嘢", "Nepodařilo se načíst naučené poznámky", "No se pudieron obtener las notas enseñadas", "无法获取已教内容", "Beigebrachte Notizen konnten nicht geladen werden"),

    // --- Counted phrases -----------------------------------------------------
    // Whole sentences with `{n}`, in the one/many pattern established by
    // `blocked_count_*`: English needs the plural `s`, Korean and Japanese
    // want a counter word rather than one, and Czech has a third form it
    // shares with the many case here. Assembling these from a number and a
    // noun produces a sentence no language actually speaks.
    (member_count_one, "{n} member", "멤버 {n}명", "メンバー{n}人", "{n} 個成員", "{n} člen", "{n} miembro", "{n} 位成员", "{n} Mitglied"),
    (member_count_many, "{n} members", "멤버 {n}명", "メンバー{n}人", "{n} 個成員", "{n} členů", "{n} miembros", "{n} 位成员", "{n} Mitglieder"),
    (sealed_keys_one, "{n} earlier key on this device couldn't be opened, so some history stays sealed.", "이 기기의 이전 키 {n}개를 열지 못해 일부 기록은 봉인된 채로 남습니다.", "この端末の以前の鍵{n}個を開けなかったため、一部の履歴は封じられたままです。", "呢部裝置有 {n} 條舊金鑰開唔到，所以有啲記錄仍然封住。", "{n} starší klíč na tomto zařízení se nepodařilo otevřít, takže část historie zůstává zapečetěná.", "No se pudo abrir {n} clave anterior en este dispositivo, así que parte del historial queda sellado.", "此设备上有 {n} 个旧密钥无法打开，部分历史记录仍处于封存状态。", "{n} älterer Schlüssel auf diesem Gerät ließ sich nicht öffnen, daher bleibt ein Teil des Verlaufs versiegelt."),
    (sealed_keys_many, "{n} earlier keys on this device couldn't be opened, so some history stays sealed.", "이 기기의 이전 키 {n}개를 열지 못해 일부 기록은 봉인된 채로 남습니다.", "この端末の以前の鍵{n}個を開けなかったため、一部の履歴は封じられたままです。", "呢部裝置有 {n} 條舊金鑰開唔到，所以有啲記錄仍然封住。", "{n} starších klíčů na tomto zařízení se nepodařilo otevřít, takže část historie zůstává zapečetěná.", "No se pudieron abrir {n} claves anteriores en este dispositivo, así que parte del historial queda sellado.", "此设备上有 {n} 个旧密钥无法打开，部分历史记录仍处于封存状态。", "{n} ältere Schlüssel auf diesem Gerät ließen sich nicht öffnen, daher bleibt ein Teil des Verlaufs versiegelt."),

    // --- Sign-in validation --------------------------------------------------
    (enter_recovery_phrase, "Enter your recovery phrase.", "복구 문구를 입력하세요.", "リカバリーフレーズを入力してください。", "請輸入你嘅復原字詞。", "Zadejte svou obnovovací frázi.", "Introduce tu frase de recuperación.", "请输入恢复助记词。", "Gib deine Wiederherstellungsphrase ein."),
    (phrase_word_count_one, "That's {n} word. A recovery phrase is 12, 15, 18, 21 or 24 words and must pass its checksum.", "{n}개 단어입니다. 복구 문구는 12, 15, 18, 21 또는 24개 단어여야 하고 체크섬도 맞아야 합니다.", "{n}語です。リカバリーフレーズは12、15、18、21、24語のいずれかで、チェックサムも一致する必要があります。", "得 {n} 個字。復原字詞要 12、15、18、21 或者 24 個字，仲要通過檢查碼。", "To je {n} slovo. Obnovovací fráze má 12, 15, 18, 21 nebo 24 slov a musí projít kontrolním součtem.", "Son {n} palabra. Una frase de recuperación tiene 12, 15, 18, 21 o 24 palabras y debe pasar su suma de verificación.", "只有 {n} 个词。恢复助记词应为 12、15、18、21 或 24 个词，且必须通过校验。", "Das ist {n} Wort. Eine Wiederherstellungsphrase hat 12, 15, 18, 21 oder 24 Wörter und muss ihre Prüfsumme bestehen."),
    (phrase_word_count_many, "That's {n} words. A recovery phrase is 12, 15, 18, 21 or 24 words and must pass its checksum.", "{n}개 단어입니다. 복구 문구는 12, 15, 18, 21 또는 24개 단어여야 하고 체크섬도 맞아야 합니다.", "{n}語です。リカバリーフレーズは12、15、18、21、24語のいずれかで、チェックサムも一致する必要があります。", "得 {n} 個字。復原字詞要 12、15、18、21 或者 24 個字，仲要通過檢查碼。", "To je {n} slov. Obnovovací fráze má 12, 15, 18, 21 nebo 24 slov a musí projít kontrolním součtem.", "Son {n} palabras. Una frase de recuperación tiene 12, 15, 18, 21 o 24 palabras y debe pasar su suma de verificación.", "共 {n} 个词。恢复助记词应为 12、15、18、21 或 24 个词，且必须通过校验。", "Das sind {n} Wörter. Eine Wiederherstellungsphrase hat 12, 15, 18, 21 oder 24 Wörter und muss ihre Prüfsumme bestehen."),
    (couldnt_derive_wallet, "Couldn't derive that wallet: {error}", "지갑을 만들지 못했습니다: {error}", "そのウォレットを導出できませんでした: {error}", "生成唔到嗰個錢包：{error}", "Peněženku se nepodařilo odvodit: {error}", "No se pudo derivar esa cartera: {error}", "无法派生该钱包：{error}", "Wallet konnte nicht abgeleitet werden: {error}"),
    (enter_private_key, "Enter your private key.", "개인 키를 입력하세요.", "秘密鍵を入力してください。", "請輸入你嘅私密金鑰。", "Zadejte svůj soukromý klíč.", "Introduce tu clave privada.", "请输入私钥。", "Gib deinen privaten Schlüssel ein."),
    (private_key_hex_only, "A private key is hexadecimal — 0-9 and a-f only.", "개인 키는 16진수입니다 — 0-9와 a-f만 사용합니다.", "秘密鍵は16進数です — 0〜9とa〜fのみです。", "私密金鑰係十六進位 — 淨係得 0-9 同 a-f。", "Soukromý klíč je šestnáctkový — pouze 0-9 a a-f.", "Una clave privada es hexadecimal: solo 0-9 y a-f.", "私钥是十六进制 — 仅限 0-9 和 a-f。", "Ein privater Schlüssel ist hexadezimal — nur 0-9 und a-f."),
    (private_key_length_one, "That's {n} hex character. A private key is exactly 64 (32 bytes), with an optional 0x prefix.", "16진수 {n}자입니다. 개인 키는 정확히 64자(32바이트)이며 0x 접두사는 선택입니다.", "16進数で{n}文字です。秘密鍵はちょうど64文字（32バイト）で、0xの接頭辞は任意です。", "得 {n} 個十六進位字元。私密金鑰啱啱好 64 個（32 位元組），0x 前綴可有可無。", "To je {n} šestnáctkový znak. Soukromý klíč má přesně 64 (32 bajtů), s volitelnou předponou 0x.", "Es {n} carácter hexadecimal. Una clave privada tiene exactamente 64 (32 bytes), con el prefijo 0x opcional.", "只有 {n} 个十六进制字符。私钥应恰好为 64 个（32 字节），0x 前缀可选。", "Das ist {n} Hex-Zeichen. Ein privater Schlüssel hat genau 64 (32 Bytes), mit optionalem 0x-Präfix."),
    (private_key_length_many, "That's {n} hex characters. A private key is exactly 64 (32 bytes), with an optional 0x prefix.", "16진수 {n}자입니다. 개인 키는 정확히 64자(32바이트)이며 0x 접두사는 선택입니다.", "16進数で{n}文字です。秘密鍵はちょうど64文字（32バイト）で、0xの接頭辞は任意です。", "得 {n} 個十六進位字元。私密金鑰啱啱好 64 個（32 位元組），0x 前綴可有可無。", "To je {n} šestnáctkových znaků. Soukromý klíč má přesně 64 (32 bajtů), s volitelnou předponou 0x.", "Son {n} caracteres hexadecimales. Una clave privada tiene exactamente 64 (32 bytes), con el prefijo 0x opcional.", "共 {n} 个十六进制字符。私钥应恰好为 64 个（32 字节），0x 前缀可选。", "Das sind {n} Hex-Zeichen. Ein privater Schlüssel hat genau 64 (32 Bytes), mit optionalem 0x-Präfix."),
    (private_key_not_scalar, "That key is not a valid secp256k1 scalar. Check it for a typo.", "이 키는 유효한 secp256k1 스칼라가 아닙니다. 오타가 없는지 확인하세요.", "この鍵は有効なsecp256k1スカラーではありません。入力ミスがないか確認してください。", "呢條金鑰唔係有效嘅 secp256k1 純量。睇下係咪打錯咗。", "Tento klíč není platný skalár secp256k1. Zkontrolujte překlep.", "Esa clave no es un escalar secp256k1 válido. Revisa si hay una errata.", "这不是有效的 secp256k1 标量。请检查是否有笔误。", "Dieser Schlüssel ist kein gültiger secp256k1-Skalar. Prüfe ihn auf Tippfehler."),

    // --- Sign-in screen ------------------------------------------------------
    (offline_can_still_create, "No connection. You can still create a wallet — sign-in will finish when you're back online.", "연결이 없습니다. 지갑은 지금 만들 수 있고, 로그인은 다시 온라인이 되면 완료됩니다.", "接続がありません。ウォレットの作成は今できます。サインインはオンラインに戻ったときに完了します。", "冇連線。你仍然可以建立錢包 — 等你返到線就會完成登入。", "Bez připojení. Peněženku si můžete vytvořit i teď — přihlášení se dokončí, až budete zpět online.", "Sin conexión. Aún puedes crear una cartera: el inicio de sesión se completará cuando vuelvas a estar en línea.", "没有网络连接。你仍可以创建钱包 — 恢复在线后将完成登录。", "Keine Verbindung. Du kannst trotzdem eine Wallet erstellen — die Anmeldung wird abgeschlossen, sobald du wieder online bist."),
    (unlocking_with_saved_phrase, "Unlocking with the recovery phrase saved on this device…", "이 기기에 저장된 복구 문구로 잠금을 해제하는 중…", "この端末に保存されたリカバリーフレーズで解除しています…", "用呢部裝置儲低嘅復原字詞解鎖緊…", "Odemykám obnovovací frází uloženou na tomto zařízení…", "Desbloqueando con la frase de recuperación guardada en este dispositivo…", "正在用此设备保存的恢复助记词解锁…", "Entsperren mit der auf diesem Gerät gespeicherten Wiederherstellungsphrase…"),
    (copy_phrase, "Copy phrase", "문구 복사", "フレーズをコピー", "複製字詞", "Kopírovat frázi", "Copiar la frase", "复制助记词", "Phrase kopieren"),
    (download_backup, "Download backup", "백업 내려받기", "バックアップをダウンロード", "下載備份", "Stáhnout zálohu", "Descargar copia de seguridad", "下载备份", "Sicherung herunterladen"),
    (back_up_first_hint, "Copy or download the phrase first — it is the only way back into this account.", "먼저 문구를 복사하거나 내려받으세요 — 이 계정으로 돌아올 수 있는 유일한 방법입니다.", "先にフレーズをコピーまたはダウンロードしてください — このアカウントに戻る唯一の方法です。", "先複製或者下載字詞 — 呢個係返返嚟呢個帳戶嘅唯一方法。", "Nejprve frázi zkopírujte nebo stáhněte — je to jediná cesta zpět k tomuto účtu.", "Copia o descarga la frase primero: es la única forma de volver a esta cuenta.", "请先复制或下载助记词 — 这是找回此账户的唯一方式。", "Kopiere oder lade die Phrase zuerst herunter — sie ist der einzige Weg zurück zu diesem Konto."),
    (nothing_is_kept_hint, "Nothing is kept. You'll re-enter your recovery phrase after every reload to read encrypted rooms.", "아무것도 저장되지 않습니다. 암호화된 채팅방을 읽으려면 새로 고칠 때마다 복구 문구를 다시 입력해야 합니다.", "何も保存されません。暗号化されたルームを読むには、再読み込みのたびにリカバリーフレーズを入力し直します。", "唔會留低任何嘢。每次重新載入都要再入復原字詞先睇到加密聊天室。", "Nic se neuchovává. Po každém načtení zadáte obnovovací frázi znovu, abyste mohli číst šifrované místnosti.", "No se guarda nada. Volverás a introducir tu frase de recuperación tras cada recarga para leer las salas cifradas.", "不保存任何内容。每次刷新后都需要重新输入恢复助记词才能读取加密聊天室。", "Nichts wird behalten. Nach jedem Neuladen gibst du deine Wiederherstellungsphrase erneut ein, um verschlüsselte Räume zu lesen."),
    (signing_the_challenge, "Signing the challenge…", "챌린지에 서명하는 중…", "チャレンジに署名しています…", "簽緊挑戰碼…", "Podepisuji výzvu…", "Firmando el desafío…", "正在签署验证信息…", "Challenge wird signiert…"),

    // --- Assorted screens ----------------------------------------------------
    (phrase_saved_hint, "Saved, so reloading signs you back in without asking. Anyone who can use this browser profile can read your messages.", "저장되어 있어 새로 고쳐도 묻지 않고 다시 로그인됩니다. 이 브라우저 프로필을 쓸 수 있는 사람은 누구나 메시지를 읽을 수 있습니다.", "保存されているため、再読み込みしても尋ねられずに再サインインします。このブラウザープロファイルを使える人は誰でもメッセージを読めます。", "已經儲低，所以重新載入會唔問你就登返入去。任何用得到呢個瀏覽器設定檔嘅人都睇到你啲訊息。", "Uloženo, takže po načtení budete přihlášeni bez dotazu. Kdokoli s přístupem k tomuto profilu prohlížeče si přečte vaše zprávy.", "Guardada, así que al recargar entrarás de nuevo sin preguntas. Cualquiera que use este perfil del navegador puede leer tus mensajes.", "已保存，刷新后无需询问即可重新登录。任何能使用此浏览器配置文件的人都能读到你的消息。", "Gespeichert — ein Neuladen meldet dich ohne Nachfrage wieder an. Wer dieses Browserprofil nutzen kann, kann deine Nachrichten lesen."),
    (private_key_saved_hint, "A private key is saved, so reloading signs you back in without asking. Anyone who can use this browser profile can read your messages.", "개인 키가 저장되어 있어 새로 고쳐도 묻지 않고 다시 로그인됩니다. 이 브라우저 프로필을 쓸 수 있는 사람은 누구나 메시지를 읽을 수 있습니다.", "秘密鍵が保存されているため、再読み込みしても尋ねられずに再サインインします。このブラウザープロファイルを使える人は誰でもメッセージを読めます。", "私密金鑰已經儲低，所以重新載入會唔問你就登返入去。任何用得到呢個瀏覽器設定檔嘅人都睇到你啲訊息。", "Je uložen soukromý klíč, takže po načtení budete přihlášeni bez dotazu. Kdokoli s přístupem k tomuto profilu prohlížeče si přečte vaše zprávy.", "Hay una clave privada guardada, así que al recargar entrarás de nuevo sin preguntas. Cualquiera que use este perfil del navegador puede leer tus mensajes.", "已保存私钥，刷新后无需询问即可重新登录。任何能使用此浏览器配置文件的人都能读到你的消息。", "Ein privater Schlüssel ist gespeichert — ein Neuladen meldet dich ohne Nachfrage wieder an. Wer dieses Browserprofil nutzen kann, kann deine Nachrichten lesen."),
    (phrase_not_saved_hint, "Not saved. You'll re-enter it after every reload to read encrypted rooms.", "저장되지 않았습니다. 암호화된 채팅방을 읽으려면 새로 고칠 때마다 다시 입력해야 합니다.", "保存されていません。暗号化されたルームを読むには、再読み込みのたびに入力し直します。", "冇儲低。每次重新載入都要再入一次先睇到加密聊天室。", "Neuloženo. Po každém načtení ji zadáte znovu, abyste mohli číst šifrované místnosti.", "No guardada. Volverás a introducirla tras cada recarga para leer las salas cifradas.", "未保存。每次刷新后需重新输入才能读取加密聊天室。", "Nicht gespeichert. Nach jedem Neuladen gibst du sie erneut ein, um verschlüsselte Räume zu lesen."),
    (working, "Working…", "처리 중…", "処理中…", "處理緊…", "Pracuji…", "Trabajando…", "处理中…", "Arbeitet…"),
    (block_someone, "Block someone", "차단할 사람 추가", "ブロックする相手を追加", "封鎖某人", "Zablokovat někoho", "Bloquear a alguien", "屏蔽某人", "Jemanden blockieren"),
    (encrypt_this_room, "Encrypt this room", "이 채팅방 암호화", "このルームを暗号化", "加密呢個聊天室", "Šifrovat tuto místnost", "Cifrar esta sala", "加密此聊天室", "Diesen Raum verschlüsseln"),
    (encrypt_this_room_hint, "Messages are readable only by members. Encryption can't be turned on later.", "메시지는 멤버만 읽을 수 있습니다. 암호화는 나중에 켤 수 없습니다.", "メッセージはメンバーだけが読めます。暗号化を後から有効にすることはできません。", "淨係成員先睇到訊息。加密之後開唔返。", "Zprávy si přečtou jen členové. Šifrování nelze zapnout dodatečně.", "Solo los miembros pueden leer los mensajes. El cifrado no se puede activar después.", "只有成员能读取消息。加密之后无法再开启。", "Nachrichten sind nur für Mitglieder lesbar. Verschlüsselung lässt sich nicht nachträglich einschalten."),
    (invite_key_on_accept, "They'll receive the room key when they accept.", "수락하면 채팅방 키를 받게 됩니다.", "承諾すると、ルームキーが渡されます。", "佢接受咗就會收到聊天室金鑰。", "Klíč místnosti dostanou, jakmile pozvánku přijmou.", "Recibirá la clave de la sala cuando acepte.", "对方接受后将收到聊天室密钥。", "Sie erhalten den Raumschlüssel, sobald sie annehmen."),
    (reply_suggestion_hint, "Suggests a reply from the last few messages. They are decrypted on this device and sent to the provider you selected — only when you press Generate.", "최근 메시지 몇 개로 답장을 제안합니다. 메시지는 이 기기에서 복호화되어 선택한 제공자에게 전송되며, 생성을 누를 때만 전송됩니다.", "直近のいくつかのメッセージから返信を提案します。メッセージはこの端末で復号され、選択したプロバイダーに送信されます — 生成を押したときだけです。", "會用最近幾條訊息建議一個回覆。訊息喺呢部裝置解密，然後送去你揀嘅供應商 — 淨係喺你㩒生成嗰陣先會送。", "Navrhne odpověď z několika posledních zpráv. Ty se dešifrují na tomto zařízení a odešlou vybranému poskytovateli — jen když stisknete Generovat.", "Sugiere una respuesta a partir de los últimos mensajes. Se descifran en este dispositivo y se envían al proveedor que elegiste, solo cuando pulsas Generar.", "根据最近几条消息建议回复。消息在此设备上解密后发送给你选择的提供商 — 仅在你按下生成时。", "Schlägt eine Antwort aus den letzten Nachrichten vor. Sie werden auf diesem Gerät entschlüsselt und an den gewählten Anbieter gesendet — nur wenn du auf Generieren drückst."),
    (send_to, "To", "받는 사람", "宛先", "收款人", "Komu", "Para", "接收方", "An"),
    (verified_suffix, "verified", "검증됨", "検証済み", "已驗證", "ověřeno", "verificado", "已验证", "verifiziert"),
    (invitation_from, "from {name} · ", "초대: {name} · ", "{name}より · ", "由 {name} · ", "od {name} · ", "de {name} · ", "来自 {name} · ", "von {name} · "),
    (unlock_tagline, "Welcome back. Enter your {method} to unlock encryption.", "다시 오셨네요. 암호화를 해제하려면 {method}을(를) 입력하세요.", "おかえりなさい。暗号化を解除するには{method}を入力してください。", "歡迎返嚟。輸入你嘅{method}嚟解鎖加密。", "Vítejte zpět. Zadejte {method} pro odemknutí šifrování.", "Bienvenido de nuevo. Introduce tu {method} para desbloquear el cifrado.", "欢迎回来。输入你的{method}以解锁加密。", "Willkommen zurück. Gib deine {method} ein, um die Verschlüsselung zu entsperren."),
    (username_suggested_hint, "Leave blank to be named {name} — the same name any other client picks for this wallet.", "비워 두면 {name}(으)로 정해집니다 — 다른 클라이언트도 이 지갑에 같은 이름을 씁니다.", "空欄のままにすると{name}になります — 他のクライアントもこのウォレットに同じ名前を付けます。", "留空就會叫做 {name} — 其他客戶端都會為呢個錢包揀同一個名。", "Nechte prázdné a budete se jmenovat {name} — stejné jméno zvolí pro tuto peněženku i každý jiný klient.", "Déjalo vacío para llamarte {name}: el mismo nombre que cualquier otro cliente elige para esta cartera.", "留空将命名为 {name} — 其他客户端也会为此钱包选择同样的名字。", "Leer lassen, um {name} zu heißen — denselben Namen wählt jeder andere Client für diese Wallet."),
    (no_rooms_match, "No rooms match “{query}”", "“{query}”와 일치하는 채팅방이 없습니다", "「{query}」に一致するルームはありません", "冇聊天室符合「{query}」", "Žádné místnosti neodpovídají „{query}“", "Ninguna sala coincide con «{query}»", "没有聊天室匹配“{query}”", "Keine Räume passen zu „{query}“"),
    (couldnt_generate_wallet, "Couldn't generate a wallet: {error}", "지갑을 만들지 못했습니다: {error}", "ウォレットを生成できませんでした: {error}", "生成唔到錢包：{error}", "Peněženku se nepodařilo vygenerovat: {error}", "No se pudo generar una cartera: {error}", "无法生成钱包：{error}", "Wallet konnte nicht erzeugt werden: {error}"),
    (message_placeholder, "Message {room}", "{room}에 메시지 보내기", "{room}にメッセージ", "喺 {room} 出訊息", "Zpráva do {room}", "Mensaje para {room}", "发消息到 {room}", "Nachricht an {room}"),
    (block_person, "Block {name}", "{name}님 차단", "{name}をブロック", "封鎖 {name}", "Blokovat {name}", "Bloquear a {name}", "屏蔽 {name}", "{name} blockieren"),
    (unblock_person, "Unblock {name}", "{name}님 차단 해제", "{name}のブロックを解除", "解除封鎖 {name}", "Odblokovat {name}", "Desbloquear a {name}", "取消屏蔽 {name}", "Blockierung von {name} aufheben"),
    (wallet_nav_label, "Wallet, {network} active", "지갑, {network} 사용 중", "ウォレット、{network}を使用中", "錢包，使用緊 {network}", "Peněženka, aktivní {network}", "Cartera, {network} activa", "钱包，当前网络 {network}", "Wallet, {network} aktiv"),
    (invitations_pending, "Invitations, {n} pending", "초대, 대기 중 {n}건", "招待、保留中{n}件", "邀請，有 {n} 個待處理", "Pozvánky, {n} čeká", "Invitaciones, {n} pendientes", "邀请，{n} 条待处理", "Einladungen, {n} ausstehend"),
    (hosting_failed, "Hosting failed: {error}", "이미지 호스팅에 실패했습니다: {error}", "ホスティングに失敗しました: {error}", "寄存失敗：{error}", "Hostování selhalo: {error}", "Falló el alojamiento: {error}", "图片托管失败：{error}", "Hosting fehlgeschlagen: {error}"),
    (room_created_unencrypted, "Room created without encryption. {reason}.", "암호화 없이 채팅방을 만들었습니다. {reason}.", "暗号化なしでルームを作成しました。{reason}。", "已經建立咗聊天室，但冇加密。{reason}。", "Místnost byla vytvořena bez šifrování. {reason}.", "Sala creada sin cifrado. {reason}.", "聊天室已创建但未加密。{reason}。", "Raum ohne Verschlüsselung erstellt. {reason}."),
    (couldnt_send_key_now, "Couldn't send it now: {reason}.", "지금은 보내지 못했습니다: {reason}.", "今は送信できませんでした: {reason}。", "而家送唔到：{reason}。", "Teď se to nepodařilo odeslat: {reason}.", "No se pudo enviar ahora: {reason}.", "现在无法发送：{reason}。", "Konnte jetzt nicht gesendet werden: {reason}."),
    (amount_error, "Amount: {error}.", "금액: {error}.", "金額: {error}。", "金額：{error}。", "Částka: {error}.", "Importe: {error}.", "金额：{error}。", "Betrag: {error}."),
    (send_amount, "Send {amount} {symbol}", "{amount} {symbol} 보내기", "{amount} {symbol}を送る", "傳送 {amount} {symbol}", "Odeslat {amount} {symbol}", "Enviar {amount} {symbol}", "发送 {amount} {symbol}", "{amount} {symbol} senden"),
    (rpc_unreachable, "Couldn't reach the RPC endpoint: {error}.", "RPC 엔드포인트에 연결하지 못했습니다: {error}.", "RPCエンドポイントに接続できませんでした: {error}。", "連唔到 RPC 端點：{error}。", "Nepodařilo se spojit s koncovým bodem RPC: {error}.", "No se pudo contactar con el endpoint RPC: {error}.", "无法连接 RPC 端点：{error}。", "RPC-Endpunkt nicht erreichbar: {error}."),
    (nonce_fetch_failed, "Couldn't fetch the account nonce: {error}.", "계정 논스를 가져오지 못했습니다: {error}.", "アカウントのnonceを取得できませんでした: {error}。", "攞唔到帳戶 nonce：{error}。", "Nepodařilo se načíst nonce účtu: {error}.", "No se pudo obtener el nonce de la cuenta: {error}.", "无法获取账户 nonce：{error}。", "Konto-Nonce konnte nicht geladen werden: {error}."),
    (signing_failed, "Signing failed: {error}.", "서명에 실패했습니다: {error}.", "署名に失敗しました: {error}。", "簽名失敗：{error}。", "Podepsání selhalo: {error}.", "Falló la firma: {error}.", "签名失败：{error}。", "Signieren fehlgeschlagen: {error}."),
    (no_evm_chain_id, "This network has no EVM chain id.", "이 네트워크에는 EVM 체인 ID가 없습니다.", "このネットワークにはEVMチェーンIDがありません。", "呢個網絡冇 EVM 鏈 ID。", "Tato síť nemá EVM chain id.", "Esta red no tiene un id de cadena EVM.", "此网络没有 EVM 链 ID。", "Dieses Netzwerk hat keine EVM-Chain-ID."),
    (bad_token_address, "The token contract address in the registry is invalid.", "레지스트리의 토큰 컨트랙트 주소가 올바르지 않습니다.", "レジストリのトークンコントラクトアドレスが無効です。", "登記表入面嘅代幣合約地址無效。", "Adresa kontraktu tokenu v registru je neplatná.", "La dirección del contrato del token en el registro no es válida.", "注册表中的代币合约地址无效。", "Die Token-Vertragsadresse in der Liste ist ungültig."),
    (network_rejected_tx, "The network rejected the transaction: {error}.", "네트워크가 트랜잭션을 거부했습니다: {error}.", "ネットワークがトランザクションを拒否しました: {error}。", "網絡拒絕咗呢筆交易：{error}。", "Síť transakci odmítla: {error}.", "La red rechazó la transacción: {error}.", "网络拒绝了该交易：{error}。", "Das Netzwerk hat die Transaktion abgelehnt: {error}."),
    // --- Screen-reader labels ------------------------------------------------
    // Not decoration: for someone using a screen reader these *are* the
    // interface, so an English label in a Korean UI is the same defect as
    // English visible text — just invisible to everyone who could report it.
    (messages_in_room, "Messages in {room}", "{room}의 메시지", "{room}のメッセージ", "{room} 嘅訊息", "Zprávy v {room}", "Mensajes en {room}", "{room} 的消息", "Nachrichten in {room}"),
    (remove_from_room, "Remove {name} from this room", "이 채팅방에서 {name}님 내보내기", "このルームから{name}を退出させる", "由呢個聊天室移除 {name}", "Odebrat {name} z této místnosti", "Quitar a {name} de esta sala", "将 {name} 移出此聊天室", "{name} aus diesem Raum entfernen"),
    (manage_admins_label, "Manage admins for this room", "이 채팅방의 관리자 관리", "このルームの管理者を管理", "管理呢個聊天室嘅管理員", "Spravovat správce této místnosti", "Gestionar administradores de esta sala", "管理此聊天室的管理员", "Admins dieses Raums verwalten"),
    (wallet_address_aria, "Wallet address {address}", "지갑 주소 {address}", "ウォレットアドレス {address}", "錢包地址 {address}", "Adresa peněženky {address}", "Dirección de cartera {address}", "钱包地址 {address}", "Wallet-Adresse {address}"),
    (react_with, "React with {emoji}", "{emoji}(으)로 반응하기", "{emoji}でリアクション", "用 {emoji} 反應", "Reagovat pomocí {emoji}", "Reaccionar con {emoji}", "用 {emoji} 回应", "Mit {emoji} reagieren"),
    (react_to_message, "React to {name}'s message", "{name}님의 메시지에 반응하기", "{name}のメッセージにリアクション", "回應 {name} 嘅訊息", "Reagovat na zprávu od {name}", "Reaccionar al mensaje de {name}", "回应 {name} 的消息", "Auf die Nachricht von {name} reagieren"),
    (more_actions_for, "More actions for {name}'s message", "{name}님의 메시지에 대한 추가 작업", "{name}のメッセージのその他の操作", "{name} 嘅訊息嘅更多操作", "Další akce pro zprávu od {name}", "Más acciones para el mensaje de {name}", "{name} 的消息的更多操作", "Weitere Aktionen für die Nachricht von {name}"),
    (copy_message_hash, "Copy message hash {hash}", "메시지 해시 {hash} 복사", "メッセージハッシュ {hash} をコピー", "複製訊息雜湊 {hash}", "Kopírovat hash zprávy {hash}", "Copiar el hash del mensaje {hash}", "复制消息哈希 {hash}", "Nachrichtenhash {hash} kopieren"),
    (copy_wallet_address, "Copy wallet address {address}", "지갑 주소 {address} 복사", "ウォレットアドレス {address} をコピー", "複製錢包地址 {address}", "Kopírovat adresu peněženky {address}", "Copiar la dirección de cartera {address}", "复制钱包地址 {address}", "Wallet-Adresse {address} kopieren"),
    (appearance_change, "Appearance: {theme}. Change.", "화면 모드: {theme}. 변경하기.", "外観: {theme}。変更する。", "外觀：{theme}。更改。", "Vzhled: {theme}. Změnit.", "Apariencia: {theme}. Cambiar.", "外观：{theme}。更改。", "Erscheinungsbild: {theme}. Ändern."),
    (message_hash_title, "Message hash {hash}. Click to copy.", "메시지 해시 {hash}. 클릭하면 복사됩니다.", "メッセージハッシュ {hash}。クリックでコピーします。", "訊息雜湊 {hash}。㩒一下就複製。", "Hash zprávy {hash}. Kliknutím zkopírujete.", "Hash del mensaje {hash}. Haz clic para copiar.", "消息哈希 {hash}。点击复制。", "Nachrichtenhash {hash}. Zum Kopieren klicken."),
    (no_one_found_for, "No one found for “{query}”", "“{query}”에 해당하는 사람이 없습니다", "「{query}」に該当する人はいません", "搵唔到「{query}」嘅人", "Pro „{query}“ se nikdo nenašel", "No se encontró a nadie para «{query}»", "没有找到与“{query}”匹配的人", "Niemand gefunden für „{query}“"),
    (dismiss_toast, "Dismiss: {title}", "닫기: {title}", "閉じる: {title}", "關閉：{title}", "Zavřít: {title}", "Descartar: {title}", "关闭：{title}", "Schließen: {title}"),

    // --- Bank (wallet: Balance / Send / Bank) --------------------------------
    (menu_balance, "Balance", "잔액", "残高", "餘額", "Zůstatek", "Saldo", "余额", "Saldo"),
    (menu_bank, "Bank", "은행", "バンク", "銀行", "Banka", "Banco", "银行", "Bank"),
    (bank_swap, "Swap", "스왑", "スワップ", "兌換", "Směna", "Intercambio", "兑换", "Tauschen"),
    (bank_tokens, "Tokens", "토큰", "トークン", "代幣", "Tokeny", "Tokens", "代币", "Token"),
    (bank_greeter, "Greeter", "그리터", "グリーター", "Greeter", "Greeter", "Greeter", "Greeter", "Greeter"),
    (from_label, "From", "보내는 자산", "スワップ元", "由", "Z", "De", "从", "Von"),
    (get_quote, "Get quote", "시세 조회", "見積もりを取得", "攞報價", "Získat nabídku", "Obtener cotización", "获取报价", "Kurs abrufen"),
    (quote_line, "{out} {sym} · min {min} after {slip}% slippage", "{out} {sym} · 슬리피지 {slip}% 적용 시 최소 {min}", "{out} {sym} · スリッページ{slip}%で最低{min}", "{out} {sym} · 滑點 {slip}% 後最少 {min}", "{out} {sym} · min {min} po skluzu {slip} %", "{out} {sym} · mín. {min} tras un deslizamiento del {slip} %", "{out} {sym} · 滑点 {slip}% 后最少 {min}", "{out} {sym} · min. {min} nach {slip} % Slippage"),
    (slippage_pct, "Slippage %", "슬리피지 %", "スリッページ %", "滑點 %", "Skluz %", "Deslizamiento %", "滑点 %", "Slippage %"),
    (swap_now, "Swap", "스왑하기", "スワップする", "兌換", "Směnit", "Intercambiar", "兑换", "Tauschen"),
    (swap_mainnet_only, "Swaps run on VVS Finance, which lives on Cronos mainnet — this deployment is on another chain.", "스왑은 Cronos 메인넷의 VVS Finance에서 실행됩니다 — 이 배포는 다른 체인에 있습니다.", "スワップはCronosメインネットのVVS Financeで実行されます — このデプロイは別のチェーンにあります。", "兌換喺 Cronos 主網嘅 VVS Finance 進行 — 呢個部署喺第二條鏈。", "Směny běží na VVS Finance na mainnetu Cronos — toto nasazení je na jiném řetězci.", "Los intercambios se ejecutan en VVS Finance, en la mainnet de Cronos; este despliegue está en otra cadena.", "兑换在 VVS Finance 上进行，它位于 Cronos 主网 — 此部署在另一条链上。", "Tausch läuft über VVS Finance auf dem Cronos-Mainnet — dieses Deployment ist auf einer anderen Chain."),
    (wrap_one_to_one, "Wrapping is 1:1 — no quote needed.", "래핑은 1:1이라 시세 조회가 필요 없습니다.", "ラップは1:1のため見積もりは不要です。", "包裝係 1:1 — 唔使報價。", "Zabalení je 1:1 — nabídka není potřeba.", "El envoltorio es 1:1: no hace falta cotización.", "封装是 1:1 的 — 无需报价。", "Wrapping ist 1:1 — kein Kurs nötig."),
    (approving_token, "Approving the router…", "라우터 승인 중…", "ルーターを承認中…", "授權緊路由器…", "Schvaluji router…", "Aprobando el router…", "正在授权路由合约…", "Router wird freigegeben…"),
    (broadcasting_tx, "Broadcasting…", "전송 중…", "ブロードキャスト中…", "廣播緊…", "Vysílám…", "Transmitiendo…", "广播中…", "Wird übertragen…"),
    (waiting_confirmation, "Waiting for confirmation…", "확인을 기다리는 중…", "承認を待っています…", "等緊確認…", "Čekám na potvrzení…", "Esperando confirmación…", "等待确认…", "Warte auf Bestätigung…"),
    (swap_confirmed, "Swap confirmed", "스왑이 완료되었습니다", "スワップが確定しました", "兌換完成", "Směna potvrzena", "Intercambio confirmado", "兑换已确认", "Tausch bestätigt"),
    (quote_failed, "Couldn't get a quote", "시세를 가져오지 못했습니다", "見積もりを取得できませんでした", "攞唔到報價", "Nabídku se nepodařilo získat", "No se pudo obtener la cotización", "无法获取报价", "Kurs konnte nicht abgerufen werden"),
    (tx_failed_generic, "Transaction failed", "트랜잭션이 실패했습니다", "トランザクションに失敗しました", "交易失敗", "Transakce selhala", "La transacción falló", "交易失败", "Transaktion fehlgeschlagen"),
    (import_token, "Import token", "토큰 가져오기", "トークンをインポート", "匯入代幣", "Importovat token", "Importar token", "导入代币", "Token importieren"),
    (token_address, "Token contract address", "토큰 컨트랙트 주소", "トークンのコントラクトアドレス", "代幣合約地址", "Adresa kontraktu tokenu", "Dirección del contrato del token", "代币合约地址", "Token-Vertragsadresse"),
    (token_added, "Token added", "토큰을 추가했습니다", "トークンを追加しました", "已加入代幣", "Token přidán", "Token añadido", "代币已添加", "Token hinzugefügt"),
    (not_an_erc20, "That address doesn't answer like an ERC-20.", "이 주소는 ERC-20처럼 응답하지 않습니다.", "このアドレスはERC-20として応答しません。", "呢個地址唔似 ERC-20 咁回應。", "Tato adresa neodpovídá jako ERC-20.", "Esa dirección no responde como un ERC-20.", "该地址的响应不像 ERC-20。", "Diese Adresse antwortet nicht wie ein ERC-20."),
    (deploy_token, "Deploy a token", "토큰 배포", "トークンをデプロイ", "部署代幣", "Nasadit token", "Desplegar un token", "部署代币", "Token bereitstellen"),
    (token_name, "Name", "이름", "名前", "名稱", "Název", "Nombre", "名称", "Name"),
    (token_symbol, "Symbol", "심볼", "シンボル", "符號", "Symbol", "Símbolo", "符号", "Symbol"),
    (token_decimals, "Decimals", "소수 자릿수", "小数桁数", "小數位", "Desetinná místa", "Decimales", "小数位数", "Dezimalstellen"),
    (initial_supply, "Initial supply", "초기 발행량", "初期供給量", "初始供應量", "Počáteční zásoba", "Suministro inicial", "初始发行量", "Anfangsbestand"),
    (deploying, "Deploying…", "배포 중…", "デプロイ中…", "部署緊…", "Nasazuji…", "Desplegando…", "部署中…", "Wird bereitgestellt…"),
    (deployed_at, "Deployed at {address}", "{address}에 배포되었습니다", "{address}にデプロイしました", "已部署喺 {address}", "Nasazeno na {address}", "Desplegado en {address}", "已部署到 {address}", "Bereitgestellt unter {address}"),
    (deploy_greeter, "Deploy a Greeter", "그리터 배포", "グリーターをデプロイ", "部署 Greeter", "Nasadit Greeter", "Desplegar un Greeter", "部署 Greeter", "Greeter bereitstellen"),
    (initial_greeting, "Initial greeting", "첫 인사말", "最初のあいさつ", "初始問候語", "Počáteční pozdrav", "Saludo inicial", "初始问候语", "Anfangsgruß"),
    (attach_existing, "Attach existing", "기존 연결", "既存をアタッチ", "連接現有", "Připojit existující", "Adjuntar existente", "连接现有合约", "Bestehenden anhängen"),
    (greeter_address, "Greeter contract address", "그리터 컨트랙트 주소", "グリーターのコントラクトアドレス", "Greeter 合約地址", "Adresa kontraktu Greeteru", "Dirección del contrato Greeter", "Greeter 合约地址", "Greeter-Vertragsadresse"),
    (set_greeting, "Set greeting", "인사말 변경", "あいさつを設定", "設定問候語", "Nastavit pozdrav", "Establecer saludo", "设置问候语", "Gruß setzen"),
    (new_greeting, "New greeting", "새 인사말", "新しいあいさつ", "新問候語", "Nový pozdrav", "Nuevo saludo", "新问候语", "Neuer Gruß"),
    (greeting_updated, "Greeting updated", "인사말을 변경했습니다", "あいさつを更新しました", "已更新問候語", "Pozdrav aktualizován", "Saludo actualizado", "问候语已更新", "Gruß aktualisiert"),
    (not_a_greeter, "That address doesn't answer greet().", "이 주소는 greet()에 응답하지 않습니다.", "このアドレスはgreet()に応答しません。", "呢個地址唔回應 greet()。", "Tato adresa neodpovídá na greet().", "Esa dirección no responde a greet().", "该地址不响应 greet()。", "Diese Adresse antwortet nicht auf greet()."),
    (no_greeters_yet, "No greeters yet — deploy one, or attach an address.", "아직 그리터가 없습니다 — 하나 배포하거나 주소를 연결하세요.", "まだグリーターがありません — デプロイするか、アドレスをアタッチしてください。", "仲未有 Greeter — 部署一個，或者連接一個地址。", "Zatím žádné greetery — nasaďte jeden, nebo připojte adresu.", "Aún no hay greeters: despliega uno o adjunta una dirección.", "还没有 Greeter — 部署一个，或连接一个地址。", "Noch keine Greeter — stelle einen bereit oder hänge eine Adresse an."),
    (wallet_locked, "Wallet locked", "지갑이 잠겨 있습니다", "ウォレットがロックされています", "錢包已鎖", "Peněženka uzamčena", "Cartera bloqueada", "钱包已锁定", "Wallet gesperrt"),
    (bank_portfolio, "Portfolio", "포트폴리오", "ポートフォリオ", "投資組合", "Portfolio", "Cartera", "投资组合", "Portfolio"),
    (bank_mainnet, "Mainnet", "메인넷", "メインネット", "主網", "Mainnet", "Mainnet", "主网", "Mainnet"),
    (bank_testnet, "Testnet", "테스트넷", "テストネット", "測試網", "Testnet", "Testnet", "测试网", "Testnet"),
    (bank_universal_hint, "Universal wallet — runs on its own network, independent of the chain configured on this server.", "유니버설 지갑 — 이 서버에 설정된 체인과 무관하게 자체 네트워크에서 동작합니다.", "ユニバーサルウォレット — このサーバーに設定されたチェーンとは無関係に、独自のネットワークで動作します。", "通用錢包 — 用自己嘅網絡運作，唔受呢個伺服器設定嘅鏈影響。", "Univerzální peněženka — běží na vlastní síti, nezávisle na řetězci nastaveném na tomto serveru.", "Cartera universal: funciona en su propia red, independiente de la cadena configurada en este servidor.", "通用钱包 — 使用自己的网络，与此服务器配置的链无关。", "Universelle Wallet — läuft auf eigenem Netzwerk, unabhängig von der auf diesem Server konfigurierten Chain."),
    (copy_address, "Copy address", "주소 복사", "アドレスをコピー", "複製地址", "Zkopírovat adresu", "Copiar dirección", "复制地址", "Adresse kopieren"),
    (view_full_address, "View full address", "전체 주소 보기", "アドレス全体を表示", "查看完整地址", "Zobrazit celou adresu", "Ver la dirección completa", "查看完整地址", "Vollständige Adresse anzeigen"),
    (hide_full_address, "Hide full address", "전체 주소 숨기기", "アドレス全体を非表示", "隱藏完整地址", "Skrýt celou adresu", "Ocultar la dirección completa", "隐藏完整地址", "Vollständige Adresse ausblenden"),
    // --- Type preferences -------------------------------------------------
    (font_face, "Font", "글꼴", "フォント", "字體", "Písmo", "Fuente", "字体", "Schriftart"),
    (text_size, "Text size", "글자 크기", "文字サイズ", "文字大小", "Velikost písma", "Tamaño del texto", "文字大小", "Textgröße"),
    (font_change, "Font: {font}. Change.", "글꼴: {font}. 변경.", "フォント: {font}。変更。", "字體：{font}。變更。", "Písmo: {font}. Změnit.", "Fuente: {font}. Cambiar.", "字体：{font}。更改。", "Schriftart: {font}. Ändern."),
    (text_size_change, "Text size: {size}. Change.", "글자 크기: {size}. 변경.", "文字サイズ: {size}。変更。", "文字大小：{size}。變更。", "Velikost písma: {size}. Změnit.", "Tamaño del texto: {size}. Cambiar.", "文字大小：{size}。更改。", "Textgröße: {size}. Ändern."),
    (font_system, "System", "시스템", "システム", "系統", "Systémové", "Sistema", "系统", "System"),
    (font_skynet, "Skynet", "스카이넷", "スカイネット", "Skynet", "Skynet", "Skynet", "Skynet", "Skynet"),
    (font_mono, "Mono", "고정폭", "等幅", "等寬", "Mono", "Mono", "等宽", "Mono"),
    (font_serif, "Serif", "명조", "明朝", "襯線", "Patkové", "Serif", "衬线", "Serife"),
    (size_compact, "Compact", "작게", "小", "細", "Kompaktní", "Compacto", "紧凑", "Kompakt"),
    (size_standard, "Standard", "보통", "標準", "標準", "Standardní", "Estándar", "标准", "Standard"),
    (size_large, "Large", "크게", "大", "大", "Velké", "Grande", "大", "Groß"),
    (size_xlarge, "Extra large", "아주 크게", "特大", "特大", "Extra velké", "Extra grande", "特大", "Sehr groß"),
    // --- Skin (art direction; orthogonal to light/dark) --------------------
    (skin, "Skin", "스킨", "スキン", "外觀", "Vzhled", "Aspecto", "外观", "Skin"),
    (skin_skynet, "Skynet", "스카이넷", "スカイネット", "Skynet", "Skynet", "Skynet", "Skynet", "Skynet"),
    (skin_cute, "Cute Skynet", "귀여운 스카이넷", "キュートスカイネット", "可愛 Skynet", "Roztomilý Skynet", "Skynet tierno", "可爱 Skynet", "Niedliches Skynet"),
    (skin_human, "Human Skynet", "휴먼 스카이넷", "ヒューマンスカイネット", "人類 Skynet", "Lidský Skynet", "Skynet humano", "人类 Skynet", "Menschliches Skynet"),
    (skin_change, "Skin: {skin}. Change.", "스킨: {skin}. 변경.", "スキン: {skin}。変更。", "外觀：{skin}。變更。", "Vzhled: {skin}. Změnit.", "Aspecto: {skin}. Cambiar.", "外观：{skin}。更改。", "Skin: {skin}. Ändern."),
    (bank_quick_actions, "Quick actions", "빠른 작업", "クイック操作", "快捷操作", "Rychlé akce", "Acciones rápidas", "快捷操作", "Schnellaktionen"),
    (bank_receive, "Receive", "받기", "受け取る", "收款", "Přijmout", "Recibir", "收款", "Empfangen"),
    (bank_receive_hint, "Share this address to receive funds on this network.", "이 네트워크에서 자산을 받으려면 이 주소를 공유하세요.", "このネットワークで資産を受け取るには、このアドレスを共有してください。", "喺呢個網絡收款，就分享呢個地址。", "Sdílejte tuto adresu pro příjem prostředků na této síti.", "Comparte esta dirección para recibir fondos en esta red.", "分享此地址即可在该网络接收资产。", "Teile diese Adresse, um auf diesem Netzwerk Guthaben zu empfangen."),
    (bank_tap_to_send, "Send {symbol}", "{symbol} 보내기", "{symbol}を送信", "傳送 {symbol}", "Odeslat {symbol}", "Enviar {symbol}", "发送 {symbol}", "{symbol} senden"),
    (bank_footnote, "Educational wallet — verify every transaction before confirming.", "교육용 지갑 — 모든 트랜잭션을 확인 전에 검증하세요.", "教育用ウォレット — すべてのトランザクションを確定前に確認してください。", "教學用錢包 — 每筆交易確認之前都要核對清楚。", "Výukový wallet — každou transakci před potvrzením ověřte.", "Cartera educativa: verifica cada transacción antes de confirmarla.", "教学用钱包 — 每笔交易确认前请仔细核对。", "Lern-Wallet — prüfe jede Transaktion, bevor du sie bestätigst."),
    (ai_banker, "AI Banker", "AI 뱅커", "AIバンカー", "AI 銀行家", "AI bankéř", "Banquero IA", "AI 银行家", "KI-Banker"),
    (banker_intro, "The banker reads the chain and can execute for you — sends, swaps, deploys. Risky moves always stop at an approval dialog first.", "뱅커는 체인을 읽고 송금·스왑·배포를 대신 실행할 수 있습니다 — 위험한 작업은 항상 먼저 승인 대화상자에서 멈춥니다.", "バンカーはチェーンを読み、送金・スワップ・デプロイを代行できます — 危険な操作は必ず先に承認ダイアログで止まります。", "銀行家會讀鏈，仲可以幫你執行 — 轉帳、兌換、部署。有風險嘅操作一定會先停喺批准對話框。", "Bankéř čte řetězec a umí za vás provádět akce — posílání, směny, nasazení. Riskantní kroky se vždy nejdřív zastaví v potvrzovacím dialogu.", "El banquero lee la cadena y puede ejecutar por ti: envíos, intercambios, despliegues. Las operaciones arriesgadas siempre se detienen antes en un diálogo de aprobación.", "银行家能读取链上数据并代你执行 — 转账、兑换、部署。有风险的操作一定会先停在批准对话框。", "Der Banker liest die Chain und kann für dich handeln — Senden, Tauschen, Deployen. Riskante Schritte halten immer zuerst an einem Freigabedialog."),
    (banker_thinking, "Counting cycles…", "연산 중…", "思考中…", "計算緊…", "Počítám…", "Calculando…", "思考中…", "Rechne…"),
    (banker_reading, "Reading the chain…", "체인을 읽는 중…", "チェーンを読み取り中…", "讀緊條鏈…", "Čtu řetězec…", "Leyendo la cadena…", "正在读取链…", "Lese die Chain…"),
    (banker_sending, "Sending transaction…", "트랜잭션 전송 중…", "トランザクション送信中…", "傳送緊交易…", "Odesílám transakci…", "Enviando transacción…", "正在发送交易…", "Sende Transaktion…"),
    (banker_confirming, "Waiting for on-chain confirmation…", "온체인 확인 대기 중…", "オンチェーン承認を待機中…", "等緊鏈上確認…", "Čekám na potvrzení on-chain…", "Esperando confirmación en cadena…", "等待链上确认…", "Warte auf On-Chain-Bestätigung…"),
    (banker_generating, "Painting a picture…", "그림 그리는 중…", "絵を描いています…", "畫緊圖…", "Maluji obrázek…", "Pintando una imagen…", "正在画图…", "Male ein Bild…"),
    (banker_out_of_steam, "I ran out of steam mid-task — try breaking it into smaller steps. 🤖", "작업 도중 한도에 도달했습니다 — 더 작은 단계로 나눠서 시도해 보세요. 🤖", "タスクの途中で上限に達しました — 小さなステップに分けてみてください。🤖", "做到一半冇晒力 — 試下拆細啲嚟做。🤖", "V půlce úkolu mi došla pára — zkuste jej rozdělit na menší kroky. 🤖", "Me quedé sin fuelle a mitad de la tarea: prueba a dividirla en pasos más pequeños. 🤖", "任务进行到一半达到上限 — 请拆成更小的步骤再试。🤖", "Mir ging mitten in der Aufgabe die Puste aus — teile sie in kleinere Schritte auf. 🤖"),
    (banker_approve_title, "Approve this transaction?", "이 트랜잭션을 승인할까요?", "このトランザクションを承認しますか？", "批准呢筆交易？", "Schválit tuto transakci?", "¿Aprobar esta transacción?", "批准这笔交易？", "Diese Transaktion freigeben?"),
    (banker_approve, "Approve", "승인", "承認", "批准", "Schválit", "Aprobar", "批准", "Freigeben"),
    (banker_clear, "Clear", "지우기", "消去", "清除", "Vymazat", "Borrar", "清除", "Leeren"),
    (banker_sug_balance, "What's my balance?", "내 잔액이 얼마야?", "残高はいくら？", "我有幾多錢？", "Jaký mám zůstatek?", "¿Cuál es mi saldo?", "我的余额是多少？", "Wie ist mein Kontostand?"),
    (banker_sug_gas, "What's the gas price right now?", "지금 가스 가격이 얼마야?", "今のガス価格は？", "而家gas幾錢？", "Jaká je teď cena gasu?", "¿Cuál es el precio del gas ahora?", "现在 Gas 价格是多少？", "Wie hoch ist der Gaspreis gerade?"),
    (banker_sug_deploy, "Deploy a token called Fruit Coin", "Fruit Coin이라는 토큰을 배포해 줘", "Fruit Coinというトークンをデプロイして", "幫我部署一隻叫 Fruit Coin 嘅代幣", "Nasaď token jménem Fruit Coin", "Despliega un token llamado Fruit Coin", "部署一个叫 Fruit Coin 的代币", "Stelle einen Token namens Fruit Coin bereit"),
    (banker_sug_swap, "Swap 1 CRO for VVS", "1 CRO를 VVS로 스왑해 줘", "1 CROをVVSにスワップして", "幫我用 1 CRO 換 VVS", "Směň 1 CRO za VVS", "Intercambia 1 CRO por VVS", "把 1 CRO 换成 VVS", "Tausche 1 CRO gegen VVS"),
    (banker_sug_image, "Draw a robot banker guarding a vault", "금고를 지키는 로봇 은행원을 그려 줘", "金庫を守るロボットバンカーを描いて", "畫個守住夾萬嘅機械人銀行家", "Nakresli robotického bankéře hlídajícího trezor", "Dibuja un banquero robot custodiando una cámara acorazada", "画一个守护金库的机器人银行家", "Zeichne einen Roboter-Banker, der einen Tresor bewacht"),
    (banker_placeholder, "Ask the banker…", "뱅커에게 물어보세요…", "バンカーに質問…", "問下銀行家…", "Zeptejte se bankéře…", "Pregunta al banquero…", "问问银行家…", "Frag den Banker…"),
    (banker_needs_key, "Add an AI provider key in the assistant's settings to chat with the banker.", "뱅커와 대화하려면 어시스턴트 설정에서 AI 제공자 키를 추가하세요.", "バンカーと話すには、アシスタントの設定でAIプロバイダーのキーを追加してください。", "要同銀行家傾偈，請喺助手設定入面加AI供應商嘅金鑰。", "Pro chat s bankéřem přidejte klíč poskytovatele AI v nastavení asistenta.", "Añade una clave de proveedor de IA en los ajustes del asistente para chatear con el banquero.", "要与银行家对话，请在助手设置中添加 AI 提供商密钥。", "Füge in den Assistenten-Einstellungen einen KI-Anbieterschlüssel hinzu, um mit dem Banker zu chatten."),
    (open_explorer, "Explorer", "익스플로러", "エクスプローラー", "區塊瀏覽器", "Průzkumník", "Explorador", "区块浏览器", "Explorer"),
    (ai_search, "AI Search", "AI 검색", "AI検索", "AI 搜尋", "AI hledání", "Búsqueda IA", "AI 搜索", "KI-Suche"),
    (quick_search_placeholder_ai, "Ask anything — answered from your rooms and knowledge", "무엇이든 물어보세요 — 채팅방과 지식에서 답을 찾아 드립니다", "何でも聞いてください — ルームとナレッジから答えます", "咩都問得 — 由你嘅聊天室同知識搵答案", "Zeptejte se na cokoli — odpovíme z vašich místností a znalostí", "Pregunta lo que sea: respondemos desde tus salas y conocimiento", "想问什么就问 — 从你的聊天室和知识中找答案", "Frag irgendetwas — beantwortet aus deinen Räumen und deinem Wissen"),
    (quick_search_placeholder, "Search all rooms and knowledge", "모든 채팅방과 지식 검색", "すべてのルームとナレッジを検索", "搜尋所有聊天室同知識", "Prohledat všechny místnosti a znalosti", "Buscar en todas las salas y el conocimiento", "搜索所有聊天室和知识", "Alle Räume und alles Wissen durchsuchen"),
    (ai_keys_hint, "Bring your own keys. They stay in this browser — never sent to this server — and power the assistant, the AI Banker, and AI Search.", "자신의 API 키를 사용하세요. 키는 이 브라우저에만 저장되고 이 서버로는 전송되지 않으며, 어시스턴트·AI 뱅커·AI 검색에 사용됩니다.", "自分のAPIキーを使います。キーはこのブラウザーにのみ保存され、このサーバーには送信されません。アシスタント、AIバンカー、AI検索で使われます。", "用你自己嘅API金鑰。金鑰只會存喺呢個瀏覽器，唔會傳去呢個伺服器，會用喺助手、AI銀行家同AI搜尋。", "Použijte vlastní klíče. Zůstávají jen v tomto prohlížeči — na tento server se nikdy neposílají — a pohánějí asistenta, AI bankéře i AI hledání.", "Usa tus propias claves. Se quedan solo en este navegador (nunca se envían a este servidor) y alimentan al asistente, el Banquero IA y la Búsqueda IA.", "使用你自己的密钥。它们只保存在此浏览器中 — 绝不会发送到此服务器 — 用于助手、AI 银行家和 AI 搜索。", "Bring deine eigenen Schlüssel mit. Sie bleiben in diesem Browser — werden nie an diesen Server gesendet — und treiben den Assistenten, den KI-Banker und die KI-Suche an."),
    (unlock_to_sign, "Unlock your wallet to sign transactions.", "트랜잭션에 서명하려면 지갑을 잠금 해제하세요.", "トランザクションに署名するにはウォレットをロック解除してください。", "要簽交易就解鎖你嘅錢包。", "Pro podepisování transakcí odemkněte peněženku.", "Desbloquea tu cartera para firmar transacciones.", "解锁钱包以签署交易。", "Entsperre deine Wallet, um Transaktionen zu signieren."),

    // --- Shout (the paid broadcast) -----------------------------------------
    (shout_title, "Shout", "외치기", "シャウト", "廣播", "Výkřik", "Grito", "呐喊", "Ruf"),
    (shout_hint, "Broadcast to everyone connected for one minute. Costs {price}, paid to the operator's wallet.", "접속 중인 모든 사용자에게 1분 동안 방송됩니다. 비용은 {price}이며 운영자 지갑으로 지불됩니다.", "接続中の全員に1分間放送されます。費用は{price}で、運営者のウォレットに支払われます。", "向所有連線用戶廣播一分鐘。費用係{price}，會支付畀營運者錢包。", "Vysílá se všem připojeným po dobu jedné minuty. Stojí {price}, platí se do peněženky provozovatele.", "Se emite a todos los conectados durante un minuto. Cuesta {price}, pagado a la cartera del operador.", "向所有在线用户广播一分钟。费用为{price}，支付到运营者钱包。", "Wird eine Minute lang an alle Verbundenen gesendet. Kostet {price}, gezahlt an die Wallet des Betreibers."),
    (shout_label, "Your message", "메시지", "メッセージ", "訊息", "Vaše zpráva", "Tu mensaje", "消息", "Deine Nachricht"),
    (shout_placeholder, "Say it to everyone…", "모두에게 외칠 말…", "みんなに伝える一言…", "想同大家講嘅嘢…", "Řekni to všem…", "Dilo a todos…", "想对所有人说的话…", "Sag es allen…"),
    (shout_pay, "Pay {price} & shout", "{price} 지불하고 외치기", "{price}を支払ってシャウト", "支付{price}並廣播", "Zaplatit {price} a vykřiknout", "Pagar {price} y gritar", "支付{price}并呐喊", "{price} zahlen & rufen"),
    (shout_retry, "Payment made — send the shout", "결제 완료 — 외침 보내기", "支払い済み — シャウトを送信", "已付款 — 發送廣播", "Zaplaceno — odeslat výkřik", "Pago hecho — enviar el grito", "已付款 — 发送呐喊", "Bezahlt — Ruf senden"),
    (shout_paid_note, "Your payment is confirmed. Submitting again will not pay twice.", "결제가 확인되었습니다. 다시 제출해도 이중 결제되지 않습니다.", "支払いは確認済みです。再送信しても二重払いにはなりません。", "已確認付款。再提交唔會重複收費。", "Platba je potvrzena. Opětovné odeslání nezaplatí dvakrát.", "Tu pago está confirmado. Reenviar no cobrará dos veces.", "付款已确认。再次提交不会重复付费。", "Deine Zahlung ist bestätigt. Erneutes Senden zahlt nicht doppelt."),
    (shout_sent, "Your shout is live!", "외침이 방송 중입니다!", "シャウトが放送中です！", "你嘅廣播上線喇！", "Tvůj výkřik je živě!", "¡Tu grito está en el aire!", "你的呐喊正在播出！", "Dein Ruf ist live!"),
    (shout_dismiss, "Dismiss shout", "외침 닫기", "シャウトを閉じる", "關閉廣播", "Zavřít výkřik", "Cerrar el grito", "关闭呐喊", "Ruf schließen"),
    (shout_text_invalid, "Enter a message of at most 200 characters.", "200자 이하의 메시지를 입력하세요.", "200文字以内のメッセージを入力してください。", "請輸入最多200字嘅訊息。", "Zadej zprávu o délce nejvýše 200 znaků.", "Escribe un mensaje de 200 caracteres como máximo.", "请输入不超过200个字符的消息。", "Gib eine Nachricht mit höchstens 200 Zeichen ein."),
    (shout_no_network, "Sign in with an unlocked wallet to shout.", "외치려면 잠금 해제된 지갑으로 로그인하세요.", "シャウトするにはロック解除されたウォレットでサインインしてください。", "要廣播請用已解鎖嘅錢包登入。", "Pro výkřik se přihlas s odemčenou peněženkou.", "Inicia sesión con una cartera desbloqueada para gritar.", "要呐喊请使用已解锁的钱包登录。", "Melde dich mit einer entsperrten Wallet an, um zu rufen."),
    (shout_no_operator_wallet, "This server has no payment wallet configured.", "이 서버에 결제 지갑이 설정되어 있지 않습니다.", "このサーバーには支払い用ウォレットが設定されていません。", "呢個伺服器未設定收款錢包。", "Tento server nemá nastavenou platební peněženku.", "Este servidor no tiene una cartera de pagos configurada.", "此服务器未配置收款钱包。", "Dieser Server hat keine Zahlungs-Wallet konfiguriert."),

    // --- Web publishing (paid hosting) --------------------------------------
    (nav_publish, "Publish", "게시", "公開", "發佈", "Publikovat", "Publicar", "发布", "Publizieren"),
    (publish_hint, "Host a page on this server for {price}, paid to the operator's wallet. Upload HTML or a zip with index.html — anyone signed in can remove it.", "이 서버에 페이지를 호스팅하려면 {price}를 운영자 지갑으로 지불하세요. HTML 또는 index.html이 포함된 zip을 올릴 수 있으며, 로그인한 누구나 삭제할 수 있습니다.", "{price}を運営者のウォレットに支払うと、このサーバーがページをホスティングします。HTMLまたはindex.html入りのzipをアップロードできます。サインイン中の誰でも削除できます。", "支付{price}畀營運者錢包，呢個伺服器就會幫你寄存網頁。可以上載HTML或者有index.html嘅zip，任何登入用戶都可以移除。", "Hostujte stránku na tomto serveru za {price}, placeno do peněženky provozovatele. Nahrajte HTML nebo zip s index.html — kdokoli přihlášený ji může odstranit.", "Aloja una página en este servidor por {price}, pagado a la cartera del operador. Sube HTML o un zip con index.html; cualquier usuario conectado puede eliminarla.", "支付{price}到运营者钱包，即可在此服务器上托管页面。可上传HTML或包含index.html的zip，任何登录用户都可以移除。", "Hoste eine Seite auf diesem Server für {price}, gezahlt an die Wallet des Betreibers. Lade HTML oder ein Zip mit index.html hoch — jeder Angemeldete kann sie entfernen."),
    (publish_your_page, "Publish a page", "페이지 게시", "ページを公開", "發佈網頁", "Publikovat stránku", "Publicar una página", "发布页面", "Eine Seite publizieren"),
    (publish_form_title, "Title", "제목", "タイトル", "標題", "Název", "Título", "标题", "Titel"),
    (publish_title_placeholder, "What is this page called?", "이 페이지의 이름은 무엇인가요?", "このページの名前は？", "呢頁叫咩名？", "Jak se tato stránka jmenuje?", "¿Cómo se llama esta página?", "这个页面叫什么？", "Wie heißt diese Seite?"),
    (publish_mode_paste, "Paste HTML", "HTML 붙여넣기", "HTMLを貼り付け", "貼上HTML", "Vložit HTML", "Pegar HTML", "粘贴HTML", "HTML einfügen"),
    (publish_mode_upload, "Upload file", "파일 업로드", "ファイルをアップロード", "上載檔案", "Nahrát soubor", "Subir archivo", "上传文件", "Datei hochladen"),
    (publish_paste_placeholder, "<html>…paste your page here…</html>", "<html>…여기에 페이지를 붙여넣으세요…</html>", "<html>…ここにページを貼り付け…</html>", "<html>…喺呢度貼上你嘅網頁…</html>", "<html>…sem vložte svou stránku…</html>", "<html>…pega tu página aquí…</html>", "<html>…在此粘贴你的页面…</html>", "<html>…füge deine Seite hier ein…</html>"),
    (publish_pick_file, "Choose an HTML or zip file", "HTML 또는 zip 파일 선택", "HTMLまたはzipファイルを選択", "揀一個HTML或zip檔案", "Vyberte soubor HTML nebo zip", "Elige un archivo HTML o zip", "选择HTML或zip文件", "HTML- oder Zip-Datei wählen"),
    (publish_picked, "Selected: {name} ({size})", "선택됨: {name} ({size})", "選択済み: {name}（{size}）", "已揀: {name}（{size}）", "Vybráno: {name} ({size})", "Seleccionado: {name} ({size})", "已选择：{name}（{size}）", "Ausgewählt: {name} ({size})"),
    (publish_pay, "Pay {price} & publish", "{price} 지불하고 게시", "{price}を支払って公開", "支付{price}並發佈", "Zaplatit {price} a publikovat", "Pagar {price} y publicar", "支付{price}并发布", "{price} zahlen & publizieren"),
    (publish_retry, "Payment made — publish", "결제 완료 — 게시", "支払い済み — 公開", "已付款 — 發佈", "Zaplaceno — publikovat", "Pago hecho — publicar", "已付款 — 发布", "Bezahlt — publizieren"),
    (publish_sent, "Your page is live!", "페이지가 게시되었습니다!", "ページが公開されました！", "你嘅網頁上線喇！", "Tvá stránka je živě!", "¡Tu página está en línea!", "你的页面已上线！", "Deine Seite ist live!"),
    (publish_open, "Open", "열기", "開く", "打開", "Otevřít", "Abrir", "打开", "Öffnen"),
    (publish_remove, "Remove", "삭제", "削除", "移除", "Odstranit", "Eliminar", "移除", "Entfernen"),
    (publish_remove_arm, "Remove for everyone?", "모두에게서 삭제할까요?", "全員から削除しますか？", "要幫所有人移除？", "Odstranit pro všechny?", "¿Eliminar para todos?", "为所有人移除？", "Für alle entfernen?"),
    (publish_removed, "Site removed", "사이트가 삭제되었습니다", "サイトを削除しました", "網站已移除", "Web odstraněn", "Sitio eliminado", "网站已移除", "Website entfernt"),
    (publish_empty, "Nothing published yet", "아직 게시된 것이 없습니다", "まだ何も公開されていません", "仲未有嘢發佈", "Zatím nic nepublikováno", "Aún no hay nada publicado", "还没有发布任何内容", "Noch nichts publiziert"),
    (publish_empty_desc, "Pay {price} and this server hosts your page for everyone on it.", "{price}를 지불하면 이 서버가 모두를 위해 페이지를 호스팅합니다.", "{price}を支払うと、このサーバーがみんなのためにページをホスティングします。", "支付{price}，呢個伺服器就會幫你寄存網頁畀大家睇。", "Zaplaťte {price} a tento server bude hostovat vaši stránku pro všechny.", "Paga {price} y este servidor alojará tu página para todos.", "支付{price}，此服务器就会为所有人托管你的页面。", "Zahle {price} und dieser Server hostet deine Seite für alle."),
    (publish_filter, "Filter sites…", "사이트 필터…", "サイトを絞り込む…", "篩選網站…", "Filtrovat weby…", "Filtrar sitios…", "筛选网站…", "Websites filtern…"),
    (publish_need_content, "Paste HTML or choose a file first.", "먼저 HTML을 붙여넣거나 파일을 선택하세요.", "先にHTMLを貼り付けるかファイルを選択してください。", "請先貼上HTML或者揀檔案。", "Nejprve vložte HTML nebo vyberte soubor.", "Primero pega HTML o elige un archivo.", "请先粘贴HTML或选择文件。", "Füge zuerst HTML ein oder wähle eine Datei."),
    (publish_title_invalid, "Enter a title of at most 100 characters.", "100자 이하의 제목을 입력하세요.", "100文字以内のタイトルを入力してください。", "請輸入最多100字嘅標題。", "Zadejte název o délce nejvýše 100 znaků.", "Escribe un título de 100 caracteres como máximo.", "请输入不超过100个字符的标题。", "Gib einen Titel mit höchstens 100 Zeichen ein."),
    (publish_meta, "{files} files · {size}", "파일 {files}개 · {size}", "{files}ファイル · {size}", "{files}個檔案 · {size}", "{files} souborů · {size}", "{files} archivos · {size}", "{files}个文件 · {size}", "{files} Dateien · {size}"),
    (publish_copy_url, "Copy URL", "URL 복사", "URLをコピー", "複製網址", "Kopírovat URL", "Copiar URL", "复制网址", "URL kopieren"),
    (publish_url_copied, "URL copied", "URL이 복사되었습니다", "URLをコピーしました", "已複製網址", "URL zkopírována", "URL copiada", "已复制网址", "URL kopiert"),
    (publish_copy_failed, "Copy is unavailable here — select the URL on the card.", "여기서는 복사를 사용할 수 없습니다 — 카드의 URL을 직접 선택하세요.", "ここではコピーを使えません — カードのURLを選択してください。", "呢度用唔到複製 — 請自己揀選卡上面嘅網址。", "Kopírování zde není dostupné — označte URL na kartě.", "Copiar no está disponible aquí: selecciona la URL en la tarjeta.", "此处无法复制 — 请手动选中卡片上的网址。", "Kopieren ist hier nicht verfügbar — markiere die URL auf der Karte."),
}

/// The fast-room descriptions, by index. A set whose *size* is part of the
/// design cannot be a single key, and callers must not index the table by
/// hand — the modulo belongs here, with the list it protects.
pub fn room_description(lang: Lang, pick: u8) -> &'static str {
    const KEYS: [Key; 4] = [
        Key::room_desc_0,
        Key::room_desc_1,
        Key::room_desc_2,
        Key::room_desc_3,
    ];
    t(lang, KEYS[pick as usize % KEYS.len()])
}

/// The first-message greetings, by index. Same reasoning as
/// [`room_description`].
pub fn greeting(lang: Lang, pick: u8) -> &'static str {
    const KEYS: [Key; 6] = [
        Key::greeting_0,
        Key::greeting_1,
        Key::greeting_2,
        Key::greeting_3,
        Key::greeting_4,
        Key::greeting_5,
    ];
    t(lang, KEYS[pick as usize % KEYS.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_translated_into_every_language() {
        // The macro guarantees a translation *exists*; this catches the other
        // failure, which is one left as an empty string to make it compile.
        for key in Key::ALL {
            for lang in Lang::ALL {
                let value = t(lang, *key);
                assert!(
                    !value.trim().is_empty(),
                    "{key:?} is empty in {}",
                    lang.tag()
                );
            }
        }
    }

    #[test]
    fn nothing_is_left_in_english_by_accident() {
        // A translation identical to the English is usually a placeholder that
        // was never filled in. Some genuinely are the same word — proper nouns
        // and loanwords — so this lists the allowed ones rather than banning
        // the case outright; an unlisted collision is a missing translation.
        let shared_by_design: &[(Lang, Key)] = &[
            (Lang::Cs, Key::nav_chat),
            (Lang::Es, Key::nav_chat),
            (Lang::Es, Key::admin),
            (Lang::Ja, Key::today),
            (Lang::Yue, Key::today),
            // "Offline" is the ordinary Czech word for it — a loanword, not a
            // gap.
            (Lang::Cs, Key::offline),
            (Lang::Cs, Key::ai_assistant),
            // "Gas" is the term of art on every EVM chain and is not
            // translated in Spanish or Czech wallet interfaces.
            (Lang::Es, Key::gas),
            (Lang::Cs, Key::testnet_badge),
            // "Tokens" is the Spanish plural too; "Greeter" is the
            // contract's proper name in every language; Czech spells
            // "Symbol" as English does.
            (Lang::Es, Key::bank_tokens),
            (Lang::Yue, Key::bank_greeter),
            (Lang::Cs, Key::bank_greeter),
            (Lang::Es, Key::bank_greeter),
            (Lang::Cs, Key::token_symbol),
            // "Portfolio" is the Czech word too; "Mainnet"/"Testnet" are
            // untranslated terms of art in Czech and Spanish alike.
            (Lang::Cs, Key::bank_portfolio),
            (Lang::Cs, Key::bank_mainnet),
            (Lang::Es, Key::bank_mainnet),
            (Lang::Cs, Key::bank_testnet),
            (Lang::Es, Key::bank_testnet),
            // Spanish spells these exactly as English does.
            (Lang::Es, Key::login_vertical),
            (Lang::Es, Key::login_horizontal),
            // German is rich in anglicisms that ARE the correct UI German:
            // Chat, Wallet, Layout, Admin, Live, System, Events, Polling,
            // Offline, Asset, Gas, Bank, Portfolio, Mainnet, Testnet,
            // Explorer, Name, Symbol, Slippage — plus Latin "auto" and
            // "Horizontal", spelled identically.
            (Lang::De, Key::nav_chat),
            (Lang::De, Key::wallet),
            (Lang::De, Key::layout),
            (Lang::De, Key::admin),
            (Lang::De, Key::live),
            (Lang::De, Key::theme_system),
            (Lang::De, Key::conn_live),
            (Lang::De, Key::conn_events),
            (Lang::De, Key::conn_polling),
            (Lang::De, Key::offline),
            (Lang::De, Key::asset),
            (Lang::De, Key::gas),
            (Lang::De, Key::login_horizontal),
            (Lang::De, Key::gas_auto),
            (Lang::De, Key::menu_bank),
            (Lang::De, Key::bank_greeter),
            (Lang::De, Key::slippage_pct),
            (Lang::De, Key::token_name),
            (Lang::De, Key::token_symbol),
            (Lang::De, Key::bank_portfolio),
            (Lang::De, Key::bank_mainnet),
            (Lang::De, Key::bank_testnet),
            (Lang::De, Key::open_explorer),
            // "Gas" and the Greeter's proper name are the same in Simplified
            // Chinese.
            (Lang::Zh, Key::gas),
            (Lang::Zh, Key::bank_greeter),
            // "Skynet" is a proper noun everywhere it isn't transliterated,
            // and "Mono"/"Serif"/"System"/"Standard" are the correct UI words
            // in these languages.
            (Lang::Yue, Key::font_skynet),
            (Lang::Cs, Key::font_skynet),
            (Lang::Es, Key::font_skynet),
            (Lang::Zh, Key::font_skynet),
            (Lang::De, Key::font_skynet),
            // The skin is named after the product, so the base skin's label is
            // the product's own name everywhere the script allows it. German
            // borrows "Skin" itself — it is the word used for exactly this in
            // German software.
            (Lang::Yue, Key::skin_skynet),
            (Lang::Cs, Key::skin_skynet),
            (Lang::Es, Key::skin_skynet),
            (Lang::Zh, Key::skin_skynet),
            (Lang::De, Key::skin_skynet),
            (Lang::De, Key::skin),
            (Lang::Cs, Key::font_mono),
            (Lang::Es, Key::font_mono),
            (Lang::De, Key::font_mono),
            (Lang::Es, Key::font_serif),
            (Lang::De, Key::font_system),
            (Lang::De, Key::size_standard),
            // German borrows both words wholesale: "Operator" is the ordinary
            // German noun, and "Dossier" is French by way of German — neither
            // has a native alternative a German reader would prefer.
            (Lang::De, Key::nav_operator),
            (Lang::De, Key::op_dossier),
            // "Video" is the ordinary Czech and German noun as well; the
            // Spanish "Vídeo" carries an accent, which is why only these two
            // collide with the English.
            (Lang::Cs, Key::video_alt),
            (Lang::De, Key::video_alt),
            (Lang::Cs, Key::tab_video),
            (Lang::De, Key::tab_video),
            // German took "Thread" wholesale for the chat sense — every German
            // messenger uses it, and "Diskussionsstrang" is what a dictionary
            // says rather than what anyone types.
            (Lang::De, Key::thread),
            // Likewise "Admin", which is the ordinary German short form.
            (Lang::De, Key::admin_is_admin),
        ];
        for key in Key::ALL {
            for lang in Lang::ALL {
                if lang == Lang::En || shared_by_design.contains(&(lang, *key)) {
                    continue;
                }
                assert_ne!(
                    t(lang, *key),
                    t(Lang::En, *key),
                    "{key:?} in {} is still the English string",
                    lang.tag()
                );
            }
        }
    }

    #[test]
    fn fast_room_text_never_contains_markup_the_server_rejects() {
        // `validate::is_forbidden_markup` on the server refuses this set in a
        // room name or description. An apostrophe in one of these once turned
        // the one-click button into "Validation failed"; a translator reaching
        // for a quotation mark would do it again, in a language the author of
        // this test may not read.
        const FORBIDDEN: [char; 9] = ['<', '>', '{', '}', ';', '"', '\'', '`', '\\'];
        for lang in Lang::ALL {
            for pick in 0u8..=255 {
                let desc = room_description(lang, pick);
                assert!(
                    !desc.contains(FORBIDDEN),
                    "room description in {} contains markup the server rejects: {desc}",
                    lang.tag()
                );
                assert!(
                    desc.chars().count() <= 500,
                    "description too long in {}",
                    lang.tag()
                );
            }
        }
    }

    #[test]
    fn every_description_actually_says_the_room_is_encrypted() {
        // The one thing a fast-room description exists to communicate. A
        // translation that reads nicely but drops the word leaves the user with
        // a room whose whole promise went unstated — so each language pins the
        // stem it uses, and a rewrite that loses it fails here rather than
        // shipping.
        const STEM: [(Lang, &str); 6] = [
            (Lang::En, "ncrypt"),
            (Lang::Ko, "암호화"),
            (Lang::Ja, "暗号化"),
            (Lang::Yue, "加密"),
            (Lang::Cs, "ifrov"),
            (Lang::Es, "ifrad"),
        ];
        for (lang, stem) in STEM {
            for pick in 0u8..4 {
                let d = room_description(lang, pick);
                assert!(
                    d.contains(stem),
                    "a fast room is always encrypted and the {} description has to say so: {d}",
                    lang.tag()
                );
            }
        }
    }

    #[test]
    fn every_greeting_and_description_index_is_reachable() {
        use std::collections::HashSet;
        for lang in Lang::ALL {
            let descs: HashSet<_> = (0u8..=255).map(|p| room_description(lang, p)).collect();
            assert_eq!(descs.len(), 4, "unreachable description in {}", lang.tag());
            let greets: HashSet<_> = (0u8..=255).map(|p| greeting(lang, p)).collect();
            assert_eq!(greets.len(), 6, "unreachable greeting in {}", lang.tag());
        }
    }

    #[test]
    fn a_placeholder_survives_every_translation() {
        // A translation that drops `{name}` silently loses the very thing the
        // sentence was about; one that misspells it prints the braces to the
        // user. Both are invisible until someone reports a toast reading
        // "Joined {nome}".
        let with_name = [
            Key::signed_in_as,
            Key::room_created_named,
            Key::joined_room,
            Key::invite_sent,
        ];
        let with_short = [Key::blocked_someone, Key::unblocked_someone];
        for lang in Lang::ALL {
            for key in with_name {
                assert!(
                    t(lang, key).contains("{name}"),
                    "{key:?} lost its {{name}} placeholder in {}",
                    lang.tag()
                );
            }
            for key in with_short {
                assert!(
                    t(lang, key).contains("{short}"),
                    "{key:?} lost its {{short}} placeholder in {}",
                    lang.tag()
                );
            }
        }
    }

    #[test]
    fn a_regional_tag_finds_its_language() {
        assert_eq!(Lang::parse("ko-KR"), Some(Lang::Ko));
        assert_eq!(Lang::parse("es-419"), Some(Lang::Es));
        assert_eq!(Lang::parse("CS"), Some(Lang::Cs));
        assert_eq!(Lang::parse("en_GB"), Some(Lang::En));
        // Hong Kong / traditional-script Chinese is Cantonese…
        assert_eq!(Lang::parse("zh-HK"), Some(Lang::Yue));
        assert_eq!(Lang::parse("zh-Hant-HK"), Some(Lang::Yue));
        assert_eq!(Lang::parse("zh-Hant"), Some(Lang::Yue));
        // …and every other Chinese tag now lands in Simplified Chinese
        // rather than falling back to English.
        assert_eq!(Lang::parse("zh"), Some(Lang::Zh));
        assert_eq!(Lang::parse("zh-CN"), Some(Lang::Zh));
        assert_eq!(Lang::parse("zh-Hans"), Some(Lang::Zh));
        assert_eq!(Lang::parse("de-AT"), Some(Lang::De));
        assert_eq!(Lang::parse("de"), Some(Lang::De));
    }

    #[test]
    fn every_language_round_trips_through_its_tag() {
        for lang in Lang::ALL {
            assert_eq!(Lang::parse(lang.tag()), Some(lang));
            assert!(!lang.endonym().trim().is_empty());
        }
    }

    #[test]
    fn the_picker_lists_each_language_once() {
        let mut tags: Vec<&str> = Lang::ALL.iter().map(|l| l.tag()).collect();
        tags.sort_unstable();
        let before = tags.len();
        tags.dedup();
        assert_eq!(before, tags.len(), "a language is listed twice");
    }
}
