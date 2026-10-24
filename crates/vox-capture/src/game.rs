//! What a shared window is (Go Live hover cards, v1.1): the program's own name and a cheap, local
//! guess of whether it is a game. Nothing here asks any online service.
//!
//! A program counts as a game when its exe lies in a game store's library (Steam
//! `steamapps\common`, Epic Games, GOG, EA, Ubisoft, Riot, Xbox `XboxGames`, the Minecraft package
//! in `WindowsApps`) or its exe name is on a short list of well-known games installed elsewhere
//! (Battle.net games, Roblox, Minecraft...). The stores' own launchers are never games.

/// What a shared window shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppInfo {
    /// The program's name: the exe's FileDescription / ProductName on Windows, the app's
    /// localized name on macOS, else the exe's file name without `.exe`. Never a window title.
    pub name: String,
    pub is_game: bool,
}

/// Path pieces of game libraries (lowercase, `\` separators).
const LIBRARIES: &[&str] = &[
    "\\steamapps\\common\\",
    "\\epic games\\",
    "\\gog galaxy\\games\\",
    "\\gog games\\",
    "\\ea games\\",
    "\\origin games\\",
    "\\electronic arts\\",
    "\\ubisoft game launcher\\games\\",
    "\\riot games\\",
    "\\xboxgames\\",
    "\\windowsapps\\microsoft.minecraftuwp",
];

/// Launchers and store clients inside those folders: never "Jogando".
const LAUNCHERS: &[&str] = &[
    "steam.exe",
    "steamwebhelper.exe",
    "epicgameslauncher.exe",
    "epicwebhelper.exe",
    "galaxyclient.exe",
    "galaxyclient helper.exe",
    "eadesktop.exe",
    "eabackgroundservice.exe",
    "origin.exe",
    "upc.exe",
    "ubisoftconnect.exe",
    "uplaywebcore.exe",
    "battle.net.exe",
    "riotclientservices.exe",
    "riotclientux.exe",
    "riotclientuxrender.exe",
    "xboxpcapp.exe",
    "gamingservices.exe",
];

/// Well-known games that live outside the libraries above (or under a name the libraries miss).
const KNOWN_GAMES: &[&str] = &[
    // Battle.net installs each game in its own folder.
    "wow.exe",
    "wowclassic.exe",
    "overwatch.exe",
    "diablo iv.exe",
    "diablo immortal.exe",
    "diablo iii64.exe",
    "sc2_x64.exe",
    "hearthstone.exe",
    "heroesofthestorm_x64.exe",
    "cod.exe",
    // Stand-alone installers.
    "robloxplayerbeta.exe",
    "minecraft.windows.exe",
    "osu!.exe",
    "league of legends.exe",
    "valorant-win64-shipping.exe",
    "fortniteclient-win64-shipping.exe",
    "r5apex.exe",
    "r5apex_dx12.exe",
    "gta5.exe",
    "gta5_enhanced.exe",
    "cs2.exe",
    "dota2.exe",
    "rocketleague.exe",
    "eldenring.exe",
    "genshinimpact.exe",
    "zenlesszonezero.exe",
    "starrail.exe",
    "pathofexile_x64.exe",
    "pathofexile.exe",
];

/// The file name of a path (either separator).
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Is the program at `exe_path` (a full path, or just a file name) a game? Cheap: string checks
/// only.
pub fn looks_like_game(exe_path: &str) -> bool {
    let p = exe_path.replace('/', "\\").to_lowercase();
    let name = file_name(&p);
    if LAUNCHERS.contains(&name) || p.contains("\\epic games\\launcher\\") || p.contains("\\riot client\\") || p.contains("\\ea desktop\\") {
        return false;
    }
    KNOWN_GAMES.contains(&name) || LIBRARIES.iter().any(|l| p.contains(l))
}

/// The name to show for a program: its FileDescription, unless that is missing or only repeats
/// the exe's own name (then the ProductName), else the exe's file name without `.exe`. Trademark
/// signs are left out, spaces tidied and the length capped (64 characters).
pub fn program_name(exe_path: &str, file_description: Option<&str>, product_name: Option<&str>) -> String {
    let stem = {
        let n = file_name(exe_path);
        match n.len().checked_sub(4) {
            Some(i) if n.is_char_boundary(i) && n[i..].eq_ignore_ascii_case(".exe") => &n[..i],
            _ => n,
        }
    };
    let tidy = |s: &str| -> String {
        let s: String = s.chars().filter(|c| !matches!(c, '™' | '®' | '©') && !c.is_control()).collect();
        s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(64).collect()
    };
    let squash = |s: &str| -> String { s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect() };
    let stem_key = squash(stem);
    let good = |s: &Option<String>| s.as_ref().is_some_and(|s| !s.is_empty() && squash(s) != stem_key);
    let desc = file_description.map(tidy);
    let product = product_name.map(tidy);
    if good(&desc) {
        return desc.unwrap_or_default();
    }
    if good(&product) {
        return product.unwrap_or_default();
    }
    // Repeating the exe's name is still better than the bare file name when it is all there is.
    desc.filter(|s| !s.is_empty()).or(product.filter(|s| !s.is_empty())).unwrap_or_else(|| tidy(stem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn games_by_library_and_by_name() {
        assert!(looks_like_game(r"D:\SteamLibrary\steamapps\common\Counter-Strike Global Offensive\game\bin\win64\cs2.exe"));
        assert!(looks_like_game(r"C:\Program Files (x86)\Steam\steamapps\common\Hades\x64\Hades.exe"));
        assert!(looks_like_game(r"C:\Program Files\Epic Games\Fortnite\FortniteGame\Binaries\Win64\FortniteClient-Win64-Shipping.exe"));
        assert!(looks_like_game(r"C:\Riot Games\League of Legends\Game\League of Legends.exe"));
        assert!(looks_like_game(r"C:\XboxGames\Forza Horizon 5\Content\ForzaHorizon5.exe"));
        assert!(looks_like_game(r"C:\Program Files (x86)\World of Warcraft\_retail_\Wow.exe"));
        assert!(looks_like_game(r"C:\Users\x\AppData\Local\Roblox\Versions\version-1\RobloxPlayerBeta.exe"));
        assert!(looks_like_game("cs2.exe"), "a bare exe name works too");
        assert!(looks_like_game("C:/Games/GOG Games/Witcher 3/bin/x64/witcher3.exe"), "forward slashes");
    }

    #[test]
    fn launchers_and_apps_are_not_games() {
        assert!(!looks_like_game(r"C:\Program Files (x86)\Steam\steam.exe"));
        assert!(!looks_like_game(r"C:\Program Files (x86)\Steam\bin\cef\cef.win7x64\steamwebhelper.exe"));
        assert!(!looks_like_game(r"C:\Program Files (x86)\Epic Games\Launcher\Portal\Binaries\Win64\EpicGamesLauncher.exe"));
        assert!(!looks_like_game(r"C:\Riot Games\Riot Client\RiotClientServices.exe"));
        assert!(!looks_like_game(r"C:\Program Files\Electronic Arts\EA Desktop\EA Desktop\EADesktop.exe"));
        assert!(!looks_like_game(r"C:\Program Files\Google\Chrome\Application\chrome.exe"));
        assert!(!looks_like_game(r"C:\Program Files\WindowsApps\SpotifyAB.SpotifyMusic_1.0\Spotify.exe"));
        assert!(!looks_like_game(r"C:\Windows\explorer.exe"));
        assert!(!looks_like_game(""));
    }

    #[test]
    fn names_come_from_the_program_not_the_window() {
        let chrome = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
        assert_eq!(program_name(chrome, Some("Google Chrome"), Some("Google Chrome")), "Google Chrome");
        // A description that only repeats the exe name: the product name.
        assert_eq!(program_name(r"C:\x\FortniteClient-Win64-Shipping.exe", Some("FortniteClient-Win64-Shipping"), Some("Fortnite")), "Fortnite");
        // Trademarks and odd spacing.
        assert_eq!(program_name(r"C:\g\game.exe", Some("  Counter-Strike™  2 "), None), "Counter-Strike 2");
        // Nothing in the exe: its file name.
        assert_eq!(program_name(r"C:\tools\test_window.exe", None, None), "test_window");
        assert_eq!(program_name("notes", Some(""), Some("")), "notes");
        // Only the repeated name: kept (it may be better cased than the file).
        assert_eq!(program_name(r"C:\x\obs64.exe", Some("OBS64"), None), "OBS64");
        assert_eq!(program_name(r"C:\x\a.exe", Some(&"Nome muito longo ".repeat(10)), None).chars().count(), 64);
    }
}
