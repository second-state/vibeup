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
        .build()?)
}

/// GET 一个 URL(GitHub API 和 Release 下载共用;GitHub API 要求带 User-Agent)。
pub fn send_get(url: &str) -> Result<reqwest::blocking::Response> {
    Ok(client()?.get(url).send()?)
}

/// 查一个仓库最近 20 个 release,返回 (最新稳定 tag, 最新 tag 含 prerelease)。
/// GitHub 的 `releases/latest` 语义等价于前者,这里一次请求把两个都拿回来。
pub fn releases(repo: &str) -> Result<(String, Option<String>)> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=20");
    let rels: Vec<GhRelease> = client()?.get(&url).send()?.error_for_status()?.json()?;
    let stable = rels
        .iter()
        .find(|r| !r.draft && !r.prerelease)
        .map(|r| r.tag_name.clone());
    let any = rels.iter().find(|r| !r.draft).map(|r| r.r#tag_name.clone());
    Ok((
        stable.ok_or_else(|| anyhow!("no stable release found in {repo}"))?,
        any,
    ))
}

/// 版本归一化:去掉 `v` 前缀,便于和 `--version` 的输出比较。
pub fn normalize(ver: &str) -> &str {
    ver.strip_prefix('v').unwrap_or(ver)
}
