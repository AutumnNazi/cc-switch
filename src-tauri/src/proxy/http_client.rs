//! 全局 HTTP 客户端模块
//!
//! 提供支持全局代理配置的 HTTP 客户端。
//! 所有需要发送 HTTP 请求的模块都应使用此模块提供的客户端。
//!
//! 出站代理支持按供应商覆盖（见 [`ProxyMode`]）：全局代理是默认值，单个
//! 供应商可强制走代理或强制直连。非 `Inherit` 的供应商各自复用一个按代理
//! 配置缓存的客户端，避免每次请求重建连接池。

use crate::provider::ProxyMode;
use once_cell::sync::OnceCell;
use reqwest::Client;
use std::collections::HashMap;
use std::env;
use std::net::IpAddr;
use std::sync::RwLock;
use std::time::Duration;

/// 全局 HTTP 客户端实例
static GLOBAL_CLIENT: OnceCell<RwLock<Client>> = OnceCell::new();

/// 按代理配置缓存的客户端池（供应商级覆盖使用）
///
/// key 为代理 URL；`None` 代表强制直连（显式 `no_proxy`）。跟随全局的供应商
/// 不进这个池，直接用 [`get`]。
static SCOPED_CLIENTS: OnceCell<RwLock<HashMap<Option<String>, Client>>> = OnceCell::new();

/// 当前代理 URL（用于日志和状态查询）
static CURRENT_PROXY_URL: OnceCell<RwLock<Option<String>>> = OnceCell::new();

/// CC Switch 代理服务器当前监听的端口
static CC_SWITCH_PROXY_PORT: OnceCell<RwLock<u16>> = OnceCell::new();

/// 设置 CC Switch 代理服务器的监听端口
///
/// 应在代理服务器启动时调用，以便系统代理检测能正确识别自己的端口
pub fn set_proxy_port(port: u16) {
    if let Some(lock) = CC_SWITCH_PROXY_PORT.get() {
        if let Ok(mut current_port) = lock.write() {
            *current_port = port;
            log::debug!("[GlobalProxy] Updated CC Switch proxy port to {port}");
        }
    } else {
        let _ = CC_SWITCH_PROXY_PORT.set(RwLock::new(port));
        log::debug!("[GlobalProxy] Initialized CC Switch proxy port to {port}");
    }
}

/// 获取 CC Switch 代理服务器的监听端口
fn get_proxy_port() -> u16 {
    CC_SWITCH_PROXY_PORT
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|port| *port)
        .unwrap_or(15721) // 默认端口作为回退
}

/// 初始化全局 HTTP 客户端
///
/// 应在应用启动时调用一次。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，如 `http://127.0.0.1:7890` 或 `socks5://127.0.0.1:1080`
///   传入 None 或空字符串表示直连
pub fn init(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let client = build_client(effective_url)?;

    // 尝试初始化全局客户端，如果已存在则记录警告并使用 apply_proxy 更新
    if GLOBAL_CLIENT.set(RwLock::new(client.clone())).is_err() {
        log::warn!(
            "[GlobalProxy] [GP-003] Already initialized, updating instead: {}",
            effective_url
                .map(mask_url)
                .unwrap_or_else(|| "direct connection".to_string())
        );
        // 已初始化，改用 apply_proxy 更新
        return apply_proxy(proxy_url);
    }

    // 初始化代理 URL 记录
    let _ = CURRENT_PROXY_URL.set(RwLock::new(effective_url.map(|s| s.to_string())));

    log::info!(
        "[GlobalProxy] Initialized: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 验证代理配置（不应用）
///
/// 只验证代理 URL 是否有效，不实际更新全局客户端。
/// 用于在持久化之前验证配置的有效性。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，None 或空字符串表示直连
///
/// # Returns
/// 验证成功返回 Ok(())，失败返回错误信息
pub fn validate_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    // 只调用 build_client 来验证，但不应用
    build_client(effective_url)?;
    Ok(())
}

/// 应用代理配置（假设已验证）
///
/// 直接应用代理配置到全局客户端，不做额外验证。
/// 应在 validate_proxy 成功后调用。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，None 或空字符串表示直连
pub fn apply_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let new_client = build_client(effective_url)?;

    // 更新客户端
    if let Some(lock) = GLOBAL_CLIENT.get() {
        let mut client = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-001] Failed to acquire write lock: {e}");
            "Failed to update proxy: lock poisoned".to_string()
        })?;
        *client = new_client;
    } else {
        // 如果还没初始化，则初始化
        return init(proxy_url);
    }

    // 更新代理 URL 记录
    if let Some(lock) = CURRENT_PROXY_URL.get() {
        let mut url = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-002] Failed to acquire URL write lock: {e}");
            "Failed to update proxy URL record: lock poisoned".to_string()
        })?;
        *url = effective_url.map(|s| s.to_string());
    }

    // 全局代理变了，按供应商缓存的客户端（Always 模式指向旧地址）必须失效
    clear_scoped_clients();

    log::info!(
        "[GlobalProxy] Applied: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 更新代理配置（热更新）
///
/// 可在运行时调用以更改代理设置，无需重启应用。
/// 注意：此函数同时验证和应用，如果需要先验证后持久化再应用，
/// 请使用 validate_proxy + apply_proxy 组合。
///
/// # Arguments
/// * `proxy_url` - 新的代理 URL，None 或空字符串表示直连
#[allow(dead_code)]
pub fn update_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let new_client = build_client(effective_url)?;

    // 更新客户端
    if let Some(lock) = GLOBAL_CLIENT.get() {
        let mut client = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-001] Failed to acquire write lock: {e}");
            "Failed to update proxy: lock poisoned".to_string()
        })?;
        *client = new_client;
    } else {
        // 如果还没初始化，则初始化
        return init(proxy_url);
    }

    // 更新代理 URL 记录
    if let Some(lock) = CURRENT_PROXY_URL.get() {
        let mut url = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-002] Failed to acquire URL write lock: {e}");
            "Failed to update proxy URL record: lock poisoned".to_string()
        })?;
        *url = effective_url.map(|s| s.to_string());
    }

    // 同 apply_proxy：全局代理变更后按供应商缓存的客户端必须失效
    clear_scoped_clients();

    log::info!(
        "[GlobalProxy] Updated: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 获取全局 HTTP 客户端
///
/// 返回配置了代理的客户端（如果已配置代理），否则返回跟随系统代理的客户端。
pub fn get() -> Client {
    GLOBAL_CLIENT
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|c| c.clone())
        .unwrap_or_else(|| {
            log::warn!("[GlobalProxy] [GP-004] Client not initialized, using fallback");
            build_client(None).unwrap_or_default()
        })
}

/// 获取当前代理 URL
///
/// 返回当前配置的代理 URL，None 表示直连。
pub fn get_current_proxy_url() -> Option<String> {
    CURRENT_PROXY_URL
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|url| url.clone())
}

/// 检查是否正在使用代理
#[allow(dead_code)]
pub fn is_proxy_enabled() -> bool {
    get_current_proxy_url().is_some()
}

/// 某个供应商实际生效的出站代理配置
///
/// 把「代理模式 + 供应商专用地址」收敛成一个值：调用点只需照它执行，
/// 不必各自重复「Always 但地址从哪来」的判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxySelection {
    /// 跟随全局设置。全局未配置代理时，由 reqwest 自行跟随系统代理。
    Inherit,
    /// 走指定代理地址（供应商专用地址优先，其次全局地址）。
    Proxy(String),
    /// 强制直连，并显式忽略系统环境变量代理。
    Direct,
}

impl ProxySelection {
    /// 解析生效配置
    ///
    /// `Always` 优先用供应商自己的 `proxy_url`；没填才回落全局地址。两者都为空
    /// 时只能直连——但这是配置缺失，要告警，因为用户的意图明确是走代理。
    pub fn resolve(mode: ProxyMode, provider_proxy_url: Option<&str>) -> Self {
        match mode {
            ProxyMode::Inherit => Self::Inherit,
            ProxyMode::Never => Self::Direct,
            ProxyMode::Always => {
                let own = provider_proxy_url
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                match own.or_else(get_current_proxy_url) {
                    Some(url) => Self::Proxy(url),
                    None => {
                        log::warn!(
                            "[GlobalProxy] [GP-012] Provider forces proxy but neither a provider proxy URL nor a global proxy is configured; falling back to direct connection"
                        );
                        Self::Direct
                    }
                }
            }
        }
    }

    /// 生效的代理地址；`None` 表示不经显式代理（`Inherit` 或 `Direct`）。
    ///
    /// 注意 `Inherit` 与 `Direct` 都返回 `None`，但语义不同：前者仍可能由
    /// reqwest 跟随系统代理，后者显式断绝。需要区分时用 [`Self::is_direct`]。
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Proxy(url) => Some(url.as_str()),
            Self::Inherit | Self::Direct => None,
        }
    }

    /// 是否为强制直连
    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct)
    }

    /// 客户端池的缓存键；`Inherit` 不进池，返回 `None`。
    fn cache_key(&self) -> Option<Option<String>> {
        match self {
            Self::Inherit => None,
            Self::Proxy(url) => Some(Some(url.clone())),
            Self::Direct => Some(None),
        }
    }
}

/// 按供应商生效的代理配置获取 HTTP 客户端
///
/// `Inherit` 直接复用全局客户端；其余从按代理地址缓存的池里取，未命中则
/// 构建后存入，避免每个请求重建连接池。
pub fn get_for_selection(selection: &ProxySelection) -> Client {
    let Some(key) = selection.cache_key() else {
        return get();
    };
    let pool = SCOPED_CLIENTS.get_or_init(|| RwLock::new(HashMap::new()));

    if let Ok(map) = pool.read() {
        if let Some(client) = map.get(&key) {
            return client.clone();
        }
    }

    let built = match key.as_deref() {
        Some(url) => build_client(Some(url)),
        // 强制直连：必须显式 no_proxy，否则 reqwest 会回落系统环境变量代理
        None => build_direct_client(),
    };

    let client = match built {
        Ok(client) => client,
        Err(e) => {
            log::warn!("[GlobalProxy] [GP-013] Failed to build scoped client ({e}), using global");
            return get();
        }
    };

    if let Ok(mut map) = pool.write() {
        map.insert(key, client.clone());
    }
    client
}

/// 清空按供应商缓存的客户端池
///
/// 全局代理变更后必须调用：池里 `Always` 的条目是按旧全局地址建的，
/// 不清掉会让改过代理的供应商继续用老地址。
fn clear_scoped_clients() {
    if let Some(pool) = SCOPED_CLIENTS.get() {
        if let Ok(mut map) = pool.write() {
            map.clear();
        }
    }
}

/// 构建 HTTP 客户端（无代理地址时跟随系统代理）
fn build_client(proxy_url: Option<&str>) -> Result<Client, String> {
    build_client_inner(proxy_url, false)
}

/// 构建强制直连的 HTTP 客户端
///
/// 与 `build_client(None)` 不同：这里显式 `no_proxy()`，连系统环境变量里的
/// HTTP_PROXY/ALL_PROXY 也一并忽略。供应商设了「强制直连」却仍走系统代理的话，
/// 这个开关就等于没生效，所以必须显式断绝而不是"不配置代理"。
fn build_direct_client() -> Result<Client, String> {
    build_client_inner(None, true)
}

fn build_client_inner(proxy_url: Option<&str>, force_direct: bool) -> Result<Client, String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Duration::from_secs(60))
        // 禁用 reqwest 自动解压：防止 reqwest 覆盖客户端原始 accept-encoding header。
        // 响应解压由 response_processor 根据 content-encoding 手动处理。
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd();

    // 有代理地址则使用代理，否则跟随系统代理
    if let Some(url) = proxy_url {
        // 先验证 URL 格式和 scheme
        let parsed = url::Url::parse(url)
            .map_err(|e| format!("Invalid proxy URL '{}': {}", mask_url(url), e))?;

        let scheme = parsed.scheme();
        if !["http", "https", "socks5", "socks5h"].contains(&scheme) {
            return Err(format!(
                "Invalid proxy scheme '{}' in URL '{}'. Supported: http, https, socks5, socks5h",
                scheme,
                mask_url(url)
            ));
        }

        let proxy = reqwest::Proxy::all(url)
            .map_err(|e| format!("Invalid proxy URL '{}': {}", mask_url(url), e))?;
        builder = builder.proxy(proxy);
        log::debug!("[GlobalProxy] Proxy configured: {}", mask_url(url));
    } else if force_direct {
        builder = builder.no_proxy();
        log::debug!("[GlobalProxy] Forced direct connection (system proxy ignored)");
    } else {
        // 未设置全局代理时，让 reqwest 自动检测系统代理（环境变量）
        // 若系统代理指向本机，禁用系统代理避免自环
        if system_proxy_points_to_loopback() {
            builder = builder.no_proxy();
            log::warn!(
                "[GlobalProxy] System proxy points to localhost, bypassing to avoid recursion"
            );
        } else {
            log::debug!("[GlobalProxy] Following system proxy (no explicit proxy configured)");
        }
    }

    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

fn system_proxy_points_to_loopback() -> bool {
    const KEYS: [&str; 6] = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];

    KEYS.iter()
        .filter_map(|key| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .any(|value| proxy_points_to_loopback(&value))
}

fn proxy_points_to_loopback(value: &str) -> bool {
    fn host_is_loopback(host: &str) -> bool {
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        host.parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
    }

    // 检查是否指向 CC Switch 自己的代理端口
    // 只有指向自己的代理才需要跳过，避免递归
    fn is_cc_switch_proxy_port(port: Option<u16>) -> bool {
        let cc_switch_port = get_proxy_port();
        port == Some(cc_switch_port)
    }

    if let Ok(parsed) = url::Url::parse(value) {
        if let Some(host) = parsed.host_str() {
            // 只有当主机是 loopback 且端口是 CC Switch 的端口时才返回 true
            return host_is_loopback(host) && is_cc_switch_proxy_port(parsed.port());
        }
        return false;
    }

    let with_scheme = format!("http://{value}");
    if let Ok(parsed) = url::Url::parse(&with_scheme) {
        if let Some(host) = parsed.host_str() {
            return host_is_loopback(host) && is_cc_switch_proxy_port(parsed.port());
        }
    }

    false
}

/// 隐藏 URL 中的敏感信息（用于日志）
pub fn mask_url(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        // 隐藏用户名和密码，保留 scheme、host 和端口
        let host = parsed.host_str().unwrap_or("?");
        match parsed.port() {
            Some(port) => format!("{}://{}:{}", parsed.scheme(), host, port),
            None => format!("{}://{}", parsed.scheme(), host),
        }
    } else {
        // URL 解析失败，返回部分内容。截断点回退到最近的字符边界，
        // 避免在多字节 UTF-8 字符中间切割导致 panic。
        if url.len() > 20 {
            let cut = (0..=20)
                .rev()
                .find(|&i| url.is_char_boundary(i))
                .unwrap_or(0);
            format!("{}...", &url[..cut])
        } else {
            url.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_mask_url() {
        assert_eq!(mask_url("http://127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(
            mask_url("http://user:pass@127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            mask_url("socks5://admin:secret@proxy.example.com:1080"),
            "socks5://proxy.example.com:1080"
        );
        // 无端口的 URL 不应显示 ":?"
        assert_eq!(
            mask_url("http://proxy.example.com"),
            "http://proxy.example.com"
        );
        assert_eq!(
            mask_url("https://user:pass@proxy.example.com"),
            "https://proxy.example.com"
        );
    }

    #[test]
    fn test_mask_url_does_not_panic_on_multibyte_boundary() {
        // 一个无法被 Url::parse 解析、且在字节 20 处正好切在多字节字符中间的字符串。
        // 回归 https://github.com/farion1231/cc-switch 的 mask_url 越界 panic。
        let bad = "这是一个无效的代理地址不能解析";
        assert!(bad.len() > 20 && !bad.is_char_boundary(20));
        let masked = mask_url(bad);
        assert!(masked.ends_with("..."));
    }

    #[test]
    fn test_build_client_direct() {
        let result = build_client(None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_with_http_proxy() {
        let result = build_client(Some("http://127.0.0.1:7890"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_with_socks5_proxy() {
        let result = build_client(Some("socks5://127.0.0.1:1080"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_invalid_url() {
        // reqwest::Proxy::all 对某些无效 URL 不会立即报错
        // 使用明确无效的 scheme 来触发错误
        let result = build_client(Some("invalid-scheme://127.0.0.1:7890"));
        assert!(result.is_err(), "Should reject invalid proxy scheme");
    }

    #[test]
    fn test_build_direct_client_ok() {
        assert!(build_direct_client().is_ok());
    }

    /// `Never` 必须无条件强制直连，即使环境变量里配了系统代理。
    ///
    /// 这是本功能最容易错的点：只要解析或客户端构建有一处回退到"读系统代理"，
    /// 用户设的「强制直连」就静默失效了。
    #[test]
    fn test_never_ignores_system_proxy_env() {
        let _guard = env_lock().lock().unwrap();
        let saved = env::var("HTTP_PROXY").ok();
        env::set_var("HTTP_PROXY", "http://198.51.100.7:8080");

        let sel = ProxySelection::resolve(ProxyMode::Never, None);
        assert_eq!(sel, ProxySelection::Direct);
        assert_eq!(sel.url(), None);
        assert!(sel.is_direct());

        match saved {
            Some(v) => env::set_var("HTTP_PROXY", v),
            None => env::remove_var("HTTP_PROXY"),
        }
    }

    /// `Never` 下即便填了供应商专用地址也必须直连：模式优先于地址。
    #[test]
    fn test_never_ignores_provider_proxy_url() {
        let sel = ProxySelection::resolve(ProxyMode::Never, Some("http://198.51.100.20:8080"));
        assert_eq!(sel, ProxySelection::Direct);
    }

    /// 强制直连的客户端不得继承环境变量代理。
    #[test]
    fn test_direct_client_built_under_system_proxy() {
        let _guard = env_lock().lock().unwrap();
        let saved = env::var("ALL_PROXY").ok();
        env::set_var("ALL_PROXY", "socks5://198.51.100.9:1080");

        assert!(build_direct_client().is_ok());

        match saved {
            Some(v) => env::set_var("ALL_PROXY", v),
            None => env::remove_var("ALL_PROXY"),
        }
    }

    /// `Inherit` 恒为 Inherit，不在解析期读取全局地址（交给全局客户端处理）。
    #[test]
    fn test_inherit_is_independent_of_provider_url() {
        assert_eq!(
            ProxySelection::resolve(ProxyMode::Inherit, None),
            ProxySelection::Inherit
        );
        // Inherit 下专用地址不生效——它只属于 Always
        assert_eq!(
            ProxySelection::resolve(ProxyMode::Inherit, Some("http://198.51.100.30:8080")),
            ProxySelection::Inherit
        );
    }

    /// 核心修复：全局代理为空时，`Always` + 供应商专用地址仍必须走该地址。
    ///
    /// 修复前 `Always` 只读全局地址，全局为空就退化成直连，开关等于无效。
    #[test]
    fn test_always_uses_provider_url_without_global_proxy() {
        let sel = ProxySelection::resolve(ProxyMode::Always, Some("http://198.51.100.11:7890"));
        assert_eq!(
            sel,
            ProxySelection::Proxy("http://198.51.100.11:7890".to_string())
        );
        assert_eq!(sel.url(), Some("http://198.51.100.11:7890"));
        assert!(!sel.is_direct());
    }

    /// 供应商专用地址优先于全局地址，从而支持不同供应商走不同出口。
    #[test]
    fn test_provider_url_takes_precedence_over_global() {
        let _guard = env_lock().lock().unwrap();
        let saved = CURRENT_PROXY_URL
            .get()
            .and_then(|l| l.read().ok())
            .and_then(|u| u.clone());
        let _ = CURRENT_PROXY_URL.set(RwLock::new(Some("http://10.0.0.1:1080".to_string())));
        if let Some(lock) = CURRENT_PROXY_URL.get() {
            if let Ok(mut w) = lock.write() {
                *w = Some("http://10.0.0.1:1080".to_string());
            }
        }

        let sel = ProxySelection::resolve(ProxyMode::Always, Some("http://10.0.0.2:7890"));
        assert_eq!(sel.url(), Some("http://10.0.0.2:7890"));

        // 未填专用地址时回落全局
        let fallback = ProxySelection::resolve(ProxyMode::Always, None);
        assert_eq!(fallback.url(), Some("http://10.0.0.1:1080"));

        if let Some(lock) = CURRENT_PROXY_URL.get() {
            if let Ok(mut w) = lock.write() {
                *w = saved;
            }
        }
    }

    /// 空白专用地址视为未填，避免用户输入空格后静默变成"走空代理"。
    #[test]
    fn test_blank_provider_url_is_ignored() {
        let sel = ProxySelection::resolve(ProxyMode::Always, Some("   "));
        // 全局在测试环境下通常未配置，因此退化为 Direct；关键是不会得到空地址
        assert_ne!(sel.url(), Some("   "));
        assert!(sel.url().is_none_or(|u| !u.trim().is_empty()));
    }

    /// 缓存键必须区分「跟随全局」「走某代理」「强制直连」三者。
    #[test]
    fn test_cache_key_separates_selections() {
        assert!(ProxySelection::Inherit.cache_key().is_none());
        assert_eq!(ProxySelection::Direct.cache_key(), Some(None));
        assert_eq!(
            ProxySelection::Proxy("http://a:1".into()).cache_key(),
            Some(Some("http://a:1".to_string()))
        );
        // 不同地址不得共用同一个客户端
        assert_ne!(
            ProxySelection::Proxy("http://a:1".into()).cache_key(),
            ProxySelection::Proxy("http://b:2".into()).cache_key()
        );
    }

    /// 缺省即 Inherit：老配置没有 proxyMode 字段时不能改变现有行为。
    #[test]
    fn test_proxy_mode_default_is_inherit() {
        assert_eq!(ProxyMode::default(), ProxyMode::Inherit);
    }

    /// 反序列化：字段缺失落到 None，读取时等价 Inherit；显式值按 lowercase 解析。
    #[test]
    fn test_proxy_mode_serde_roundtrip() {
        let missing: Option<ProxyMode> = serde_json::from_str("null").unwrap();
        assert_eq!(missing.unwrap_or_default(), ProxyMode::Inherit);

        assert_eq!(
            serde_json::from_str::<ProxyMode>("\"never\"").unwrap(),
            ProxyMode::Never
        );
        assert_eq!(
            serde_json::from_str::<ProxyMode>("\"always\"").unwrap(),
            ProxyMode::Always
        );
        assert_eq!(
            serde_json::to_string(&ProxyMode::Never).unwrap(),
            "\"never\""
        );
    }

    #[test]
    fn test_proxy_points_to_loopback() {
        // 设置 CC Switch 代理端口为 15721（默认值）
        set_proxy_port(15721);

        // 只有指向 CC Switch 自己端口的 loopback 地址才返回 true
        assert!(proxy_points_to_loopback("http://127.0.0.1:15721"));
        assert!(proxy_points_to_loopback("socks5://localhost:15721"));
        assert!(proxy_points_to_loopback("127.0.0.1:15721"));

        // 其他 loopback 端口不应该被跳过（允许使用其他本地代理工具）
        assert!(!proxy_points_to_loopback("http://127.0.0.1:7890"));
        assert!(!proxy_points_to_loopback("socks5://localhost:1080"));

        // 非 loopback 地址不应该被跳过
        assert!(!proxy_points_to_loopback("http://192.168.1.10:7890"));
        assert!(!proxy_points_to_loopback("http://192.168.1.10:15721"));
    }

    #[test]
    fn test_system_proxy_points_to_loopback() {
        let _guard = env_lock().lock().unwrap();

        // 设置 CC Switch 代理端口
        set_proxy_port(15721);

        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];

        for key in &keys {
            std::env::remove_var(key);
        }

        // 指向 CC Switch 端口的代理应该被跳过
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:15721");
        assert!(system_proxy_points_to_loopback());

        // 指向其他端口的本地代理不应该被跳过
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:7890");
        assert!(!system_proxy_points_to_loopback());

        // 非 loopback 地址不应该被跳过
        std::env::set_var("HTTP_PROXY", "http://10.0.0.2:7890");
        assert!(!system_proxy_points_to_loopback());

        for key in &keys {
            std::env::remove_var(key);
        }
    }
}
