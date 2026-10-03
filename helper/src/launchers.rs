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
    pub anti_cheat: Option<String>,
    pub multiplayer: bool,
    pub online_only: bool,
}

/// GOG's game modes that mean playing with other people.
const SHARED_MODES: &[&str] = &["multiplayer", "co-operative", "massively-multiplayer", "battle-royale"];

/// AreWeAntiCheatYet's games.json (MIT), trimmed by data/areweanticheatyet/update.sh.
const BUNDLED: &str = include_str!("../../data/areweanticheatyet/games.json");

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
            a.anti_cheat = app.vac.then(|| "Valve Anti-Cheat".to_owned());
        }
    } else if let (Some(runner), Some(app)) = (env.get("HEROIC_APP_RUNNER"), env.get("HEROIC_APP_NAME")) {
        heroic_game(&heroic, runner, app, &mut a, &mut ids);
    } else if let Some(name) = env.get("GAME_NAME") {
        a.name = Some(name.clone());
    }
    if a.anti_cheat.is_none() {
        a.anti_cheat = listed_anti_cheat(&heroic, &ids, a.name.as_deref());
    }
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
        a.online_only = shared && !modes.contains(&"single-player");
        ids.steam = game.get("releases").map(Value::arr).unwrap_or_default().iter().find_map(|r| {
            (r.get("platform_id")?.str()? == "steam").then_some(())?;
            r.get("external_id")?.str()?.parse().ok()
        });
        return;
    }
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
        let by_steam = steam.is_some() && store.and_then(|s| s.get("steam")?.str()) == steam.as_deref();
        let namespace = store.and_then(|s| s.get("epic")?.get("namespace")?.str());
        let by_epic = ids.namespace.is_some() && namespace == ids.namespace.as_deref();
        let by_name = name.is_some() && g.get("name").and_then(Value::str).map(simple) == name;
        if !(by_steam || by_epic || by_name) {
            return None;
        }
        let acs: Vec<&str> = g.get("anticheats").map(Value::arr).unwrap_or_default().iter().filter_map(Value::str).collect();
        (!acs.is_empty()).then(|| acs.join(" / "))
    })
}
