//! libretro's two GitHub data sources: the No-Intro DAT files in
//! libretro-database (what a ROM *is*) and the libretro-thumbnails
//! repositories (box art, and the list of standard game names their file
//! names amount to). Nothing here touches a library or a card.

use std::{collections::BTreeSet, path::Path, time::Duration};

use serde::Deserialize;
use sha1::{Digest, Sha1};

use crate::{
    Error, Result,
    catalog::Dat,
    fsutil::names::stem,
    net::{Cache, Http, Policy, Source},
};

/// How long a cached catalog download is trusted before a conditional re-fetch.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

// ---------------------------------------------------------------------------
// DAT files

pub const DAT_BASE: &str =
    "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro";

pub fn dat_url(base: &str, system: &str) -> String {
    format!(
        "{}/{}.dat",
        base.trim_end_matches('/'),
        system.replace(' ', "%20")
    )
}

pub fn dat_cache_key(system: &str) -> String {
    format!("dat-{:x}.dat", Sha1::digest(system.as_bytes()))
}

/// A DAT as fetched: parsed, with the digest of its bytes as its revision
/// (a negative match is only valid for one revision) and where it came from.
pub struct LoadedDat {
    pub dat: Dat,
    pub revision: String,
    pub source: Source,
}

/// Fetches (or reuses the cached copy of) the No-Intro DAT for `system`. A
/// DAT without size + SHA-1 entries is an error, not evidence of no match.
pub fn load_dat(
    http: &Http,
    cache: &Cache,
    base: &str,
    system: &str,
    policy: Policy,
) -> Result<LoadedDat> {
    let url = dat_url(base, system);
    let fetched = cache.fetch(http, &dat_cache_key(system), &url, "ROM DAT", policy)?;
    let dat = Dat::parse(&fetched.bytes, Path::new(&url))?;
    if !dat
        .games
        .iter()
        .flat_map(|g| &g.roms)
        .any(|r| r.size.is_some() && r.sha1.is_some())
    {
        return Err(Error::DatParse {
            path: url.into(),
            message: "DAT has no size + SHA-1 entries".into(),
        });
    }
    Ok(LoadedDat {
        dat,
        revision: format!("{:x}", Sha1::digest(&fetched.bytes)),
        source: fetched.source,
    })
}

// ---------------------------------------------------------------------------
// Thumbnails

pub const THUMBNAILS_BASE: &str = "https://raw.githubusercontent.com/libretro-thumbnails";

/// libretro replaces characters that are awkward in file names with `_`.
pub fn thumbnail_name(game: &str) -> String {
    game.chars()
        .map(|c| match c {
            '&' | '*' | '/' | ':' | '`' | '<' | '>' | '?' | '\\' | '|' | '"' => '_',
            c => c,
        })
        .collect::<String>()
        + ".png"
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `https://raw.githubusercontent.com/libretro-thumbnails/<repo>/master/Named_Boxarts/<name>.png`.
/// The GitHub repositories are named with underscores (`Nintendo_-_Game_Boy_Advance`),
/// unlike the system names libretro shows elsewhere.
pub fn thumbnail_url(base: &str, system: &str, game: &str) -> String {
    format!(
        "{}/{}/master/Named_Boxarts/{}",
        base.trim_end_matches('/'),
        encode(&system.replace(' ', "_")),
        encode(&thumbnail_name(game))
    )
}

// ---------------------------------------------------------------------------
// Standard names. libretro-thumbnails names every cover after the No-Intro
// entry, so a repository's file list doubles as a list of standard names;
// `suggest` maps a loosely named file (GoodTools tags like `(U) [!]`,
// release-group tags, dropped punctuation) onto the entry it most likely is.
// It is a name match, not a content check: a DAT is still the only proof.

#[derive(Deserialize)]
struct Tree {
    tree: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    path: String,
}

/// GitHub API URL of a system's thumbnail repository tree.
pub fn names_tree_url(system: &str) -> String {
    format!(
        "https://api.github.com/repos/libretro-thumbnails/{}/git/trees/master?recursive=1",
        system.replace(' ', "_")
    )
}

/// Every game name libretro has box art for on `system` (e.g.
/// `"Nintendo - Game Boy Advance"`), sorted. Offline, the cached list is used
/// if there is one.
pub fn standard_names(
    http: &Http,
    cache: &Cache,
    system: &str,
    policy: Policy,
) -> Result<Vec<String>> {
    let url = names_tree_url(system);
    let policy = match policy {
        Policy::Offline => Policy::Offline,
        Policy::Refresh => Policy::Refresh,
        Policy::MaxAge(_) => Policy::MaxAge(MAX_AGE),
    };
    let key = format!("names-{}.json", system.replace(' ', "_"));
    let fetched = cache.fetch(http, &key, &url, "standard name list", policy)?;
    let tree: Tree =
        serde_json::from_slice(&fetched.bytes).map_err(|source| Error::ParseResponse {
            url: url.clone(),
            source,
        })?;
    let mut names: Vec<String> = tree
        .tree
        .into_iter()
        .filter_map(|e| {
            e.path
                .strip_prefix("Named_Boxarts/")
                .and_then(|n| n.strip_suffix(".png"))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Standard names for a Pocket platform id, through the libretro system
/// mapping; `None` when the platform has no mapping or the list can't be had
/// (offline with nothing cached). Never fatal: suggestions are a nicety.
pub fn standard_names_for_platform(
    http: &Http,
    cache: &Cache,
    platform: &str,
    policy: Policy,
) -> Option<Vec<String>> {
    let system = super::thumbnail_system(platform)?;
    standard_names(http, cache, system, policy).ok()
}

/// One parsed name: the title and its parenthesised tags.
struct Parsed {
    /// Lower-case alphanumerics of the title only.
    title_key: String,
    /// Tags as written, `(…)` only (`[…]` are dump flags and dropped).
    tags: Vec<String>,
}

fn parse(name: &str) -> Parsed {
    let name = stem(name);
    let title_end = name.find(['(', '[']).unwrap_or(name.len());
    let title_key = key(&name[..title_end]);
    let mut tags = Vec::new();
    let mut rest = &name[title_end..];
    while let Some(start) = rest.find('(') {
        let Some(len) = rest[start..].find(')') else {
            break;
        };
        tags.push(rest[start + 1..start + len].trim().to_string());
        rest = &rest[start + len + 1..];
    }
    Parsed { title_key, tags }
}

/// Lower-case alphanumerics: `"Super Mario Bros. 3"` and `"super mario bros 3"` agree.
fn key(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// GoodTools / old-style region letters → No-Intro region words.
fn regions(tag: &str) -> Option<BTreeSet<&'static str>> {
    let set = |xs: &[&'static str]| Some(xs.iter().copied().collect());
    match tag {
        "U" | "USA" => set(&["USA"]),
        "E" | "Europe" => set(&["Europe"]),
        "J" | "Japan" => set(&["Japan"]),
        "F" | "France" => set(&["France"]),
        "G" | "Germany" => set(&["Germany"]),
        "S" | "Spain" => set(&["Spain"]),
        "I" | "Italy" => set(&["Italy"]),
        "A" | "Australia" => set(&["Australia"]),
        "K" | "Korea" => set(&["Korea"]),
        "C" | "China" => set(&["China"]),
        "W" | "World" => set(&["World"]),
        "UE" | "EU" | "USA, Europe" => set(&["USA", "Europe"]),
        "JU" | "UJ" | "Japan, USA" => set(&["Japan", "USA"]),
        "JE" | "EJ" | "Europe, Japan" => set(&["Europe", "Japan"]),
        "USA, Australia" => set(&["USA", "Australia"]),
        _ => None,
    }
}

/// Tags that mark a variant most people don't mean when they type a plain name.
const VARIANT_TAGS: [&str; 8] = [
    "Rev ",
    "Beta",
    "Proto",
    "Demo",
    "Sample",
    "Promo",
    "Virtual Console",
    "Switch Online",
];

/// The standard name `name` most likely refers to, or `None` when nothing in
/// `standard` has the same title or `name` already is one of them.
pub fn suggest(name: &str, standard: &[String]) -> Option<String> {
    let input = parse(name);
    if input.title_key.is_empty() {
        return None;
    }
    let stem_of = stem(name);
    if standard.iter().any(|s| s == stem_of) {
        return None;
    }
    let wanted: BTreeSet<&str> = input
        .tags
        .iter()
        .filter_map(|t| regions(t))
        .flatten()
        .collect();

    let mut best: Option<(i32, usize, &String)> = None;
    for cand in standard {
        let p = parse(cand);
        if p.title_key != input.title_key {
            continue;
        }
        let cand_regions: BTreeSet<&str> =
            p.tags.iter().filter_map(|t| regions(t)).flatten().collect();
        let mut score = 0;
        if !wanted.is_empty() {
            if cand_regions == wanted {
                score += 20;
            } else if wanted.iter().all(|r| cand_regions.contains(r)) {
                score += 10;
            } else {
                score -= 20;
            }
        }
        for t in &p.tags {
            if VARIANT_TAGS.iter().any(|v| t.starts_with(v)) {
                score -= 5;
            }
        }
        let len = cand.len();
        let better = match best {
            None => true,
            Some((s, l, _)) => score > s || (score == s && len < l),
        };
        if better {
            best = Some((score, len, cand));
        }
    }
    best.filter(|(score, _, _)| *score >= 0)
        .map(|(_, _, c)| c.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_and_urls_follow_libretro_conventions() {
        assert_eq!(thumbnail_name("Tom & Jerry (USA)"), "Tom _ Jerry (USA).png");
        assert_eq!(
            thumbnail_name("Legend of Zelda, The (USA)"),
            "Legend of Zelda, The (USA).png"
        );
        assert_eq!(
            thumbnail_url(
                THUMBNAILS_BASE,
                "Nintendo - Game Boy Advance",
                "Metroid Fusion (USA)"
            ),
            "https://raw.githubusercontent.com/libretro-thumbnails/Nintendo_-_Game_Boy_Advance/master/Named_Boxarts/Metroid%20Fusion%20%28USA%29.png"
        );
        assert_eq!(
            super::super::thumbnail_system("GBA"),
            Some("Nintendo - Game Boy Advance")
        );
        assert_eq!(super::super::thumbnail_system("nope"), None);
    }

    fn list(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn maps_goodtools_names_onto_no_intro_ones() {
        let std = list(&[
            "Harvest Moon - Friends of Mineral Town (Europe)",
            "Harvest Moon - Friends of Mineral Town (USA)",
            "Harvest Moon - More Friends of Mineral Town (USA)",
            "Super Mario Advance 4 - Super Mario Bros. 3 (Europe) (En,Fr,De,Es,It)",
            "Super Mario Advance 4 - Super Mario Bros. 3 (Europe) (En,Fr,De,Es,It) (Rev 1)",
            "Super Mario Advance 4 - Super Mario Bros. 3 (USA)",
            "Super Mario Advance 4 - Super Mario Bros. 3 (USA, Australia) (Rev 1)",
            "Metroid Fusion (USA)",
            "Metroid Fusion (Japan) (Virtual Console)",
        ]);
        assert_eq!(
            suggest("Harvest Moon - Friends of Mineral Town (U) [!].gba", &std).as_deref(),
            Some("Harvest Moon - Friends of Mineral Town (USA)")
        );
        assert_eq!(
            suggest(
                "Super Mario Advance 4 - Super Mario Bros 3 (E)(Menace).gba",
                &std
            )
            .as_deref(),
            Some("Super Mario Advance 4 - Super Mario Bros. 3 (Europe) (En,Fr,De,Es,It)")
        );
        // No region given: plain release beats Virtual Console.
        assert_eq!(
            suggest("metroid fusion.gba", &std).as_deref(),
            Some("Metroid Fusion (USA)")
        );
        // Already standard → nothing to suggest.
        assert_eq!(suggest("Metroid Fusion (USA).gba", &std), None);
        // Unknown title → nothing.
        assert_eq!(suggest("2021MoonEscape.gb", &std), None);
        // Region that doesn't exist for the title → nothing rather than a wrong region.
        assert_eq!(suggest("Metroid Fusion (G).gba", &std), None);
    }

    #[test]
    fn names_tree_url_uses_underscored_repository_names() {
        assert_eq!(
            names_tree_url("Nintendo - Game Boy Advance"),
            "https://api.github.com/repos/libretro-thumbnails/Nintendo_-_Game_Boy_Advance/git/trees/master?recursive=1"
        );
    }
}
