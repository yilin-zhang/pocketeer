//! Standard (No-Intro style) game names without a DAT.
//!
//! libretro-thumbnails names every cover after the No-Intro entry, so the
//! file list of a system's thumbnail repository doubles as a list of standard
//! names. [`standard_names`] fetches that list (cached, revalidated weekly)
//! and [`suggest`] maps a loosely named file — GoodTools tags like `(U) [!]`,
//! release-group tags, dropped punctuation — onto the entry it most likely is.
//! It is a name match, not a content check: a DAT is still the only proof.

use std::{collections::BTreeSet, time::Duration};

use serde::Deserialize;

use crate::{
    Error, Result,
    net::{Cache, Http, Policy},
    roms::rename::stem,
};

/// How long a cached name list is trusted before a conditional re-fetch.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Deserialize)]
struct Tree {
    tree: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    path: String,
}

/// GitHub API URL of a system's thumbnail repository tree.
pub fn tree_url(system: &str) -> String {
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
    let url = tree_url(system);
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
pub fn for_platform(
    http: &Http,
    cache: &Cache,
    platform: &str,
    policy: Policy,
) -> Option<Vec<String>> {
    let system = super::covers::system_for(platform)?;
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

/// Library games whose file names aren't standard but match one: the rename
/// each would get. `standard` is looked up per platform via `names`.
pub fn standardize_plan(
    lib: &super::Library,
    pocket: Option<&crate::Pocket>,
    platform: Option<&str>,
    mut names: impl FnMut(&str) -> Option<Vec<String>>,
) -> Result<Vec<super::rename::RenamePlan>> {
    let games = super::db::list_games(lib.conn(), platform, None)?;
    let mut lists: std::collections::HashMap<String, Option<Vec<String>>> = Default::default();
    let mut out = Vec::new();
    for g in &games {
        let list = lists
            .entry(g.platform.clone())
            .or_insert_with(|| names(&g.platform));
        let Some(list) = list else {
            continue;
        };
        for rom in &g.roms {
            if rom.verified {
                continue;
            }
            let Some(s) = suggest(&rom.name, list) else {
                continue;
            };
            let new_name = format!("{s}{}", crate::roms::rename::extension(&rom.name));
            if let Ok(plan) = super::rename::plan(
                lib,
                pocket,
                Some(&g.platform),
                &rom.id.to_string(),
                &new_name,
            ) {
                out.push(plan);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tree_url_uses_underscored_repository_names() {
        assert_eq!(
            tree_url("Nintendo - Game Boy Advance"),
            "https://api.github.com/repos/libretro-thumbnails/Nintendo_-_Game_Boy_Advance/git/trees/master?recursive=1"
        );
    }
}
