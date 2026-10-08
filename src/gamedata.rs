//! gamedata.rs — resolves a game's name and cover art from a Steam AppID,
//! via the public (no-auth) Steam Store API and CDN, caching both locally
//! so a trainer's game info is fetched once at add time, never on every
//! app launch.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
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

fn online_cache_path(appid: u32) -> Result<PathBuf> {
    Ok(cache_dir()?.join(format!("{appid}.online.txt")))
}

/// Steam Store category ids that mean a game has online play: 36 = Online
/// PvP, 38 = Online Co-op, 20 = MMO.
const ONLINE_CATEGORY_IDS: &[u64] = &[36, 38, 20];

/// Whether an appdetails `data` object lists an online-play category.
fn has_online_category(data: &serde_json::Value) -> bool {
    data.get("categories")
        .and_then(|v| v.as_array())
        .is_some_and(|cats| {
            cats.iter()
                .filter_map(|c| c.get("id").and_then(|id| id.as_u64()))
                .any(|id| ONLINE_CATEGORY_IDS.contains(&id))
        })
}

/// Whether the game is known to have online play, from the cache only (no
/// network). `None` means unknown: never looked up, or the lookup failed.
pub fn cached_online(appid: u32) -> Option<bool> {
    let text = std::fs::read_to_string(online_cache_path(appid).ok()?).ok()?;
    match text.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Looks up the game's online-play categories once and caches the result.
/// An existing cache entry skips the request; a failure writes nothing, so
/// the game stays "unknown" and the next association retries.
async fn fetch_and_cache_online(client: &reqwest::Client, appid: u32) -> Result<()> {
    if cached_online(appid).is_some() {
        return Ok(());
    }
    let url = format!("https://store.steampowered.com/api/appdetails?appids={appid}&filters=categories");
    let body: serde_json::Value = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("requesting categories for AppID {appid}"))?
        .error_for_status()
        .with_context(|| format!("categories request returned an error status for AppID {appid}"))?
        .json()
        .await
        .context("parsing categories response as JSON")?;
    let data = body
        .get(appid.to_string())
        .and_then(|v| v.get("data"))
        .with_context(|| format!("categories response for AppID {appid} has no data"))?;
    let online = has_online_category(data);
    std::fs::write(online_cache_path(appid)?, if online { "1" } else { "0" })
        .with_context(|| format!("caching online flag for AppID {appid}"))?;
    Ok(())
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
///
/// Local Steam data is tried first: the name from the game's appmanifest and
/// the cover from Steam's own library art cache, so an installed game needs no
/// network at all. Only what the local lookup could not supply is fetched.
pub async fn fetch_and_cache(appid: u32) -> Result<()> {
    let result = fetch_name_and_cover(appid).await;
    // The online-play flag is independent of name and cover; a failure here
    // only leaves it unknown (no anti-cheat warning for unlisted games).
    let online = match http_client() {
        Ok(c) => fetch_and_cache_online(&c, appid).await,
        Err(e) => Err(e),
    };
    if let Err(e) = online {
        crate::applog::log(&format!("gamedata: online-play lookup failed for AppID {appid}: {e}"));
    }
    result
}

async fn fetch_name_and_cover(appid: u32) -> Result<()> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir)?;

    let local = tokio::task::spawn_blocking(move || local_game_info(appid))
        .await
        .unwrap_or_default();

    let mut name = local.name;
    let mut header_image: Option<String> = None;
    let mut client: Option<reqwest::Client> = None;

    if name.is_none() || local.cover.is_none() {
        let c = http_client()?;
        match fetch_appdetails(&c, appid).await {
            Ok(details) => {
                if name.is_none() {
                    name = Some(details.name);
                }
                header_image = details.header_image;
            }
            Err(e) if name.is_some() => {
                crate::applog::log(&format!("gamedata: appdetails failed for AppID {appid}: {e}"));
            }
            Err(e) => return Err(e),
        }
        client = Some(c);
    }

    let name = name.with_context(|| format!("no name found for AppID {appid}"))?;
    std::fs::write(name_cache_path(appid)?, &name)
        .with_context(|| format!("caching name for AppID {appid}"))?;

    if let Some(src) = &local.cover {
        match cache_local_cover(src, appid) {
            Ok(()) => {
                crate::applog::log(&format!(
                    "gamedata: using local Steam cover art {} for AppID {appid}",
                    src.display()
                ));
                return Ok(());
            }
            Err(e) => crate::applog::log(&format!(
                "gamedata: could not use local cover art for AppID {appid}: {e}"
            )),
        }
    }

    let client = match client {
        Some(c) => c,
        None => http_client()?,
    };
    if let Err(e) = fetch_cover(&client, appid, header_image.as_deref()).await {
        crate::applog::log(&format!("gamedata: cover art fetch failed for AppID {appid}: {e}"));
    }

    Ok(())
}

fn cache_local_cover(src: &Path, appid: u32) -> Result<()> {
    let len = std::fs::metadata(src)?.len();
    if len > MAX_IMAGE_BYTES as u64 {
        anyhow::bail!("{} is larger than {MAX_IMAGE_BYTES} bytes", src.display());
    }
    std::fs::copy(src, cover_cache_path(appid)?)
        .with_context(|| format!("caching local cover art for AppID {appid}"))?;
    Ok(())
}

/// What the local Steam install knows about a game.
#[derive(Default)]
struct LocalInfo {
    name: Option<String>,
    cover: Option<PathBuf>,
}

fn local_game_info(appid: u32) -> LocalInfo {
    local_game_info_in(&crate::steam::metadata_roots(), appid)
}

/// Best effort across every Steam install in `roots`: the first manifest name
/// and the first usable cover found. Missing files just mean `None`.
fn local_game_info_in(roots: &[PathBuf], appid: u32) -> LocalInfo {
    let mut info = LocalInfo::default();
    for root in roots {
        if info.name.is_none() {
            let libs = crate::steam::library_folders(root);
            info.name = crate::steam::game_name(&libs, &appid.to_string());
        }
        if info.cover.is_none() {
            info.cover = local_cover(root, appid);
        }
        if info.name.is_some() && info.cover.is_some() {
            break;
        }
    }
    info
}

/// Image width and height from a JPEG or PNG header, without decoding it.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.starts_with(PNG_SIG) {
        if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
            return None;
        }
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        match marker {
            0xFF => {
                i += 1;
                continue;
            }
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD8 => {
                i += 2;
                continue;
            }
            0xD9 => return None,
            _ => {}
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if i + 9 > bytes.len() {
                return None;
            }
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// Dimensions of an image file, reading only its first 128 KB.
fn image_size(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(128 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    image_dimensions(&buf)
}

/// Header-style art: a modest landscape image (about 460x215 or 600x338).
/// Heroes (1920 wide) and square icons are deliberately not matched.
fn pick_header(candidates: &[(PathBuf, u32, u32)]) -> Option<PathBuf> {
    candidates
        .iter()
        .filter(|(_, w, h)| *h > 0 && *w <= 800 && (*w as f32 / *h as f32) >= 1.5 && (*w as f32 / *h as f32) <= 2.5)
        .min_by_key(|(_, w, h)| w.abs_diff(460) + h.abs_diff(215))
        .map(|(p, _, _)| p.clone())
}

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png"))
}

/// Image files under `dir`, descending at most `depth` levels (Steam's newer
/// layout nests hashed folders under the per-AppID folder).
fn collect_images(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                collect_images(&path, depth - 1, out);
            }
        } else if is_image_file(&path) {
            out.push(path);
        }
    }
}

/// Cover art for `appid` from Steam's local library cache under `root`:
/// the per-AppID folder (hashed file names, so identified by decoded
/// dimensions) plus the older flat `<appid>_*` files. Only header art is
/// used, never the portrait capsule, so every cover has the same shape.
fn local_cover(root: &Path, appid: u32) -> Option<PathBuf> {
    let cache = root.join("appcache/librarycache");
    let mut files: Vec<PathBuf> = Vec::new();
    collect_images(&cache.join(appid.to_string()), 2, &mut files);
    let prefix = format!("{appid}_");
    if let Ok(entries) = std::fs::read_dir(&cache) {
        for entry in entries.flatten() {
            let path = entry.path();
            let flat = path.is_file()
                && is_image_file(&path)
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix));
            if flat {
                files.push(path);
            }
        }
    }
    files.sort();

    let candidates: Vec<(PathBuf, u32, u32)> = files
        .into_iter()
        .filter_map(|p| image_size(&p).map(|(w, h)| (p, w, h)))
        .collect();
    pick_header(&candidates)
}

/// Normalizes a title for comparison: lowercase alphanumeric words, with
/// "the" dropped so "The Witcher 3" and "Witcher 3" line up.
fn title_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && *t != "the")
        .map(|t| t.to_string())
        .collect()
}

/// Suggests the installed game a trainer is probably for, from the trainer's
/// filename stem. Conservative on purpose: only a title that matches after
/// normalization, or one that differs by a token in a long title, counts, and
/// an ambiguous best match yields nothing. "Grand Theft Auto V" must not be
/// suggested for a "Grand Theft Auto V Enhanced" trainer.
pub fn suggest_installed(stem: &str, installed: &[(u32, String)]) -> Option<(u32, String)> {
    let wanted = title_tokens(&guess_search_term(stem));
    if wanted.is_empty() {
        return None;
    }
    let wanted_set: std::collections::HashSet<&String> = wanted.iter().collect();

    let mut best: Option<(f32, u32, &String)> = None;
    let mut ambiguous = false;
    for (appid, name) in installed {
        let tokens = title_tokens(name);
        if tokens.is_empty() {
            continue;
        }
        let score = if tokens == wanted {
            1.0
        } else {
            let set: std::collections::HashSet<&String> = tokens.iter().collect();
            let inter = set.intersection(&wanted_set).count() as f32;
            let union = set.union(&wanted_set).count() as f32;
            inter / union
        };
        if score < 0.85 {
            continue;
        }
        match best {
            Some((s, id, _)) if score < s || (score == s && id == *appid) => {}
            Some((s, _, _)) if score == s => ambiguous = true,
            _ => {
                best = Some((score, *appid, name));
                ambiguous = false;
            }
        }
    }
    if ambiguous {
        return None;
    }
    best.map(|(_, id, name)| (id, name.clone()))
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

/// Header art (460x215), the shape every row's cover slot is sized for: the
/// guessed flat header path first, then — the one guaranteed to work, since
/// it's the exact URL Steam's own store page serves for this AppID —
/// `header_image` from the appdetails response already fetched in
/// `fetch_and_cache`.
async fn fetch_cover(client: &reqwest::Client, appid: u32, header_image: Option<&str>) -> Result<()> {
    let header_url = format!("https://cdn.akamai.steamstatic.com/steam/apps/{appid}/header.jpg");

    let bytes = match download_image(client, &header_url).await {
        Ok(b) => b,
        Err(e) => match header_image {
            Some(url) => download_image(client, url).await?,
            None => return Err(e),
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

    let stripped = strip_early_access(&base);
    let mut words: Vec<&str> = stripped.split_whitespace().collect();
    // A stem with no version or count ("Some Game Trainer") keeps the word
    // "Trainer", which only hurts a title search or comparison.
    if words.len() > 1 && words.last().is_some_and(|w| w.eq_ignore_ascii_case("trainer")) {
        words.pop();
    }
    words.join(" ")
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
    use super::{guess_search_term, has_online_category};
    use std::path::PathBuf;

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
    /// Reflection) confirmed to 404 on the flat header.jpg guess — verifies the
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

    /// Header-only JPEG: SOI, a JFIF APP0 segment, then SOF0 with the size.
    fn fake_jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        b.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&[0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        b
    }

    /// Header-only PNG: signature plus an IHDR chunk with the size.
    fn fake_png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "steam-punk-gamedata-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&p).expect("scratch dir");
            Self(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn online_categories_are_parsed_from_appdetails() {
        let online = |s: &str| has_online_category(&serde_json::from_str(s).unwrap());
        assert!(online(r#"{"categories":[{"id":2,"description":"Single-player"},{"id":36,"description":"Online PvP"}]}"#));
        assert!(online(r#"{"categories":[{"id":38}]}"#));
        assert!(online(r#"{"categories":[{"id":20}]}"#));
        // Plain Multi-player (1) or local co-op does not count as online.
        assert!(!online(r#"{"categories":[{"id":1},{"id":2},{"id":24}]}"#));
        assert!(!online(r#"{"categories":[]}"#));
        assert!(!online(r#"{"name":"x"}"#));
        assert!(!online(r#"{"categories":"bad"}"#));
    }

    #[test]
    fn reads_jpeg_and_png_dimensions() {
        assert_eq!(super::image_dimensions(&fake_jpeg(600, 900)), Some((600, 900)));
        assert_eq!(super::image_dimensions(&fake_png(1920, 620)), Some((1920, 620)));
        assert_eq!(super::image_dimensions(b"GIF89a not supported"), None);
        assert_eq!(super::image_dimensions(&[0xFF, 0xD8, 0xFF]), None);
        assert_eq!(super::image_dimensions(&[]), None);
    }

    #[test]
    fn header_pick_ignores_heroes_and_portraits() {
        let c = vec![
            (PathBuf::from("hero.jpg"), 1920, 620),
            (PathBuf::from("header.jpg"), 460, 215),
            (PathBuf::from("small.jpg"), 300, 450),
            (PathBuf::from("capsule.jpg"), 600, 900),
        ];
        assert_eq!(super::pick_header(&c), Some(PathBuf::from("header.jpg")));
        assert_eq!(super::pick_header(&c[..1]), None);
        assert_eq!(super::pick_header(&c[2..]), None);
    }

    #[test]
    fn local_cover_scans_hashed_subfolders_by_dimensions() {
        let s = Scratch::new("hashed");
        let dir = s.0.join("appcache/librarycache/3240220");
        std::fs::create_dir_all(dir.join("0a1b2c3d")).unwrap();
        std::fs::write(dir.join("0a1b2c3d/9f8e7d.jpg"), fake_jpeg(1920, 620)).unwrap();
        std::fs::write(dir.join("0a1b2c3d/library_header.jpg"), fake_jpeg(460, 215)).unwrap();
        std::fs::write(dir.join("4e5f6a.jpg"), fake_jpeg(600, 900)).unwrap();
        std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();

        assert_eq!(super::local_cover(&s.0, 3240220), Some(dir.join("0a1b2c3d/library_header.jpg")));
    }

    #[test]
    fn local_cover_accepts_old_flat_names_and_never_takes_a_portrait() {
        let s = Scratch::new("flat");
        let cache = s.0.join("appcache/librarycache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("42_library_600x900.jpg"), fake_jpeg(600, 900)).unwrap();
        std::fs::write(cache.join("42_header.jpg"), fake_jpeg(460, 215)).unwrap();
        std::fs::write(cache.join("43_header.jpg"), fake_jpeg(460, 215)).unwrap();
        std::fs::write(cache.join("420_library_600x900.jpg"), fake_jpeg(600, 900)).unwrap();

        assert_eq!(super::local_cover(&s.0, 42), Some(cache.join("42_header.jpg")));
        assert_eq!(super::local_cover(&s.0, 43), Some(cache.join("43_header.jpg")));
        assert_eq!(super::local_cover(&s.0, 420), None);
        assert_eq!(super::local_cover(&s.0, 44), None);
    }

    #[test]
    fn local_info_reads_name_and_cover_from_a_steam_root() {
        let s = Scratch::new("info");
        std::fs::create_dir_all(s.0.join("steamapps")).unwrap();
        std::fs::write(
            s.0.join("steamapps/appmanifest_7.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\"7\"\n\t\"name\"\t\"Seven\"\n}\n",
        )
        .unwrap();
        std::fs::create_dir_all(s.0.join("appcache/librarycache/7")).unwrap();
        std::fs::write(s.0.join("appcache/librarycache/7/x.jpg"), fake_jpeg(460, 215)).unwrap();

        let info = super::local_game_info_in(std::slice::from_ref(&s.0), 7);
        assert_eq!(info.name.as_deref(), Some("Seven"));
        assert!(info.cover.is_some());
        let none = super::local_game_info_in(std::slice::from_ref(&s.0), 8);
        assert!(none.name.is_none() && none.cover.is_none());
        assert!(super::local_game_info_in(&[], 7).name.is_none());
    }

    fn games() -> Vec<(u32, String)> {
        vec![
            (271590, "Grand Theft Auto V Legacy".to_string()),
            (3240220, "Grand Theft Auto V Enhanced".to_string()),
            (1245620, "ELDEN RING".to_string()),
            (1, "The Witcher 3: Wild Hunt".to_string()),
            (2, "Hades".to_string()),
            (3, "Hades II".to_string()),
        ]
    }

    #[test]
    fn suggests_the_installed_game_for_typical_trainer_names() {
        let g = games();
        let id = |stem: &str| super::suggest_installed(stem, &g).map(|(id, _)| id);
        assert_eq!(id("Elden Ring v1.16 Plus 25 Trainer"), Some(1245620));
        assert_eq!(id("Elden Ring Trainer"), Some(1245620));
        assert_eq!(id("Witcher 3 Wild Hunt v4.04 Plus 12 Trainer"), Some(1));
        assert_eq!(id("Grand Theft Auto V Enhanced v1.0.811 Plus 22 Trainer"), Some(3240220));
        assert_eq!(id("Hades II Early Access v1.0 Plus 5 Trainer"), Some(3));
        assert_eq!(id("Hades v1.38 Plus 8 Trainer"), Some(2));
    }

    #[test]
    fn suggests_nothing_when_unsure() {
        let g = games();
        assert_eq!(super::suggest_installed("Crimson Desert v1.0 Plus 12 Trainer", &g), None);
        assert_eq!(super::suggest_installed("", &g), None);
        // Only the legacy game installed: the Enhanced trainer must not match it.
        let legacy_only = vec![(271590, "Grand Theft Auto V".to_string())];
        assert_eq!(
            super::suggest_installed("Grand Theft Auto V Enhanced v1.0 Plus 22 Trainer", &legacy_only),
            None
        );
        // Two different installs with the same title is ambiguous.
        let dup = vec![(10, "Same Game".to_string()), (11, "Same Game".to_string())];
        assert_eq!(super::suggest_installed("Same Game Trainer", &dup), None);
    }

    #[test]
    fn trailing_trainer_word_is_dropped_from_the_search_term() {
        assert_eq!(guess_search_term("Some Game Trainer"), "Some Game");
        assert_eq!(guess_search_term("Trainer"), "Trainer");
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
