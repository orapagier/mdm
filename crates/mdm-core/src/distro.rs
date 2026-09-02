//! Which package manager this machine has.
//!
//! Every "install it with…" line MDM prints ends up being pasted into a
//! terminal, so it has to name a command that exists there. `sudo dnf install`
//! on a Debian box is worse than no advice at all: it sends someone looking
//! for a package manager they do not have, instead of at the dependency they
//! are actually missing.
//!
//! Only the *command* is guessed. The three packages MDM ever names — aria2,
//! yt-dlp, nodejs — are spelled the same in every family's repositories, so
//! there is no name table to drift out of date.

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackageManager {
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
    Unknown,
}

/// This machine's package manager, worked out once.
pub fn package_manager() -> PackageManager {
    static PM: OnceLock<PackageManager> = OnceLock::new();
    *PM.get_or_init(detect)
}

/// `/etc/os-release` first, then whatever is on PATH.
///
/// The file is the authority when it is there, but stripped images and
/// containers often carry a useless one (or none), and by then the binary is
/// the better answer anyway: a machine with `apt-get` takes apt commands.
fn detect() -> PackageManager {
    if let Some(pm) = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| from_os_release(&text))
    {
        return pm;
    }
    for (bin, pm) in [
        ("apt-get", PackageManager::Apt),
        ("dnf", PackageManager::Dnf),
        ("pacman", PackageManager::Pacman),
        ("zypper", PackageManager::Zypper),
        ("apk", PackageManager::Apk),
    ] {
        if crate::supervisor::which(bin).is_some() {
            return pm;
        }
    }
    PackageManager::Unknown
}

/// Read `ID`, then `ID_LIKE`.
///
/// `ID_LIKE` is where derivatives name their parent, which is precisely the
/// question being asked: Mint says `ubuntu debian`, Nobara says `fedora`. It
/// is a space-separated list in preference order, so an unknown `ID` is never
/// the end of the road.
pub fn from_os_release(text: &str) -> Option<PackageManager> {
    let value = |key: &str| {
        text.lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix(key))
            .map(|rest| rest.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
            .next()
            .unwrap_or_default()
    };
    let id = value("ID=");
    let like = value("ID_LIKE=");
    std::iter::once(id.as_str())
        .chain(like.split_whitespace())
        .find_map(family_of)
}

fn family_of(id: &str) -> Option<PackageManager> {
    Some(match id {
        "debian" | "ubuntu" | "linuxmint" | "pop" | "elementary" | "zorin" | "kali"
        | "raspbian" | "devuan" | "neon" | "deepin" | "mx" => PackageManager::Apt,
        "fedora" | "rhel" | "centos" | "rocky" | "almalinux" | "ol" | "nobara" | "bazzite" => {
            PackageManager::Dnf
        }
        "arch" | "manjaro" | "endeavouros" | "cachyos" | "garuda" | "artix" => {
            PackageManager::Pacman
        }
        "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "suse" | "sles" | "sled" => {
            PackageManager::Zypper
        }
        "alpine" | "postmarketos" => PackageManager::Apk,
        _ => return None,
    })
}

/// How to install `package` here — `sudo apt install aria2` on Debian,
/// `sudo dnf install aria2` on Fedora.
///
/// When the family is unknown the package is still named, because the name is
/// the part the user cannot look up on their own.
pub fn install(package: &str) -> String {
    match package_manager() {
        PackageManager::Apt => format!("sudo apt install {package}"),
        PackageManager::Dnf => format!("sudo dnf install {package}"),
        PackageManager::Pacman => format!("sudo pacman -S {package}"),
        PackageManager::Zypper => format!("sudo zypper install {package}"),
        PackageManager::Apk => format!("sudo apk add {package}"),
        PackageManager::Unknown => {
            format!("your package manager (the package is called {package})")
        }
    }
}

/// How to update one already-installed package.
///
/// apt has no verb for this: a bare `apt upgrade yt-dlp` upgrades the whole
/// system on older releases, so it is spelled out the way that only ever
/// touches the one package.
pub fn upgrade(package: &str) -> String {
    match package_manager() {
        PackageManager::Apt => format!("sudo apt install --only-upgrade {package}"),
        PackageManager::Dnf => format!("sudo dnf upgrade {package}"),
        PackageManager::Pacman => format!("sudo pacman -Syu {package}"),
        PackageManager::Zypper => format!("sudo zypper update {package}"),
        PackageManager::Apk => format!("sudo apk upgrade {package}"),
        PackageManager::Unknown => {
            format!("your package manager (the package is called {package})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_id() {
        assert_eq!(
            from_os_release("NAME=\"Ubuntu\"\nID=ubuntu\nVERSION_ID=\"24.04\"\n"),
            Some(PackageManager::Apt)
        );
        assert_eq!(
            from_os_release("ID=fedora\nVERSION_ID=42\n"),
            Some(PackageManager::Dnf)
        );
    }

    /// A derivative nobody has heard of still resolves through its parent.
    #[test]
    fn falls_back_to_id_like() {
        assert_eq!(
            from_os_release("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"),
            Some(PackageManager::Apt)
        );
        assert_eq!(
            from_os_release("ID=someremix\nID_LIKE=debian\n"),
            Some(PackageManager::Apt)
        );
        assert_eq!(
            from_os_release("ID=endless\nID_LIKE=\"rhel fedora\"\n"),
            Some(PackageManager::Dnf)
        );
    }

    #[test]
    fn unknown_when_nothing_matches() {
        assert_eq!(from_os_release("ID=plan9\n"), None);
        assert_eq!(from_os_release(""), None);
    }

    /// `PRETTY_NAME` also ends in `ID=`-looking text on some distros; the
    /// match is anchored to the start of a line for exactly that reason.
    #[test]
    fn ignores_other_keys() {
        assert_eq!(
            from_os_release("VERSION_ID=\"12\"\nBUILD_ID=rolling\nID=debian\n"),
            Some(PackageManager::Apt)
        );
    }
}
