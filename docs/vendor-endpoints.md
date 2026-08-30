# Vendor endpoints and live tests

Some providers do not publish a stable usage API. ai-usagebar keeps its parsers
defensive and includes opt-in live tests for catching response changes.

## Support matrix

| Vendor | Endpoint | What you see | Native desktop selector (v0.13) |
|---|---|---|---|
| **Claude** | `api.anthropic.com/api/oauth/usage` (undocumented) | Session (5h), Weekly (7d), model-scoped weekly (e.g. Fable), Extra usage $ | Yes |
| **Codex** | `chatgpt.com/backend-api/wham/usage` (undocumented; used by official `codex` CLI) | Codex 5h and/or weekly, Code-review weekly, Credits | Yes |
| **Z.AI** | `api.z.ai/api/monitor/usage/quota/limit` (undocumented) | Session 5h, Weekly 7d, MCP tools monthly | Yes |
| **OpenRouter** | `openrouter.ai/api/v1/{credits,key}` (documented) | Balance, today/week/month spend, free vs paid tier | Yes |
| **DeepSeek** | `api.deepseek.com/user/balance` (documented) | Balance, granted, topped-up credits | Yes |
| **Kimi** | `api.kimi.com\|.ai/coding/v1/usages` (undocumented; community-confirmed), plus `auth.kimi.com\|.ai/api/oauth/token` to refresh a Kimi Code CLI login | Weekly subscription quota + 5h rolling rate-limit window | No — widget/TUI only; desktop protocol and marker parity are future work |
| **MiniMax** | `api.minimax.io/v1/token_plan/remains` (official Token Plan quota route) | Token Plan rolling interval window + weekly, per model bucket (text, video) | No — widget/TUI only |
| **Kilo** | `api.kilo.ai/api/profile/balance` (undocumented; extension-internal) | Remaining credit balance ($) | No — widget/TUI only |
| **Novita** | `api.novita.ai/openapi/v1/billing/balance/detail` (documented) | Remaining credit balance ($) | No — widget/TUI only |
| **Moonshot** | `api.moonshot.ai\|.cn/v1/users/me/balance` (documented) | Account balance ($ on `.ai`, ¥ on `.cn`) | No — widget/TUI only |
| **Grok (xAI)** | `management-api.x.ai/v1/billing/teams/{team}/prepaid/balance` (Management API; documented) | Prepaid credit balance ($) | No — widget/TUI only |
| **SuperGrok** | `cli-chat-proxy.grok.com/v1/billing` with the Grok Build login's key, falling back to its `x.ai/billing` ACP extension | Current weekly/monthly included-credit %, prepaid API balance, reset | No — widget/TUI only |
| **Anthropic API** | `api.anthropic.com/v1/organizations/cost_report` (Admin API; documented) | Month-to-date spend ($, excludes Priority Tier), optional spend-vs-limit % | No — widget/TUI only |
| **Cursor** | `cursor.com/api/usage-summary` (undocumented; the dashboard's own frontend) | Two included-usage pools this billing cycle — Cursor Models (Auto/Composer) % and Other Models (named/API) % — plus plan, reset, on-demand | Yes |
| **Kiro CLI** | `codewhisperer.<region>.amazonaws.com` `GetUsageLimits` (undocumented; the same call kiro-cli's own `/usage` slash command makes) | Single credit pool this cycle — used/limit/%, plan, reset | No — widget/TUI only |
| **Nous Research** | `portal.nousresearch.com/api/oauth/account` (OAuth-authenticated Portal account response) | Subscription usage %, subscription credits, top-up/purchased credits, total usable credits, renewal | Yes |
| **OpenCode Go** | `opencode.ai/zen/go/v1/usage` | Rolling, weekly, and monthly `percent` windows with absolute reset timestamps | Yes |
| **Command Code** | `api.commandcode.ai` `/alpha/billing/credits` + `/alpha/billing/subscriptions` (undocumented; the same calls the official `commandcode` CLI's `/usage` makes) | 5-hour and weekly rolling spend windows ($ used of $ cap), plan, and remaining monthly credits | No — widget/TUI only |


## Stability notes

| Provider | Status |
|---|---|
| Claude | Undocumented usage endpoint, but used by the official `claude` CLI. Less fragile than a scraped web page. |
| Codex | Undocumented ChatGPT usage endpoint used by the official `codex` CLI. Windows are identified by duration instead of response position. |
| Z.AI | Reverse-engineered from a third-party plugin. Treat this as the most fragile integration. |
| Kimi | Community-confirmed `/coding/v1/usages` route used by third-party quota tools. Drift is possible. The refresh grant is the Kimi Code CLI's own documented-by-behaviour device-flow token endpoint, using the CLI's public client id. |
| Cursor | Undocumented endpoint called by Cursor's dashboard. Its shape may change with Cursor pricing. |
| MiniMax | The Token Plan route is official, but no formal response schema is published. |
| Kiro CLI | `GetUsageLimits` is the same undocumented CodeWhisperer operation used by kiro-cli's `/usage` command. AWS SSO OIDC `CreateToken`, used for refresh, is documented. |
| Command Code | Undocumented `/alpha/*` routes called by the official `commandcode` CLI. The `alpha` path segment is the vendor's own signal that these may move. Windows are read by name (`fiveHour`, `weekly`) rather than by position, and `windowLimits` is accepted both at the top level and beside the ledger, so the most likely reshuffles are already tolerated. |

Codex's known five-hour and seven-day windows are matched by their reported
duration, not by `primary_window` or `secondary_window` position. This handles
both the normal response and the temporary
[weekly-only response](https://github.com/openai/codex/issues/32707) without a
config switch.

## Run the live tests

```bash
make smoke
```

Claude, Codex, Z.AI, and OpenRouter tests require their normal credentials or
API keys. Command Code needs no key of its own — it reuses whichever local
agent harness is signed in, and skips when none is. Kimi is optional: its test
prints a skip reason when `KIMI_API_KEY` is unset (the smoke test covers the
API-key path; a subscription login is exercised by `ai-usagebar --vendor kimi`).

To test only Kimi:

```bash
cargo test --test live kimi_live -- --ignored --nocapture
```

The tests validate the fields used by ai-usagebar and report which part of a
response changed.
