//! 本机信息查询与宿主侧配置:tailscale、局域网 IP、broker 地址处理、
//! `~/.vibetty/config.toml`。wizard 与 webui 共用;Linux 向
//! (`hostname -I` / `tailscale ip`)。

/// `tailscale ip -4`(命令不存在/未登录 = None)。
pub(crate) fn tailscale_ip() -> Option<String> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// tailscale 命令在本机是否可用(`version` 能跑通 = 已安装)。
pub(crate) fn tailscale_installed() -> bool {
    std::process::Command::new("tailscale")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 把 url 的 host 换成 ip,保留 scheme/userinfo/port/路径。
pub(crate) fn broker_with_host(url: &str, ip: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (userinfo, after) = match rest.rsplit_once('@') {
        Some((u, a)) => (format!("{u}@"), a),
        None => (String::new(), rest),
    };
    let (hostport, tail) = match after.split_once('/') {
        Some((h, t)) => (h, Some(t)),
        None => (after, None),
    };
    let port = hostport
        .rsplit_once(':')
        .and_then(|(_, p)| (!p.is_empty()).then(|| format!(":{p}")))
        .unwrap_or_default();
    let tail = tail.map(|t| format!("/{t}")).unwrap_or_default();
    format!("{scheme}://{userinfo}{ip}{port}{tail}")
}

/// 本机非回环 IPv4 候选列表(默认路由 + Linux 全量 + tailscale,去重)。
pub(crate) fn local_ips() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // 默认路由地址(UDP connect 技巧,不发包)。
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
        && s.connect("8.8.8.8:80").is_ok()
        && let Ok(a) = s.local_addr()
    {
        out.push(a.ip().to_string());
    }
    // Linux:hostname -I 列出全部 IPv4。
    if let Ok(o) = std::process::Command::new("hostname").arg("-I").output()
        && o.status.success()
    {
        for ip in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            out.push(ip.to_string());
        }
    }
    // tailscale 网段。
    if let Some(ip) = tailscale_ip() {
        out.push(ip);
    }
    out.retain(|ip| !ip.starts_with("127.") && ip != "::1");
    out.sort();
    out.dedup();
    out
}

/// 步骤 15 的 broker 地址校验:scheme 合法 + host 非空。
pub(crate) fn broker_url_valid(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "mqtt" | "mqtts" | "ws" | "wss") {
        return false;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    // 允许 user:pass@host:port,取 @ 后面的部分。
    let host = authority.rsplit('@').next().unwrap_or("");
    !host.is_empty()
}

/// 读 `~/.vibetty/config.toml` 里 `[mqtt] broker` 的值(没有/无文件 = None)。
pub(crate) fn vibetty_broker() -> Option<String> {
    let path = dirs::home_dir()?.join(".vibetty").join("config.toml");
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_mqtt = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_mqtt = t.eq_ignore_ascii_case("[mqtt]");
            continue;
        }
        if in_mqtt
            && let Some((k, v)) = t.split_once('=')
            && k.trim() == "broker"
        {
            let s = v.trim().trim_matches('"').trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 写 `~/.vibetty/config.toml` 的 `[mqtt] broker`。文件不存在就建;已存在
/// 则行级替换/插入,保留其余内容(不引 toml 依赖,注释也保得住)。
pub(crate) fn set_vibetty_broker(url: &str) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let path = home.join(".vibetty").join("config.toml");
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut mqtt_at: Option<usize> = None;
    let mut broker_at: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            if t.eq_ignore_ascii_case("[mqtt]") {
                mqtt_at = Some(i);
            } else if mqtt_at.is_some() {
                break; // [mqtt] 段结束,没找到 broker 行
            }
            continue;
        }
        if mqtt_at.is_some()
            && let Some((k, _)) = t.split_once('=')
            && k.trim() == "broker"
        {
            broker_at = Some(i);
            break;
        }
    }
    let newline = format!("broker = \"{url}\"");
    match (mqtt_at, broker_at) {
        (_, Some(i)) => lines[i] = newline,
        (Some(i), None) => lines.insert(i + 1, newline),
        (None, _) => {
            if !lines.is_empty() && !lines[lines.len() - 1].trim().is_empty() {
                lines.push(String::new());
            }
            lines.push("[mqtt]".into());
            lines.push(newline);
        }
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_swap_preserves_everything_but_host() {
        assert_eq!(
            broker_with_host("mqtt://127.0.0.1:1883", "192.168.1.250"),
            "mqtt://192.168.1.250:1883"
        );
        assert_eq!(
            broker_with_host("mqtt://127.0.0.1", "10.0.0.2"),
            "mqtt://10.0.0.2"
        );
        assert_eq!(
            broker_with_host("wss://user:pw@127.0.0.1:8080/mqtt", "192.168.1.2"),
            "wss://user:pw@192.168.1.2:8080/mqtt"
        );
        assert_eq!(
            broker_with_host("mqtts://user:pass@abc.emqx.cloud:8883/v5", "10.0.0.2"),
            "mqtts://user:pass@10.0.0.2:8883/v5"
        );
        assert_eq!(broker_with_host("nonsense", "1.2.3.4"), "nonsense");
    }

    #[test]
    fn broker_url_validation() {
        assert!(broker_url_valid("mqtt://192.168.1.1:1883"));
        assert!(broker_url_valid("wss://user:pass@abc.emqx.cloud:8883"));
        assert!(!broker_url_valid("http://x"));
        assert!(!broker_url_valid("mqtt://"));
        assert!(!broker_url_valid("no-scheme"));
    }
}
