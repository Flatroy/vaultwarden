use crate::CONFIG;

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

    /// `EMAIL_DEFAULT_LOCALE`, which config validation guarantees to be supported.
    pub fn server_default() -> Locale {
        Locale::from_code(&CONFIG.email_default_locale()).unwrap_or(Locale::En)
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
}
