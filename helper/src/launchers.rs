// What the launcher that started a game knows about it: Steam's app info, Heroic's copy of
// GOG's game database (game modes of Epic, GOG and Amazon games) and Lutris's game name; plus
// the anti-cheat AreWeAntiCheatYet lists for it (bundled, and Heroic's newer copy if there is one).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::json::{self, Value};
use crate::steam;

#[derive(Clone, Default)]
pub struct About {
    pub name: Option<String>,
    /// The anti-cheat AreWeAntiCheatYet lists for it: only a warning, it may never run.
    pub listed: Option<String>,
    /// Steam lists Valve Anti-Cheat: it can't be seen running, so the game is refused.
    pub vac: bool,
    pub multiplayer: bool,
    pub online_only: bool,
    /// Its characters are flagged online after edits made offline (`FLAGS_EDITS`).
    pub flags_edits: bool,
}

/// GOG's game modes that mean playing with other people, and the ones that are never played alone.
const SHARED_MODES: &[&str] = &["multiplayer", "co-operative", "massively-multiplayer", "battle-royale"];
const ONLINE_MODES: &[&str] = &["massively-multiplayer", "battle-royale"];

/// AreWeAntiCheatYet's games.json (MIT), trimmed by data/areweanticheatyet/update.sh.
const BUNDLED: &str = include_str!("../../data/areweanticheatyet/games.json");

/// AreWeAntiCheatYet entries that are wrong (name, Steam app ID), skipped in its list and
/// Heroic's copy: Prey (2017, Steam 480490) is listed with PunkBuster, which only Prey (2006)
/// had, for its multiplayer.
const WRONG: &[(&str, &str)] = &[("Prey", "480490")];

/// Games whose characters made offline go online later and get flagged there for edited stats
/// or items (FromSoftware's, by Steam app ID): refused even when played offline.
const FLAGS_EDITS: &[(&str, u32)] = &[
    ("Dark Souls: Prepare to Die Edition", 211420),
    ("Dark Souls Remastered", 570940),
    ("Dark Souls II", 236430),
    ("Dark Souls II: Scholar of the First Sin", 335300),
    ("Dark Souls III", 374320),
    ("Elden Ring", 1245620),
    ("Elden Ring Nightreign", 2622380),
    ("Armored Core VI", 1888160),
];

/// The games list asks every second; a game's launcher info doesn't change while it runs.
static CACHE: Mutex<Option<HashMap<String, About>>> = Mutex::new(None);

/// `steam_id` = the Steam app ID the game was started with.
pub fn about(env: &HashMap<String, String>, steam_id: Option<u32>) -> About {
    let heroic = (env.get("HEROIC_APP_RUNNER"), env.get("HEROIC_APP_NAME"));
    let key = match (steam_id, heroic, env.get("GAME_NAME")) {
        (Some(id), ..) => format!("steam {id}"),
        (None, (Some(runner), Some(app)), _) => format!("heroic {runner} {app}"),
        (None, _, Some(name)) => format!("lutris {name}"),
        _ => return About::default(),
    };
    if let Some(a) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return a.clone();
    }
    let a = look_up(env, steam_id);
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, a.clone());
    a
}

fn look_up(env: &HashMap<String, String>, steam_id: Option<u32>) -> About {
    let heroic = heroic_dirs(env);
    let mut a = About::default();
    let mut ids = Ids { steam: steam_id, namespace: None };
    if let Some(id) = steam_id {
        if let Some(app) = steam::app(env.get("STEAM_COMPAT_CLIENT_INSTALL_PATH").map(String::as_str), id) {
            a.name = Some(app.name.clone()).filter(|n| !n.is_empty());
            a.multiplayer = app.multiplayer;
            a.online_only = app.online_only();
            a.vac = app.vac;
        }
    } else if let (Some(runner), Some(app)) = (env.get("HEROIC_APP_RUNNER"), env.get("HEROIC_APP_NAME")) {
        heroic_game(&heroic, runner, app, &mut a, &mut ids);
    } else if let Some(name) = env.get("GAME_NAME") {
        a.name = Some(name.clone());
    }
    a.flags_edits = ids.steam.is_some_and(|id| FLAGS_EDITS.iter().any(|&(_, f)| f == id));
    a.listed = listed_anti_cheat(&heroic, &ids, a.name.as_deref());
    // VAC games Steam hasn't shown yet (not in appinfo.vdf): AreWeAntiCheatYet's word for it.
    a.vac |= a.listed.as_deref().is_some_and(|l| l.split(" / ").any(|ac| ac == "VAC" || ac == "Valve Anti-Cheat"));
    a
}

struct Ids {
    steam: Option<u32>,
    /// Epic's namespace for the game.
    namespace: Option<String>,
}

/// Heroic's folder from the game's environment (the flatpak's XDG_CONFIG_HOME), then the
/// flatpak's and the native one.
fn heroic_dirs(env: &HashMap<String, String>) -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let from_env = env.get("XDG_CONFIG_HOME").map(|d| PathBuf::from(d).join("heroic"));
    let usual = [".var/app/com.heroicgameslauncher.hgl/config/heroic", ".config/heroic"].map(|d| home.join(d));
    from_env.into_iter().chain(usual).filter(|d| d.is_dir()).collect()
}

fn load(path: PathBuf) -> Option<Value> {
    json::parse(&fs::read_to_string(path).ok()?)
}

fn heroic_game(dirs: &[PathBuf], runner: &str, app: &str, a: &mut About, ids: &mut Ids) {
    let store = match runner {
        "legendary" => "epic",
        "gog" => "gog",
        "nile" => "amazon",
        _ => return,
    };
    for dir in dirs {
        let cache = dir.join("store_cache");
        if runner == "legendary" {
            let library = load(cache.join("legendary_library.json"));
            let entry = library.as_ref().and_then(|l| {
                l.get("library")?.arr().iter().find(|g| g.get("app_name").and_then(Value::str) == Some(app))
            });
            if let Some(g) = entry {
                a.name = g.get("title").and_then(Value::str).map(str::to_owned);
                ids.namespace = g.get("namespace").and_then(Value::str).map(str::to_owned);
            }
        }
        let info = load(cache.join("gog_api_info.json"));
        let Some(game) = info.as_ref().and_then(|i| i.get(&format!("{store}_{app}"))?.get("game")) else {
            if a.name.is_some() {
                return;
            }
            continue;
        };
        if a.name.is_none() {
            a.name = game.get("title").and_then(Value::str).map(str::to_owned);
        }
        let modes: Vec<&str> =
            game.get("game_modes").map(Value::arr).unwrap_or_default().iter().filter_map(|m| m.get("slug")?.str()).collect();
        let shared = modes.iter().any(|m| SHARED_MODES.iter().any(|s| m.starts_with(s)));
        a.multiplayer = shared;
        let online = modes.iter().any(|m| ONLINE_MODES.iter().any(|s| m.starts_with(s)));
        a.online_only = online || shared && !modes.contains(&"single-player");
        ids.steam = game.get("releases").map(Value::arr).unwrap_or_default().iter().find_map(|r| {
            (r.get("platform_id")?.str()? == "steam").then_some(())?;
            r.get("external_id")?.str()?.parse().ok()
        });
        return;
    }
}

/// AreWeAntiCheatYet's anti-cheat for a game named like one of `names` (its folder and
/// program), whatever started it. Kept per names: the games list asks every second.
pub fn listed_as(env: &HashMap<String, String>, names: &[String]) -> Option<String> {
    static CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);
    let key = names.join("\n");
    if let Some(found) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return found.clone();
    }
    let dirs = heroic_dirs(env);
    let ids = Ids { steam: None, namespace: None };
    let found = names.iter().find_map(|n| listed_anti_cheat(&dirs, &ids, Some(n)));
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, found.clone());
    found
}

/// AreWeAntiCheatYet's entry by Steam app ID, Epic namespace or name (letters and digits only).
fn listed_anti_cheat(dirs: &[PathBuf], ids: &Ids, name: Option<&str>) -> Option<String> {
    static LIST: OnceLock<Option<Value>> = OnceLock::new();
    let simple = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let name = name.map(simple).filter(|n| !n.is_empty());
    let heroic = dirs.iter().find_map(|d| load(d.join("areweanticheatyet.json")));
    let bundled = LIST.get_or_init(|| json::parse(BUNDLED));
    let steam = ids.steam.map(|id| id.to_string());
    heroic.iter().chain(bundled).flat_map(Value::arr).find_map(|g| {
        let store = g.get("storeIds");
        let listed_steam = store.and_then(|s| s.get("steam")?.str());
        let listed_name = g.get("name").and_then(Value::str);
        if WRONG.iter().any(|&(n, id)| listed_name == Some(n) && listed_steam == Some(id)) {
            return None;
        }
        let by_steam = steam.is_some() && listed_steam == steam.as_deref();
        let namespace = store.and_then(|s| s.get("epic")?.get("namespace")?.str());
        let by_epic = ids.namespace.is_some() && namespace == ids.namespace.as_deref();
        let by_name = name.is_some() && listed_name.map(simple) == name;
        if !(by_steam || by_epic || by_name) {
            return None;
        }
        let acs: Vec<&str> = g.get("anticheats").map(Value::arr).unwrap_or_default().iter().filter_map(Value::str).collect();
        (!acs.is_empty()).then(|| acs.join(" / "))
    })
}
