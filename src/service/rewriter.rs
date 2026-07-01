use base64::Engine;
use once_cell::sync::Lazy;
use rand::Rng;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::model::account::{
    Account, BillingMode, CanonicalEnvData, CanonicalProcessData, CanonicalPromptEnvData,
};
use crate::model::mimic_profile;

/// header wire 大小写映射。
/// Go 的 HTTP 服务器规范化 header，此映射还原 Claude CLI 抓包原始大小写。
static HEADER_WIRE_CASING: Lazy<HashMap<&str, &str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("accept", "Accept");
    m.insert("user-agent", "User-Agent");
    m.insert("x-stainless-retry-count", "X-Stainless-Retry-Count");
    m.insert("x-stainless-timeout", "X-Stainless-Timeout");
    m.insert("x-stainless-lang", "X-Stainless-Lang");
    m.insert("x-stainless-package-version", "X-Stainless-Package-Version");
    m.insert("x-stainless-os", "X-Stainless-OS");
    m.insert("x-stainless-arch", "X-Stainless-Arch");
    m.insert("x-stainless-runtime", "X-Stainless-Runtime");
    m.insert("x-stainless-runtime-version", "X-Stainless-Runtime-Version");
    m.insert("x-stainless-helper-method", "x-stainless-helper-method");
    m.insert(
        "anthropic-dangerous-direct-browser-access",
        "anthropic-dangerous-direct-browser-access",
    );
    m.insert("anthropic-version", "anthropic-version");
    m.insert("anthropic-beta", "anthropic-beta");
    m.insert("x-app", "x-app");
    m.insert("content-type", "content-type");
    m.insert("accept-language", "accept-language");
    m.insert("sec-fetch-mode", "sec-fetch-mode");
    m.insert("accept-encoding", "accept-encoding");
    m.insert("authorization", "authorization");
    m.insert("x-claude-code-session-id", "X-Claude-Code-Session-Id");
    m.insert("x-client-request-id", "x-client-request-id");
    m.insert("content-length", "content-length");
    m
});

/// 将规范化 key 转换为真实 wire 大小写。
fn resolve_wire_casing(key: &str) -> String {
    let lower = key.to_lowercase();
    if let Some(wk) = HEADER_WIRE_CASING.get(lower.as_str()) {
        wk.to_string()
    } else {
        key.to_string()
    }
}

/// 请求来源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientType {
    ClaudeCode,
    API,
}

/// 剥离 model id 末尾的 `[1m]` 后缀（Claude Code CLI 用于标记 1M 上下文模式）。
/// 返回 (去后缀的 model_id, 是否命中 1m)。Anthropic API 不认 `[1m]`，必须剥离并
/// 另外发 `context-1m-2025-08-07` beta。
fn strip_1m_suffix(model_id: &str) -> (&str, bool) {
    if let Some(stripped) = model_id.strip_suffix("[1m]") {
        (stripped, true)
    } else {
        (model_id, false)
    }
}

/// 根据模型返回正确的 anthropic-beta 值。
///
/// 实测自 Claude Code 2.1.196 `/v1/messages` 抓包（API key / AuthToken / OAuth 三种
/// 认证形态在 messages 上的 beta 集一致；**均不发** `oauth-2025-04-20` 与
/// `redact-thinking-2026-02-12`——这两个只用于 `/v1/files` 等其它端点）。
///
/// 顺序参照 opus（最全样本）；sonnet/haiku 为其子集：
/// - opus-4-8 ：claude-code, context-1m, interleaved-thinking, thinking-token-count,
///   context-management, prompt-caching-scope, mid-conversation-system, advisor-tool,
///   advanced-tool-use, effort, cache-diagnosis
/// - sonnet-4-6：去掉 context-1m / mid-conversation-system / advanced-tool-use / cache-diagnosis
/// - haiku-4-5 ：再去掉 effort（haiku 不带 output_config.effort），但**仍发** claude-code-20250219
///
/// 注：本函数主要服务 **API 模式**（把裸 API client 伪装成 Claude Code）与 OAuth 校验；
/// CC 客户端模式下原样保留客户端自带 beta，不调用本函数 clobber。
pub fn compute_betas_for_model(model_id: &str) -> Vec<&'static str> {
    let (base, needs_1m) = strip_1m_suffix(model_id);
    let lower = base.to_lowercase();
    let is_claude3 = lower.contains("claude-3-");
    // 现代特性等价于「非 legacy claude-3」（实测 haiku-4-5 同样发送 claude-code-20250219）
    let modern = !is_claude3;
    let is_haiku = lower.contains("haiku");
    // 仅现代 opus（opus-4+）享受 opus 专属 beta；claude-3-opus 走 legacy
    let is_opus = lower.contains("claude-opus-4");
    // modelSupportsContextManagement：Claude 4+
    let is_claude4_plus = lower.contains("claude-opus-4")
        || lower.contains("claude-sonnet-4")
        || lower.contains("claude-haiku-4")
        || lower.contains("claude-fable-");

    let mut out: Vec<&'static str> = Vec::new();
    if modern {
        out.push("claude-code-20250219");
    }
    // opus 默认开启 1M 上下文；其它模型仅在 [1m] 后缀时
    if needs_1m || is_opus {
        out.push("context-1m-2025-08-07");
    }
    if modern {
        out.push("interleaved-thinking-2025-05-14");
        out.push("thinking-token-count-2026-05-13");
    }
    if is_claude4_plus {
        out.push("context-management-2025-06-27");
    }
    out.push("prompt-caching-scope-2026-01-05");
    if is_opus {
        out.push("mid-conversation-system-2026-04-07");
    }
    if modern {
        out.push("advisor-tool-2026-03-01");
    }
    if is_opus {
        out.push("advanced-tool-use-2025-11-20");
    }
    // effort 仅 opus/sonnet（haiku 不带 output_config.effort）
    if is_claude4_plus && !is_haiku {
        out.push("effort-2025-11-24");
    }
    if is_opus {
        out.push("cache-diagnosis-2026-04-07");
    }
    out
}

fn beta_header_for_model(model_id: &str) -> String {
    compute_betas_for_model(model_id).join(",")
}

/// 处理所有请求的反检测改写。
pub struct Rewriter;

impl Rewriter {
    pub fn new() -> Self {
        Self
    }

    // --- Header 改写 ---

    /// 处理出站 header 的反检测改写。
    pub fn rewrite_headers(
        &self,
        headers: &HashMap<String, String>,
        account: &Account,
        client_type: ClientType,
        model_id: &str,
        body_map: &serde_json::Value,
    ) -> HashMap<String, String> {
        let env = self.parse_env(account);
        // version 仅用于 API 模式合成 UA/stainless；CC 模式逐字透传客户端原值（见下），
        // 避免 mimic_profile 与客户端版本漂移导致 UA↔billing 不自洽。
        let version = mimic_profile::VERSION;

        let mut out = HashMap::new();

        if client_type == ClientType::API {
            // API 模式：使用与真实 Claude CLI 匹配的固定 header 集合。
            out.insert("Accept".into(), "application/json".into());
            out.insert("User-Agent".into(), mimic_profile::user_agent_cli(version));
            out.insert(
                "anthropic-beta".into(),
                beta_header_for_model(model_id).into(),
            );
            out.insert("anthropic-version".into(), "2023-06-01".into());
            out.insert(
                "anthropic-dangerous-direct-browser-access".into(),
                "true".into(),
            );
            out.insert("x-app".into(), "cli".into());
            out.insert("content-type".into(), "application/json".into());
            out.insert("accept-encoding".into(), "gzip, deflate, br, zstd".into());
            let stainless_os = stainless_os_from_platform(&env.platform);
            out.insert("X-Stainless-Lang".into(), "js".into());
            out.insert(
                "X-Stainless-Package-Version".into(),
                mimic_profile::STAINLESS_PACKAGE_VERSION.into(),
            );
            out.insert("X-Stainless-OS".into(), stainless_os.into());
            out.insert("X-Stainless-Arch".into(), env.arch.clone());
            out.insert("X-Stainless-Runtime".into(), "node".into());
            out.insert(
                "X-Stainless-Runtime-Version".into(),
                mimic_profile::NODE_VERSION.into(),
            );
            out.insert("X-Stainless-Retry-Count".into(), "0".into());
            out.insert("X-Stainless-Timeout".into(), "600".into());

            let session_id =
                extract_session_id_from_body(body_map).unwrap_or_else(generate_session_uuid);
            out.insert("X-Claude-Code-Session-Id".into(), session_id);
            out.insert("x-client-request-id".into(), generate_session_uuid());
        } else {
            // CC 客户端模式：白名单 + 改写
            let allowed: std::collections::HashSet<&str> = [
                "accept",
                "user-agent",
                "content-type",
                "accept-encoding",
                "accept-language",
                "anthropic-beta",
                "anthropic-version",
                "anthropic-dangerous-direct-browser-access",
                "x-app",
                "sec-fetch-mode",
                "x-stainless-retry-count",
                "x-stainless-timeout",
                "x-stainless-lang",
                "x-stainless-package-version",
                "x-stainless-os",
                "x-stainless-arch",
                "x-stainless-runtime",
                "x-stainless-runtime-version",
                "x-stainless-helper-method",
                "x-claude-code-session-id",
                "x-client-request-id",
            ]
            .into_iter()
            .collect();

            let stainless_os = stainless_os_from_platform(&env.platform);
            for (k, v) in headers {
                let lower = k.to_lowercase();
                if !allowed.contains(lower.as_str()) {
                    continue;
                }
                let wire_key = resolve_wire_casing(k);
                match lower.as_str() {
                    // OS/arch 编码设备身份 → 保持池账号身份（与 body Platform/OS 改写一致）
                    "x-stainless-os" => {
                        out.insert(wire_key, stainless_os.to_string());
                    }
                    "x-stainless-arch" => {
                        out.insert(wire_key, env.arch.clone());
                    }
                    // user-agent / x-stainless-package-version / x-stainless-runtime-version
                    // 编码客户端版本 → 逐字透传（与 billing 透传哲学一致）。真实 CC 客户端 UA
                    // 本就权威；强钉 mimic_profile 会在官方升级后造成 UA↔billing 版本漂移。
                    _ => {
                        out.insert(wire_key, v.clone());
                    }
                }
            }

            // 确保必需 header 存在
            out.entry("anthropic-dangerous-direct-browser-access".into())
                .or_insert_with(|| "true".into());

            // CC 客户端模式：原样保留客户端自带的 anthropic-beta。真实 Claude Code 已按
            // 模型/认证/参数发对了现代 beta 集（含顺序），用旧集合 merge 反而会引入错误 token
            // （如已被 messages 移除的 oauth-2025-04-20）。仅当客户端没带 beta 时才兜底计算。
            let existing_beta = out.get("anthropic-beta").cloned().unwrap_or_default();
            if existing_beta.trim().is_empty() {
                out.insert("anthropic-beta".into(), beta_header_for_model(model_id));
            }
        }

        out
    }

    // --- Body 改写 ---

    /// 根据端点和客户端类型改写请求体。
    pub fn rewrite_body(
        &self,
        body: &[u8],
        path: &str,
        account: &Account,
        client_type: ClientType,
    ) -> Vec<u8> {
        if body.is_empty() {
            return body.to_vec();
        }

        let mut parsed: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => return body.to_vec(), // 非 JSON，直接透传
        };

        if path.starts_with("/v1/messages") {
            strip_empty_text_blocks(&mut parsed);
            self.rewrite_messages(&mut parsed, account, client_type);
        } else if path.contains("/event_logging/batch") || path.contains("/event_logging/v2/batch")
        {
            self.rewrite_event_batch(&mut parsed, account);
        } else if path.starts_with("/api/eval/") {
            self.rewrite_growthbook_eval(&mut parsed, account);
        } else {
            self.rewrite_generic_identity(&mut parsed, account);
        }

        serde_json::to_vec(&parsed).unwrap_or_else(|_| body.to_vec())
    }

    /// 处理 /v1/messages 请求体。
    fn rewrite_messages(
        &self,
        body: &mut serde_json::Value,
        account: &Account,
        client_type: ClientType,
    ) {
        let env = self.parse_env(account);
        let prompt_env = self.parse_prompt_env(account);

        // Claude Code CLI 把 `[1m]` 后缀当作"启用 1M 上下文"的标记；Anthropic API 不认，
        // 必须剥离成真实 model id（beta header 里另加 `context-1m-2025-08-07`）。
        if let Some(obj) = body.as_object_mut() {
            if let Some(m) = obj.get("model").and_then(|v| v.as_str()) {
                if let Some(stripped) = m.strip_suffix("[1m]") {
                    obj.insert(
                        "model".into(),
                        serde_json::Value::String(stripped.to_string()),
                    );
                }
            }
        }

        if client_type == ClientType::ClaudeCode {
            // 替换模式
            self.rewrite_metadata_user_id(body, account);
            self.rewrite_system_prompt(body, &prompt_env, &env.version, &account.billing_mode);
            scrub_git_user_in_reminders(body, &account.name);
        } else {
            // 注入模式
            let session_id = self.inject_metadata_user_id(body, account);
            if let Some(sid) = &session_id {
                if let Some(metadata) = body.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                    metadata.insert("_session_id".into(), serde_json::Value::String(sid.clone()));
                }
            }

            // 剥离 Claude Code 不会发送的字段
            if let Some(obj) = body.as_object_mut() {
                obj.remove("temperature");
                obj.remove("top_k");
                obj.remove("top_p");
                obj.remove("stop_sequences");
                obj.remove("tool_choice");

                // 确保 tools 字段存在
                obj.entry("tools")
                    .or_insert(serde_json::Value::Array(vec![]));

                // 确保 stream 为 true
                obj.insert("stream".into(), serde_json::Value::Bool(true));
            }

            // 剥离 system 块中的 cache_control
            strip_cache_control(body);

            // 规范化 max_tokens
            if let Some(max_tokens) = body.get("max_tokens").and_then(|v| v.as_f64()) {
                if max_tokens > 32768.0 {
                    body.as_object_mut()
                        .unwrap()
                        .insert("max_tokens".into(), serde_json::json!(16384));
                }
            }

            // 注入 Claude Code 系统提示词
            self.inject_system_prompt(body);
        }
    }

    /// 替换已有 metadata.user_id 中的 device_id（CC 客户端模式）。
    fn rewrite_metadata_user_id(&self, body: &mut serde_json::Value, account: &Account) {
        let user_id_str = {
            let metadata = match body.get("metadata").and_then(|m| m.as_object()) {
                Some(m) => m,
                None => return,
            };
            match metadata.get("user_id").and_then(|u| u.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return,
            }
        };

        // 尝试 JSON 格式
        if let Ok(mut uid) = serde_json::from_str::<serde_json::Value>(&user_id_str) {
            if let Some(obj) = uid.as_object_mut() {
                obj.insert(
                    "device_id".into(),
                    serde_json::Value::String(account.device_id.clone()),
                );
                // 补 account_uuid：真实 claude 会从 OAuth 带上 account_uuid；池账号也有，
                // 入口若缺则用账号值补齐，避免 metadata 与身份不自洽。
                let uuid = account
                    .account_uuid
                    .clone()
                    .unwrap_or_else(|| derive_account_uuid(account));
                obj.insert("account_uuid".into(), serde_json::Value::String(uuid));
                let new_str = serde_json::to_string(&uid).unwrap_or_default();
                if let Some(metadata) = body.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                    metadata.insert("user_id".into(), serde_json::Value::String(new_str));
                }
                return;
            }
        }

        // 旧格式：user_{device}_account_{uuid}_session_{uuid}
        if let Some(idx) = user_id_str.find("_account_") {
            let new_val = format!(
                "user_{}_account_{}",
                account.device_id,
                &user_id_str[idx + 9..]
            );
            if let Some(metadata) = body.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                metadata.insert("user_id".into(), serde_json::Value::String(new_val));
            }
        }
    }

    /// 为纯 API 调用创建 metadata.user_id。返回使用的 session_id。
    fn inject_metadata_user_id(
        &self,
        body: &mut serde_json::Value,
        account: &Account,
    ) -> Option<String> {
        // 确保 metadata 存在
        if body.get("metadata").is_none() {
            body.as_object_mut()
                .unwrap()
                .insert("metadata".into(), serde_json::json!({}));
        }

        // 已有 user_id，改为改写
        if body
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .is_some()
        {
            self.rewrite_metadata_user_id(body, account);
            return None;
        }

        let session_id = generate_session_uuid();
        let account_uuid = account.account_uuid.clone().unwrap_or_default();
        let uid = serde_json::json!({
            "device_id": account.device_id,
            "account_uuid": account_uuid,
            "session_id": session_id,
        });
        let uid_str = serde_json::to_string(&uid).unwrap_or_default();
        if let Some(metadata) = body.get_mut("metadata").and_then(|m| m.as_object_mut()) {
            metadata.insert("user_id".into(), serde_json::Value::String(uid_str));
        }
        Some(session_id)
    }

    /// 将 Claude Code 系统提示词添加到请求体前面（仅 API 注入模式）。
    fn inject_system_prompt(&self, body: &mut serde_json::Value) {
        let banner_block = serde_json::json!({
            "type": "text",
            "text": CLAUDE_CODE_SYSTEM_PROMPT,
            "cache_control": { "type": "ephemeral" }
        });

        match body.get("system") {
            None => {
                body.as_object_mut().unwrap().insert(
                    "system".into(),
                    serde_json::Value::Array(vec![banner_block]),
                );
            }
            Some(serde_json::Value::String(sys)) => {
                if sys.starts_with(CLAUDE_CODE_SYSTEM_PROMPT) {
                    return;
                }
                let user_block = serde_json::json!({
                    "type": "text",
                    "text": sys,
                });
                body.as_object_mut().unwrap().insert(
                    "system".into(),
                    serde_json::Value::Array(vec![banner_block, user_block]),
                );
            }
            Some(serde_json::Value::Array(arr)) => {
                if let Some(first) = arr.first() {
                    if let Some(text) = first.get("text").and_then(|t| t.as_str()) {
                        if text.starts_with(CLAUDE_CODE_SYSTEM_PROMPT) {
                            return;
                        }
                    }
                }
                let mut new_arr = vec![banner_block];
                new_arr.extend(arr.iter().cloned());
                body.as_object_mut()
                    .unwrap()
                    .insert("system".into(), serde_json::Value::Array(new_arr));
            }
            _ => {}
        }
    }

    // --- 系统提示词改写（仅 CC 客户端模式）---

    fn rewrite_system_prompt(
        &self,
        body: &mut serde_json::Value,
        pe: &CanonicalPromptEnvData,
        version: &str,
        billing_mode: &BillingMode,
    ) {
        let _ = version; // 版本随二进制固定（mimic_profile）；billing 不再据此重算

        let rewrite = |text: &str| -> String {
            let mut text = text.to_string();
            // billing header：
            // - Rewrite：原样透传，不重算 cc_version 后缀、不重置 cch。
            //   cc_version 的 3-hex 后缀只依赖「首条用户文本 + 版本号」（二进制 two()/_tf()），
            //   cc-bridge 不改这两者，故客户端自带的后缀对修改后 body 依然正确；重算反而会
            //   因取到 <system-reminder> 文本而算错（cd9 ≠ 官方 1f5）。
            //   cch attestation（5-hex）依赖完整 body 序列化，cc-bridge 改 device_id 后本需
            //   重算，但其算法在 2.1.196 已变（旧 xxh64 seed 失效），待逆向——此处保留客户端
            //   原值不动。OAuth 池不发 cch，此限制仅影响 firstParty/vertex 池。
            // - Strip：删除整行 billing header。
            if *billing_mode != BillingMode::Rewrite {
                text = BILLING_LINE_REGEX.replace_all(&text, "").to_string();
                text = BILLING_REGEX.replace_all(&text, "").to_string();
            }
            text = PLATFORM_REGEX
                .replace_all(&text, &format!("Platform: {}", pe.platform))
                .to_string();
            text = SHELL_REGEX
                .replace_all(&text, &format!("Shell: {}", pe.shell))
                .to_string();
            text = OS_VERSION_REGEX
                .replace_all(&text, &format!("OS Version: {}", pe.os_version))
                .to_string();
            text = WORKING_DIR_REGEX
                .replace_all(&text, &format!("${{1}}{}", pe.working_dir))
                .to_string();
            let home_prefix = if let Some(idx) = nth_index(&pe.working_dir, '/', 3) {
                &pe.working_dir[..idx + 1]
            } else {
                &pe.working_dir
            };
            text = HOME_PATH_REGEX.replace_all(&text, home_prefix).to_string();
            text
        };

        let rewrite_in_reminders = |text: &str| -> String {
            SYSTEM_REMINDER_REGEX
                .replace_all(text, |caps: &regex::Captures| rewrite(&caps[0]))
                .to_string()
        };

        // 改写 body.system
        match body.get("system").cloned() {
            Some(serde_json::Value::String(sys)) => {
                body.as_object_mut()
                    .unwrap()
                    .insert("system".into(), serde_json::Value::String(rewrite(&sys)));
            }
            Some(serde_json::Value::Array(sys)) => {
                let filtered: Vec<serde_json::Value> = if *billing_mode == BillingMode::Strip {
                    sys.iter()
                        .filter(|item| {
                            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                if BILLING_LINE_REGEX.is_match(text) {
                                    let cleaned =
                                        BILLING_LINE_REGEX.replace_all(text, "").to_string();
                                    if cleaned.trim().is_empty() {
                                        return false;
                                    }
                                }
                            }
                            true
                        })
                        .cloned()
                        .collect()
                } else {
                    sys.clone()
                };

                let rewritten: Vec<serde_json::Value> = filtered
                    .into_iter()
                    .map(|mut item| {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            let new_text = rewrite(text);
                            item.as_object_mut()
                                .unwrap()
                                .insert("text".into(), serde_json::Value::String(new_text));
                        }
                        item
                    })
                    .collect();

                body.as_object_mut()
                    .unwrap()
                    .insert("system".into(), serde_json::Value::Array(rewritten));
            }
            _ => {}
        }

        // 改写消息 — 仅在 <system-reminder> 标签内替换
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for msg in messages.iter_mut() {
                rewrite_message_content(msg, &rewrite_in_reminders);
            }
        }
    }

    // --- 事件日志批量改写 ---

    fn rewrite_event_batch(&self, body: &mut serde_json::Value, account: &Account) {
        let env = self.parse_env(account);
        let proc = self.parse_process(account);

        let events = match body.get_mut("events").and_then(|e| e.as_array_mut()) {
            Some(e) => e,
            None => return,
        };

        let canonical_env = build_canonical_env_map(&env);

        for event in events.iter_mut() {
            // v2 envelope（/api/event_logging/v2/batch）把身份字段放在 event_data 内；
            // 旧扁平 schema 直接在 event 顶层。两者都要正确改写，否则真实 device_id 会漏改泄漏。
            let has_event_data = event
                .get("event_data")
                .map(|d| d.is_object())
                .unwrap_or(false);
            let e = if has_event_data {
                match event.get_mut("event_data").and_then(|d| d.as_object_mut()) {
                    Some(e) => e,
                    None => continue,
                }
            } else {
                match event.as_object_mut() {
                    Some(e) => e,
                    None => continue,
                }
            };

            if e.contains_key("device_id") {
                e.insert(
                    "device_id".into(),
                    serde_json::Value::String(account.device_id.clone()),
                );
            }
            if e.contains_key("email") {
                e.insert(
                    "email".into(),
                    serde_json::Value::String(account.email.clone()),
                );
            }

            e.remove("baseUrl");
            e.remove("base_url");
            e.remove("gateway");

            // 改写 account_uuid / organization_uuid
            if e.contains_key("account_uuid") {
                let uuid = account
                    .account_uuid
                    .clone()
                    .unwrap_or_else(|| derive_account_uuid(account));
                e.insert("account_uuid".into(), serde_json::Value::String(uuid));
            }
            if e.contains_key("organization_uuid") {
                if let Some(ref org) = account.organization_uuid {
                    e.insert(
                        "organization_uuid".into(),
                        serde_json::Value::String(org.clone()),
                    );
                } else {
                    e.remove("organization_uuid");
                }
            }

            if e.contains_key("env") {
                e.insert("env".into(), canonical_env.clone());
            }

            if let Some(p) = e.remove("process") {
                e.insert("process".into(), rewrite_process(&p, &proc));
            }

            if let Some(am) = e.get("additional_metadata").and_then(|v| v.as_str()) {
                let rewritten = rewrite_additional_metadata(am);
                e.insert(
                    "additional_metadata".into(),
                    serde_json::Value::String(rewritten),
                );
            }

            // 改写 user_attributes（GrowthBook 实验事件中的 JSON 字符串）
            if let Some(ua_str) = e.get("user_attributes").and_then(|v| v.as_str()) {
                let rewritten = rewrite_user_attributes_json(ua_str, account);
                e.insert(
                    "user_attributes".into(),
                    serde_json::Value::String(rewritten),
                );
            }
        }
    }

    // --- GrowthBook remoteEval 改写 (POST /api/eval/{clientKey}) ---

    fn rewrite_growthbook_eval(&self, body: &mut serde_json::Value, account: &Account) {
        let env = self.parse_env(account);
        let attrs = match body.get_mut("attributes").and_then(|a| a.as_object_mut()) {
            Some(a) => a,
            None => return,
        };

        // 身份字段
        attrs.insert(
            "id".into(),
            serde_json::Value::String(account.device_id.clone()),
        );
        attrs.insert(
            "deviceID".into(),
            serde_json::Value::String(account.device_id.clone()),
        );

        if attrs.contains_key("email") {
            attrs.insert(
                "email".into(),
                serde_json::Value::String(account.email.clone()),
            );
        }
        if attrs.contains_key("accountUUID") {
            let uuid = account
                .account_uuid
                .clone()
                .unwrap_or_else(|| derive_account_uuid(account));
            attrs.insert("accountUUID".into(), serde_json::Value::String(uuid));
        }
        if let Some(ref org) = account.organization_uuid {
            attrs.insert(
                "organizationUUID".into(),
                serde_json::Value::String(org.clone()),
            );
        } else {
            attrs.remove("organizationUUID");
        }
        if let Some(ref sub) = account.subscription_type {
            attrs.insert(
                "subscriptionType".into(),
                serde_json::Value::String(sub.clone()),
            );
        }

        // 移除代理暴露字段
        attrs.remove("apiBaseUrlHost");

        // 环境对齐
        attrs.insert(
            "platform".into(),
            serde_json::Value::String(env.platform.clone()),
        );
        if attrs.contains_key("appVersion") {
            attrs.insert(
                "appVersion".into(),
                serde_json::Value::String(env.version.clone()),
            );
        }
    }

    // --- 通用身份改写 ---

    fn rewrite_generic_identity(&self, body: &mut serde_json::Value, account: &Account) {
        if let Some(obj) = body.as_object_mut() {
            if obj.contains_key("device_id") {
                obj.insert(
                    "device_id".into(),
                    serde_json::Value::String(account.device_id.clone()),
                );
            }
            if obj.contains_key("email") {
                obj.insert(
                    "email".into(),
                    serde_json::Value::String(account.email.clone()),
                );
            }
        }
    }

    // --- 辅助解析 ---

    fn parse_env(&self, account: &Account) -> CanonicalEnvData {
        serde_json::from_value(account.canonical_env.clone()).unwrap_or_default()
    }

    fn parse_prompt_env(&self, account: &Account) -> CanonicalPromptEnvData {
        serde_json::from_value(account.canonical_prompt.clone()).unwrap_or_default()
    }

    fn parse_process(&self, account: &Account) -> CanonicalProcessData {
        serde_json::from_value(account.canonical_process.clone()).unwrap_or_default()
    }
}

// --- 正则表达式 ---

static PLATFORM_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"Platform:\s*\S+").unwrap());
static SHELL_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"Shell:\s*[^\n<]+").unwrap());
static OS_VERSION_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"OS Version:\s*[^\n<]+").unwrap());
static WORKING_DIR_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"((?:Primary )?[Ww]orking directory:\s*)/\S+").unwrap());
static HOME_PATH_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/(?:Users|home)/[^/\s]+/").unwrap());
static BILLING_LINE_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*x-anthropic-billing-header:[^\n]*\n?").unwrap());
static BILLING_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"cc_version=[\d.]+\.[a-f0-9]{3};[^;]*;?").unwrap());
static GIT_USER_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"Git user:\s*[^\n]+").unwrap());
static SYSTEM_REMINDER_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)<system-reminder>(.*?)</system-reminder>").unwrap());

/// 从 messages 数组中提取首条用户消息文本。
fn extract_first_user_message(body: &serde_json::Value) -> String {
    let messages = match body.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return String::new(),
    };
    for msg in messages {
        let m = match msg.as_object() {
            Some(m) => m,
            None => continue,
        };
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        match m.get("content") {
            Some(serde_json::Value::String(c)) => return c.clone(),
            Some(serde_json::Value::Array(arr)) => {
                for item in arr {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        // 跳过 <system-reminder> 包裹块：官方 two()/_tf() 在注入 reminder
                        // 之前计算，取的是真实用户文本。wire body 上 reminder 与真实文本混在
                        // 同一 content 数组，必须跳过，否则取到 reminder 首字节算错后缀。
                        if text.trim_start().starts_with("<system-reminder>") {
                            continue;
                        }
                        return text.to_string();
                    }
                }
            }
            _ => {}
        }
    }
    String::new()
}

fn rewrite_message_content<F>(msg: &mut serde_json::Value, rewrite_fn: &F)
where
    F: Fn(&str) -> String,
{
    match msg.get("content").cloned() {
        Some(serde_json::Value::String(s)) => {
            msg.as_object_mut()
                .unwrap()
                .insert("content".into(), serde_json::Value::String(rewrite_fn(&s)));
        }
        Some(serde_json::Value::Array(arr)) => {
            let rewritten: Vec<serde_json::Value> = arr
                .into_iter()
                .map(|mut item| {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        let new_text = rewrite_fn(text);
                        item.as_object_mut()
                            .unwrap()
                            .insert("text".into(), serde_json::Value::String(new_text));
                    }
                    item
                })
                .collect();
            msg.as_object_mut()
                .unwrap()
                .insert("content".into(), serde_json::Value::Array(rewritten));
        }
        _ => {}
    }
}

fn build_canonical_env_map(env: &CanonicalEnvData) -> serde_json::Value {
    crate::model::identity::build_full_env_json(env)
}

// --- 进程指纹改写 ---

fn rewrite_process(original: &serde_json::Value, proc: &CanonicalProcessData) -> serde_json::Value {
    let engine = base64::engine::general_purpose::STANDARD;
    match original {
        serde_json::Value::String(s) => {
            let decoded = match engine.decode(s) {
                Ok(d) => d,
                Err(_) => return original.clone(),
            };
            let mut obj: serde_json::Value = match serde_json::from_slice(&decoded) {
                Ok(v) => v,
                Err(_) => return original.clone(),
            };
            rewrite_process_fields(&mut obj, proc);
            let out = serde_json::to_vec(&obj).unwrap_or_default();
            serde_json::Value::String(engine.encode(&out))
        }
        serde_json::Value::Object(_) => {
            let mut obj = original.clone();
            rewrite_process_fields(&mut obj, proc);
            obj
        }
        _ => original.clone(),
    }
}

fn rewrite_process_fields(obj: &mut serde_json::Value, proc: &CanonicalProcessData) {
    if let Some(map) = obj.as_object_mut() {
        map.insert(
            "constrainedMemory".into(),
            serde_json::json!(proc.constrained_memory),
        );
        map.insert(
            "rss".into(),
            serde_json::json!(random_in_range(proc.rss_range[0], proc.rss_range[1])),
        );
        map.insert(
            "heapTotal".into(),
            serde_json::json!(random_in_range(
                proc.heap_total_range[0],
                proc.heap_total_range[1]
            )),
        );
        map.insert(
            "heapUsed".into(),
            serde_json::json!(random_in_range(
                proc.heap_used_range[0],
                proc.heap_used_range[1]
            )),
        );
        map.insert(
            "external".into(),
            serde_json::json!(random_in_range(
                proc.external_range[0],
                proc.external_range[1]
            )),
        );
        map.insert(
            "arrayBuffers".into(),
            serde_json::json!(random_in_range(
                proc.array_buffers_range[0],
                proc.array_buffers_range[1]
            )),
        );
    }
}

// --- Base64 additional_metadata 改写 ---

fn rewrite_additional_metadata(encoded: &str) -> String {
    let engine = base64::engine::general_purpose::STANDARD;
    let decoded = match engine.decode(encoded) {
        Ok(d) => d,
        Err(_) => return encoded.to_string(),
    };
    let mut obj: serde_json::Value = match serde_json::from_slice(&decoded) {
        Ok(v) => v,
        Err(_) => return encoded.to_string(),
    };
    if let Some(map) = obj.as_object_mut() {
        map.remove("baseUrl");
        map.remove("base_url");
        map.remove("gateway");
    }
    let out = serde_json::to_vec(&obj).unwrap_or_default();
    engine.encode(&out)
}

/// 改写 GrowthBook 实验事件中 user_attributes JSON 字符串内的身份字段。
fn rewrite_user_attributes_json(json_str: &str, account: &Account) -> String {
    let mut obj: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };
    if let Some(map) = obj.as_object_mut() {
        if map.contains_key("id") {
            map.insert(
                "id".into(),
                serde_json::Value::String(account.device_id.clone()),
            );
        }
        if map.contains_key("deviceID") {
            map.insert(
                "deviceID".into(),
                serde_json::Value::String(account.device_id.clone()),
            );
        }
        if map.contains_key("email") {
            map.insert(
                "email".into(),
                serde_json::Value::String(account.email.clone()),
            );
        }
        if map.contains_key("accountUUID") {
            let uuid = account
                .account_uuid
                .clone()
                .unwrap_or_else(|| derive_account_uuid(account));
            map.insert("accountUUID".into(), serde_json::Value::String(uuid));
        }
        if let Some(ref org) = account.organization_uuid {
            map.insert(
                "organizationUUID".into(),
                serde_json::Value::String(org.clone()),
            );
        } else {
            map.remove("organizationUUID");
        }
        if let Some(ref sub) = account.subscription_type {
            map.insert(
                "subscriptionType".into(),
                serde_json::Value::String(sub.clone()),
            );
        }
        map.remove("apiBaseUrlHost");
    }
    serde_json::to_string(&obj).unwrap_or_else(|_| json_str.to_string())
}

/// 移除 system 和消息内容块中的 cache_control。
fn strip_cache_control(body: &mut serde_json::Value) {
    if let Some(sys) = body.get_mut("system").and_then(|s| s.as_array_mut()) {
        for item in sys.iter_mut() {
            if let Some(block) = item.as_object_mut() {
                block.remove("cache_control");
            }
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                for item in content.iter_mut() {
                    if let Some(block) = item.as_object_mut() {
                        block.remove("cache_control");
                    }
                }
            }
        }
    }
}

/// 移除消息和 system 中的空文本内容块。
fn strip_empty_text_blocks(body: &mut serde_json::Value) {
    fn filter_blocks(blocks: &mut Vec<serde_json::Value>) {
        blocks.retain(|item| {
            if let Some(block) = item.as_object() {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if text.is_empty() {
                        return false;
                    }
                }
            }
            true
        });
        // Handle tool_result nested content
        for item in blocks.iter_mut() {
            if let Some(block) = item.as_object_mut() {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    if let Some(content) = block.get_mut("content").and_then(|c| c.as_array_mut()) {
                        filter_blocks(content);
                    }
                }
            }
        }
    }

    if let Some(sys) = body.get_mut("system").and_then(|s| s.as_array_mut()) {
        filter_blocks(sys);
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                filter_blocks(content);
            }
        }
    }
}

/// 从注入模式 body 中获取暂存的 _session_id。
pub fn extract_session_id_from_body(body: &serde_json::Value) -> Option<String> {
    body.get("metadata")
        .and_then(|m| m.get("_session_id"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

/// 清理 body 中的内部 _session_id 标记。
pub fn clean_session_id_from_body(body: &mut serde_json::Value) {
    if let Some(metadata) = body.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        metadata.remove("_session_id");
    }
}

/// 判断请求来自 Claude Code 还是纯 API。
pub fn detect_client_type(user_agent: &str, body: &serde_json::Value) -> ClientType {
    let ua_lower = user_agent.to_lowercase();
    if ua_lower.starts_with("claude-code/") || ua_lower.starts_with("claude-cli/") {
        return ClientType::ClaudeCode;
    }
    if let Some(metadata) = body.get("metadata").and_then(|m| m.as_object()) {
        if metadata.contains_key("user_id") {
            return ClientType::ClaudeCode;
        }
    }
    ClientType::API
}

const CLAUDE_CODE_SYSTEM_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// 通过账号信息生成稳定的 UUID 标识符。
fn derive_account_uuid(account: &Account) -> String {
    let seed = if account.email.is_empty() {
        format!("account-{}", account.id)
    } else {
        account.email.clone()
    };
    let hash = Sha256::digest(seed.as_bytes());
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&hash[0..4]),
        hex::encode(&hash[4..6]),
        hex::encode(&hash[6..8]),
        hex::encode(&hash[8..10]),
        hex::encode(&hash[10..16])
    )
}

pub fn generate_session_uuid() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&b[0..4]),
        hex::encode(&b[4..6]),
        hex::encode(&b[6..8]),
        hex::encode(&b[8..10]),
        hex::encode(&b[10..16])
    )
}

fn random_in_range(min: i64, max: i64) -> i64 {
    if max <= min {
        return min;
    }
    rand::thread_rng().gen_range(min..max)
}

/// 仅在 `<system-reminder>` 标签内替换 `Git user:` 行。
/// 不影响 messages、tools 和 `<system-reminder>` 外部的文本，避免破坏 git 操作。
fn scrub_git_user_in_reminders(body: &mut serde_json::Value, replacement_name: &str) {
    let replacement = format!("Git user: {}", replacement_name);
    let scrub = |text: &str| -> String {
        SYSTEM_REMINDER_REGEX
            .replace_all(text, |caps: &regex::Captures| {
                GIT_USER_REGEX
                    .replace_all(&caps[0], replacement.as_str())
                    .to_string()
            })
            .to_string()
    };

    if let Some(system) = body.get_mut("system") {
        match system {
            serde_json::Value::String(s) => {
                *s = scrub(s);
            }
            serde_json::Value::Array(arr) => {
                for item in arr.iter_mut() {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        let new_text = scrub(text);
                        item.as_object_mut()
                            .unwrap()
                            .insert("text".into(), serde_json::Value::String(new_text));
                    }
                }
            }
            _ => {}
        }
    }
}

/// 将 canonical env 的 platform 映射为 X-Stainless-OS 值。
fn stainless_os_from_platform(platform: &str) -> &str {
    mimic_profile::stainless_os(platform)
}

fn nth_index(s: &str, c: char, n: usize) -> Option<usize> {
    let mut count = 0;
    for (i, ch) in s.chars().enumerate() {
        if ch == c {
            count += 1;
            if count == n {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod beta_tests {
    use super::{compute_betas_for_model, strip_1m_suffix};

    fn contains(set: &[&str], s: &str) -> bool {
        set.iter().any(|x| *x == s)
    }

    #[test]
    fn sonnet_4_set_matches_2_1_196() {
        // 实测 2.1.196 sonnet /v1/messages（API key 样本）的精确集合与顺序
        let b = compute_betas_for_model("claude-sonnet-4-6");
        assert_eq!(
            b,
            vec![
                "claude-code-20250219",
                "interleaved-thinking-2025-05-14",
                "thinking-token-count-2026-05-13",
                "context-management-2025-06-27",
                "prompt-caching-scope-2026-01-05",
                "advisor-tool-2026-03-01",
                "effort-2025-11-24",
            ]
        );
        // 已被 messages 移除的 token 不应出现
        assert!(!contains(&b, "oauth-2025-04-20"));
        assert!(!contains(&b, "redact-thinking-2026-02-12"));
        // sonnet 非 opus：无 1m / mid-conversation / advanced-tool-use / cache-diagnosis
        assert!(!contains(&b, "context-1m-2025-08-07"));
        assert!(!contains(&b, "mid-conversation-system-2026-04-07"));
    }

    #[test]
    fn opus_4_8_set_matches_2_1_196() {
        // 实测 2.1.196 opus-4-8 /v1/messages（OAuth 抓包）的精确集合与顺序
        let b = compute_betas_for_model("claude-opus-4-8");
        assert_eq!(
            b,
            vec![
                "claude-code-20250219",
                "context-1m-2025-08-07",
                "interleaved-thinking-2025-05-14",
                "thinking-token-count-2026-05-13",
                "context-management-2025-06-27",
                "prompt-caching-scope-2026-01-05",
                "mid-conversation-system-2026-04-07",
                "advisor-tool-2026-03-01",
                "advanced-tool-use-2025-11-20",
                "effort-2025-11-24",
                "cache-diagnosis-2026-04-07",
            ]
        );
        assert!(!contains(&b, "oauth-2025-04-20"));
    }

    #[test]
    fn haiku_4_5_includes_claude_code_but_no_effort() {
        let b = compute_betas_for_model("claude-haiku-4-5");
        // 实测 2.1.196：haiku-4-5 仍发送 claude-code-20250219
        assert!(contains(&b, "claude-code-20250219"));
        assert!(contains(&b, "interleaved-thinking-2025-05-14"));
        assert!(contains(&b, "thinking-token-count-2026-05-13"));
        assert!(contains(&b, "context-management-2025-06-27"));
        assert!(contains(&b, "prompt-caching-scope-2026-01-05"));
        assert!(contains(&b, "advisor-tool-2026-03-01"));
        // haiku 不带 output_config.effort → 无 effort beta；也无 opus 专属 / oauth / redact
        assert!(!contains(&b, "effort-2025-11-24"));
        assert!(!contains(&b, "context-1m-2025-08-07"));
        assert!(!contains(&b, "oauth-2025-04-20"));
        assert!(!contains(&b, "redact-thinking-2026-02-12"));
    }

    #[test]
    fn haiku_3_5_is_legacy() {
        let b = compute_betas_for_model("claude-3-5-haiku-20241022");
        // claude-3-* 为 legacy：无现代 beta
        assert!(!contains(&b, "claude-code-20250219"));
        assert!(!contains(&b, "interleaved-thinking-2025-05-14"));
        assert!(!contains(&b, "thinking-token-count-2026-05-13"));
        assert!(!contains(&b, "context-management-2025-06-27"));
        assert!(!contains(&b, "advisor-tool-2026-03-01"));
        assert!(!contains(&b, "oauth-2025-04-20"));
        // prompt-caching-scope 始终
        assert!(contains(&b, "prompt-caching-scope-2026-01-05"));
    }

    #[test]
    fn claude_3_opus_is_legacy_not_modern_opus() {
        let b = compute_betas_for_model("claude-3-opus-20240229");
        // 名字含 opus 但不是 opus-4+：不应享受现代/opus 专属 beta
        assert!(!contains(&b, "claude-code-20250219"));
        assert!(!contains(&b, "interleaved-thinking-2025-05-14"));
        assert!(!contains(&b, "context-management-2025-06-27"));
        assert!(!contains(&b, "context-1m-2025-08-07"));
        assert!(!contains(&b, "mid-conversation-system-2026-04-07"));
        assert!(contains(&b, "prompt-caching-scope-2026-01-05"));
    }

    #[test]
    fn ordering_is_stable_across_calls() {
        let a = compute_betas_for_model("claude-sonnet-4-6");
        let b = compute_betas_for_model("claude-sonnet-4-6");
        assert_eq!(a, b);
    }

    #[test]
    fn model_id_with_1m_suffix_adds_context_1m_beta() {
        let b = compute_betas_for_model("claude-sonnet-4-6[1m]");
        assert!(contains(&b, "context-1m-2025-08-07"));
        // 基础 beta 按剥离后的 base model id（claude-sonnet-4-6）计算
        assert!(contains(&b, "context-management-2025-06-27"));
        assert!(contains(&b, "claude-code-20250219"));
    }

    #[test]
    fn model_id_without_1m_suffix_omits_context_1m_beta() {
        let b = compute_betas_for_model("claude-sonnet-4-5-20250929");
        assert!(!contains(&b, "context-1m-2025-08-07"));
    }

    #[test]
    fn strip_1m_suffix_helper() {
        assert_eq!(
            strip_1m_suffix("claude-sonnet-4-6[1m]"),
            ("claude-sonnet-4-6", true)
        );
        assert_eq!(
            strip_1m_suffix("claude-opus-4-7[1m]"),
            ("claude-opus-4-7", true)
        );
        assert_eq!(
            strip_1m_suffix("claude-sonnet-4-5-20250929"),
            ("claude-sonnet-4-5-20250929", false)
        );
        assert_eq!(strip_1m_suffix("[1m]"), ("", true));
    }
}

#[cfg(test)]
mod mimic_contract_tests {
    //! 端到端契约比对：把 cc-bridge rewrite 管线的输出对照真实 Claude Code 2.1.196
    //! `/v1/messages` 抓包（GOLD 样本）。故意用 stale 账号存值构造，验证 rewriter 强制
    //! 矫正到当前 profile，且 CC 模式**原样保留**客户端 anthropic-beta（不被旧集合 clobber）。
    use super::*;
    use crate::model::account::{AccountAuthType, AccountStatus};
    use chrono::Utc;

    // 真实 2.1.196 opus-4-8 /v1/messages 抓到的 anthropic-beta（含顺序）。
    const GOLD_OPUS_BETA: &str = "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,cache-diagnosis-2026-04-07";

    fn stale_linux_account() -> Account {
        // 全部填 stale：rewriter 应忽略并矫正到 profile（2.1.196 / v26.3.0 / 0.94.0）。
        let env = CanonicalEnvData {
            platform: "linux".into(),
            platform_raw: "linux".into(),
            arch: "x64".into(),
            node_version: "v22.15.0".into(),
            terminal: "ssh-session".into(),
            package_managers: "npm".into(),
            runtimes: "node".into(),
            is_claude_ai_auth: true,
            version: "2.1.81".into(),
            version_base: "2.1.81".into(),
            build_time: "2026-03-20T21:26:18Z".into(),
            deployment_environment: "unknown-linux".into(),
            vcs: "git".into(),
            ..Default::default()
        };
        Account {
            id: 1,
            name: "tester".into(),
            email: "tester@example.com".into(),
            status: AccountStatus::Active,
            auth_type: AccountAuthType::Oauth,
            setup_token: String::new(),
            access_token: "acc".into(),
            refresh_token: "ref".into(),
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            oauth_refreshed_at: None,
            auth_error: String::new(),
            proxy_url: String::new(),
            device_id: "a".repeat(64),
            canonical_env: serde_json::to_value(env).unwrap(),
            canonical_prompt: serde_json::json!({}),
            canonical_process: serde_json::json!({}),
            billing_mode: BillingMode::Strip,
            account_uuid: Some("11111111-2222-3333-4444-555555555555".into()),
            organization_uuid: None,
            subscription_type: Some("max".into()),
            concurrency: 3,
            priority: 50,
            rate_limited_at: None,
            rate_limit_reset_at: None,
            disable_reason: String::new(),
            auto_telemetry: false,
            telemetry_count: 0,
            usage_data: serde_json::json!({}),
            usage_fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// CC 客户端模式：版本类 header（UA / x-stainless-package-version / runtime-version）
    /// 逐字透传客户端原值（与 billing 透传哲学一致，避免 mimic_profile 与客户端版本漂移
    /// 导致 UA↔billing 不自洽）；身份类 header（x-stainless-os）矫正到池账号身份；
    /// 客户端 anthropic-beta 原样保留。
    #[test]
    fn cc_mode_preserves_client_version_headers_and_beta() {
        let account = stale_linux_account();
        let mut headers = HashMap::new();
        headers.insert(
            "user-agent".into(),
            "claude-cli/2.1.150 (external, cli)".into(),
        );
        headers.insert("anthropic-beta".into(), GOLD_OPUS_BETA.into());
        headers.insert("anthropic-version".into(), "2023-06-01".into());
        headers.insert("x-stainless-package-version".into(), "0.80.0".into());
        headers.insert("x-stainless-os".into(), "Linux".into());
        headers.insert("x-stainless-runtime-version".into(), "v24.0.0".into());
        headers.insert("x-app".into(), "cli".into());

        let body = serde_json::json!({});
        let out = Rewriter::new().rewrite_headers(
            &headers,
            &account,
            ClientType::ClaudeCode,
            "claude-opus-4-8",
            &body,
        );

        // 版本类：逐字透传客户端原值（不再强钉 mimic_profile，避免版本漂移不自洽）
        assert_eq!(
            out.get("User-Agent").unwrap(),
            "claude-cli/2.1.150 (external, cli)"
        );
        assert_eq!(out.get("X-Stainless-Package-Version").unwrap(), "0.80.0");
        assert_eq!(out.get("X-Stainless-Runtime-Version").unwrap(), "v24.0.0");
        // 身份类：矫正到池账号身份（linux 账号 → Linux）
        assert_eq!(out.get("X-Stainless-OS").unwrap(), "Linux");
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
        // 关键：客户端自带的现代 beta 集原样透传，不被旧规则改写
        assert_eq!(out.get("anthropic-beta").unwrap(), GOLD_OPUS_BETA);
    }

    /// API 模式（裸 client 伪装成 CC）：完整发齐 2.1.196 契约。
    #[test]
    fn api_mode_emits_full_2_1_196_contract() {
        let account = stale_linux_account();
        let headers = HashMap::new();
        let body = serde_json::json!({});
        let out = Rewriter::new().rewrite_headers(
            &headers,
            &account,
            ClientType::API,
            "claude-opus-4-8",
            &body,
        );
        assert_eq!(
            out.get("User-Agent").unwrap(),
            "claude-cli/2.1.196 (external, cli)"
        );
        assert_eq!(out.get("X-Stainless-Package-Version").unwrap(), "0.94.0");
        assert_eq!(out.get("X-Stainless-Runtime-Version").unwrap(), "v26.3.0");
        assert_eq!(out.get("X-Stainless-OS").unwrap(), "Linux");
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(out.get("x-app").unwrap(), "cli");
        // API 模式 beta 由 profile 计算，opus 全集且不含 oauth-2025-04-20 / redact-thinking
        assert_eq!(out.get("anthropic-beta").unwrap(), GOLD_OPUS_BETA);
        let beta = out.get("anthropic-beta").unwrap();
        assert!(!beta.contains("oauth-2025-04-20"));
        assert!(!beta.contains("redact-thinking"));
    }

    /// extract_first_user_message 必须跳过 <system-reminder> 块，取注入前的真实用户文本
    /// （官方 _tf 在注入 reminder 之前计算）。
    #[test]
    fn extract_first_user_message_skips_system_reminder() {
        let body = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "<system-reminder>\nAs you answer the user's questions...\n</system-reminder>"},
                    {"type": "text", "text": "hi\nhi\n"}
                ]
            }]
        });
        assert_eq!(extract_first_user_message(&body), "hi\nhi\n");
    }

    /// CC 模式 Rewrite：billing header 原样透传，cc_version 后缀与 cch 均不被重算覆盖。
    #[test]
    fn cc_mode_rewrite_preserves_client_billing() {
        let mut account = stale_linux_account();
        account.billing_mode = BillingMode::Rewrite;

        let body = serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.196.1f5; cc_entrypoint=sdk-cli; cch=bf8ab;"},
                {"type": "text", "text": "You are Claude Code."}
            ],
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "hi\nhi\n"}]
            }]
        });
        let out = Rewriter::new().rewrite_body(
            &serde_json::to_vec(&body).unwrap(),
            "/v1/messages",
            &account,
            ClientType::ClaudeCode,
        );
        let out_json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys0 = out_json["system"][0]["text"].as_str().unwrap();
        // cc_version 后缀保留客户端原值 1f5，不被重算成 cd9
        assert!(
            sys0.contains("cc_version=2.1.196.1f5"),
            "suffix not preserved: {sys0}"
        );
        assert!(!sys0.contains("cd9"), "suffix recomputed to cd9: {sys0}");
        // cch 保留客户端原值 bf8ab（不重置 00000、不用旧 seed 覆盖）
        assert!(sys0.contains("cch=bf8ab"), "cch not preserved: {sys0}");
    }
}
