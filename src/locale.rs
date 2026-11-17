use std::sync::OnceLock;

use handlebars::Handlebars;
use rocket::http::Status;

use crate::{
    CONFIG,
    api::ApiResult,
    db::{DbPool, models::User},
};

const EMAIL_TEMPLATE_PREFIX: &str = "email/";

const CODE_EN: &str = "en";
const CODE_RU: &str = "ru";
const SUBTAG_SEPARATORS: [char; 2] = ['-', '_'];
const QUALITY_PARAM_NAME: &str = "q";
const QUALITY_DEFAULT: f32 = 1.0;
const QUALITY_MIN: f32 = 0.0;

/// The languages user-facing emails can be sent in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    En,
    Ru,
}

impl Locale {
    pub const SUPPORTED_CODES: [&'static str; 2] = [CODE_EN, CODE_RU];

    pub fn code(self) -> &'static str {
        match self {
            Locale::En => CODE_EN,
            Locale::Ru => CODE_RU,
        }
    }

    /// Boundary parser: only the primary subtag counts, case-insensitive ("ru", "RU", "ru-RU" and "ru_RU" are all Ru).
    pub fn from_code(code: &str) -> Option<Locale> {
        let primary = code.trim().split(SUBTAG_SEPARATORS).next().unwrap_or_default();
        match primary.to_ascii_lowercase().as_str() {
            CODE_EN => Some(Locale::En),
            CODE_RU => Some(Locale::Ru),
            _ => None,
        }
    }

    /// Boundary parser for an RFC 9110 `Accept-Language` header: the supported language with the highest q-value,
    /// the first one in the header on a tie. Unsupported languages, `*`, `q=0` and malformed entries are skipped.
    pub fn from_accept_language(header: &str) -> Option<Locale> {
        let mut best: Option<(f32, Locale)> = None;
        for entry in header.split(',') {
            let Some((tag, quality)) = parse_language_range(entry) else {
                continue;
            };
            let Some(locale) = Locale::from_code(tag) else {
                continue;
            };
            if quality > QUALITY_MIN && best.is_none_or(|(best_quality, _)| quality > best_quality) {
                best = Some((quality, locale));
            }
        }
        best.map(|(_, locale)| locale)
    }

    /// Boundary parser for the `locale` of an invite request: absent means the server default, a language that is
    /// not supported is rejected with a 400.
    pub fn from_invite(code: Option<&str>) -> ApiResult<Locale> {
        let Some(code) = code else {
            return Ok(Locale::server_default());
        };
        let Some(locale) = Locale::from_code(code) else {
            err_code!(
                format!(
                    "Unsupported locale \"{}\", supported locales: {}",
                    code.escape_debug(),
                    Locale::SUPPORTED_CODES.join(", ")
                ),
                Status::BadRequest.code
            )
        };
        Ok(locale)
    }

    /// `EMAIL_DEFAULT_LOCALE`, which config validation guarantees to be supported.
    pub fn server_default() -> Locale {
        Locale::from_code(&CONFIG.email_default_locale()).unwrap_or(Locale::En)
    }
}

static DB_POOL: OnceLock<DbPool> = OnceLock::new();

/// Gives the email code a way to look recipients up by address; the `send_*` functions have no database connection.
pub fn init_db_pool(pool: DbPool) {
    if DB_POOL.set(pool).is_err() {
        warn!("Locale database pool is already initialized");
    }
}

/// The locale of the user with this address; the server default when there is no such user or no database.
pub async fn locale_for_address(address: &str) -> Locale {
    let Some(pool) = DB_POOL.get() else {
        return Locale::server_default();
    };
    match pool.get().await {
        Ok(conn) => User::find_by_mail(address, &conn).await.map_or_else(Locale::server_default, |u| u.locale()),
        Err(e) => {
            warn!("Cannot look up the locale of the recipient, using the server default: {e:?}");
            Locale::server_default()
        }
    }
}

/// Name of the template to try first for `locale`: `email/ru/welcome` for `email/welcome`, English is unchanged.
pub fn localized_template_name(locale: Locale, template_name: &str) -> String {
    match (locale, template_name.strip_prefix(EMAIL_TEMPLATE_PREFIX)) {
        (Locale::Ru, Some(name)) => format!("{EMAIL_TEMPLATE_PREFIX}{}/{name}", Locale::Ru.code()),
        _ => template_name.to_owned(),
    }
}

macro_rules! ru_email_templates {
    ($($name:literal),+ $(,)?) => {
        const RU_EMAIL_TEMPLATES: &[(&str, &str)] = &[$(
            (concat!("email/ru/", $name), include_str!(concat!("static/templates/email/ru/", $name, ".hbs"))),
            (
                concat!("email/ru/", $name, ".html"),
                include_str!(concat!("static/templates/email/ru/", $name, ".html.hbs")),
            ),
        )+];
    };
}

ru_email_templates!(
    "admin_account_recovery",
    "change_email",
    "change_email_existing",
    "change_email_invited",
    "delete_account",
    "emergency_access_invite_accepted",
    "emergency_access_invite_confirmed",
    "emergency_access_recovery_approved",
    "emergency_access_recovery_initiated",
    "emergency_access_recovery_rejected",
    "emergency_access_recovery_reminder",
    "emergency_access_recovery_timed_out",
    "incomplete_2fa_login",
    "invite_accepted",
    "invite_confirmed",
    "new_device_logged_in",
    "protected_action",
    "pw_hint_none",
    "pw_hint_some",
    "recover_twofactor",
    "register_verify_email",
    "send_2fa_removed_from_org",
    "send_emergency_access_invite",
    "send_org_invite",
    "send_single_org_removed_from_org",
    "smtp_test",
    "sso_change_email",
    "twofactor_email",
    "verify_email",
    "welcome",
    "welcome_must_verify",
);

/// Registers the embedded Russian emails. Call it before loading the user templates folder, so that its files override them.
pub fn register_email_templates(hb: &mut Handlebars<'_>) {
    for (name, template) in RU_EMAIL_TEMPLATES {
        hb.register_template_string(name, template).expect("embedded Russian email template must compile");
    }
}

/// Splits `ru;q=0.8` into the language tag and its quality; `None` for a malformed quality.
fn parse_language_range(entry: &str) -> Option<(&str, f32)> {
    let Some((tag, params)) = entry.split_once(';') else {
        return Some((entry.trim(), QUALITY_DEFAULT));
    };
    let (name, value) = params.split_once('=')?;
    if !name.trim().eq_ignore_ascii_case(QUALITY_PARAM_NAME) {
        return None;
    }
    let quality = value.trim().parse::<f32>().ok()?;
    (QUALITY_MIN..=QUALITY_DEFAULT).contains(&quality).then_some((tag.trim(), quality))
}

/// CONFIG reads the environment and the filesystem. A test that needs it calls this first and returns when it is
/// `false`: it then ran itself in a child process with an isolated config, plus `env`, and asserted that it passed.
#[cfg(test)]
pub fn in_isolated_config(env: &[(&str, &str)]) -> bool {
    const ISOLATION_MARKER: &str = "VAULTWARDEN_ISOLATED_CONFIG_TEST";

    if std::env::var_os(ISOLATION_MARKER).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", std::thread::current().name().unwrap(), "--nocapture"])
        .env(ISOLATION_MARKER, "1")
        .env("DATABASE_URL", "sqlite://:memory:")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_round_trips_through_from_code() {
        for locale in [Locale::En, Locale::Ru] {
            assert_eq!(Locale::from_code(locale.code()), Some(locale), "code {:?} of {locale:?}", locale.code());
        }
        assert_eq!(Locale::En.code(), "en");
        assert_eq!(Locale::Ru.code(), "ru");
    }

    #[test]
    fn test_supported_codes_all_parse() {
        for code in Locale::SUPPORTED_CODES {
            assert!(Locale::from_code(code).is_some(), "supported code {code:?} does not parse");
        }
    }

    #[test]
    fn test_from_code() {
        let cases = [
            ("ru", Some(Locale::Ru)),
            ("RU", Some(Locale::Ru)),
            ("ru-RU", Some(Locale::Ru)),
            ("ru_RU", Some(Locale::Ru)),
            (" ru ", Some(Locale::Ru)),
            ("en", Some(Locale::En)),
            ("en-GB", Some(Locale::En)),
            ("EN_us", Some(Locale::En)),
            ("de", None),
            ("", None),
            ("-ru", None),
            ("*", None),
            ("russian", None),
            ("garbage!!", None),
        ];
        for (input, expected) in cases {
            assert_eq!(Locale::from_code(input), expected, "from_code({input:?})");
        }
    }

    #[test]
    fn test_from_accept_language() {
        let cases = [
            ("de-DE,ru;q=0.8,en;q=0.5", Some(Locale::Ru)),
            ("en;q=0.5,ru;q=0.8", Some(Locale::Ru)),
            ("ru,en;q=0.9", Some(Locale::Ru)),
            ("en-US,en;q=0.9,ru;q=0.8", Some(Locale::En)),
            ("ru-RU", Some(Locale::Ru)),
            ("RU;Q=0.3", Some(Locale::Ru)),
            ("ru;q=0.3", Some(Locale::Ru)),
            // Whitespace around everything
            ("  de , ru ; q=0.8 ,en;q=0.5", Some(Locale::Ru)),
            // Ties go to the first in header order
            ("ru,en", Some(Locale::Ru)),
            ("en,ru", Some(Locale::En)),
            ("en;q=0.5,ru;q=0.5", Some(Locale::En)),
            ("ru;q=0.5,en;q=0.5", Some(Locale::Ru)),
            // q=0 means "not acceptable"
            ("ru;q=0,en;q=0.1", Some(Locale::En)),
            ("ru;q=0", None),
            ("ru;q=0.000,en;q=0", None),
            // Wildcard and unsupported languages never match
            ("*", None),
            ("*;q=0.5,ru;q=0.1", Some(Locale::Ru)),
            ("de,fr;q=0.9,*;q=0.1", None),
            // Empty and degenerate headers
            ("", None),
            (",", None),
            (" , ,, ", None),
            // Malformed entries are skipped, valid ones still count
            ("ru;q=abc,en;q=0.5", Some(Locale::En)),
            ("ru;q=,en;q=0.5", Some(Locale::En)),
            ("ru;q=1.5,en;q=0.5", Some(Locale::En)),
            ("ru;q=-0.5,en;q=0.5", Some(Locale::En)),
            ("ru;q=NaN,en;q=0.5", Some(Locale::En)),
            ("ru;level=1,en;q=0.5", Some(Locale::En)),
            ("ru;q=abc", None),
        ];
        for (header, expected) in cases {
            assert_eq!(Locale::from_accept_language(header), expected, "from_accept_language({header:?})");
        }
    }

    #[test]
    fn test_parse_language_range() {
        let cases = [
            ("ru", Some(("ru", 1.0))),
            (" ru-RU ", Some(("ru-RU", 1.0))),
            ("ru;q=0.8", Some(("ru", 0.8))),
            (" ru ; q=0.8 ", Some(("ru", 0.8))),
            ("ru;Q=0.5", Some(("ru", 0.5))),
            ("ru;q=0", Some(("ru", 0.0))),
            ("ru;q=1", Some(("ru", 1.0))),
            ("*;q=0.1", Some(("*", 0.1))),
            ("ru;q=1.01", None),
            ("ru;q=-1", None),
            ("ru;q=x", None),
            ("ru;charset=utf-8", None),
        ];
        for (entry, expected) in cases {
            assert_eq!(parse_language_range(entry), expected, "parse_language_range({entry:?})");
        }
    }

    #[test]
    fn test_server_default_follows_config() {
        if !in_isolated_config(&[("EMAIL_DEFAULT_LOCALE", "ru-RU")]) {
            return;
        }
        assert_eq!(Locale::server_default(), Locale::Ru, "EMAIL_DEFAULT_LOCALE=ru-RU");
    }

    #[test]
    fn test_server_default_is_english_without_config() {
        if !in_isolated_config(&[]) {
            return;
        }
        assert_eq!(Locale::server_default(), Locale::En, "EMAIL_DEFAULT_LOCALE unset");
    }

    #[test]
    fn test_from_invite() {
        if !in_isolated_config(&[("EMAIL_DEFAULT_LOCALE", "ru")]) {
            return;
        }
        let accepted = [
            (None, Locale::Ru),
            (Some("en"), Locale::En),
            (Some("ru"), Locale::Ru),
            (Some("ru-RU"), Locale::Ru),
            (Some("EN_us"), Locale::En),
        ];
        for (code, expected) in accepted {
            let locale = Locale::from_invite(code).unwrap_or_else(|e| panic!("from_invite({code:?}) failed: {e}"));
            assert_eq!(locale, expected, "from_invite({code:?}), EMAIL_DEFAULT_LOCALE=ru");
        }

        for code in ["de", "", "russian", "*", "x\ny"] {
            let Err(error) = Locale::from_invite(Some(code)) else {
                panic!("from_invite(Some({code:?})) was accepted");
            };
            let message = error.message();
            assert!(message.starts_with("Unsupported locale"), "from_invite(Some({code:?})) message: {message:?}");
            assert!(message.contains("en, ru"), "message for {code:?} must list the supported codes: {message:?}");
            assert!(!message.contains('\n'), "message for {code:?} must escape the input: {message:?}");
        }
    }

    #[test]
    fn test_localized_template_name() {
        let cases = [
            (Locale::En, "email/welcome", "email/welcome"),
            (Locale::En, "email/welcome.html", "email/welcome.html"),
            (Locale::Ru, "email/welcome", "email/ru/welcome"),
            (Locale::Ru, "email/welcome.html", "email/ru/welcome.html"),
            (Locale::Ru, "admin/base", "admin/base"),
            (Locale::Ru, "404", "404"),
        ];
        for (locale, template, expected) in cases {
            assert_eq!(
                localized_template_name(locale, template),
                expected,
                "localized_template_name({locale:?}, {template:?})"
            );
        }
    }

    #[test]
    fn test_embedded_russian_templates_match_the_files() {
        let mut embedded: Vec<String> =
            RU_EMAIL_TEMPLATES.iter().map(|(name, _)| name.strip_prefix("email/ru/").unwrap().to_owned()).collect();
        let mut on_disk: Vec<String> =
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src/static/templates/email/ru"))
                .unwrap()
                .filter_map(|entry| {
                    entry.unwrap().file_name().into_string().unwrap().strip_suffix(".hbs").map(str::to_owned)
                })
                .collect();
        embedded.sort();
        on_disk.sort();
        assert_eq!(embedded, on_disk, "templates registered in ru_email_templates! vs .hbs files in email/ru");
    }

    #[test]
    fn test_register_email_templates() {
        let mut hb = Handlebars::new();
        register_email_templates(&mut hb);
        for (name, _) in RU_EMAIL_TEMPLATES {
            assert!(hb.has_template(name), "template {name:?} is not registered");
        }
        assert_eq!(
            hb.get_templates().len(),
            RU_EMAIL_TEMPLATES.len(),
            "registered templates: {:?}",
            hb.get_templates().keys()
        );
    }

    #[test]
    fn test_locale_for_address_without_database_is_server_default() {
        if !in_isolated_config(&[("EMAIL_DEFAULT_LOCALE", "ru")]) {
            return;
        }
        let locale = block_on_runtime(locale_for_address("nobody@example.com"));
        assert_eq!(locale, Locale::Ru, "unknown address, no pool, EMAIL_DEFAULT_LOCALE=ru");
    }

    #[test]
    fn test_locale_for_address_reads_the_user_locale() {
        let database = std::env::temp_dir().join("vaultwarden_locale_test.sqlite3");
        drop(std::fs::remove_file(&database));
        if !in_isolated_config(&[
            ("EMAIL_DEFAULT_LOCALE", "ru"),
            ("DATABASE_URL", &format!("sqlite://{}", database.to_str().unwrap())),
        ]) {
            return;
        }
        let pool = DbPool::from_config().unwrap();
        block_on_runtime(async {
            let conn = pool.get().await.unwrap();
            let mut english_user = User::new("english@example.com", None);
            english_user.set_locale(Locale::En);
            english_user.save(&conn).await.unwrap();
        });
        init_db_pool(pool);

        let cases = [
            ("english@example.com", Locale::En, "user stored with en overrides the ru server default"),
            ("ENGLISH@Example.com", Locale::En, "lookup ignores the case of the address"),
            ("nobody@example.com", Locale::Ru, "unknown address gets the ru server default"),
        ];
        for (address, expected, why) in cases {
            assert_eq!(block_on_runtime(locale_for_address(address)), expected, "{address}: {why}");
        }
        drop(std::fs::remove_file(&database));
    }

    fn block_on_runtime<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(future)
    }
}
