//! 包注册表、平台资产名检测、已装版本查询与 GitHub Releases 查询。

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Pkg {
    /// 包名(vibeup 里的键)。
    pub name: &'static str,
    /// GitHub 仓库 slug。
    pub repo: &'static str,
    /// 装好后的二进制名(Windows 自动加 .exe)。
    pub bin: &'static str,
}

/// vibeup 自身(供 `uninstall self` 使用)。
pub static SELF: Pkg = Pkg {
    name: "vibeup",
    repo: "second-state/vibeup",
    bin: "vibeup",
};

/// 注册表:加新包只需在这里加一行,前提是 Release 资产叫 `<bin>-<平台>`。
pub const PKGS: &[Pkg] = &[
    Pkg {
        name: "vibetty",
        repo: "second-state/vibetty",
        bin: "vibetty",
    },
    Pkg {
        name: "vibekeys",
        repo: "second-state/vibekeys_app",
        bin: "vibekeys",
    },
];

pub fn find(name: &str) -> Result<&'static Pkg> {
    PKGS.iter().find(|p| p.name == name).ok_or_else(|| {
        anyhow!(
            "unknown package `{name}` — available: {}",
            PKGS.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")
        )
    })
}

impl Pkg {
    /// 当前平台的 Release 资产名。与各仓库 release.yml 的命名一致。
    pub fn asset(&self) -> Result<String> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Ok(format!("{}-linux-x64", self.bin)),
            ("macos", "aarch64") => Ok(format!("{}-macos-arm64", self.bin)),
            ("windows", "x86_64") => Ok(format!("{}-windows-x64.exe", self.bin)),
            (os, arch) => bail!(
                "no prebuilt binary for {os}/{arch} — check https://github.com/{}/releases",
                self.repo
            ),
        }
    }

    /// symlink(Windows 上是副本)的名字:vibetty / vibekeys / vibetty.exe。
    pub fn link_name(&self) -> String {
        if std::env::consts::OS == "windows" {
            format!("{}.exe", self.bin)
        } else {
            self.bin.to_string()
        }
    }

    /// 下载 URL。latest 走 releases/latest(不含 prerelease);显式 tag 直接 pin
    /// (tag 含 `-` 即 prerelease,latest 不会命中)。
    pub fn download_url(&self, asset: &str, tag: &str) -> String {
        if tag == "latest" {
            format!(
                "https://github.com/{}/releases/latest/download/{asset}",
                self.repo
            )
        } else {
            format!(
                "https://github.com/{}/releases/download/{tag}/{asset}",
                self.repo
            )
        }
    }
}

/// 安装目录:默认 `~/.cargo/bin`(与各仓库 install.sh 约定一致),`--dir` 覆盖。
pub fn install_dir(over: Option<&Path>) -> PathBuf {
    over.map(|p| p.to_path_buf())
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo").join("bin")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 已装版本:跑 symlink 的 `--version`,输出形如 `vibetty 0.4.0`,取第二个 token。
/// 跑不起来(不存在/损坏)一律视为未装。
pub fn installed_version(pkg: &Pkg, dir: &Path) -> Option<String> {
    let out = Command::new(dir.join(pkg.link_name()))
        .arg("--version")
        .output()
        .ok()?;
    let v = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)?
        .to_string();
    (!v.is_empty()).then_some(v)
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::ClientBuilder::new()
        .user_agent(concat!("vibeup/", env!("CARGO_PKG_VERSION")))
        // API 查询很快,但 release 二进制几十 MB、慢链路(代理)可能要一两分钟;
        // reqwest 默认 30s 总超时会把下载中途掐掉。连接超时保持灵敏,坏端点快速失败。
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?)
}

/// GET 一个 URL(GitHub API 和 Release 下载共用;GitHub API 要求带 User-Agent)。
pub fn send_get(url: &str) -> Result<reqwest::blocking::Response> {
    Ok(client()?.get(url).send()?)
}

/// 查一个仓库最近 20 个 release,返回 (最新稳定 tag, 最新 tag 含 prerelease)。
/// GitHub 的 `releases/latest` 语义等价于前者,这里一次请求把两个都拿回来。
/// ⚠️ API 列表顺序不保证语义正确(rc.10 可能排在 rc.9 前面),必须自己按
/// 版本语义排序后再挑,见 [`version_gt`]。
pub fn releases(repo: &str) -> Result<(String, Option<String>)> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=20");
    let rels: Vec<GhRelease> = client()?.get(&url).send()?.error_for_status()?.json()?;
    let mut tags: Vec<(&str, bool)> = rels
        .iter()
        .filter(|r| !r.draft)
        .map(|r| (r.tag_name.as_str(), r.prerelease))
        .collect();
    tags.sort_by(|a, b| cmp_version(b.0, a.0));
    let stable = tags
        .iter()
        .find(|(_, pre)| !*pre)
        .map(|(t, _)| t.to_string());
    let any = tags.first().map(|(t, _)| t.to_string());
    Ok((
        stable.ok_or_else(|| anyhow!("no stable release found in {repo}"))?,
        any,
    ))
}

/// 语义化比较两个版本(tag 或裸版本,允许 `v` 前缀):数字段按数值比
/// (`rc.10 > rc.9`),同版本号时无 prerelease 的更大(`0.5.0 > 0.5.0-rc.1`)。
/// 返回 Greater = a 更新。
pub fn cmp_version(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    fn split(ver: &str) -> (Vec<Chunk>, Option<Vec<Chunk>>) {
        let norm = ver.strip_prefix('v').unwrap_or(ver);
        match norm.split_once('-') {
            Some((main, pre)) => (chunks(main), Some(chunks(pre))),
            None => (chunks(norm), None),
        }
    }
    let (a_main, a_pre) = split(a);
    let (b_main, b_pre) = split(b);
    match a_main.cmp(&b_main) {
        Ordering::Equal => {}
        other => return other,
    }
    match (a_pre, b_pre) {
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(&y),
        (None, None) => Ordering::Equal,
    }
}

/// 版本号的比较单元:连续数字段按数值,其余按字节序。Num 在 Str 前
/// (semver:prerelease 里数字标识符小于字母标识符)。
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Chunk {
    Num(u64),
    Str(String),
}

fn chunks(s: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_digits = false;
    for c in s.chars() {
        let d = c.is_ascii_digit();
        if !cur.is_empty() && d != cur_digits {
            push_chunk(&mut out, std::mem::take(&mut cur), cur_digits);
        }
        cur_digits = d;
        cur.push(c);
    }
    if !cur.is_empty() {
        push_chunk(&mut out, cur, cur_digits);
    }
    out
}

fn push_chunk(out: &mut Vec<Chunk>, s: String, digits: bool) {
    if digits {
        out.push(Chunk::Num(s.parse().unwrap_or(0)));
    } else {
        out.push(Chunk::Str(s));
    }
}

/// 版本归一化:去掉 `v` 前缀,便于和 `--version` 的输出比较。
pub fn normalize(ver: &str) -> &str {
    ver.strip_prefix('v').unwrap_or(ver)
}

#[cfg(test)]
mod version_tests {
    use super::cmp_version;
    use std::cmp::Ordering::*;

    #[test]
    fn numeric_segments_compare_numerically() {
        assert_eq!(cmp_version("v0.2.1-rc.10", "v0.2.1-rc.9"), Greater);
        assert_eq!(cmp_version("v0.2.1-rc.9", "v0.2.1-rc.10"), Less);
        assert_eq!(cmp_version("v0.2.1-rc.2", "v0.2.1-rc.9"), Less);
    }

    #[test]
    fn stable_beats_prerelease_of_same_version() {
        assert_eq!(cmp_version("v0.5.0", "v0.5.0-rc.1"), Greater);
        assert_eq!(cmp_version("0.5.0-rc.1", "0.5.0"), Less);
    }

    #[test]
    fn normal_semver_ordering() {
        assert_eq!(cmp_version("v0.5.0", "v0.4.3"), Greater);
        assert_eq!(cmp_version("v1.0.0", "v0.9.9"), Greater);
        assert_eq!(cmp_version("v0.2.1-rc.1", "v0.2.0"), Greater);
    }

    #[test]
    fn aliases_normalize() {
        assert_eq!(cmp_version("v0.2.1-rc.10", "0.2.1-rc.10"), Equal);
        assert_eq!(cmp_version("v0.5.0", "0.5.0"), Equal);
    }

    #[test]
    fn right_modifier_names_dont_confuse_the_parser() {
        // 防呆:确保 chunks 不把 "rc" 与 "10" 粘成一个字符串段。
        assert_eq!(cmp_version("v0.2.1-rc.10", "v0.2.1-rc.1"), Greater);
    }
}
