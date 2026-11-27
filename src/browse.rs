//! Catalog discovery: Crunchyroll's browse and search endpoints, turned into simple cards.
//!
//! The discover API is undocumented and its shape drifts, so results are read loosely from
//! JSON values: a missing or renamed field degrades one card (no poster, no counts) instead
//! of failing the whole page. Series and movie listings (films) are returned; both are
//! links the downloader's fetch understands.

use serde_json::Value;

const BASE: &str = "https://www.crunchyroll.com/content/v2";

/// What a card is: a series (seasons of episodes) or a movie listing (one or more films).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Series,
    Movie,
}

/// One show as a card in the Browse grid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogItem {
    pub id: String,
    pub kind: Kind,
    pub title: String,
    pub description: String,
    /// A tall poster image URL (2:3), sized for a grid card.
    pub poster: Option<String>,
    /// A larger tall poster for the detail page.
    pub poster_large: Option<String>,
    pub season_count: Option<i64>,
    pub episode_count: Option<i64>,
    pub year: Option<i64>,
    pub dubbed: bool,
    pub subbed: bool,
    /// Audio (dub) and subtitle locales, e.g. "ja-JP"; empty when the catalog doesn't say.
    pub audio_locales: Vec<String>,
    pub subtitle_locales: Vec<String>,
}

impl CatalogItem {
    /// The link the downloader's fetch understands for this card.
    pub fn link(&self) -> String {
        match self.kind {
            Kind::Series => format!("https://www.crunchyroll.com/series/{}", self.id),
            Kind::Movie => format!("https://www.crunchyroll.com/movie_listing/{}", self.id),
        }
    }
}

/// Which kind of title the Browse grid lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Show {
    #[default]
    Series,
    Movies,
}

impl Show {
    pub const ALL: [Show; 2] = [Show::Series, Show::Movies];

    pub fn param(self) -> &'static str {
        match self {
            Show::Series => "series",
            Show::Movies => "movie_listing",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Show::Series => "Series",
            Show::Movies => "Movies",
        }
    }
}

/// A genre the catalog can be filtered by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Genre {
    /// The value for the browse `categories` parameter, e.g. "action".
    pub id: String,
    pub title: String,
}

/// How the Browse grid is ordered when no search text is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Popular,
    Newest,
    Alphabetical,
}

impl Sort {
    pub const ALL: [Sort; 3] = [Sort::Popular, Sort::Newest, Sort::Alphabetical];

    pub fn param(self) -> &'static str {
        match self {
            Sort::Popular => "popularity",
            Sort::Newest => "newly_added",
            Sort::Alphabetical => "alphabetical",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Popular => "Popular",
            Sort::Newest => "Newest",
            Sort::Alphabetical => "A–Z",
        }
    }
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass through).
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn browse_url(sort: Sort, show: Show, genre: Option<&str>, start: usize, n: usize, locale: &str) -> String {
    let mut url = format!(
        "{BASE}/discover/browse?start={start}&n={n}&sort_by={}&type={}&ratings=false&locale={}",
        sort.param(),
        show.param(),
        encode(locale)
    );
    if let Some(g) = genre.filter(|g| !g.is_empty()) {
        url.push_str(&format!("&categories={}", encode(g)));
    }
    url
}

/// Search across all kinds; series and movie listings are picked out of the grouped result.
pub fn search_url(query: &str, n: usize, locale: &str) -> String {
    format!("{BASE}/discover/search?q={}&n={n}&ratings=false&locale={}", encode(query.trim()), encode(locale))
}

pub fn categories_url(locale: &str) -> String {
    format!("{BASE}/discover/categories?locale={}", encode(locale))
}

/// Genres: `{ data: [ { id, localization: { title } } ] }`, read loosely (an entry without
/// an id is skipped; without a title, the id is shown capitalised).
pub fn parse_categories(v: &Value) -> Vec<Genre> {
    let mut out: Vec<Genre> = Vec::new();
    for c in v.get("data").and_then(Value::as_array).into_iter().flatten() {
        let mut id = str_of(c, "id");
        if id.is_empty() {
            id = str_of(c, "tenant_category");
        }
        if id.is_empty() || out.iter().any(|g| g.id == id) {
            continue;
        }
        let mut title = c.get("localization").map(|l| str_of(l, "title")).unwrap_or_default();
        if title.is_empty() {
            title = str_of(c, "title");
        }
        if title.is_empty() {
            let mut chars = id.chars();
            let cap: String = chars.next().map(|f| f.to_uppercase().chain(chars).collect()).unwrap_or_default();
            title = cap.replace(['-', '_'], " ");
        }
        out.push(Genre { id, title });
    }
    out
}

/// Asks the image CDN for a plain JPEG at a given size: its `format=auto` may answer with
/// WebP or AVIF, which not every image decoder handles.
pub fn jpeg_url(url: &str) -> String {
    url.replace("format=auto", "format=jpeg")
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_string()
}

fn int_of(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64).filter(|n| *n > 0)
}

/// Picks an image URL from `images.<key>` (a list of lists of {width, height, source}):
/// the smallest one at least `want` pixels wide, or the largest if none is that wide.
pub fn pick_image(item: &Value, key: &str, want: i64) -> Option<String> {
    let groups = item.get("images")?.get(key)?.as_array()?;
    let mut sizes: Vec<(i64, String)> = groups
        .iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|img| {
            let src = img.get("source")?.as_str()?.trim();
            (!src.is_empty()).then(|| (img.get("width").and_then(Value::as_i64).unwrap_or(0), src.to_string()))
        })
        .collect();
    if sizes.is_empty() {
        return None;
    }
    sizes.sort_by_key(|(w, _)| *w);
    let chosen = sizes.iter().find(|(w, _)| *w >= want).unwrap_or_else(|| sizes.last().expect("non-empty"));
    Some(jpeg_url(&chosen.1))
}

/// Reads one card from a discover or cms object: a series or a movie listing. Anything
/// else (episodes, single films, music) is skipped, as is an entry without an id or title.
pub fn item(v: &Value) -> Option<CatalogItem> {
    let kind = match str_of(v, "type").as_str() {
        "" | "series" => Kind::Series,
        "movie_listing" => Kind::Movie,
        _ => return None,
    };
    let id = str_of(v, "id");
    let title = str_of(v, "title");
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let meta = v
        .get("series_metadata")
        .or_else(|| v.get("movie_listing_metadata"))
        .unwrap_or(v);
    let mut description = str_of(v, "description");
    if description.is_empty() {
        description = str_of(v, "promo_description");
    }
    Some(CatalogItem {
        id,
        kind,
        title,
        description,
        poster: pick_image(v, "poster_tall", 240),
        poster_large: pick_image(v, "poster_tall", 480),
        season_count: int_of(meta, "season_count"),
        episode_count: int_of(meta, "episode_count"),
        year: int_of(meta, "series_launch_year").or_else(|| int_of(meta, "movie_release_year")),
        dubbed: meta.get("is_dubbed").and_then(Value::as_bool).unwrap_or(false),
        subbed: meta.get("is_subbed").and_then(Value::as_bool).unwrap_or(false),
        audio_locales: strs_of(meta, "audio_locales"),
        subtitle_locales: strs_of(meta, "subtitle_locales"),
    })
}

/// A list of non-empty strings, or empty when the field is missing or not a list.
fn strs_of(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// A browse page: `{ total, data: [series...] }`. Returns the cards and the total available.
pub fn parse_browse(v: &Value) -> (Vec<CatalogItem>, usize) {
    let items: Vec<CatalogItem> = v
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(item).collect())
        .unwrap_or_default();
    let total = v.get("total").and_then(Value::as_u64).map(|t| t as usize).unwrap_or(items.len());
    (items, total)
}

/// Search results: `{ data: [ { type, count, items: [...] }, ... ] }`, grouped by kind
/// (top_results, series, episode, ...). Series from every group, de-duplicated, best first.
pub fn parse_search(v: &Value) -> Vec<CatalogItem> {
    let mut out: Vec<CatalogItem> = Vec::new();
    let groups = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    for group in &groups {
        // Some responses put the items straight in `data`; treat such an entry as an item.
        let entries: Vec<&Value> = match group.get("items").and_then(Value::as_array) {
            Some(items) => items.iter().collect(),
            None => vec![group],
        };
        for entry in entries {
            if let Some(card) = item(entry) {
                if !out.iter().any(|c| c.id == card.id) {
                    out.push(card);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn series(id: &str, title: &str) -> Value {
        json!({
            "id": id, "type": "series", "title": title, "description": "A story.",
            "images": { "poster_tall": [[
                { "width": 60, "height": 90, "source": "https://img/x/format=auto/60.jpg" },
                { "width": 240, "height": 360, "source": "https://img/x/format=auto/240.jpg" },
                { "width": 480, "height": 720, "source": "https://img/x/format=auto/480.jpg" }
            ]]},
            "series_metadata": { "season_count": 2, "episode_count": 24, "series_launch_year": 2004,
                                  "is_dubbed": true, "is_subbed": true,
                                  "audio_locales": ["ja-JP", "en-US"], "subtitle_locales": ["en-US", "th-TH", ""] }
        })
    }

    #[test]
    fn reads_a_series_card() {
        let c = item(&series("GY5V", "Paranoia Agent")).unwrap();
        assert_eq!(c.id, "GY5V");
        assert_eq!(c.title, "Paranoia Agent");
        assert_eq!(c.poster.as_deref(), Some("https://img/x/format=jpeg/240.jpg"));
        assert_eq!(c.poster_large.as_deref(), Some("https://img/x/format=jpeg/480.jpg"));
        assert_eq!((c.season_count, c.episode_count, c.year), (Some(2), Some(24), Some(2004)));
        assert!(c.dubbed && c.subbed);
        assert_eq!(c.audio_locales, ["ja-JP", "en-US"]);
        assert_eq!(c.subtitle_locales, ["en-US", "th-TH"]);
        assert_eq!(c.link(), "https://www.crunchyroll.com/series/GY5V");
    }

    #[test]
    fn missing_fields_degrade_instead_of_failing() {
        let c = item(&json!({ "id": "A", "type": "series", "title": "Bare" })).unwrap();
        assert_eq!(c.poster, None);
        assert_eq!(c.season_count, None);
        assert!(c.audio_locales.is_empty() && c.subtitle_locales.is_empty());
        assert!(item(&json!({ "id": "", "title": "No id" })).is_none());
        assert!(item(&json!({ "id": "E1", "type": "episode", "title": "An episode" })).is_none());
    }

    #[test]
    fn parses_browse_page_and_total() {
        let v = json!({ "total": 1500, "data": [series("A", "One"), series("B", "Two")] });
        let (items, total) = parse_browse(&v);
        assert_eq!(items.len(), 2);
        assert_eq!(total, 1500);
    }

    #[test]
    fn parses_grouped_search_and_dedupes() {
        let v = json!({ "data": [
            { "type": "top_results", "count": 2, "items": [series("A", "One"),
                { "id": "E", "type": "episode", "title": "Ep" }] },
            { "type": "series", "count": 2, "items": [series("A", "One"), series("B", "Two")] }
        ]});
        let ids: Vec<String> = parse_search(&v).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["A", "B"]);
    }

    #[test]
    fn reads_a_movie_listing_card() {
        let v = json!({ "id": "GY8V", "type": "movie_listing", "title": "Akira",
            "movie_listing_metadata": { "movie_release_year": 1988, "is_dubbed": true } });
        let c = item(&v).unwrap();
        assert_eq!(c.kind, Kind::Movie);
        assert_eq!(c.year, Some(1988));
        assert_eq!(c.link(), "https://www.crunchyroll.com/movie_listing/GY8V");
    }

    #[test]
    fn browse_url_filters() {
        let u = browse_url(Sort::Newest, Show::Movies, Some("action"), 36, 36, "en-US");
        assert!(u.contains("sort_by=newly_added") && u.contains("type=movie_listing"));
        assert!(u.contains("categories=action") && u.contains("start=36"));
        assert!(!browse_url(Sort::Popular, Show::Series, None, 0, 36, "en-US").contains("categories"));
    }

    #[test]
    fn parses_genres_loosely() {
        let v = json!({ "data": [
            { "id": "action", "localization": { "title": "Action" } },
            { "id": "slice-of-life" },
            { "title": "no id" },
            { "id": "action", "localization": { "title": "Dup" } }
        ]});
        let g = parse_categories(&v);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].title, "Action");
        assert_eq!(g[1].title, "Slice of life");
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(encode("one piece"), "one%20piece");
        assert_eq!(encode("ก"), "%E0%B8%81");
        assert!(search_url("Frieren & co", 40, "en-US").contains("q=Frieren%20%26%20co"));
    }
}
