//! Claude Code 客户端伪装的单一 profile 来源。
//!
//! 这里的常量随上游二进制版本走——跟版本时**只改本文件**。
//! 实测自 `~/.local/share/claude/versions/2.1.196`：
//!   BUILD_TIME=2026-06-29T00:53:27Z, GIT_SHA=a4ca500b…,
//!   @anthropic-ai/sdk(Stainless)=0.94.0, 打包运行时 Node=v26.3.0。
//!
//! 注意：version / build_time / node_version / stainless 版本都**随二进制固定**，
//! 同一 Claude Code 版本的所有真实客户端这些字段完全一致；它们不是逐机器变化的指纹，
//! 所以应统一用本 profile，而非按账号存值漂移（否则反而成为破绽）。
//! 逐机器变化的只有 platform / arch / terminal / device_id 等，仍由账号 canonical_env 提供。

/// 当前模仿的 Claude Code CLI 版本号。
pub const VERSION: &str = "2.1.196";

/// 二进制 `BUILD_TIME` 常量（env 指纹 `build_time` 字段）。
pub const BUILD_TIME: &str = "2026-06-29T00:53:27Z";

/// 二进制 `GIT_SHA` 常量。
pub const GIT_SHA: &str = "a4ca500badcac68511fb5f04303e32e4360f3dfb";

/// `X-Stainless-Package-Version`（`@anthropic-ai/sdk` 版本）。
pub const STAINLESS_PACKAGE_VERSION: &str = "0.94.0";

/// `X-Stainless-Runtime-Version`（打包 Node 运行时版本，整版本固定）。
pub const NODE_VERSION: &str = "v26.3.0";

/// `/v1/messages` 等主 API 的 User-Agent：`claude-cli/{version} (external, cli)`。
pub fn user_agent_cli(version: &str) -> String {
    format!("claude-cli/{version} (external, cli)")
}

/// bootstrap / usage / telemetry 类端点的 User-Agent：`claude-code/{version}`。
pub fn user_agent_code(version: &str) -> String {
    format!("claude-code/{version}")
}

/// 把 Node `process.platform` 映射为 Stainless `X-Stainless-OS` 值。
/// 实测自二进制：`darwin→MacOS`、`win32→Windows`、其它→`Linux`。
pub fn stainless_os(platform: &str) -> &'static str {
    match platform {
        "darwin" => "MacOS",
        "win32" => "Windows",
        _ => "Linux",
    }
}
