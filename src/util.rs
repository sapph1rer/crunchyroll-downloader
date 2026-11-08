use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Human-readable names for Crunchyroll locales.
pub static LANGUAGE_NAMES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("ja-JP", "日本語"),
        ("en-US", "English"),
        ("en-IN", "English (India)"),
        ("id-ID", "Bahasa Indonesia"),
        ("ms-MY", "Bahasa Melayu"),
        ("ca-ES", "Català"),
        ("de-DE", "Deutsch"),
        ("es-419", "Español (América Latina)"),
        ("es-ES", "Español (España)"),
        ("fr-FR", "Français"),
        ("it-IT", "Italiano"),
        ("pl-PL", "Polski"),
        ("pt-BR", "Português (Brasil)"),
        ("pt-PT", "Português (Portugal)"),
        ("vi-VN", "Tiếng Việt"),
        ("tr-TR", "Türkçe"),
        ("ru-RU", "Русский"),
        ("ar-SA", "العربية"),
        ("hi-IN", "हिंदी"),
        ("ta-IN", "தமிழ்"),
        ("te-IN", "తెలుగు"),
        ("zh-CN", "中文 (普通话)"),
        ("zh-HK", "中文 (粵語)"),
        ("zh-TW", "中文 (國語)"),
        ("ko-KR", "한국어"),
        ("th-TH", "ไทย"),
    ])
});

/// ISO 639-2 codes for ffmpeg's `language` stream tag.
pub static LANGUAGE_CODES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("ja-JP", "jpn"),
        ("en-US", "eng"),
        ("en-IN", "eng"),
        ("id-ID", "ind"),
        ("ms-MY", "msa"),
        ("ca-ES", "cat"),
        ("de-DE", "deu"),
        ("es-419", "spa"),
        ("es-ES", "spa"),
        ("fr-FR", "fra"),
        ("it-IT", "ita"),
        ("pl-PL", "pol"),
        ("pt-BR", "por"),
        ("pt-PT", "por"),
        ("vi-VN", "vie"),
        ("tr-TR", "tur"),
        ("ru-RU", "rus"),
        ("ar-SA", "ara"),
        ("hi-IN", "hin"),
        ("ta-IN", "tam"),
        ("te-IN", "tel"),
        ("zh-CN", "zho"),
        ("zh-HK", "zho"),
        ("zh-TW", "zho"),
        ("ko-KR", "kor"),
        ("th-TH", "tha"),
    ])
});

/// Returns a readable track name, falling back to the raw locale.
pub fn track_title(locale: &str) -> String {
    LANGUAGE_NAMES
        .get(locale)
        .map(|s| s.to_string())
        .unwrap_or_else(|| locale.to_string())
}

static ILLEGAL_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[\\/:*?"<>|'’`“”]"#).unwrap());
static UNDERSCORE_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_+").unwrap());

/// Replaces characters illegal in filenames, collapses underscores and trims
/// trailing spaces/dots. Empty input yields "Unknown".
pub fn sanitize_filename(name: &str) -> String {
    if name.is_empty() {
        return "Unknown".into();
    }
    let res = ILLEGAL_CHARS.replace_all(name, "_");
    let res = UNDERSCORE_RUNS.replace_all(&res, "_");
    res.trim_end_matches([' ', '.']).to_string()
}

/// Splits a comma-separated locale list, trimming and dropping empties.
pub fn parse_langs(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

pub fn requests_all(langs: &[String]) -> bool {
    langs.iter().any(|l| l.eq_ignore_ascii_case("all"))
}

/// First explicit (non-ALL) locale, or `fallback`.
pub fn preferred_request_locale(langs: &[String], fallback: &str) -> String {
    langs
        .iter()
        .find(|l| !l.eq_ignore_ascii_case("all"))
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

/// Keeps explicit locales first, then appends available ones in sorted order.
/// Lists without ALL are returned unchanged.
pub fn expand_langs(requested: &[String], mut available: Vec<String>, preferred: &str) -> Vec<String> {
    if !requests_all(requested) {
        return requested.to_vec();
    }
    let mut selected: Vec<String> = Vec::new();
    for l in requested {
        if !l.eq_ignore_ascii_case("all") && !selected.contains(l) {
            selected.push(l.clone());
        }
    }
    if selected.is_empty() && available.iter().any(|a| a == preferred) {
        selected.push(preferred.to_string());
    }
    available.sort();
    for l in available {
        if !selected.contains(&l) {
            selected.push(l);
        }
    }
    selected
}

/// Parses "/watch/ID" or "/series/ID" (tolerating a locale prefix).
/// Parses a whole number typed by the user and checks it is within `min..=max`.
/// Errors name the allowed range, so the message can be shown as it is.
pub fn parse_count(s: &str, min: usize, max: usize) -> Result<usize, String> {
    let n: usize = s
        .trim()
        .parse()
        .map_err(|_| format!("\"{}\" is not a whole number from {min} to {max}", s.trim()))?;
    if (min..=max).contains(&n) {
        Ok(n)
    } else {
        Err(format!("{n} is outside the allowed range {min} to {max}"))
    }
}

pub fn parse_url(url: &str) -> Option<(&str, &str)> {
    let trimmed = url.trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    for (i, p) in parts.iter().enumerate() {
        if (*p == "watch" || *p == "series") && i + 1 < parts.len() {
            return Some((p, parts[i + 1]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize_filename(""), "Unknown");
        assert_eq!(sanitize_filename("a/b:c?."), "a_b_c_");
    }

    #[test]
    fn parses_urls() {
        assert_eq!(
            parse_url("https://www.crunchyroll.com/fr/watch/GE001/x"),
            Some(("watch", "GE001"))
        );
        assert_eq!(parse_url("https://example.com/none"), None);
    }

    #[test]
    fn expands_all() {
        let req = parse_langs("fr-FR,ALL");
        let out = expand_langs(&req, vec!["en-US".into(), "de-DE".into()], "en-US");
        assert_eq!(out, vec!["fr-FR", "de-DE", "en-US"]);
    }

    /// Port of Go TestSanitizeFilename.
    #[test]
    fn sanitize_filename_table() {
        for (input, want) in [
            ("", "Unknown"),
            ("Frieren Beyond Journey's End", "Frieren Beyond Journey_s End"),
            ("a/b\\c", "a_b_c"),
            (r#"a:b*c?d"e<f>g|h"#, "a_b_c_d_e_f_g_h"),
            ("“Hello” ‘s ’t", "_Hello_ ‘s _t"),
            ("a`b", "a_b"),
            ("a///b", "a_b"),
            ("a___b", "a_b"),
            ("Episode 1...", "Episode 1"),
            ("Episode 1   ", "Episode 1"),
            ("Episode 1. . ", "Episode 1"),
            (" Episode", " Episode"),
            ("進撃の巨人", "進撃の巨人"),
            ("///", "_"),
        ] {
            assert_eq!(sanitize_filename(input), want, "sanitize_filename({input:?})");
        }
    }

    /// Port of Go TestPreferredRequestLocale.
    #[test]
    fn preferred_request_locale_cases() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(preferred_request_locale(&s(&["fr-FR", "ALL"]), "en-US"), "fr-FR");
        assert_eq!(preferred_request_locale(&s(&["ALL", "fr-FR"]), "en-US"), "fr-FR");
        assert_eq!(preferred_request_locale(&s(&["all"]), "ja-JP"), "ja-JP");
        assert_eq!(preferred_request_locale(&[], "en-US"), "en-US");
    }

    /// Port of Go TestParseUrl. Go returns empty strings for no match; Rust returns None.
    #[test]
    fn parse_url_cases() {
        for (url, want) in [
            ("https://www.crunchyroll.com/series/G0XHWM0D3/some-slug", Some(("series", "G0XHWM0D3"))),
            (
                "https://www.crunchyroll.com/fr/series/G0XHWM0D3/trapped-in-a-dating-sim-the-world-of-otome-games-is-tough-for-mobs",
                Some(("series", "G0XHWM0D3")),
            ),
            ("https://www.crunchyroll.com/fr/watch/G0XHWM0D3/episode-slug", Some(("watch", "G0XHWM0D3"))),
            ("https://www.crunchyroll.com/series/G0XHWM0D3/", Some(("series", "G0XHWM0D3"))),
            ("https://www.crunchyroll.com/", None),
            ("https://www.crunchyroll.com/series", None),
        ] {
            assert_eq!(parse_url(url), want, "parse_url({url:?})");
        }
    }

    /// Port of Go TestExpandLangs.
    #[test]
    fn expand_langs_cases() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cases: Vec<(Vec<String>, Vec<String>, &str, Vec<String>)> = vec![
            (s(&["fr-FR", "ALL"]), s(&["es-419", "en-US", "fr-FR"]), "en-US", s(&["fr-FR", "en-US", "es-419"])),
            (s(&["ALL", "fr-FR"]), s(&["en-US", "fr-FR"]), "en-US", s(&["fr-FR", "en-US"])),
            (
                s(&["fr-FR", "ALL", "en-US", "all", "fr-FR"]),
                s(&["es-419", "en-US", "fr-FR"]),
                "en-US",
                s(&["fr-FR", "en-US", "es-419"]),
            ),
            (s(&["ALL"]), s(&["it-IT", "ja-JP", "en-US"]), "ja-JP", s(&["ja-JP", "en-US", "it-IT"])),
            (s(&["all"]), s(&["fr-FR", "en-US", "de-DE"]), "en-US", s(&["en-US", "de-DE", "fr-FR"])),
            (s(&["ALL"]), s(&["fr-FR", "de-DE"]), "en-US", s(&["de-DE", "fr-FR"])),
            (s(&["hi-IN", "ALL"]), s(&["fr-FR"]), "en-US", s(&["hi-IN", "fr-FR"])),
            (s(&["ALL"]), vec![], "en-US", vec![]),
            (s(&["hi-IN", "en-US", "en-US"]), s(&["en-US"]), "en-US", s(&["hi-IN", "en-US", "en-US"])),
        ];
        for (requested, available, preferred, want) in cases {
            let got = expand_langs(&requested, available.clone(), preferred);
            assert_eq!(got, want, "expand_langs({requested:?}, {available:?}, {preferred:?})");
        }
    }

    #[test]
    fn parse_count_checks_range() {
        assert_eq!(parse_count(" 10 ", 1, 16), Ok(10));
        assert!(parse_count("0", 1, 16).unwrap_err().contains("outside"));
        assert!(parse_count("17", 1, 16).is_err());
        assert!(parse_count("ten", 1, 16).unwrap_err().contains("whole number"));
        assert!(parse_count("", 1, 16).is_err());
    }
}
