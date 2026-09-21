//! gamedata.rs — resolves a game's name and cover art from a Steam AppID,
//! via the public (no-auth) Steam Store API and CDN, caching both locally
//! so a trainer's game info is fetched once at add time, never on every
//! app launch.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

/// Upper bound on a downloaded image, so a wrong/huge response can't be
/// cached or held in memory.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// One HTTP client shared by every request (connection pooling, one TLS
/// setup) instead of building a new one per call.
fn http_client() -> Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let built = reqwest::Client::builder()
        .user_agent("steam-punk (https://github.com/labj1987/steam-punk)")
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(CLIENT.get_or_init(|| built).clone())
}

fn cache_dir() -> Result<PathBuf> {
    Ok(crate::library::data_dir()?.join("cache"))
}

fn name_cache_path(appid: u32) -> Result<PathBuf> {
    Ok(cache_dir()?.join(format!("{appid}.name.txt")))
}

fn cover_cache_path(appid: u32) -> Result<PathBuf> {
    Ok(cache_dir()?.join(format!("{appid}.jpg")))
}

/// The cached game name, if a fetch has previously succeeded. No network
/// access — a cache miss just means the caller falls back to the trainer's
/// filename-derived title.
pub fn cached_name(appid: u32) -> Option<String> {
    let path = name_cache_path(appid).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The cached cover-art image path, if a fetch has previously succeeded.
pub fn cached_cover(appid: u32) -> Option<PathBuf> {
    let path = cover_cache_path(appid).ok()?;
    path.is_file().then_some(path)
}

/// Fetches the game's name and cover art for `appid` and caches both to
/// disk. Called once per app session, right after a trainer is associated
/// with an AppID and again on every later launch for any trainer that has
/// an AppID but no cached cover yet — see the retry note on `fetch_cover`
/// for why a failed cover fetch shouldn't be treated as permanent the way
/// `cached_name`/`cached_cover` otherwise let it be.
///
/// The name fetch failing is a real error (nothing to show). The cover
/// fetch failing is logged but not propagated — a game with a name and no
/// art is still strictly better than falling back to the filename.
pub async fn fetch_and_cache(appid: u32) -> Result<()> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir)?;

    let client = http_client()?;

    let details = fetch_appdetails(&client, appid).await?;
    std::fs::write(name_cache_path(appid)?, &details.name)
        .with_context(|| format!("caching name for AppID {appid}"))?;

    if let Err(e) = fetch_cover(&client, appid, details.header_image.as_deref()).await {
        crate::applog::log(&format!("gamedata: cover art fetch failed for AppID {appid}: {e}"));
    }

    Ok(())
}

struct AppDetails {
    name: String,
    /// The `header_image` field from Steam's own appdetails response — a
    /// content-hashed `shared.akamai.steamstatic.com/store_item_assets/...`
    /// URL Steam's own store page uses, unlike the flat `cdn.akamai.
    /// steamstatic.com/steam/apps/<id>/header.jpg` guess `fetch_cover` tries
    /// first. That flat-path guess 404s for a growing number of apps now
    /// that Steam has moved most current games onto hashed asset paths
    /// (confirmed live, e.g. AppID 2852190 — Monster Hunter Stories 3 — 404s
    /// on both the library and flat header guesses), so this is the
    /// reliable fallback rather than a second unreliable guess.
    header_image: Option<String>,
}

async fn fetch_appdetails(client: &reqwest::Client, appid: u32) -> Result<AppDetails> {
    let url = format!("https://store.steampowered.com/api/appdetails?appids={appid}");
    let body: serde_json::Value = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("requesting appdetails for AppID {appid}"))?
        .error_for_status()
        .with_context(|| format!("appdetails returned an error status for AppID {appid}"))?
        .json()
        .await
        .context("parsing appdetails response as JSON")?;

    let data = body.get(appid.to_string()).and_then(|v| v.get("data"));

    let name = data
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str())
        .with_context(|| format!("appdetails response for AppID {appid} has no data.name — is the AppID valid?"))?
        .to_string();

    let header_image = data
        .and_then(|v| v.get("header_image"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(AppDetails { name, header_image })
}

/// Tries the guessed horizontal library capsule art first (matching Steam's
/// own library-list aesthetic, when it exists), then the guessed flat
/// header path, then — the one guaranteed to work, since it's the exact
/// URL Steam's own store page serves for this AppID — `header_image` from
/// the appdetails response already fetched in `fetch_and_cache`.
async fn fetch_cover(client: &reqwest::Client, appid: u32, header_image: Option<&str>) -> Result<()> {
    let library_url =
        format!("https://cdn.akamai.steamstatic.com/steam/apps/{appid}/library_600x338.jpg");
    let header_url = format!("https://cdn.akamai.steamstatic.com/steam/apps/{appid}/header.jpg");

    let bytes = match download_image(client, &library_url).await {
        Ok(b) => b,
        Err(_) => match download_image(client, &header_url).await {
            Ok(b) => b,
            Err(e) => match header_image {
                Some(url) => download_image(client, url).await?,
                None => return Err(e),
            },
        },
    };
    std::fs::write(cover_cache_path(appid)?, bytes)
        .with_context(|| format!("caching cover art for AppID {appid}"))?;
    Ok(())
}

pub(crate) async fn download_image(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("{url} returned an error status"))?;
    let is_image = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("image/"));
    if !is_image {
        anyhow::bail!("{url} did not return an image (wrong Content-Type)");
    }
    if resp.content_length().is_some_and(|n| n > MAX_IMAGE_BYTES as u64) {
        anyhow::bail!("{url} image is larger than {MAX_IMAGE_BYTES} bytes");
    }
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("{url} image is larger than {MAX_IMAGE_BYTES} bytes");
    }
    Ok(bytes.to_vec())
}

/// Minimal percent-encoding for a query-string value. `reqwest`'s own
/// `.query()` helper needs the `query` feature, which pulls in
/// `serde_urlencoded` as a new dependency — not worth it just for one
/// simple parameter, so this hand-rolls the same RFC 3986 unreserved-char
/// allowlist instead.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// One Steam Store search hit, trimmed down to what the AppID picker needs.
pub struct SearchResult {
    pub appid: u32,
    pub name: String,
    pub thumbnail_url: Option<String>,
}

/// Caps how many rows the AppID search dropdown shows.
const MAX_SEARCH_RESULTS: usize = 8;

/// Downloads a search result's thumbnail into memory (no disk cache — the
/// picker shows results for many candidate games the user won't pick, so
/// caching them to `cache_dir()` would just accumulate cruft for AppIDs
/// nobody ends up choosing). Builds its own short-lived client, same as
/// `fetch_and_cache`'s image fetch, since there's no long-lived client to
/// share across a UI-triggered one-off call.
pub async fn fetch_thumbnail(url: &str) -> Result<Vec<u8>> {
    let client = http_client()?;
    download_image(&client, url).await
}

/// Live search against Steam's public storesearch API — used by the AppID
/// picker so the user can find a game by name instead of typing a raw
/// AppID. A blank/whitespace-only term is treated as "no search yet" rather
/// than an error, so the picker can call this on every keystroke without
/// special-casing an empty box.
pub async fn search(term: &str) -> Result<Vec<SearchResult>> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }

    let client = http_client()?;

    let url = format!(
        "https://store.steampowered.com/api/storesearch/?term={}&cc=us&l=en",
        percent_encode(term)
    );
    let body: serde_json::Value = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("requesting storesearch for {term:?}"))?
        .error_for_status()
        .with_context(|| format!("storesearch returned an error status for {term:?}"))?
        .json()
        .await
        .context("parsing storesearch response as JSON")?;

    let items = body.get("items").and_then(|v| v.as_array());
    let Some(items) = items else {
        return Ok(Vec::new());
    };

    let results = items
        .iter()
        .filter_map(|item| {
            let appid = item.get("id")?.as_u64()? as u32;
            let name = item.get("name")?.as_str()?.to_string();
            let thumbnail_url = item
                .get("tiny_image")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(SearchResult {
                appid,
                name,
                thumbnail_url,
            })
        })
        .take(MAX_SEARCH_RESULTS)
        .collect();

    Ok(results)
}

/// Guesses a Steam search term from a trainer's filename stem, by cutting
/// the string at the first word that looks like a version token (`v` or `V`
/// immediately followed by a digit — so a bare `"V"`, as in "Grand Theft
/// Auto V", is left alone) or that is exactly "plus" (case-insensitive),
/// which is how trainer filenames introduce their cheat count. Everything
/// before the cut is joined back together as the guessed title; if nothing
/// matches, the whole stem is returned unchanged.
///
/// "Early Access" is then stripped from wherever it appears in that result
/// (case-insensitive, exact two-word phrase) — trainer filenames for
/// early-access games often include it ahead of the version token (so it
/// survives the cut above), but Steam's storesearch API returns zero
/// results for a query containing it, confirmed live: appending "Early
/// Access" to an otherwise-matching search term reliably drops the hit
/// count to 0, even for real, currently-listed games.
pub fn guess_search_term(filename_stem: &str) -> String {
    let is_version_token = |word: &str| {
        let mut chars = word.chars();
        matches!(chars.next(), Some('v') | Some('V')) && matches!(chars.next(), Some(c) if c.is_ascii_digit())
    };

    let words: Vec<&str> = filename_stem.split_whitespace().collect();
    let cut = words
        .iter()
        .position(|w| is_version_token(w) || w.eq_ignore_ascii_case("plus"));

    let base = match cut {
        Some(i) => words[..i].join(" "),
        None => filename_stem.to_string(),
    };

    strip_early_access(&base)
}

fn strip_early_access(s: &str) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        if i + 1 < words.len()
            && words[i].eq_ignore_ascii_case("early")
            && words[i + 1].eq_ignore_ascii_case("access")
        {
            i += 2;
            continue;
        }
        out.push(words[i]);
        i += 1;
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::guess_search_term;

    #[test]
    fn strips_version_and_plus_count() {
        assert_eq!(
            guess_search_term("Grand Theft Auto V Enhanced v1.0.811 Plus 22 Trainer"),
            "Grand Theft Auto V Enhanced"
        );
        assert_eq!(
            guess_search_term("Crimson Desert v1.0-v1.16 Plus 12 Trainer"),
            "Crimson Desert"
        );
        assert_eq!(
            guess_search_term(
                "Grand Theft Auto San Andreas The Definitive Edition v1.0-v1.0.8.11827 Plus 49 Trainer"
            ),
            "Grand Theft Auto San Andreas The Definitive Edition"
        );
    }

    #[test]
    fn bare_v_is_not_a_version_token() {
        // "V" with nothing after it (or a non-digit after it) must not
        // trigger the cut — only Roman-numeral-free digit versions do.
        assert_eq!(guess_search_term("Grand Theft Auto V"), "Grand Theft Auto V");
    }

    #[test]
    fn no_match_returns_whole_stem() {
        assert_eq!(guess_search_term("Crimson Desert"), "Crimson Desert");
    }

    #[test]
    fn strips_early_access() {
        assert_eq!(
            guess_search_term("Some Game Early Access v1.0 Plus 20 Trainer"),
            "Some Game"
        );
        // Case-insensitive, and doesn't require it to be right before the
        // version token.
        assert_eq!(
            guess_search_term("Schedule I EARLY ACCESS v0.3.5f8 Plus 10 Trainer"),
            "Schedule I"
        );
    }

    /// Live check against a real AppID (Monster Hunter Stories 3: Twisted
    /// Reflection) confirmed to 404 on both guessed CDN paths
    /// (library_600x338.jpg and the flat header.jpg) — verifies the
    /// appdetails header_image fallback actually rescues cover art for it.
    #[tokio::test]
    #[ignore]
    async fn live_fetch_cover_falls_back_to_appdetails_header_image() {
        let appid = 2852190;
        let _ = std::fs::remove_file(super::cover_cache_path(appid).unwrap());
        super::fetch_and_cache(appid).await.expect("fetch_and_cache");
        let cover = super::cached_cover(appid);
        assert!(cover.is_some(), "expected cover art to be cached after fallback");
        println!("cover cached at {:?}", cover.unwrap());
    }

    /// Live check that a real, currently-listed game is actually findable
    /// once "Early Access" is stripped from the guessed term — the bug
    /// report was that these games returned zero search results.
    #[tokio::test]
    #[ignore]
    async fn live_search_finds_game_after_stripping_early_access() {
        let term = guess_search_term("Schedule I Early Access v0.3.5f8 Plus 10 Trainer");
        assert_eq!(term, "Schedule I");
        let results = super::search(&term).await.expect("search");
        assert!(!results.is_empty(), "expected at least one result for {term:?}");
        println!("results for {term:?}: {}", results.len());
        for r in &results {
            println!("  {} {}", r.appid, r.name);
        }
    }

    #[test]
    fn encodes_reserved_characters_and_passes_through_unreserved_ones() {
        assert_eq!(super::percent_encode("hello"), "hello");
        assert_eq!(super::percent_encode("hello world"), "hello%20world");
        assert_eq!(super::percent_encode("100%"), "100%25");
        assert_eq!(super::percent_encode(""), "");
        assert_eq!(super::percent_encode("a-b_c.d~e"), "a-b_c.d~e");
    }
}
