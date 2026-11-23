//! Public reads from Rhythia's API. Answers are checked and cut down to what Atlas shows.
//! Maps seen in answers are remembered for a while (`Seen`): their cover address for the cover
//! protocol and their title for the download list.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::library::{clip, is_hash};
use crate::net::{Host, Net};

pub const SORTS: [&str; 5] = ["newest", "playCount", "likeRatio", "starRating", "length"];
/// Rhythia's map statuses; APPROVED is what the website calls Legacy.
pub const STATUSES: [&str; 4] = ["RANKED", "APPROVED", "QUALIFIED", "UNRANKED"];
/// A collection has at most this many pages of 100 maps for Atlas (10 000 maps).
const MAX_COLLECTION_PAGES: u32 = 100;
pub const COLLECTIONS_PER_PAGE: u32 = 30;
const MAX_SEEN: usize = 50_000;

/// A map seen in one of Rhythia's answers.
#[derive(Clone)]
pub struct SeenMap {
    pub title: String,
    /// Small preview on static.rhythia.com.
    pub cover: Option<String>,
    /// The map file on static.rhythia.com (`.rhm` or `.sspm`), for ZIP and file downloads.
    pub file: Option<String>,
}

#[derive(Default)]
pub struct Seen(Mutex<HashMap<i64, SeenMap>>);

impl Seen {
    pub fn get(&self, id: i64) -> Option<SeenMap> {
        self.0.lock().unwrap().get(&id).cloned()
    }

    fn remember(&self, maps: &[(i64, SeenMap)]) {
        let mut seen = self.0.lock().unwrap();
        if seen.len() + maps.len() > MAX_SEEN {
            seen.clear();
        }
        for (id, m) in maps {
            seen.insert(*id, m.clone());
        }
    }
}

#[derive(Serialize)]
struct BeatmapsQuery<'a> {
    session: &'static str,
    page: u32,
    sort: &'a str,
    #[serde(rename = "sortDirection")]
    sort_direction: &'static str,
    #[serde(rename = "textFilter", skip_serializing_if = "Option::is_none")]
    text_filter: Option<&'a str>,
    #[serde(rename = "tagsFilter", skip_serializing_if = "Option::is_none")]
    tags_filter: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    #[serde(rename = "minStars", skip_serializing_if = "Option::is_none")]
    min_stars: Option<f64>,
    #[serde(rename = "maxStars", skip_serializing_if = "Option::is_none")]
    max_stars: Option<f64>,
}

#[derive(Deserialize)]
struct BeatmapsAnswer {
    #[serde(default)]
    total: Option<f64>,
    #[serde(default)]
    beatmaps: Vec<RawMap>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMap {
    id: Option<f64>,
    title: Option<String>,
    owner_username: Option<String>,
    star_rating: Option<f64>,
    length: Option<f64>,
    playcount: Option<f64>,
    status: Option<String>,
    map_hash: Option<String>,
    preview_image: Option<RawPreview>,
    tags: Option<String>,
    beatmap_file: Option<String>,
    /// Qualified maps come as UNRANKED with this flag.
    qualified: Option<bool>,
}

#[derive(Deserialize)]
struct RawPreview {
    small: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OnlineMap {
    pub id: i64,
    /// The mapper's words for the map ("Jumps Streams Russian"), as Rhythia has them.
    pub tags: Vec<String>,
    pub title: String,
    pub mapper: String,
    pub stars: Option<f64>,
    pub length_ms: Option<i64>,
    pub plays: i64,
    pub status: Option<String>,
    pub hash: Option<String>,
    /// Rhythia has a cover for it (`cover://o-<id>`).
    pub cover: bool,
    pub installed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPage {
    pub total: i64,
    pub maps: Vec<OnlineMap>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogQuery {
    pub page: u32,
    pub sort: String,
    /// Smallest first instead of largest (newest: oldest first).
    pub ascending: bool,
    pub text: String,
    pub tags: String,
    pub status: Option<String>,
    pub min_stars: Option<f64>,
    pub max_stars: Option<f64>,
}

/// A cover address from Rhythia: https on its file server, nothing else.
fn cover_url(p: Option<RawPreview>) -> Option<String> {
    let url = p?.small?;
    let ok = url.len() <= 400
        && url::Url::parse(&url).ok().as_ref().and_then(Host::of) == Some(Host::RhythiaFiles);
    ok.then_some(url)
}

fn online_map(m: RawMap, seen: &mut Vec<(i64, SeenMap)>) -> Option<OnlineMap> {
    let id = whole(m.id?).filter(|n| *n > 0)?;
    let title = clip(&m.title.unwrap_or_default(), 300);
    let mapper = clip(&m.owner_username.unwrap_or_default(), 100);
    let cover = cover_url(m.preview_image);
    let has_cover = cover.is_some();
    seen.push((id, SeenMap { title: title.clone(), cover, file: m.beatmap_file.filter(|f| map_file_url(f)) }));
    let tags = m.tags.unwrap_or_default();
    let tags = tags.split_whitespace().filter(|t| t.chars().count() <= 30).take(12).map(str::to_string).collect();
    Some(OnlineMap {
        id,
        tags,
        title,
        mapper,
        stars: m.star_rating.filter(|s| s.is_finite() && (0.0..=100.0).contains(s)),
        length_ms: m.length.and_then(whole).filter(|n| *n >= 0),
        plays: m.playcount.and_then(whole).unwrap_or(0).max(0),
        status: match m.status.filter(|s| STATUSES.contains(&s.as_str())) {
            Some(s) if s == "UNRANKED" && m.qualified == Some(true) => Some("QUALIFIED".to_string()),
            s => s,
        },
        hash: m.map_hash.map(|h| h.to_ascii_lowercase()).filter(|h| is_hash(h)),
        cover: has_cover,
        installed: false,
    })
}

fn online_maps(raw: Vec<RawMap>, limit: usize, seen: &Seen) -> Vec<OnlineMap> {
    let mut found = Vec::new();
    let maps = raw.into_iter().take(limit).filter_map(|m| online_map(m, &mut found)).collect();
    seen.remember(&found);
    maps
}

pub async fn beatmaps(net: &Net, seen: &Seen, q: &CatalogQuery) -> Result<CatalogPage, String> {
    if !SORTS.contains(&q.sort.as_str()) {
        return Err("Unknown sort.".into());
    }
    let status = q.status.as_deref().filter(|s| !s.is_empty());
    if status.is_some_and(|s| !STATUSES.contains(&s)) {
        return Err("Unknown status.".into());
    }
    let stars = |v: Option<f64>| v.filter(|n| n.is_finite() && (0.0..=20.0).contains(n));
    let text = clip(&q.text, 100);
    let tags = clip(&q.tags, 60);
    let query = BeatmapsQuery {
        session: "",
        page: q.page.clamp(1, 10_000),
        sort: &q.sort,
        sort_direction: if q.ascending { "asc" } else { "desc" },
        text_filter: (!text.is_empty()).then_some(text.as_str()),
        tags_filter: (!tags.is_empty()).then_some(tags.as_str()),
        status,
        min_stars: stars(q.min_stars),
        max_stars: stars(q.max_stars),
    };
    let answer: BeatmapsAnswer = net.post_json(Host::RhythiaApi, "/api/getBeatmaps", &query, &[]).await?;
    Ok(CatalogPage {
        total: answer.total.and_then(whole).unwrap_or(0).max(0),
        maps: online_maps(answer.beatmaps, 100, seen),
    })
}

// ---------- collections ----------

#[derive(Serialize)]
struct CollectionsQuery<'a> {
    session: &'static str,
    page: u32,
    #[serde(rename = "itemsPerPage")]
    items_per_page: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    search: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<i64>,
    /// Only collections with at least this many maps (Rhythia's own filter; 0 sends nothing).
    #[serde(rename = "minBeatmaps", skip_serializing_if = "Option::is_none")]
    min_beatmaps: Option<u32>,
}

#[derive(Deserialize)]
struct CollectionsAnswer {
    #[serde(default)]
    collections: Vec<RawListed>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawListed {
    id: Option<f64>,
    title: Option<String>,
    owner: Option<f64>,
    owner_username: Option<String>,
    beatmap_count: Option<f64>,
    created_at: Option<String>,
    description: Option<String>,
    #[serde(default)]
    star_rating_distribution: Vec<RawStars>,
}

#[derive(Deserialize)]
struct RawStars {
    stars: Option<f64>,
    count: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedCollection {
    pub id: i64,
    pub title: String,
    pub owner: Option<i64>,
    pub owner_name: String,
    pub maps: i64,
    pub created: Option<String>,
    pub description: String,
    /// How many of its maps are in each whole-star band, from 1 star (index 0) to 18 and up.
    pub stars: Vec<i64>,
}

/// Rhythia's star bands as 18 counts (1 to 18 stars); anything odd is left at 0.
fn star_bands(raw: Vec<RawStars>) -> Vec<i64> {
    let mut out = vec![0; 18];
    for b in raw.into_iter().take(40) {
        let (Some(s), Some(n)) = (b.stars.and_then(whole), b.count.and_then(whole)) else { continue };
        if (1..=18).contains(&s) {
            out[s as usize - 1] = n.clamp(0, 1_000_000);
        }
    }
    out
}

/// One owner's collections come on one page of up to this many (fewer after a change: `Net::fresh_size`).
pub const OWN_COLLECTIONS: u32 = 100;


/// One page of public collections: newest first, by search text, or by one owner (all on one page);
/// `min_maps` leaves out smaller ones (Rhythia filters).
pub async fn collections(net: &Net, page: u32, search: &str, owner: Option<i64>, min_maps: u32) -> Result<Vec<ListedCollection>, String> {
    let search = clip(search, 100);
    let owner = owner.filter(|n| *n > 0);
    let size = if owner.is_some() { net.fresh_size(OWN_COLLECTIONS) } else { COLLECTIONS_PER_PAGE };
    let query = CollectionsQuery {
        session: "",
        page: if owner.is_some() { 1 } else { page.clamp(1, 1000) },
        items_per_page: size,
        search: (!search.is_empty()).then_some(search.as_str()),
        owner,
        min_beatmaps: (min_maps > 0).then_some(min_maps.min(10_000)),
    };
    let answer: CollectionsAnswer = net.post_json(Host::RhythiaApi, "/api/getCollections", &query, &[]).await?;
    Ok(answer
        .collections
        .into_iter()
        .take(size as usize)
        .filter_map(|c| {
            Some(ListedCollection {
                id: whole(c.id?).filter(|n| *n > 0)?,
                title: clip(&c.title.unwrap_or_default(), 200),
                owner: c.owner.and_then(whole),
                owner_name: clip(&c.owner_username.unwrap_or_default(), 100),
                maps: c.beatmap_count.and_then(whole).unwrap_or(0).max(0),
                created: c.created_at.map(|t| clip(&t, 40)),
                description: clip(&c.description.unwrap_or_default(), 300),
                stars: star_bands(c.star_rating_distribution),
            })
        })
        .collect())
}

#[derive(Serialize)]
struct CollectionQuery {
    session: &'static str,
    collection: i64,
    page: u32,
    #[serde(rename = "itemsPerPage")]
    items_per_page: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionAnswer {
    total_pages: Option<f64>,
    collection: Option<RawCollection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCollection {
    title: Option<String>,
    description: Option<String>,
    is_list: Option<bool>,
    owner: Option<RawOwner>,
    #[serde(default)]
    beatmaps: Vec<RawMap>,
}

#[derive(Deserialize)]
struct RawOwner {
    username: Option<String>,
    id: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineCollection {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub owner: Option<i64>,
    pub owner_name: String,
    /// Shown as a ranked list on Rhythia instead of a grid.
    pub is_list: bool,
    pub maps: Vec<OnlineMap>,
}

/// Every map of a public collection, page by page (100 maps a page); with `first_page`, only the
/// first 100 (a card's covers and types).
pub async fn collection(net: &Net, seen: &Seen, id: i64, first_page: bool) -> Result<OnlineCollection, String> {
    let mut out = OnlineCollection { id, title: String::new(), description: String::new(), owner: None, owner_name: String::new(), is_list: false, maps: Vec::new() };
    let (mut pages, mut page) = (1u32, 1u32);
    while page <= pages {
        let query = CollectionQuery { session: "", collection: id, page, items_per_page: net.fresh_size(100) };
        let answer: CollectionAnswer = net.post_json(Host::RhythiaApi, "/api/getCollection", &query, &[]).await?;
        let found = answer.collection.ok_or("Rhythia has no such collection.")?;
        if page == 1 {
            out.title = clip(&found.title.unwrap_or_default(), 200);
            out.description = clip(&found.description.unwrap_or_default(), 2000);
            out.is_list = found.is_list.unwrap_or(false);
            if let Some(o) = found.owner {
                out.owner = o.id.and_then(whole);
                out.owner_name = clip(&o.username.unwrap_or_default(), 100);
            }
            pages = answer.total_pages.and_then(whole).unwrap_or(1).clamp(1, MAX_COLLECTION_PAGES as i64) as u32;
            if first_page {
                pages = 1;
            }
        }
        out.maps.extend(online_maps(found.beatmaps, 100, seen));
        page += 1;
    }
    let mut ids = std::collections::HashSet::new();
    out.maps.retain(|m| ids.insert(m.id));
    Ok(out)
}

// ---------- a map's leaderboard ----------

#[derive(Serialize)]
struct BoardQuery {
    session: &'static str,
    id: i64,
    mode: &'static str,
    limit: u32,
    /// Rhythia adds this player's best with its place (public, no sign-in needed).
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<i64>,
    /// Only players from this country (two letters).
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoardAnswer {
    #[serde(default)]
    scores: Vec<RawScore>,
    user_score: Option<RawScore>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawScore {
    /// Rhythia writes these two in snake case, the rest in camel case.
    #[serde(rename = "awarded_sp")]
    awarded_sp: Option<f64>,
    accuracy: Option<f64>,
    misses: Option<f64>,
    speed: Option<f64>,
    mods: Option<Vec<String>>,
    #[serde(rename = "created_at")]
    created_at: Option<String>,
    user_id: Option<f64>,
    username: Option<String>,
    flag: Option<String>,
    place: Option<f64>,
    total_scores: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardScore {
    pub place: i64,
    pub user_id: Option<i64>,
    pub username: String,
    pub flag: Option<String>,
    pub rp: Option<f64>,
    pub accuracy: Option<f64>,
    pub misses: Option<i64>,
    pub speed: Option<f64>,
    pub mods: Vec<String>,
    pub at: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Board {
    pub scores: Vec<BoardScore>,
    /// The player's own best on Rhythia, with its place.
    pub mine: Option<BoardScore>,
    /// How many players have a score there.
    pub total: Option<i64>,
}

fn board_score(s: RawScore, place: i64) -> BoardScore {
    BoardScore {
        place: s.place.and_then(whole).filter(|n| *n > 0).unwrap_or(place),
        user_id: s.user_id.and_then(whole).filter(|n| *n > 0),
        username: clip(&s.username.unwrap_or_default(), 60),
        flag: s.flag.filter(|f| f.len() == 2 && f.bytes().all(|b| b.is_ascii_alphabetic())).map(|f| f.to_ascii_uppercase()),
        rp: s.awarded_sp.filter(|n| n.is_finite() && (0.0..=100_000.0).contains(n)),
        accuracy: s.accuracy.filter(|a| a.is_finite() && (0.0..=100.0).contains(a)),
        misses: s.misses.and_then(whole).filter(|n| *n >= 0),
        speed: s.speed.filter(|n| n.is_finite() && (0.1..=10.0).contains(n)),
        mods: s.mods.unwrap_or_default().into_iter().take(20).map(|m| clip(&m, 40)).collect(),
        at: s.created_at.map(|t| clip(&t, 40)),
    }
}

/// A country as Rhythia names it: two letters, upper case.
pub fn country(code: &str) -> Option<String> {
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_alphabetic())).then(|| code.to_ascii_uppercase())
}

/// The top 50 of a map on Rhythia (normal mode; Rhythia shows no more), of everyone or of one country,
/// and, for `player` (only with everyone), their best there.
pub async fn board(net: &Net, id: i64, player: Option<i64>, country: Option<String>) -> Result<Board, String> {
    let user_id = player.filter(|n| *n > 0 && country.is_none());
    let query = BoardQuery { session: "", id, mode: "normal", limit: 50, user_id, country };
    let answer: BoardAnswer = net.post_json(Host::RhythiaApi, "/api/getBeatmapPage", &query, &[]).await?;
    let total = answer.user_score.as_ref().and_then(|s| s.total_scores).and_then(whole);
    Ok(Board {
        scores: answer.scores.into_iter().take(50).enumerate().map(|(i, s)| board_score(s, i as i64 + 1)).collect(),
        mine: answer.user_score.map(|s| board_score(s, 0)).filter(|s| s.place > 0),
        total,
    })
}

#[derive(Serialize)]
struct PlayersQuery {
    session: &'static str,
    page: i64,
    spin: bool,
    vr: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    flag: Option<String>,
}

#[derive(Deserialize)]
struct PlayersAnswer {
    total: Option<f64>,
    #[serde(default)]
    leaderboard: Vec<RawPlayer>,
}

#[derive(Deserialize)]
struct RawPlayer {
    id: Option<f64>,
    username: Option<String>,
    flag: Option<String>,
    position: Option<f64>,
    skill_points: Option<f64>,
    play_count: Option<f64>,
    rank_change: Option<f64>,
    clans: Option<RawClan>,
}

#[derive(Deserialize)]
struct RawClan {
    acronym: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RankedPlayer {
    /// The place in this list (in the country, with a country).
    pub place: i64,
    /// The place among everyone.
    pub position: Option<i64>,
    pub id: i64,
    pub name: String,
    pub flag: Option<String>,
    pub clan: Option<String>,
    pub rp: Option<f64>,
    pub plays: Option<i64>,
    /// Places won (more than 0) or lost lately, as Rhythia counts it.
    pub change: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayersPage {
    pub total: i64,
    pub page: i64,
    pub pages: i64,
    pub players: Vec<RankedPlayer>,
}

const PLAYERS_PER_PAGE: i64 = 50;

/// One page (50 players) of Rhythia's ranking of active players, of everyone or of one country.
pub async fn players(net: &Net, page: i64, flag: Option<String>) -> Result<PlayersPage, String> {
    if !(1..=100_000).contains(&page) {
        return Err("No such page.".into());
    }
    let answer: PlayersAnswer = net.post_json(Host::RhythiaApi, "/api/getLeaderboard", &PlayersQuery { session: "", page, spin: false, vr: false, flag }, &[]).await?;
    let total = answer.total.and_then(whole).filter(|n| *n >= 0).unwrap_or(0);
    let first = (page - 1) * PLAYERS_PER_PAGE;
    let players = answer
        .leaderboard
        .into_iter()
        .take(PLAYERS_PER_PAGE as usize)
        .enumerate()
        .filter_map(|(i, p)| {
            Some(RankedPlayer {
                place: first + i as i64 + 1,
                position: p.position.and_then(whole).filter(|n| *n > 0),
                id: p.id.and_then(whole).filter(|n| *n > 0)?,
                name: clip(&p.username.unwrap_or_default(), 60),
                flag: p.flag.as_deref().and_then(country),
                clan: p.clans.and_then(|c| c.acronym).map(|a| clip(&a, 12)).filter(|a| !a.is_empty()),
                rp: p.skill_points.filter(|n| n.is_finite() && (0.0..=10_000_000.0).contains(n)),
                plays: p.play_count.and_then(whole).filter(|n| *n >= 0),
                change: p.rank_change.and_then(whole).filter(|n| n.abs() < 10_000_000),
            })
        })
        .collect();
    Ok(PlayersPage { total, page, pages: ((total + PLAYERS_PER_PAGE - 1) / PLAYERS_PER_PAGE).max(1), players })
}

#[derive(Serialize)]
struct ProfileQuery {
    session: &'static str,
    id: i64,
}

#[derive(Deserialize)]
struct PublicProfile {
    user: Option<RawRank>,
}

#[derive(Deserialize)]
struct RawRank {
    username: Option<String>,
    flag: Option<String>,
    position: Option<f64>,
    country_position: Option<f64>,
}

/// A player's places on Rhythia (public profile).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Rank {
    pub id: i64,
    pub name: String,
    pub flag: Option<String>,
    pub position: Option<i64>,
    pub country_position: Option<i64>,
}

pub async fn rank(net: &Net, id: i64) -> Result<Rank, String> {
    let answer: PublicProfile = net.post_json(Host::RhythiaApi, "/api/getProfile", &ProfileQuery { session: "", id }, &[]).await?;
    let u = answer.user.ok_or("Rhythia has no such player.")?;
    Ok(Rank {
        id,
        name: clip(&u.username.unwrap_or_default(), 60),
        flag: u.flag.as_deref().and_then(country),
        position: u.position.and_then(whole).filter(|n| *n > 0),
        country_position: u.country_position.and_then(whole).filter(|n| *n > 0),
    })
}

/// A JSON number that is a whole, safe integer.
fn whole(n: f64) -> Option<i64> {
    (n.is_finite() && n.fract() == 0.0 && n.abs() < 9e15).then_some(n as i64)
}

// ---- With the player's Rhythia session (account.rs): only these calls carry it, only to Rhythia's API. ----

#[derive(Serialize)]
struct SessionOnly<'a> {
    session: &'a str,
}

#[derive(Deserialize)]
struct ProfileAnswer {
    user: Option<RawOwner>,
}

/// The signed-in player: `getProfile` with only a session answers with their own profile.
pub async fn me(net: &Net, session: &str) -> Result<crate::account::Player, String> {
    let answer: ProfileAnswer = net.post_json(Host::RhythiaApi, "/api/getProfile", &SessionOnly { session }, &[]).await?;
    let user = answer.user.ok_or("Rhythia did not accept that sign-in.")?;
    let id = user.id.and_then(whole).filter(|n| *n > 0).ok_or("Rhythia did not accept that sign-in.")?;
    Ok(crate::account::Player { id, name: clip(&user.username.unwrap_or_default(), 60) })
}

#[derive(Serialize)]
struct CreateQuery<'a> {
    session: &'a str,
    title: &'a str,
}

#[derive(Deserialize)]
struct CreatedAnswer {
    id: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionMapQuery<'a> {
    session: &'a str,
    collection: i64,
    beatmap_page: i64,
}

/// A new collection on the player's Rhythia profile; its number.
pub async fn create_collection(net: &Net, session: &str, title: &str) -> Result<i64, String> {
    let title = clip(title.trim(), 60);
    if title.is_empty() {
        return Err("Give the collection a name.".into());
    }
    let answer: CreatedAnswer = net.post_json(Host::RhythiaApi, "/api/createCollection", &CreateQuery { session, title: &title }, &[]).await?;
    answer.id.and_then(whole).filter(|n| *n > 0).ok_or_else(|| "Rhythia did not create the collection.".into())
}

/// Adds a map (its page number) to one of the player's Rhythia collections, or removes it. Rhythia says nothing when
/// the map did not go in, so callers read the collection again (lib.rs `rhythia_collection_maps`).
pub async fn collection_map(net: &Net, session: &str, collection: i64, map: i64, add: bool) -> Result<(), String> {
    if collection <= 0 || map <= 0 {
        return Err("Unknown collection or map.".into());
    }
    let path = if add { "/api/addCollectionMap" } else { "/api/deleteCollectionMap" };
    let _: serde_json::Value = net.post_json(Host::RhythiaApi, path, &CollectionMapQuery { session, collection, beatmap_page: map }, &[]).await?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EditQuery<'a> {
    session: &'a str,
    collection: i64,
    title: &'a str,
    description: &'a str,
    is_list: bool,
}

/// A collection's name, description and layout (grid or ranked list), on the player's profile.
pub async fn edit_collection(net: &Net, session: &str, collection: i64, title: &str, description: &str, is_list: bool) -> Result<(), String> {
    let title = clip(title.trim(), 60);
    let description = clip(description.trim(), 500);
    if collection <= 0 {
        return Err("Unknown collection.".into());
    }
    if title.is_empty() {
        return Err("Give the collection a name.".into());
    }
    let query = EditQuery { session, collection, title: &title, description: &description, is_list };
    let _: serde_json::Value = net.post_json(Host::RhythiaApi, "/api/editCollection", &query, &[]).await?;
    Ok(())
}

#[derive(Serialize)]
struct DeleteQuery<'a> {
    session: &'a str,
    collection: i64,
}

/// Removes one of the player's collections from Rhythia.
pub async fn delete_collection(net: &Net, session: &str, collection: i64) -> Result<(), String> {
    if collection <= 0 {
        return Err("Unknown collection.".into());
    }
    let _: serde_json::Value = net.post_json(Host::RhythiaApi, "/api/deleteCollection", &DeleteQuery { session, collection }, &[]).await?;
    Ok(())
}

/// A map file address Atlas downloads: Rhythia's file server, a `.rhm` or `.sspm` file.
pub fn map_file_url(url: &str) -> bool {
    url.len() <= 600 && url.starts_with("https://static.rhythia.com/") && (url.ends_with(".rhm") || url.ends_with(".sspm"))
}

#[derive(Serialize)]
struct PageQuery {
    session: &'static str,
    id: i64,
}

#[derive(Deserialize)]
struct PageAnswer {
    beatmap: Option<RawMap>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SongQuery<'a> {
    session: &'static str,
    map_id: &'a str,
}

/// A map by its song key (`songId`, the game's `Maps.LegacyId`), for maps known only from players' scores.
pub async fn map_by_song(net: &Net, seen: &Seen, song: &str) -> Result<Option<OnlineMap>, String> {
    let song = clip(song, 200);
    if song.is_empty() {
        return Ok(None);
    }
    let answer: PageAnswer = net.post_json(Host::RhythiaApi, "/api/getBeatmapPageById", &SongQuery { session: "", map_id: &song }, &[]).await?;
    Ok(online_maps(answer.beatmap.into_iter().collect(), 1, seen).pop())
}

/// Where a map's file is and its title: from the maps Atlas has seen, else from the map's page.
pub async fn map_file(net: &Net, seen: &Seen, id: i64) -> Result<(String, String), String> {
    if let Some(m) = seen.get(id) {
        if let Some(file) = m.file {
            return Ok((file, m.title));
        }
    }
    let answer: PageAnswer = net.post_json(Host::RhythiaApi, "/api/getBeatmapPage", &PageQuery { session: "", id }, &[]).await?;
    let b = answer.beatmap.ok_or("Rhythia has no such map.")?;
    let file = b.beatmap_file.filter(|f| map_file_url(f)).ok_or("Rhythia gives no file for this map.")?;
    Ok((file, clip(&b.title.unwrap_or_default(), 300)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_map_top_as_rhythia_sends_it() {
        let raw = r#"{"scores":[{"awarded_sp":602,"created_at":"2026-09-07T17:53:05.832165","accuracy":99.03,"misses":16,
            "speed":1.35,"mods":[],"userId":7,"username":"tony","flag":"us"}]}"#;
        let a: BoardAnswer = serde_json::from_str(raw).unwrap();
        let s = board_score(a.scores.into_iter().next().unwrap(), 1);
        assert_eq!((s.rp, s.at.as_deref(), s.user_id, s.flag.as_deref()), (Some(602.0), Some("2026-09-07T17:53:05.832165"), Some(7), Some("US")));
        assert_eq!(country("pl").as_deref(), Some("PL"));
        assert_eq!(country("POL"), None);
    }
}
