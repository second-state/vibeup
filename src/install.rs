//! 下载与安装。逻辑泛化自 vibetty 的 `src/update.rs`(行为与各仓库 install.sh 一致):
//! staging 临时文件 → chmod 755 → 跑 `--version` 取版本 → 原子 rename 到
//! `<bin>-<版本>` → symlink(Windows 副本)切换 → 旧版本清理。
//!
//! 函数**不直接打印**:过程行攒进返回的日志,由调用方决定去向 —— CLI 原样
//! 输出,TUI 只看结果(打印会打穿 TUI 的整屏)。

use crate::pkg::{self, Pkg};
use anyhow::{Context, Result, anyhow, bail};
use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
}

/// 下载进度回调:`(已下载字节, 总字节)`。总字节未知时为 None。
pub type ProgressFn = dyn Fn(u64, Option<u64>) + Send + Sync;

/// 安装/更新一个包。`tag` 缺省时:pre 取最新 prerelease,否则最新稳定版。
/// 同版本已装且入口可用时跳过下载,报 up to date。`interactive` = 允许交互
/// 确认旧版本清理(CLI);TUI 传 false(自动保留,写进日志)。
/// 返回过程日志(每行一条,带换行)。
pub fn install(
    pkg: &Pkg,
    tag: Option<String>,
    pre: bool,
    dir: &Path,
    dry_run: bool,
    interactive: bool,
    progress: &ProgressFn,
) -> Result<String> {
    let mut out = String::new();
    let (tag, source) = match tag {
        Some(t) => (t, "requested"),
        None if pre => {
            let (_, any) = pkg::releases(pkg.repo)?;
            (
                any.ok_or_else(|| anyhow!("no release found in {}", pkg.repo))?,
                "latest prerelease",
            )
        }
        // 先解析出具体 tag:一是能做下面的同版本预检查(免得下载完才发现
        // up to date),二是下载 URL 精确 pin 到版本。
        None => {
            let (latest, _) = pkg::releases(pkg.repo)?;
            (latest, "latest release")
        }
    };

    let asset = pkg.asset()?;
    let url = pkg.download_url(&asset, &tag);

    // 目标 tag 与已装版本一致 → 不用下。显式指定 tag 时也照查,幂等。
    let want = pkg::normalize(&tag);
    if pkg::installed_version(pkg, dir).is_some_and(|v| pkg::normalize(&v) == want) {
        out.push_str(&format!(
            "{} {want} is already installed — up to date\n",
            pkg.name
        ));
        return Ok(out);
    }

    if dry_run {
        out.push_str(&format!("would download {url}\n"));
        out.push_str(&format!(
            "would install into {} ({source})\n",
            dir.display()
        ));
        return Ok(out);
    }

    std::fs::create_dir_all(dir)?;
    // 清掉之前被杀进程留下的 staging 残留(每次 pid 不同,不会自愈)。
    for e in std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()) {
        if e.file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(&format!(".vibeup.{}.", pkg.name)))
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let staging = dir.join(format!(".vibeup.{}.{}", pkg.name, std::process::id()));

    out.push_str(&format!("Installing {} {tag} ({source})\n", pkg.name));
    out.push_str(&format!("Downloading {url}\n"));
    download(&url, &staging, progress)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755));
    }

    // 版本号问二进制(输出如 "vibetty 0.4.2");拿不到说明下载坏了,别装。
    let out_run = Command::new(&staging)
        .arg("--version")
        .output()
        .map_err(|e| {
            let _ = std::fs::remove_file(&staging);
            anyhow::anyhow!("cannot run the downloaded binary: {e}")
        })?;
    let binver = String::from_utf8_lossy(&out_run.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_string();
    if binver.is_empty() {
        let _ = std::fs::remove_file(&staging);
        bail!("cannot read version from the downloaded binary");
    }

    #[cfg(windows)]
    {
        let target = dir.join(pkg.link_name());
        let versioned = dir.join(format!("{}-{binver}.exe", pkg.bin));
        // 同版本已装(下载后才拿到真实版本号,所以这里再查一次)→ up to date。
        if versioned.exists() {
            let _ = std::fs::remove_file(&staging);
            out.push_str(&format!(
                "{0} {binver} is already installed at {1} — up to date\n",
                pkg.name,
                target.display()
            ));
            return Ok(out);
        }
        // Windows 允许 rename 运行中的 exe、不允许覆盖:先把旧 exe 改名成它的
        // 版本号(进旧版本清理的候选),再把新 exe 改名到位。
        if target.exists() {
            let oldver = Command::new(&target)
                .arg("--version")
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .split_whitespace()
                        .nth(1)
                        .map(String::from)
                })
                .unwrap_or_else(|| "old".to_string());
            let old_path = dir.join(format!("{}-{oldver}.exe", pkg.bin));
            let _ = std::fs::remove_file(&old_path);
            std::fs::rename(&target, &old_path)?;
        }
        std::fs::rename(&staging, &target)?;
        out.push_str(&format!("Installed: {} ({binver})\n", target.display()));
    }

    #[cfg(unix)]
    {
        let versioned = dir.join(format!("{}-{binver}", pkg.bin));
        let link = dir.join(pkg.link_name());
        // 装之前同版本已存在且链接完好 → up to date(下载后才知道精确版本,再查一次)。
        if versioned.exists() && std::fs::read_link(&link).is_ok_and(|l| l == versioned) {
            let _ = std::fs::remove_file(&staging);
            out.push_str(&format!(
                "{0} {binver} is already installed at {1} — up to date\n",
                pkg.name,
                versioned.display()
            ));
            return Ok(out);
        }
        std::fs::rename(&staging, &versioned)?;
        // 建链前先移除旧文件/旧软链:cargo install 装出的是普通文件,直接
        // symlink 会报 EEXIST。rm 对运行中的进程是安全的。
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&versioned, &link)?;
        out.push_str(&format!(
            "Installed: {} -> {} ({binver})\n",
            versioned.display(),
            link.display()
        ));
    }

    #[cfg(unix)]
    let installed = dir.join(format!("{}-{binver}", pkg.bin));
    #[cfg(windows)]
    let installed = dir.join(pkg.link_name());
    cleanup_old_versions(pkg, dir, &installed, interactive, &mut out);
    note_path(dir, &mut out);
    Ok(out)
}

/// 分块下载,边下边报进度(每块回调一次)。
fn download(url: &str, dest: &Path, progress: &ProgressFn) -> Result<()> {
    use std::io::{Read, Write};
    let mut resp = pkg::send_get(url)?.error_for_status()?;
    let total = resp.content_length();
    let mut file = std::fs::File::create(dest).map_err(|e| anyhow!("write {}: {e}", dest.display()))?;
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    Ok(())
}

/// 旧版本清理:列出 `<bin>-<数字>*`(排除刚装的这份),交互时问一次(默认保留);
/// 非交互只把清单写进日志。
fn cleanup_old_versions(
    pkg: &Pkg,
    dir: &Path,
    installed: &Path,
    interactive: bool,
    out: &mut String,
) {
    let prefix = format!("{}-", pkg.bin);
    let olds: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with(&prefix)
                        && n[prefix.len()..].starts_with(|c: char| c.is_ascii_digit())
                })
        })
        // 排除刚装上的这份(它也匹配 `<bin>-<数字>*` 的形状)。
        .filter(|p| *p != installed)
        .collect();
    if olds.is_empty() {
        return;
    }
    out.push_str("Found old version(s):\n");
    for p in &olds {
        out.push_str(&format!("  {}\n", p.display()));
    }
    let delete = interactive
        && is_interactive()
        && dialoguer::Confirm::new()
            .with_prompt("Delete old version(s)?")
            .default(false)
            .interact()
            .unwrap_or(false);
    if delete {
        for p in &olds {
            let _ = std::fs::remove_file(p);
            out.push_str(&format!("Removed: {}\n", p.display()));
        }
    } else {
        out.push_str(&format!(
            "kept — run `vibeup uninstall {}` or delete manually\n",
            pkg.name
        ));
    }
}

fn note_path(dir: &Path, out: &mut String) {
    let dir_str = dir.display().to_string();
    let in_path = std::env::var("PATH")
        .map(|p| p.split(':').chain(p.split(';')).any(|s| s == dir_str))
        .unwrap_or(false);
    if !in_path {
        out.push_str(&format!(
            "note: {dir_str} is not on your PATH — add it to your shell rc if needed\n"
        ));
    }
}

/// 卸载:删入口 + 全部版本化文件。文件不存在不报错。返回过程日志。
pub fn uninstall(pkg: &Pkg, dir: &Path) -> Result<String> {
    let mut out = String::new();
    let mut removed = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let ours = name == pkg.link_name()
            || (name.starts_with(&format!("{}-", pkg.bin))
                && name[pkg.bin.len() + 1..].starts_with(|c: char| c.is_ascii_digit()));
        if ours {
            let path = entry.path();
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            out.push_str(&format!("Removed: {}\n", path.display()));
            removed += 1;
        }
    }
    if removed == 0 {
        out.push_str(&format!(
            "{} is not installed in {}\n",
            pkg.name,
            dir.display()
        ));
    }
    Ok(out)
}
