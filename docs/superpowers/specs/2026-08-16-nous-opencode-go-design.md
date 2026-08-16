# Nous Research OAuth and OpenCode Go usage providers

**Status:** Approved design, pending written-spec review  
**Date:** 2026-08-16  
**Target:** `ai-usagebar` Omarchy Quattro plugin  
**Scope:** Nous Research subscription usage and OpenCode Go quota usage

## 1. Purpose

Extend `ai-usagebar` with two independent providers:

1. **Nous Research** account and subscription usage, authenticated by an OAuth device flow implemented inside `ai-usagebar`.
2. **OpenCode Go** rolling, weekly, and monthly quota usage, authenticated with an OpenCode Go API key.

The product names and credentials must remain conceptually distinct:

- Nous Research is the account and subscription provider.
- Hermes is a Nous model family.
- Hermes Agent is a separate software product.
- `hermes-cli` is reused only as the technical OAuth client identifier accepted by Nous Portal.
- OpenCode Go is a quota subscription.
- OpenCode Zen is a separate pay-as-you-go product and is out of scope.

## 2. Accepted risk

Nous Portal does not document `hermes-cli` as a public OAuth client for third-party applications and does not publish a client-registration process for `ai-usagebar`.

This implementation deliberately sends `client_id=hermes-cli` while remaining an independent application. Nous may restrict or change this client or its endpoints. Such a change may require reauthentication, a plugin update, or migration to an officially assigned client ID.

The implementation must not hide this limitation or fall back to browser cookies, Hermes Agent credentials, or an inference API key.

## 3. Non-goals

- OpenCode Zen balance or spending.
- Reading or modifying `~/.hermes/auth.json`.
- Importing Hermes Agent Python modules.
- Modifying or depending on Hermes Agent.
- A local Hermes broker.
- Embedded OAuth inside QML.
- Browser-cookie scraping.
- Supporting multiple Nous accounts in the first release.
- A generic reusable OAuth framework.
- Inferring missing usage as zero.

## 4. Architecture

All new runtime logic lives in `ai-usagebar`.

```text
Interactive setup

ai-usagebar auth nous login
    -> POST portal.nousresearch.com/api/oauth/device/code
    -> user authorizes in browser
    -> poll POST portal.nousresearch.com/api/oauth/token
    -> atomically save private credentials

Normal widget polling

ai-usagebar --vendor nous
    -> lock credentials
    -> refresh expiring access token when necessary
    -> atomically persist rotated refresh token
    -> GET portal.nousresearch.com/api/oauth/account
    -> normalize a non-secret Nous snapshot
    -> render Waybar/Omarchy output

ai-usagebar --vendor opencode-go
    -> resolve OpenCode Go API key
    -> GET opencode.ai/zen/go/v1/usage
    -> normalize rolling/weekly/monthly windows
    -> render Waybar/Omarchy output
```

The existing `VendorSnapshot` boundary remains responsible for representing genuinely different provider data shapes. Nous OAuth storage and transport stay inside a dedicated Nous module rather than entering generic widget or QML code.

## 5. Nous OAuth CLI

Add an administrative command group:

```text
ai-usagebar auth nous login
ai-usagebar auth nous status [--json]
ai-usagebar auth nous logout
```

Administrative commands use meaningful nonzero exit codes. They do not use the widget binary's always-exit-zero Waybar failure contract.

### 5.1 Login

Request a device code:

```http
POST https://portal.nousresearch.com/api/oauth/device/code
Content-Type: application/x-www-form-urlencoded

client_id=hermes-cli
scope=inference:invoke
```

The response is validated for the fields needed by the device flow, including device code, user code, verification URL, expiration, and polling interval. Missing or invalid fields cause an explicit contract error.

The CLI:

1. Displays the verification URL and user code.
2. Attempts to open the verification URL using an established system browser-opening mechanism.
3. Continues correctly when opening the browser fails; the printed URL remains sufficient.
4. Polls the token endpoint no faster than the server-authorized interval.
5. Handles pending authorization, slowdown, denial, expiration, and transport failures separately.
6. Never prints an access token, refresh token, raw JWT, or full token response.

Token polling:

```http
POST https://portal.nousresearch.com/api/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=urn:ietf:params:oauth:grant-type:device_code
client_id=hermes-cli
device_code=<device code>
```

A successful result is written to the credential store before login is reported as successful. The implementation then performs an authenticated account probe to prove that the stored credential works.

### 5.2 Status

`auth nous status` reads local state without refreshing by default. It reports only:

- logged in or logged out;
- access-token expiration time;
- whether a refresh token is present;
- whether file ownership and permissions are safe;
- whether reauthentication is required.

`--json` emits a versioned, non-secret object suitable for the Omarchy settings view. Token previews are prohibited.

### 5.3 Logout

Logout removes only the Nous entry from the credential document using the same lock and atomic-write path. If no other credentials remain, the file may be removed. Logout must be idempotent and must not alter `config.toml`, caches, OpenCode credentials, or Hermes Agent files.

## 6. Nous credential store

Default path:

```text
~/.config/ai-usagebar/credentials.json
```

Versioned shape:

```json
{
  "version": 1,
  "nous": {
    "client_id": "hermes-cli",
    "access_token": "[REDACTED]",
    "refresh_token": "[REDACTED]",
    "expires_at": "2026-08-16T00:00:00Z"
  }
}
```

### 6.1 Filesystem controls

On Unix:

- create the parent directory for the current user;
- create credential files with mode `0600` without a permissive intermediate state;
- reject a credential file not owned by the current effective user;
- reject group- or world-readable/writable/executable credential files;
- reject symlinks for the credential file and atomic replacement target;
- serialize read-refresh-write operations with `fs2` locking;
- write a temporary file in the same directory;
- flush file contents;
- rename atomically;
- preserve `0600` after every replacement;
- never place token material in process arguments, logs, panic text, caches, or `config.toml`.

A malformed or unsafe credential file fails closed. The widget reports an actionable authentication error and does not silently replace the file.

### 6.2 Refresh-token rotation

Refresh access tokens that expire within 120 seconds:

```http
POST https://portal.nousresearch.com/api/oauth/token
Content-Type: application/x-www-form-urlencoded
x-nous-refresh-token: <refresh token>

grant_type=refresh_token
client_id=hermes-cli
```

Nous refresh tokens are treated as single-use and rotating. The complete sequence occurs under an exclusive lock:

1. Re-read the current credential after acquiring the lock.
2. Skip refresh if another process already stored a sufficiently fresh access token.
3. Exchange the current refresh token once.
4. Validate both access and refresh token fields and expiration metadata.
5. Atomically persist the complete replacement credential.
6. Only then call the account endpoint.

`invalid_grant`, `invalid_token`, and refresh-token reuse are terminal authentication failures. The plugin preserves enough non-secret error state to request login again but never retries the same refresh token in a loop.

## 7. Nous account usage

Authoritative query:

```http
GET https://portal.nousresearch.com/api/oauth/account
Authorization: Bearer <access token>
Accept: application/json
```

The transport model must tolerate additive response fields while requiring the fields needed for any displayed metric. The normalized snapshot may contain:

- plan name;
- subscription tier;
- monthly credits;
- credits remaining;
- rollover or additional credits;
- current period end;
- total available credits when directly supported by response fields;
- usage percentage derived only from complete and valid numerator/denominator values.

The normalized model must not retain or expose:

- access or refresh tokens;
- raw JWTs;
- full raw account payloads;
- internal user or organization identifiers not required by the UI.

A missing value renders as unavailable. It is never defaulted to zero. Derived percentages must guard against negative, non-finite, or zero denominators.

The display name is always **Nous Research**. “Hermes” must not be used as the provider or subscription label.

## 8. OpenCode Go provider

Authoritative endpoint:

```http
GET https://opencode.ai/zen/go/v1/usage
Authorization: Bearer <OpenCode Go API key>
Accept: application/json
```

Expected contract:

```json
{
  "usage": {
    "rolling": {
      "status": "ok",
      "percent": 12.3,
      "resetsAt": "2026-08-16T20:00:00Z"
    },
    "weekly": {
      "status": "ok",
      "percent": 45.6,
      "resetsAt": "2026-08-20T00:00:00Z"
    },
    "monthly": {
      "status": "ok",
      "percent": 78.9,
      "resetsAt": "2026-09-01T00:00:00Z"
    }
  }
}
```

The parser uses `percent`, not the obsolete assumption `usagePercent`. Each window validates:

- finite percentage;
- valid reset timestamp;
- recognized or safely representable status.

The three windows are independent: one malformed window must not be fabricated, and the provider must follow the project's existing snapshot/error policy rather than silently presenting partial data as complete.

### 8.1 Credential resolution

The canonical environment variable is:

```text
OPENCODE_GO_API_KEY
```

If the project supports a protected configured-secret mechanism for existing API-key providers, OpenCode Go follows that same mechanism with `OPENCODE_GO_API_KEY` as the documented source. It must not read `OPENCODE_ZEN_API_KEY`, generic `OPENCODE_API_KEY`, browser cookies, or OpenCode Zen credentials.

## 9. Omarchy Quattro integration

Add provider entries:

- Nous Research
- OpenCode Go

The normal vendor selector, active-vendor cycling, report output, QML settings ledger, placeholders, and enabled-provider behavior must use the same patterns as existing providers.

The settings view exposes:

- enable/disable for each provider;
- non-secret Nous authentication status;
- the exact login command `ai-usagebar auth nous login`;
- OpenCode Go API-key configuration guidance using `OPENCODE_GO_API_KEY`.

The first Nous login remains terminal-based. No embedded browser, QML token handling, or QML credential write path is introduced.

## 10. Display and error semantics

Required user-visible states:

| Condition | Result |
|---|---|
| Nous not logged in | Authentication-required status with exact login command |
| Nous token refresh revoked/reused | Reauthentication-required status; no retry loop |
| Nous credential file unsafe | Security error naming the path and required permissions, without secrets |
| OpenCode Go key absent | Configuration-required status naming `OPENCODE_GO_API_KEY` |
| HTTP 401/403 | Authentication error, not zero usage |
| HTTP 429 | Rate-limit status and retry guidance |
| Network or timeout error | Transient unavailable state |
| Response schema incompatible | Explicit provider-contract error |
| Metric missing | Unavailable metric, never fabricated zero |

Existing cache conventions may preserve the last valid snapshot only when the UI clearly marks it stale and carries its fetch timestamp. A cached snapshot must never be labeled current after an authentication or schema failure.

## 11. Configuration and placeholders

Provider IDs are stable:

```text
nous
opencode-go
```

Configuration remains non-secret and uses these exact sections:

```toml
[nous]
enabled = false
# credentials_path = "~/.config/ai-usagebar/credentials.json"

[opencode_go]
enabled = false
api_key_env = "OPENCODE_GO_API_KEY"
# api_key = "..." # existing protected inline-key mechanism; config must be 0600
```

`credentials_path` exists for isolated testing and multiple operating-system accounts, follows the project's existing path-expansion rules, and defaults to the credential path in section 6. Production endpoint overrides are not exposed in user configuration; HTTP endpoints are injected directly into test constructors.

OAuth tokens must never appear in `config.toml`. An inline OpenCode Go key is allowed only because the project already supports the same protected mechanism for API-key providers; environment resolution takes precedence and unsafe config-file permissions fail closed.

The provider-specific placeholder contract is fixed:

### Nous Research

- `{vendor_short}` = `nrs`;
- `{plan}` and `{nous_plan}` = plan name;
- `{session_pct}`, `{weekly_pct}`, and `{nous_pct}` = total subscription usage percentage when derivable;
- `{session_reset}`, `{weekly_reset}`, and `{nous_renewal}` = countdown to the current period end;
- `{nous_credits_remaining}` = remaining monthly credits;
- `{nous_monthly_credits}` = monthly allocation;
- `{nous_rollover_credits}` = rollover or additional credits.

Default format:

```text
{nous_pct}% · {nous_renewal}
```

When percentage cannot be derived, the default format omits the percentage fragment rather than rendering `0%`.

### OpenCode Go

- `{vendor_short}` = `ocg`;
- `{session_pct}` = rolling percentage;
- `{session_reset}` = rolling reset countdown;
- `{weekly_pct}` = weekly percentage;
- `{weekly_reset}` = weekly reset countdown;
- `{ocg_rolling_pct}`, `{ocg_rolling_reset}`, `{ocg_rolling_status}`;
- `{ocg_weekly_pct}`, `{ocg_weekly_reset}`, `{ocg_weekly_status}`;
- `{ocg_monthly_pct}`, `{ocg_monthly_reset}`, `{ocg_monthly_status}`.

Default format:

```text
{ocg_rolling_pct}% · {ocg_rolling_reset}
```

Missing windows expand to the project's neutral unavailable value and are identified as unavailable in the tooltip; they never become `0%`.

## 12. TDD implementation sequence

Implementation begins only after this specification is reviewed and approved.

1. Add RED fixtures for the current official Nous and OpenCode Go contracts.
2. Add RED credential-store permission, symlink, lock, and atomic-write tests.
3. Implement the minimal credential store to GREEN.
4. Add RED device-flow state and token redaction tests.
5. Implement Nous login/status/logout to GREEN.
6. Add RED refresh rotation, concurrent refresh, and terminal-auth-error tests.
7. Implement refresh to GREEN.
8. Add RED Nous account parsing and normalization tests.
9. Implement Nous fetching and rendering to GREEN.
10. Add RED OpenCode Go `percent` parsing and auth-source tests.
11. Implement OpenCode Go fetching and rendering to GREEN.
12. Add RED config, vendor registry, placeholders, report, and QML ledger tests.
13. Implement UI integration to GREEN.
14. Refactor only after all focused tests pass.

## 13. Verification gates

Before installation:

- `cargo fmt --check` passes;
- `cargo clippy --all-targets --all-features -- -D warnings` passes;
- `cargo test --all-targets --all-features` passes;
- release build succeeds;
- credential files are verified as `0600` on the live filesystem;
- logs and JSON outputs are searched for token leakage;
- Nous OAuth login and account usage are tested with explicit user authorization;
- OpenCode Go is tested with an authorized real Go credential;
- the development plugin is run under Quickshell/Omarchy and visually inspected;
- existing providers receive regression smoke coverage;
- the exact staged snapshot receives one independent fail-closed review;
- installation occurs only after PASS.

The real authenticated smoke tests must report endpoint status and normalized non-secret fields only. Test evidence must never include keys or tokens.

## 14. Rollback

The implementation remains in an isolated worktree until review passes. Local installation must retain a restorable copy or known-good revision of the current plugin binary and QML files.

Rollback consists of:

1. restoring the previous plugin revision;
2. reloading Quickshell/Omarchy;
3. verifying the existing provider widget works;
4. optionally running `ai-usagebar auth nous logout` to remove the independent Nous credentials.

Rollback must not touch Hermes Agent or OpenCode's own configuration.

## 15. Acceptance criteria

The work is accepted only when all of the following are demonstrated:

1. Nous login succeeds without Hermes Agent and writes a `0600` credential file atomically.
2. A forced/short-expiry test proves refresh-token rotation is persisted safely.
3. Concurrent widget requests do not reuse one refresh token.
4. Nous Research subscription data is rendered without labeling it Hermes.
5. OpenCode Go renders rolling, weekly, and monthly percentages from `percent` and their reset timestamps.
6. OpenCode Zen is absent from runtime code, settings, documentation, and tests.
7. Missing or invalid credentials produce actionable errors rather than fabricated usage.
8. No token or API key appears in stdout, logs, cache, config, test snapshots, or review artifacts.
9. Existing providers continue to work.
10. The Omarchy Quattro plugin displays and cycles the two new providers in a real development run.

## 16. Primary evidence

- Nous Portal API documentation: <https://portal.nousresearch.com/api-docs>
- Nous Portal OpenAPI: <https://portal.nousresearch.com/api/openapi>
- Nous account endpoint behavior and OAuth device/refresh flow: official `NousResearch/hermes-agent` source, especially `hermes_cli/auth.py` and `hermes_cli/nous_account.py`
- OpenCode Go documentation: <https://opencode.ai/docs/go/>
- OpenCode Go usage endpoint implementation: <https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/routes/zen/go/v1/usage.ts>
- OpenCode Go endpoint merge: <https://github.com/anomalyco/opencode/pull/16513>
- OpenCode Zen public balance endpoint remains unavailable: <https://github.com/anomalyco/opencode/issues/10448>
