# statusline 显示池账号额度（5h / 7d）

在 [claude-code-statusline-pro](https://github.com/Wangnov/claude-code-statusline-pro)（即 `ccsp`）状态栏显示 cc-bridge 池账号的 5h / 7d 用量：

```
⏳ 5h 12%   📅 7d 63%
```

## 为什么需要这个（不能直接用 ccsp 内置 rate_limit）

ccsp 内置的 `rate_limit` 组件读的是 claude 注入 stdin 的 `rate_limits` 字段。但 **claude 只在 claude.ai OAuth 登录态才填这个字段**——用 `ANTHROPIC_AUTH_TOKEN`（所有 cc-bridge 客户端的连接方式）时，claude 直接不填，与 cc-bridge 是否透传响应头无关。这是 claude 客户端侧的硬门控。

所以改用 **api-widget 主动查 cc-bridge 的 `GET /v1/usage`**：该端点用客户端 token 鉴权，返回 cc-bridge 转发请求时吸收进内存热态的 5h/7d 用量（明文 JSON，**自身不打上游**）。

## 安装

1. **复制组件配置**（本目录的 `rate_limit.toml`）到：
   ```
   ~/.claude/statusline-pro/components/rate_limit.toml
   ```

2. **改 detection**：编辑该文件，把两处 `detection.contains` 的 `REPLACE_WITH_YOUR_CCBRIDGE_HOST_OR_PORT` 换成你 cc-bridge base URL 的独有特征（域名或端口），例如：
   ```toml
   contains = "cc.example.com"     # 或端口 "5674"
   ```
   > detection 让 widget **只在 `ANTHROPIC_BASE_URL` 指向 cc-bridge 时才触发**。不改的话，指向官方 API 时会每次渲染都白打一次 `官方/v1/usage`（带 token，404），且把 token 泄漏到无效端点。

3. **确认主配置** `~/.claude/statusline-pro/config.toml`：
   ```toml
   [components]
   order = ["...", "rate_limit"]   # order 里含 rate_limit（数组形式）
   [multiline]
   enabled = true
   ```

4. **确认 statusLine** `~/.claude/settings.json`：
   ```json
   { "statusLine": { "type": "command", "command": "npx ccsp@latest" } }
   ```

## 双模式自适应

一套配置同时适配两种连接方式，互不干扰：

| 连接方式 | 额度来源 |
|---|---|
| **cc-bridge**（`ANTHROPIC_AUTH_TOKEN=sk-…`） | detection 匹配 → api-widget 查 `/v1/usage` → 显示池账号额度 |
| **官方 / claude.ai 登录态** | detection 不匹配 → api-widget **不触发、零请求**；ccsp 内置 `rate_limit` 读 stdin 正常显示 |

## 验证

```bash
# cc-bridge 端点（TOKEN = 你的 cc-bridge 客户端 token）
curl -s --compressed https://<你的 cc-bridge>/v1/usage -H "Authorization: Bearer $TOKEN"
# 期望 200 + JSON：{"five_hour":{"utilization":12.0,...},"seven_day":{...}}
# 若返回 {}：cc-bridge 刚重启、内存热态还空，来第一发真实请求即填
```

> `utilization` 已是 0-100 百分比。容器重启后热态清空（返回 `{}`），无需持久化——正常流量几秒内即填充。
