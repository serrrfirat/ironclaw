# Security & Correctness Review: Open Pull Requests

**Reviewer:** Claude (automated)
**Date:** 2026-02-11
**Repository:** nearai/ironclaw
**Scope:** All 7 open PRs reviewed for security vulnerabilities and correctness issues

---

## Summary

| PR | Title | Risk | Verdict |
|----|-------|------|---------|
| #14 | NEAR key management + transaction signing | **CRITICAL** | Request changes |
| #18 | Lifecycle hooks system | **HIGH** | Request changes |
| #17 | DM pairing + Telegram channel | **HIGH** | Request changes |
| #20 | Direct API key auth + cheap model routing | **MEDIUM** | Request changes |
| #28 | Multi-provider LLM failover | **MEDIUM** | Request changes |
| #13 | Okta SSO WASM tool | **LOW** | Approve with comments |
| #10 | Benchmarking harness | **LOW** | Approve with comments |

---

## PR #14 — NEAR Key Management with Transaction Signing and Policy Engine

**Author:** ilblackdragon | **Files:** 26 | **Risk: CRITICAL**

### Overview
Adds cryptographic key management, ed25519 signing, transaction construction, and a policy engine for authorizing transactions. Dependencies include `ed25519-dalek`, `borsh`, `bs58`, `argon2`, and `zeroize`.

### Critical Security Issues

1. **Hand-rolled NEAR transaction serialization** — The PR description explicitly states "hand-rolled borsh-serializable NEAR transaction types." This is a major red flag. Custom-serializing transaction types rather than using the official `near-primitives` crate creates risk of:
   - Signing semantically different data than intended (transaction malleability)
   - Incompatibility with protocol upgrades
   - Subtle serialization bugs that could allow an attacker to trick the signer into signing unexpected transactions
   - **Recommendation:** Use `near-primitives` or at minimum add exhaustive round-trip serialization tests against known-good NEAR transactions.

2. **Key derivation parameters need verification** — Argon2 is used for key derivation. The security depends entirely on the parameters (memory cost, time cost, parallelism). Weak parameters would make encrypted keys trivially brute-forceable.
   - **Recommendation:** Verify argon2 parameters meet OWASP minimums (Argon2id, ≥19 MiB memory, ≥2 iterations). Add parameter constants as named constants, not magic numbers.

3. **Zeroize coverage** — The `zeroize` dependency is present but it's unclear whether ALL sensitive key material (private keys, derived keys, intermediate buffers) is properly zeroized on drop.
   - **Recommendation:** Audit every `Vec<u8>` and `[u8; N]` holding key material. Use `Zeroizing<T>` wrapper or implement `Drop` with zeroize. Ensure no `clone()` of key material creates non-zeroized copies.

4. **Policy engine bypass risk** — A policy engine gating transaction signing is only as strong as its enforcement. If there's any code path that signs without checking the policy, the entire engine is defeated.
   - **Recommendation:** Ensure signing is only possible through a single entry point that always checks policy. Consider making the signer struct hold the policy engine as an enforced dependency rather than having callers check policy before calling sign.

5. **ed25519-dalek version** — Older versions of ed25519-dalek had vulnerabilities (double-pub-key attacks). Need to verify this uses v2.0+ which fixed CVE-2022-25912 and related issues.
   - **Recommendation:** Pin to `ed25519-dalek >= 2.0` and verify in Cargo.lock.

### Correctness Issues

6. **WASM integration for key management** — Exposing key signing to WASM tools would be extremely dangerous. Need to verify that key operations are NOT exposed through the WASM host interface.

7. **CLI key commands** — Key management CLI commands must validate input thoroughly (key file paths, passphrase handling via TTY not command-line args, etc.).

**Verdict:** This PR needs significant additional review by a cryptography-aware engineer. Do not merge without: (a) replacing hand-rolled serialization, (b) verifying argon2 parameters, (c) auditing zeroize coverage, and (d) ensuring no signing bypass paths exist.

---

## PR #18 — Lifecycle Hooks System (6 Interception Points)

**Author:** serrrfirat | **Files:** ~11 | **Risk: HIGH**

### Overview
Adds a hook system with 6 interception points: `BeforeInbound`, `BeforeToolCall`, `BeforeOutbound`, `OnSessionStart`, `OnSessionEnd`, `TransformResponse`. Hooks can modify content, reject events, or pass through. Well-tested registry with priority ordering, timeout, and fail-open/fail-closed modes.

### Security Issues

1. **Hooks can modify tool parameters without sanitization** (`src/agent/agent_loop.rs`):
   ```rust
   Ok(crate::hooks::HookOutcome::Continue {
       modified: Some(new_params),
   }) => {
       if let Ok(parsed) = serde_json::from_str(&new_params) {
           tc.arguments = parsed;
       }
   }
   ```
   A malicious or compromised hook can rewrite tool call parameters. For example, a hook could change a `ReadFile` tool's path parameter to read sensitive files, or modify shell command parameters. The modified parameters bypass the original user intent and approval flow.
   - **Recommendation:** Modified tool parameters should be re-validated through the safety layer. At minimum, log when a hook modifies tool parameters. Consider requiring explicit capability for hooks that modify tool calls.

2. **BeforeInbound hook can inject prompt injection** — A hook modifying inbound content could inject LLM prompt injection payloads that bypass the sanitizer (since the sanitizer runs after hook modification in the current flow).
   - **Recommendation:** Ensure the safety layer's sanitizer runs AFTER hook modifications, not before.

3. **No hook registration authorization** — `HookRegistry::register()` is a public method with no access control. Any code with a reference to the registry can register a hook. While currently hooks are only registered in `main.rs`, this should be locked down as the system grows.
   - **Recommendation:** Consider making hook registration an admin-only operation or requiring a capability token.

4. **Silent failure on malformed hook modifications** (`src/agent/worker.rs:425`):
   ```rust
   Ok(HookOutcome::Continue {
       modified: Some(new_params),
   }) => serde_json::from_str(&new_params).unwrap_or_else(|_| params.clone()),
   ```
   If a hook returns malformed JSON, the original parameters are used silently. This is reasonable fail-safe behavior but should log a warning.

5. **Session hooks are fire-and-forget** (`src/agent/session_manager.rs`):
   ```rust
   tokio::spawn(async move {
       // OnSessionStart hook
       if let Err(e) = hooks.run(&event).await {
           tracing::warn!("OnSessionStart hook error: {}", e);
       }
   });
   ```
   Session lifecycle hooks run in detached tasks. This means:
   - A `FailClosed` hook on `OnSessionStart` will log but NOT prevent session creation
   - Race conditions: the session is created and returned before the hook runs
   - **Recommendation:** If `OnSessionStart` hooks should be able to reject session creation, they need to run synchronously before returning the session.

### Correctness Issues

6. **TransformResponse and BeforeOutbound both modify responses** — There are two hook points that can modify outgoing content (`TransformResponse` in the agentic loop, `BeforeOutbound` in the message dispatch). This creates confusion about ordering and double-modification.
   - **Recommendation:** Document the exact ordering and ensure hooks registered at one point can't accidentally interact with the other.

7. **Hook chain modification propagation is correct** — The registry properly clones the event, applies modifications in priority order, and returns the final result. The `apply_modification` method correctly handles each event type. Tests cover chaining, rejection, timeouts, and fail modes. This is well-implemented.

**Verdict:** The hook architecture is solid and well-tested. The main concerns are: (a) hooks can bypass safety checks on modified content, (b) session hooks don't actually gate session creation, and (c) tool parameter modification is a privilege escalation vector.

---

## PR #17 — DM Pairing + Telegram Channel Improvements

**Author:** nightfullstar | **Files:** 22 | **Risk: HIGH**

### Overview
Implements a DM pairing system for associating Telegram users with IronClaw accounts, plus Telegram channel improvements (media caption support, /start command). Includes WASM binary and new pairing store.

### Security Issues

1. **Pairing token security** — DM pairing involves generating and validating tokens that link external messaging accounts to IronClaw user accounts. This is an authentication-critical flow. Issues to verify:
   - Token entropy: must use CSPRNG, not PRNG
   - Token expiration: pairing tokens should have a short TTL (minutes, not hours)
   - Token single-use: consumed tokens must be invalidated immediately
   - Rate limiting: pairing attempts should be rate-limited to prevent brute-force
   - **Recommendation:** Verify all four properties. If any are missing, this is a critical vulnerability allowing account takeover.

2. **WASM binary included in PR** — A compiled WASM binary for the Telegram channel is included. Compiled binaries in PRs are a supply chain risk — the binary may not match the source code.
   - **Recommendation:** Do not include compiled binaries in the repository. Build from source in CI. If a binary must be committed, provide the exact build command and verify reproducibility.

3. **Telegram bot token handling** — The Telegram bot token is a bearer credential. Verify it's:
   - Stored in the secrets store (encrypted), not in config files
   - Never logged or included in error messages
   - Transmitted only over HTTPS to api.telegram.org

4. **`fs4` dependency** — New dependency `fs4 = "0.6"` added. This is a file locking library. Verify it's used for the pairing store (preventing concurrent writes) and doesn't introduce unnecessary filesystem access.

5. **WASM channel router/wrapper changes** — Modifications to the WASM channel infrastructure could affect isolation boundaries. Need to verify WASM channels can't access resources outside their sandbox.

### Correctness Issues

6. **Missing integration test coverage** — While integration tests are mentioned, the 22-file scope suggests significant functionality that should be tested end-to-end (pairing flow, message routing, media handling).

**Verdict:** The DM pairing flow is security-critical and needs careful review of token generation, validation, and lifecycle. The WASM binary should not be committed. Telegram bot token handling needs verification.

---

## PR #20 — Direct API Key Auth + Cheap Model Routing

**Author:** desamtralized | **Files:** 5 | **Risk: MEDIUM**

### Overview
Adds support for direct API key authentication (bypassing session-based auth) and a "cheap" LLM provider for lightweight tasks (heartbeat, routing, evaluation).

### Security Issues

1. **Authentication bypass path** (`src/main.rs:190-192`):
   ```rust
   if config.llm.nearai.api_mode == ironclaw::config::NearAiApiMode::Responses {
       session.ensure_authenticated().await?;
   }
   ```
   When using `ChatCompletions` mode with an API key, session authentication is skipped entirely. This is intentional but creates a risk: if `api_mode` is set to `ChatCompletions` without an actual API key being configured, the agent starts without any authentication.
   - **Recommendation:** Add an explicit check: if `api_mode == ChatCompletions`, verify that `api_key.is_some()` before skipping session auth. Fail with a clear error otherwise.

2. **Onboarding bypass** (`src/main.rs:801-807`):
   ```rust
   if std::env::var("NEARAI_API_KEY").is_err() {
       let session_path = ironclaw::llm::session::default_session_path();
       if !settings.onboard_completed && !session_path.exists() {
           return Some("First run");
       }
   }
   ```
   Setting `NEARAI_API_KEY` to any value (even empty string) skips the onboarding check. `std::env::var()` returns `Ok("")` for empty values.
   - **Recommendation:** Use `optional_env()` or check `.is_ok_and(|v| !v.is_empty())`.

3. **Cheap LLM shares credentials** (`src/llm/mod.rs`):
   ```rust
   let mut cheap_config = config.nearai.clone();
   cheap_config.model = cheap_model.clone();
   ```
   The cheap provider clones the full config including session tokens and API keys. This is fine for now but means the cheap provider has identical privileges to the main provider. If different permission scoping is ever intended, this would be a problem.

### Correctness Issues

4. **Cheap LLM fallback is clean** — `cheap_llm()` properly falls back to the main LLM when no cheap model is configured:
   ```rust
   fn cheap_llm(&self) -> &Arc<dyn LlmProvider> {
       self.deps.cheap_llm.as_ref().unwrap_or(&self.deps.llm)
   }
   ```
   This is correct and avoids null-related issues.

5. **Only heartbeat uses cheap LLM** — Currently only the heartbeat is wired to use `cheap_llm()`. The routing and evaluation mentioned in the description aren't actually connected yet. This is a documentation/scope issue, not a security concern.

**Verdict:** The main security concern is the authentication bypass when `ChatCompletions` mode is set without an API key, and the onboarding bypass with empty env vars. Both are easily fixable.

---

## PR #28 — Multi-Provider LLM Failover

**Author:** ztsalexey | **Files:** 6 (+485/-13) | **Risk: MEDIUM**

### Overview
Adds a `FailoverProvider` that wraps multiple LLM providers and tries them sequentially on retryable errors. Configurable via `NEARAI_FALLBACK_MODEL` env var.

### Security Issues

1. **Retryable error classification** — The failover provider needs to distinguish between retryable errors (rate limits, timeouts, temporary unavailability) and non-retryable errors (auth failures, invalid requests, safety blocks). If safety-related rejections (e.g., content policy violations) are classified as retryable, the failover would attempt the same harmful request against a different model that might not have the same safety filters.
   - **Recommendation:** Verify the error classification logic explicitly excludes content policy / safety responses from retry. Authentication errors (401/403) should also NOT be retried against the same provider.

2. **Credential isolation between providers** — When failing over from one model to another, verify that provider-specific auth tokens aren't leaked across provider boundaries. Since both providers share the NEAR AI infrastructure, this is lower risk, but should be verified if external providers are ever added.

3. **Logging of failover events** — Failover events should be logged at INFO level so operators can detect degraded service. Avoid logging request content in failover error messages (could leak sensitive user data).

### Correctness Issues

4. **Config is straightforward** — The `fallback_model: Option<String>` config addition is clean and follows existing patterns. The `optional_env("NEARAI_FALLBACK_MODEL")?` usage is consistent.

5. **Need to verify timeout handling** — If the primary provider times out, the total latency for a failover request doubles. Need to verify the failover provider has its own timeout or respects overall request budgets.

**Verdict:** Sound architecture. Main concern is ensuring safety-related errors are not retried. Need to see the full `failover.rs` to verify error classification logic.

---

## PR #13 — Okta SSO WASM Tool

**Author:** ilblackdragon | **Files:** 5 (+662) | **Risk: LOW**

### Overview
Adds a WASM-sandboxed tool for Okta SSO integration: user profile management, app catalog, and SSO launch links. Uses OAuth2 with PKCE.

### Security Issues

1. **Wildcard host patterns** (`okta-tool.capabilities.json`):
   ```json
   "host": "*.okta.com"
   ```
   While wildcards are necessary for multi-tenant Okta, the pattern `*.okta.com` would match any subdomain, including attacker-controlled ones if Okta ever allows custom subdomains. This is acceptable risk given Okta's domain model.

2. **Bearer token injection is properly scoped** — The credential injection only applies to matching host patterns:
   ```json
   "host_patterns": ["*.okta.com", "*.oktapreview.com", "*.okta-emea.com"]
   ```
   This is correct. The WASM sandbox's allowlist enforcement prevents the token from being sent elsewhere.

3. **OAuth scopes include `okta.users.manage.self`** — This allows the tool to modify the user's own Okta profile. This is intentional (the `update_profile` function) but should be called out: a compromised or confused agent could modify the user's Okta profile fields.
   - **Recommendation:** Consider whether `manage.self` is strictly necessary. If the tool only needs read access to profiles, remove this scope.

4. **Domain read from workspace** — The Okta domain is read from workspace storage (`okta/domain`). If an attacker can write to workspace paths, they could redirect API calls to a malicious server. The allowlist mitigates this since only `*.okta.com` etc. are allowed.

### Correctness Issues

5. **Error handling is solid** — Okta API errors are properly parsed, with fallback to status code + body display. UTF-8 validation is done on response bodies.

6. **App search is case-insensitive** — `search_apps` and `get_app_sso_link` use `to_lowercase()` comparison, which is correct.

7. **No pagination** — `list_apps` and `search_apps` fetch all app links in one call. For users with many apps, this could hit Okta's default page size (20). The code doesn't handle `Link` header pagination.
   - **Recommendation:** Add pagination support or document the limitation.

**Verdict:** Clean, well-sandboxed WASM tool. The capabilities file properly restricts access. Minor concerns about the `manage.self` scope and missing pagination.

---

## PR #10 — Benchmarking Harness for Agent Evaluation

**Author:** ilblackdragon | **Files:** ~15 | **Risk: LOW**

### Overview
Adds `ironclaw-bench`, a separate Rust crate in `benchmarks/` for evaluating agent performance. Supports custom JSONL benchmarks, GAIA, SWE-bench, and TAU-bench adapters. Pluggable scoring with exact, contains, regex, and LLM-based modes.

### Security Issues

1. **Regex scoring — potential ReDoS** (`benchmarks/src/adapters/custom.rs`):
   ```rust
   "regex" => { /* regex matching on response */ }
   ```
   If benchmark definition files come from untrusted sources, a malicious regex pattern could cause catastrophic backtracking (ReDoS). Since this is a dev tool reading local files, the risk is low.
   - **Recommendation:** Use `regex` crate's default configuration (which has built-in protection against catastrophic backtracking). Consider adding a timeout for regex evaluation if accepting external benchmark definitions.

2. **File path handling** — Benchmark JSONL files are read from the filesystem. If file paths are ever derived from user input, this could be a path traversal issue. As a CLI tool, this is acceptable.

3. **LLM scoring placeholder** — The `"llm"` scoring mode is a placeholder. When implemented, ensure the LLM scoring prompt can't be influenced by the benchmark response (prompt injection through evaluation).

### Correctness Issues

4. **Edition 2024 + rust-version 1.85** — The benchmarks crate uses Rust edition 2024 which is very new. Verify all CI environments have Rust 1.85+.

5. **Workspace member addition** — Adding `benchmarks` to the workspace members in root `Cargo.toml` is correct. Verify it doesn't affect the main binary's compilation or dependencies.

6. **Test coverage** — Tests validate exact and contains matching. Regex and LLM modes should have tests added before merge.

**Verdict:** Low-risk dev tooling PR. The main code-level concern is ReDoS in regex scoring, which is mitigated by the `regex` crate's defaults. Approve with minor comments.

---

## Cross-Cutting Concerns

### 1. Safety Layer Integration
PRs #18 (hooks) and #20 (cheap LLM) both create paths where content can bypass or skip the safety layer:
- Hooks can modify content after safety checks
- The cheap LLM is used for heartbeat but may not have the same safety enforcement as the main LLM

**Recommendation:** Audit the safety layer enforcement order in the agent loop after all PRs are merged.

### 2. Merge Conflict Risk
PRs #18 and #20 both modify `AgentDeps`, `src/agent/agent_loop.rs`, and `src/main.rs`. Merging order matters and will require conflict resolution.

### 3. Dependency Audit
Across all PRs, new dependencies include:
- `ed25519-dalek`, `argon2`, `zeroize`, `borsh`, `bs58` (PR #14 — crypto)
- `fs4` (PR #17 — file locking)
- No new deps for PRs #18, #20, #28 (use existing tokio/serde)

Run `cargo audit` after merging crypto dependencies.

---

## Recommended Merge Order

1. **PR #10** (benchmarks) — Independent, low risk
2. **PR #13** (Okta WASM) — Independent, low risk, self-contained
3. **PR #20** (API key auth + cheap LLM) — After fixing auth bypass
4. **PR #28** (failover) — After verifying error classification
5. **PR #18** (hooks) — After addressing safety bypass concerns
6. **PR #17** (Telegram + pairing) — After verifying token security, removing binary
7. **PR #14** (key management) — Last, needs most review, highest risk
