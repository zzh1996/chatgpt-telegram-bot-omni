# Anthropic Messages API (Claude) and how OpenRouter maps it

Research date: 2026-11-11. Every claim comes from live docs fetched that day: the raw Markdown of platform.claude.com/docs (`*.md` URLs), OpenRouter docs and OpenAPI specs, and the public OpenRouter `GET /api/v1/models` and `/endpoints` JSON. Citations are reference-style links (`[name]`), defined at the bottom. **UNVERIFIED** marks anything the docs don't state outright (inferences, behavior nobody tested live). No request was sent to either API.

---

## 0. Takeaways for the Rust bot

1. **There are two API versions, and they behave differently.** `anthropic-version: 2026-10-21` is current: `temperature`/`top_p`/`top_k` return a 400 on **every** model, a `document` block in an assistant message returns a 400, and `citations` on user `text` blocks return a 400. `2023-06-01` still accepts sampling parameters on older models such as Sonnet 4.6 ([versioning], [mig1021]). Because the bot exposes every parameter, it has to pin the version deliberately, either per model or as a user toggle.
2. **Claude 5.x thinking is opinionated.** On Opus 5.5 and Fable 5.x, thinking is always on, `disabled` and `budget_tokens` return a 400, and depth is controlled by `output_config.effort`. Sonnet 5.5's "off" setting is `thinking:{type:"between_tools"}`. `display` defaults to `"omitted"`, which returns empty thinking text ([thinking]).
3. **History must be append-only on preserved-thinking models** (Fable 5.2/5.1, Mythos 5.2/5.1, Opus 5.5, Sonnet 5.5, Haiku 5.5). Changing `system` or `tools`, or editing any earlier message, invalidates later thinking blocks. That can return a 400, and on Fable 5.2 it does for every account ([preserved]). A Telegram reply-fork that replays the exact prefix is fine. Toggling web search mid-chat is an edit, so use `tool_removal`/`tool_addition` system-message blocks instead. A changed system prompt goes in a mid-conversation `role:"system"` message ([midsys]).
4. **Store each `content` array verbatim and replay it verbatim.** That includes empty `thinking` blocks (`thinking:""` plus `signature`), `redacted_thinking`, `server_tool_use`, `*_tool_result` with `encrypted_content`, and `fallback` and `compaction` blocks ([preserved], [websearch], [refusals]).
5. **No re-attach or resume exists for a dropped stream.** The options are: (a) a client-side continuation request, (b) the Message Batches API, which you poll, finishes within 24h or expires, and keeps results 29 days, or (c) the Message Threads beta, which is not a re-attach either ([streaming], [batch], [threads]). Through OpenRouter, an aborted connection **cancels** generation on the Anthropic and Azure providers. On Bedrock and Vertex it is not cancelled ([or-stream]).
6. **Use OpenRouter's Anthropic-compatible endpoint `POST https://openrouter.ai/api/v1/messages`.** It keeps native blocks (signatures, server-tool blocks, `cache_control`, `block_binding`, citations, `input_transformations`). Chat Completions flattens them into `reasoning_details` and annotations ([or-msgs], [or-reason], [or-opus5]).
7. **OpenRouter injects or rewrites some parameters.** It sends `thinking.display:"summarized"` by default, silently drops `budget_tokens` on 5.5 models, and returns its own 400 when you disable reasoning on mandatory-reasoning models. Its `openrouter:web_search` server tool silently falls back to **Exa** (not Anthropic's search) on endpoints without native search. Pin `provider:{only:["anthropic"],allow_fallbacks:false,require_parameters:true}` and always send `thinking.display` explicitly ([or-c55], [or-websearch], [or-provider]).

---

## 1. Request parameters

### 1.1 Endpoint, auth, versions, size

| Item | Value | Src |
|---|---|---|
| Endpoint | `POST https://api.anthropic.com/v1/messages` (+ `/v1/messages/count_tokens`, `/v1/messages/batches`, `/v1/files`, `/v1/models`) | [create], [apioverview] |
| Auth | `x-api-key: <key>` or `Authorization: Bearer <key>` (both appear in the official examples) | [create], [betas] |
| `anthropic-version` | `2026-10-21` (latest) or `2023-06-01`. On Bedrock, `2026-10-21` exists only for Claude 4.7+ | [versioning] |
| `anthropic-beta` | Comma-separated list (repeated headers also work on the Claude API; Google-managed Vertex reads only one header). Unknown value → 400 `invalid_request_error` | [betas] |
| Other headers | `anthropic-workspace-id` (multi-workspace credentials), `anthropic-user-profile-id` (attribute the request to an end customer) | [create], [relnotes] |
| Max request | 128 MB for Messages and count_tokens on the 1P API (raised from 32 MB on 2026-10-28). Batches 256 MB, Files 500 MB. Bedrock 20 MB, Google-managed Vertex 30 MB, CPoAWS/GC-Anthropic/Foundry 32 MB. Over the limit → 413 `request_too_large` | [apioverview], [relnotes] |
| Messages per request | ≤ 100,000. Consecutive same-role turns are merged | [create] |
| Rust SDK | None official (SDKs: Python, TS, Java, Go, Ruby, C#, PHP). Use raw HTTP + SSE | [mig1021] |

### 1.2 Current model matrix (Claude API)

| Model (API id) | Context | Max out | `thinking` omitted → | `disabled` | `enabled`+budget | `between_tools` | Default effort | Effort levels | Prefix check | Account-bound thinking |
|---|---|---|---|---|---|---|---|---|---|---|
| Fable 5.2 `claude-fable-5-2` (and Mythos 5.2) | 1M | 128K | adaptive | 400 | 400 | 400 | high | low…max | ✔ (400 on **all** accounts) | ✔ |
| Opus 5.5 `claude-opus-5-5` | 1M | 128K | adaptive | 400 | 400 | 400 | **medium** | low…max | ✔ | ✗ |
| Sonnet 5.5 `claude-sonnet-5-5` | 1M | 128K | adaptive | 400 | 400 | ✔ (≤high effort; no other field) | high | low…max | ✔ | ✔ |
| Haiku 5.5 `claude-haiku-5-5` | 1M | 128K | adaptive | ✔ at ≤high | 400 | 400 | **medium** | low…max | ✔ | ✔ |
| Fable 5.1 / 5, Mythos 5.1 / 5 | 1M | 128K | adaptive | 400 | 400 | 400 | high | low…max | 5.1 ✔ | ✗ |
| Opus 5 `claude-opus-5` | 1M | 128K | adaptive | ✔ at ≤high | 400 | 400 | high | low…max | ✗ | ✗ |
| Sonnet 5 `claude-sonnet-5` | 1M | 128K | adaptive | ✔ | 400 | 400 | high | low…max | ✗ | ✗ |
| Opus 4.8 / 4.7 | 1M | 128K | **off** | ✔ | 400 | 400 | high | low…max | ✗ | ✗ |
| Opus 4.6 / Sonnet 4.6 | 1M | 128K | off | ✔ | deprecated (works) | 400 | high | low/med/high/max (no xhigh) | ✗ | ✗ |
| Opus 4.5, Sonnet 4.5 (retires 2026-11-30), Haiku 4.5 | 200K | 64K | off | ✔ | required for thinking | 400 (`adaptive` also 400) | Opus 4.5 high; others n/a | Opus 4.5 low/med/high only | ✗ | ✗ |

Sources: config table [thinking]; output limits [thinking]; context [ctxwin]; defaults and levels [effort], [models]; prefix-check and account rules [preserved]; Sonnet 4.5 deprecation [create].
- **Prices (USD/MTok in/out, cache read):** Fable 5.2 $8/$40, $0.20. Opus 5.5 $4/$20, $0.20 (0.05×). Sonnet 5.5 $2/$10, $0.10 (0.05×). Haiku 5.5 $0.10/$0.50 for prompts ≤100K and $0.50/$2.50 above. Opus 4.5–4.8 and Opus 5: $5/$25. Sonnet 4.6: $3/$15. Haiku 4.5: $1/$5 ([pricing]).
- **Long context:** 1M is the default on every 1M model, with no beta header and standard pricing, except Haiku 5.5's tier above 100K ([ctxwin], [pricing]). The `context-1m-2025-08-07` beta for Sonnet 4.5 and Sonnet 4 was **retired on 2026-04-30** ([relnotes]).
- **Overflow:** if the input alone exceeds the window → 400 "prompt is too long". On 4.5+ models, input+`max_tokens` may exceed the window; generation then stops with `stop_reason:"model_context_window_exceeded"` ([ctxwin]).
- **Batches only:** `output-300k-2026-03-24` raises `max_tokens` to 300K on Opus 5.5/5/4.8/4.7/4.6, Sonnet 5.5/5/4.6 and Haiku 5.5 ([models]).
- **Models API:** `GET /v1/models/{id}` returns `max_input_tokens`, `max_tokens`, `line`, `lifecycle`, and `capabilities.{thinking.types.{adaptive,enabled,disabled,between_tools}, effort.{low..max}, server_tools.{web_search,code_execution}, code_execution, structured_outputs, citations, pdf_input, image_input, context_management, batch}`. Use it to drive the bot's per-model toggles ([models-api], [models]).

### 1.3 Top-level body parameters

| Param | Shape / values | Notes | Src |
|---|---|---|---|
| `model` | string | required | [create] |
| `max_tokens` | int ≥ 0 | Required. Includes thinking. `0` = cache pre-warm (empty content, `stop_reason:"max_tokens"`) | [create], [caching] |
| `messages` | `[{role:"user"\|"assistant"\|"system", content}]` | `role:"system"` mid-conversation messages are only on the models listed in §2.3 | [create], [midsys] |
| `system` | string or `[{type:"text",text,cache_control?}]` | | [create] |
| `stop_sequences` | string[] | Response gets `stop_reason:"stop_sequence"` and `stop_sequence` | [create] |
| `stream` | bool | SSE | [create] |
| `metadata.user_id` | string ≤ 512 | Opaque id (uuid/hash, no PII). Used for abuse detection | [create] |
| `thinking` | `{type:"adaptive",display?}` \| `{type:"enabled",budget_tokens,display?}` \| `{type:"disabled"}` \| `{type:"between_tools"}` | See 1.4 | [create], [thinking] |
| `output_config` | `{effort?: low\|medium\|high\|xhigh\|max, format?: {type:"json_schema",schema}}` (+ beta `task_budget:{type:"tokens",total≥20000,remaining?}`) | See 1.5 and 1.7 | [create], [effort], [so] |
| `tools` / `tool_choice` | `tool_choice`: `auto` (with optional `disable_parallel_tool_use`) \| `any` \| `tool{name}` \| `none` | `any`/`tool` → 400 on Opus 5.5, Sonnet 5.5, Fable 5.2/5.1, Mythos 5.2/5.1 | [create], [errors] |
| `cache_control` | `{type:"ephemeral",ttl?:"5m"\|"1h"}` | Top-level = automatic caching (§7) | [create], [caching] |
| `container` | string id or `{id?, skills?[]}` | Reuses a code-execution container | [create], [codeexec] |
| `service_tier` | `"auto"`\|`"standard_only"` | **Legacy.** Priority Tier is no longer sold, and isn't supported on Fable 5.2/5.1, Mythos 5.x, Opus 5.5/5, Sonnet 5.5/5 or Haiku 5.5 (Fable 5 is supported) | [create], [tiers] |
| `inference_geo` | `"global"` (default) \| `"us"` | 4.6+ only (400 on older models). `"us"` costs 1.1× on all token types. 1P, CPoAWS and GC-Anthropic only. Response: `usage.inference_geo` | [residency], [pricing] |
| `speed` | `"standard"`\|`"fast"` | Fast mode on Fable 5.2 ($16/$80), Opus 5.5 ($8/$40), Opus 5/4.8 (research preview, needs `fast-mode-2026-02-01`). Response: `usage.speed`. Not available in Batches. Fast mode on Opus 4.8 is removed on 2026-11-30 | [create], [pricing], [relnotes] |
| `diagnostics` | `{previous_message_id: "msg_…"\|null}` | GA. Response `diagnostics.cache_miss_reason` | [create], [relnotes] |
| `temperature`/`top_p`/`top_k` | `2023-06-01` only | See 1.6 | [mig1021] |
| **Beta-only:** `context_management`, `mcp_servers`, `fallbacks`, `fallback_credit_token`, `thread`, `compaction`, deprecated `output_format` | | §1.8 | [beta-create] |

### 1.4 `thinking`

```json
{"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}
```
- **`display`:** `"summarized"` returns a summary written by a *different* model. `"omitted"` returns `thinking:""` plus `signature`, and has the fastest time to first text token. `"updates"` (beta `thinking-display-updates-2026-08-18`) returns only the progress-update notes as text. There is **no "full" option**: raw chain of thought is never returned ("contact sales"). Defaults: `omitted` on every Claude 5.x model and on Opus 4.8/4.7; `summarized` on Opus/Sonnet 4.6 and earlier. `display` is billed the same under every value. Switching `display` between turns is allowed ([thinking]).
- **`budget_tokens`:** ≥1024 and < `max_tokens`, except with interleaved manual mode. Can't be combined with `max_tokens:0`. Removed on 4.7+ (400: `"thinking.type.enabled" is not supported for this model…`) ([ext-thinking], [errors]).
- **Interleaved thinking** is automatic with `adaptive`, no header. In manual mode, Opus 4.5, Sonnet 4.5 and earlier need `interleaved-thinking-2025-05-14`. Sonnet 4.6 manual is deprecated, Opus 4.6 manual never interleaves, and Haiku 4.5 doesn't support it ([ext-thinking], [thinking]).
- **Progress updates:** Fable 5.2/5.1/5, Mythos 5.2/5.1, Opus 5.5 and Sonnet 5.5 write between-tool notes as their own `thinking` blocks, each with its own `signature`, immediately before a `tool_use`/`server_tool_use` block. After an interruption, such a block can carry `"This part of the response was interrupted before it finished."` ([thinking]).
- **Billing:** full thinking is billed as output, whatever `display` is. `usage.output_tokens_details.thinking_tokens` gives the count (only on the final `message_delta` when streaming) ([ext-thinking], [create]).
- **Changing** the mode, `budget_tokens` or top-level effort invalidates the prompt cache (message-level breakpoints always miss). Toggling thinking *within* a tool-use turn silently disables thinking for that request ([thinking]).

### 1.5 `output_config.effort`

`max` is on Fable/Mythos 5.x, Opus 5.5/5/4.8/4.7/4.6, Sonnet 5.5/5/4.6 and Haiku 5.5. `xhigh` is the same set minus Opus/Sonnet 4.6. Opus 4.5 has low/medium/high only. Sending the model's default value is identical to omitting it. Effort affects all output tokens (text, tool calls, thinking) and is a soft signal, while `max_tokens` is the hard cap ([effort]).
**Per-message effort (beta `mid-conversation-output-config-2026-07-01`):** append `{"role":"system","content":[],"output_config":{"effort":"low"}}`. This keeps the prompt cache. On the Claude API it works on Fable 5.2/5.1, Mythos 5.2/5.1, Opus 5.5, Opus 5, Sonnet 5.5 and Haiku 5.5 (Haiku only with adaptive thinking). With Sonnet 5.5 `between_tools` or Haiku 5.5 `disabled`, an effort change returns a 400 ([effort]).

### 1.6 Sampling parameters

- `2026-10-21`: any `temperature`/`top_p`/`top_k` → 400 on every model, even at default values ([mig1021]).
- `2023-06-01`: on Fable/Mythos 5.x, Opus 5.5/5/4.8/4.7, Sonnet 5.5/5, Haiku 5.5 and Mythos Preview, non-default values → 400. The `top_p` default there is `0.99`, so `top_p:1` → 400, and sending both `temperature` and `top_p` → 400. On older models, sampling is unrestricted when thinking is off. With thinking on, `temperature` and `top_k` are incompatible and `top_p` must be 0.95–1 ([thinking]).
- On Sonnet 4.6, non-default sampling values may drop the text watermark ([watermark]).

### 1.7 Structured outputs

`output_config.format: {"type":"json_schema","schema":{…}}` (GA, all models) and/or `strict:true` on tools. The deprecated `output_format` needs the `structured-outputs-2025-11-13` header, otherwise 400. Incompatible with **citations** (400) and **prefill**. The API injects an extra system prompt (more input tokens), and changing the format invalidates the cache. Compiled grammars are cached for 24h ([so]).

### 1.8 Beta headers: what's still needed (2026-11)

| Feature | Header | Status |
|---|---|---|
| 1M context | `context-1m-2025-08-07` | **Not needed.** No effect on 1M-default models; retired for Sonnet 4.5/4 ([ctxwin], [relnotes], [fallback-credit]) |
| 128K output | `output-128k-2025-02-19` | Remove; no effect on Claude 4+ ([opus55-mig]) |
| 300K output (Batches) | `output-300k-2026-03-24` | Needed on Batches ([models]) |
| Files API | `files-api-2025-04-14` | Not needed (GA). Sending it switches responses to the old shapes ([files]) |
| Code execution / web search / web fetch | none (legacy `code-execution-*` still accepted) | GA ([codeexec], [tool-ref]) |
| Interleaved thinking (manual) | `interleaved-thinking-2025-05-14` | Older models only ([ext-thinking]) |
| `display:"updates"` | `thinking-display-updates-2026-08-18` | beta |
| `block_binding` / `input_transformations` | `thinking-binding-controls-2026-08-01` | beta ([preserved]) |
| Per-message effort | `mid-conversation-output-config-2026-07-01` | beta |
| `clear_at` turn-scoped system msgs | `mid-conversation-system-clear-at-2026-08-21` | beta ([midsys]) |
| `tool_addition`/`tool_removal` | `inline-tools-2026-09-15` (incl. inline `tool_definition`) or `mid-conversation-tool-changes-2026-07-01` (references only) | beta ([midsys]) |
| Context editing | `context-management-2025-06-27` (`clear_tool_uses_20250919`, `clear_thinking_20251015`) | beta ([ctxedit]) |
| Compaction | `compact-2026-01-12` (`context_management.edits:[{type:"compact_20260112"}]`, default trigger 150K) / `compact-2026-09-04` (on-demand `compaction` param) | beta ([compaction-threshold], [compaction]) |
| Server-side refusal fallback | `server-side-fallback-2026-07-01` (`fallbacks:"default"` or ≤3 models) | beta, Claude API + GC-Anthropic ([refusals]) |
| Fast mode Opus 5 / 4.8 | `fast-mode-2026-02-01` | Opus 5.5 / Fable 5.2 don't need it ([relnotes]) |
| Task budgets | `task-budgets-2026-03-13` | beta |
| Advisor tool | `advisor-tool-2026-03-01` | beta ([advisor]) |
| MCP connector | `mcp-client-2025-11-20` / `mcp-client-2026-09-15` | beta ([tool-ref]) |
| Message threads | `message-threads-2026-08-12` | beta ([threads]) |
| Cache diagnostics | `cache-diagnosis-2026-04-07` | Not needed (GA since 2026-09-23) ([relnotes]) |

**Prompts the API adds that the bot can't remove** (this matters for the "transparent request" principle):
- A tool-use system prompt whenever `tools` is set: 286 tokens on Opus 5.5 ([pricing]).
- Extra system-prompt text when web search or citations are enabled ([caching], [citations]).
- The structured-output instructions ([so]).
- Context-awareness token-budget tags on Sonnet 5, Sonnet 4.6, Sonnet 4.5 and Haiku 4.5 ([ctxwin]).
- The advisor's own system prompt ([advisor]).

---

## 2. Multi-turn state (replaying history statelessly)

### 2.1 Thinking blocks

Response shapes: `{"type":"thinking","thinking":"…|''","signature":"…"}` and `{"type":"redacted_thinking","data":"…"}` (opaque, safety-redacted) ([thinking]).

| Rule | Detail | Src |
|---|---|---|
| Required | Within a tool-use turn (assistant `tool_use` → user `tool_result` → …), pass back **every** `thinking` and `redacted_thinking` block of that assistant turn, complete, unmodified, in order. Otherwise 400 `"…blocks in the latest assistant message cannot be modified…"` | [thinking], [errors] |
| Recommended | Across turns, pass everything back. The API filters automatically and bills only the blocks it shows the model | [thinking] |
| Allowed | Outside tool use, omitting prior turns' thinking is allowed. On preserved-thinking models, remove only a prefix run, a suffix run, or all blocks: a gap in the middle invalidates later blocks, and so does re-adding a removed block | [thinking], [preserved] |
| Keep empty blocks | With `display:"omitted"`, keep the block (`thinking:""`). Text you put in that field is ignored, but dropping the field → 400, and an empty `signature` fails | [preserved], [thinking] |
| Manual mode | The final assistant turn of a thinking-enabled request must start with a thinking block. Adaptive mode drops that rule | [thinking] |
| Preservation | **All prior turns kept** (and billed as input): Opus 4.5+, Sonnet 4.6+, Haiku 5.5, Fable/Mythos. **Last turn only** (older blocks auto-stripped): earlier Opus/Sonnet and Haiku ≤4.5 | [thinking] |
| Cross-platform | Signatures work across Claude API, Bedrock and Vertex, **except** blocks from Sonnet 5.5, Haiku 5.5, Fable 5.2 and Mythos 5.2, which work only in the producing account or an account linked to it (dropped silently otherwise; `reason:"organization_binding_mismatch"`) | [thinking], [preserved] |

**Model switching:** keep sending every block. The API silently drops (without billing) any block the target model can't read, and the dropped block doesn't count as an edit. Claude API reader matrix:

| From \ To | Fable/Mythos 5.2 | Fable/Mythos 5.1 | Opus 5.5 | Sonnet 5.5 | Haiku 5.5 |
|---|---|---|---|---|---|
| Haiku 5.5 | ✅ | — | ✅ | ✅ | ✅ |
| Sonnet 5.5 | ✅ | — | ✅ | ✅ | — |
| Opus 5 / Opus 5.5 | ✅ | ✅ | ✅ | — | — |
| Fable/Mythos 5 or 5.1 | ✅ | ✅ | — | — | — |
| Fable/Mythos 5.2 | ✅ | — | — | — | — |

Sonnet 5 and earlier 4.x models → any 5.x model keeps the reasoning; the reverse doesn't ([preserved]).

### 2.2 Preserved thinking: the prefix check

The prefix check runs on Fable 5.2, Mythos 5.2, Fable 5.1, Mythos 5.1, Opus 5.5, Sonnet 5.5 and Haiku 5.5. It compares top-level `system`, `tools`, and every message before the block, byte-equal by value (JSON key order doesn't matter). Parameters such as `effort`, `max_tokens`, `thinking.display`, `tool_choice`, `metadata` and `cache_control` aren't part of the prefix ([preserved]).

| Outcome when `prefix_mismatch_behavior` is unset | Account created ≥ 2026-08-31 | Account created earlier |
|---|---|---|
| Fable 5.2, Mythos 5.2, Mythos 5.1 | 400 | 400 |
| Fable 5.1, Opus 5.5, Sonnet 5.5, Haiku 5.5 | 400 | No error (failing blocks reach the model) |

- **400 text:** ``messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation. Remove the block, or set `thinking.block_binding.prefix_mismatch_behavior` to "drop_block".`` Resending the same body fails every time ([preserved], [errors]).
- **Opt-in control** (beta `thinking-binding-controls-2026-08-01`): `thinking.block_binding.prefix_mismatch_behavior: "error"|"drop_block"`, plus a top-level `input_transformations:[{type:"thinking_dropped"|"thinking_mismatch_allowed", path:"messages.3.content.0", reason:"prefix_binding_mismatch"|"model_binding_mismatch"|"organization_binding_mismatch"}]`. When streaming, the array arrives on `message_start`. `drop_block` can raise output tokens. On Sonnet 5.5 and Haiku 5.5, `block_binding` works only with `adaptive` ([preserved]).
- **What counts as an edit:** editing, reordering or deleting an earlier message; any change to `system` or `tools`; shortening a `tool_result`; re-encoding an image; a URL whose bytes changed; an unpinned MCP tool list changing. **Not edits:** appending messages, a `defer_loading` tool that nothing references yet, a model change, `cache_control` moves, server-side compaction or context editing ([preserved]).
- **Branching:** "A branch that replays that history unchanged up to the fork point keeps its thinking." That is exactly the Telegram reply-tree case ([preserved]).
- **Guidance for gateways:** pass `anthropic-beta` and `block_binding` through unchanged, return `input_transformations` to the caller, don't move a `role:"system"` message into `system`, use `tool_choice:{type:"none"}` instead of removing tools, and never silently strip thinking and retry ([preserved]).

### 2.3 Replacing prefix edits (bot features)

| Bot action | Append-only replacement | Header |
|---|---|---|
| User changes the system prompt mid-chat | Append `{"role":"system","content":"…"}` after the newest user message | none |
| Toggle a tool (e.g. web search) | Declare every tool up front with `defer_loading:true` on the inactive ones, then append a system message with `{"type":"tool_addition"\|"tool_removal","tool":{"type":"tool_reference","name":"web_search"}}`, or an inline `{"type":"tool_definition","definition":{…}}` | `inline-tools-2026-09-15` |
| One-turn reminder | `{"role":"system","clear_at":"next_user_message","content":"…"}`. Keep it verbatim afterwards | `mid-conversation-system-clear-at-2026-08-21` |
| Effort change | Per-message `output_config` (§1.5) | `mid-conversation-output-config-2026-07-01` |
| Files that might change | Send base64 or a Files API `file_id` from the first request, not a URL | none |

- **Models that accept mid-conversation system messages:** Fable 5.2/5.1/5, Mythos 5.2/5.1/5, Opus 5.5, Opus 5, Opus 4.8, Sonnet 5.5, Haiku 5.5. Not Sonnet 5 ([midsys]).
- **Placement rules:** such a message can't be the first entry; it must follow a `user` turn (or an assistant turn ending in a server-tool result); it can't sit between `tool_use` and `tool_result`. Never put untrusted content in it, because it carries operator authority ([midsys]).

### 2.4 Server-tool blocks and other state

Minimal replay after a web search turn. The second request appends one user message and changes nothing before it:
```json
{"model":"claude-opus-5-5","max_tokens":16000,
 "thinking":{"type":"adaptive","display":"omitted"},
 "tools":[{"type":"web_search_20250305","name":"web_search","max_uses":5}],
 "messages":[
  {"role":"user","content":"Who won yesterday?"},
  {"role":"assistant","content":[
    {"type":"thinking","thinking":"","signature":"EosnCk…"},
    {"type":"server_tool_use","id":"srvtoolu_01…","name":"web_search","input":{"query":"match result"}},
    {"type":"web_search_tool_result","tool_use_id":"srvtoolu_01…","content":[
      {"type":"web_search_result","url":"https://…","title":"…","encrypted_content":"EqgfCio…","page_age":null}]},
    {"type":"text","text":"X won 2–1.","citations":[{"type":"web_search_result_location","url":"https://…","title":"…","encrypted_index":"Eo8BCio…","cited_text":"…"}]}]},
  {"role":"user","content":"And the scorer?"}]}
```

- **Web search:** replay the assistant blocks exactly, including every `encrypted_content` (search results) and `encrypted_index` (citations). A missing or modified value → 400 ([websearch]).
- **`pause_turn`** (server loop hit 10 iterations): resend the paused assistant content unchanged, with the **same `tools`** and the same `container` id. A continuation can pause again; keep every paused response in order ([server-tools], [stop]).
- **Server and client tools mixed in one turn:** the response ends with `stop_reason:"tool_use"` and a `server_tool_use` block that has no result. Reply with a user message containing **only** `tool_result` blocks. The next response starts with the deferred server result ([server-tools]).
- **Code-execution containers:** the response carries `container:{id,expires_at,skills}`. Send `"container":"<id>"` on later requests, or every request gets a fresh container. Containers checkpoint after about 5 min idle and expire **30 days** after creation (`expires_at` is a shorter rolling value). An expired id → error, so retry without it ([codeexec]).
- **Server-side fallback:** keep the `fallback` block exactly where it is. Drop thinking and client `tool_use` blocks that come *before* the final `fallback`, and keep `server_tool_use` blocks only when they're paired with a result ([refusals]).
- **Compaction:** append the full `response.content`, because the `compaction` block replaces the history ([compaction]).
- **Beta-only response blocks to store verbatim:** `fallback`, `compaction`, `mcp_tool_use`/`mcp_tool_result`/`mcp_tool_listing`, `advisor_tool_result`, `container_upload` ([beta-create]).

---

## 3. Server tools (official, run by Anthropic)

| Tool | `type` versions | `name` | Server? | Platforms (Anthropic docs) | Price |
|---|---|---|---|---|---|
| Web search | `web_search_20250305` (basic), `web_search_20260209` (dynamic filtering), `web_search_20260318` (+`response_inclusion`) | `web_search` | ✔ | 1P, CPoAWS, GC-Anthropic, Foundry-on-Anthropic: all versions. Google-managed Vertex and Foundry-on-Azure: basic only. **Not Bedrock** | $10 / 1,000 searches + tokens. Failed searches not billed |
| Web fetch | `web_fetch_20250910`, `_20260209` (dynamic filtering), `_20260309` (+`use_cache`), `_20260318` (+`response_inclusion`) | `web_fetch` | ✔ | 1P, CPoAWS, GC-Anthropic, Foundry (Azure-hosted: `_20250910` only). Not Bedrock or Google-managed Vertex | Tokens only |
| Code execution | `code_execution_20250825` (bash + files), `_20260120` (+REPL persistence and programmatic tool calling), `_20260521` (same runtime; tells the model about the 90 s cell limit). Legacy `_20250522` is Python-only | `code_execution` | ✔ | 1P, CPoAWS, GC-Anthropic, Foundry (Anthropic-hosted) | Free when the request has web_search/web_fetch `_20260209`+. Otherwise 1,550 free h/org/month, then $0.05/h/container, 5-min minimum |
| Advisor (beta) | `advisor_20260301` | `advisor` | ✔ | 1P, CPoAWS, GC-Anthropic | Advisor model rates |
| Tool search | `tool_search_tool_regex_20251119`, `tool_search_tool_bm25_20251119` | same | ✔ | — (only useful with `defer_loading` tools) | — |
| MCP connector (beta) | `mcp_toolset` + `mcp_servers[]` → `mcp_tool_use`/`mcp_tool_result` | — | ✔ | 1P, CPoAWS, GC-Anthropic, Foundry ([mcp]) | — |
| Memory / bash / text editor / computer / browser | `memory_20250818`, `bash_20250124`, `text_editor_20250728`, `computer_toolset_20260801`, `browser_toolset_20260801` | — | ✗ **client tools**, your app executes them (out of scope under "never fake locally") | — | — |

Sources: [tool-ref], [websearch], [webfetch], [codeexec], [advisor], [pricing].
- **Capability check:** `capabilities.server_tools.web_search.supported` / `.code_execution.supported` in the Models API ([models]).
- **Org-level gating:** an org admin can disable web search or web fetch in the Console; a request then returns a 400. Org-level domain lists also apply, and a request's `allowed_domains` must be a subset of them ([websearch], [webfetch], [server-tools]).

**Dynamic filtering and `allowed_callers`** ([server-tools], [websearch]):
- The `_20260209`+ web tools default to `allowed_callers:["code_execution_20260120"]`, and the API provisions code execution automatically. Don't also declare `code_execution`; if you do, use `_20260120`+. Older code-execution versions return a 400 alongside these web tools.
- Set `"allowed_callers":["direct"]` to disable dynamic filtering. This is required for ZDR and on models without programmatic tool calling, where the API returns a 400 telling you to set it.
- Nested `server_tool_use` and result pairs carry `caller:{type:"code_execution_20260120",tool_id}`.

### 3.1 `web_search`

```json
{"type":"web_search_20260318","name":"web_search","max_uses":5,
 "allowed_domains":["example.com"],          // or blocked_domains, never both (400)
 "user_location":{"type":"approximate","city":"Tokyo","region":"Tokyo","country":"JP","timezone":"Asia/Tokyo"},
 "allowed_callers":["direct"], "response_inclusion":"full"}
```
- **Parameters:**
  - `max_uses` caps searches per *turn* (failed searches count).
  - Domain entries are bare hosts with optional paths; subdomains are included; `*` is allowed only in the path. Web *fetch* ignores entries that have paths.
  - `user_location.country` takes ISO alpha-2 codes (an unsupported code → 400).
  - `response_inclusion:"excluded"` hides nested result pairs that a completed code-execution call consumed ([websearch], [server-tools]).
- **Blocks:** `server_tool_use{id:"srvtoolu_…",name:"web_search",input:{query}}`, then `web_search_tool_result{tool_use_id, content:[{type:"web_search_result",url,title,encrypted_content,page_age}]}`. On error, `content` is a single object `{type:"web_search_tool_result_error", error_code: too_many_requests|invalid_tool_input|max_uses_exceeded|query_too_long|request_too_large|unavailable}`, still HTTP 200. Zero results → empty list ([websearch], [create]).
- **Citations are always on:** text blocks carry `citations:[{type:"web_search_result_location",url,title,encrypted_index,cited_text(≤150 chars)}]`. `cited_text`, `title` and `url` aren't counted as tokens. When showing output to end users, citations must be shown ([websearch]).
- **Usage:** `usage.server_tool_use.web_search_requests` ([websearch]).

### 3.2 `web_fetch`

```json
{"type":"web_fetch_20260318","name":"web_fetch","max_uses":10,"max_content_tokens":100000,
 "citations":{"enabled":true},"use_cache":false,
 "url_sources":{"client_tool_results":{"type":"none"}}}
```
- **Which URLs it can fetch:** only URLs already present in user messages, client tool results, or earlier web_search/web_fetch results. Not URLs that appear only in Claude's own output or the system prompt. Credential-looking URLs are refused unless the credential appears in the system prompt or user text ([webfetch]).
- **Content limits:** text, HTML and PDF only, no JavaScript rendering, URL ≤ 250 chars. Results may come from a fetch cache unless `use_cache:false` (needs `_20260309`+) ([webfetch]).
- **Result:** `web_fetch_tool_result{content:{type:"web_fetch_result",url,retrieved_at,content:{type:"document",source:{type:"text",media_type:"text/plain",data}|{type:"base64",media_type:"application/pdf",data},title,citations}}}` ([webfetch]).
- **Error codes:** `invalid_tool_input, url_too_long, url_not_allowed, url_not_in_prior_context, url_not_accessible, too_many_requests, unsupported_content_type, content_too_large, max_uses_exceeded, unavailable` ([webfetch]).
- **Citations** are off by default; with `citations.enabled` they use `char_location`/`page_location` ([webfetch]).
- **Usage:** `usage.server_tool_use.web_fetch_requests` ([webfetch]).

### 3.3 `code_execution`

- **Environment:** Python 3.11, x86_64, 5 GiB RAM, 5 GiB disk, 1 CPU, **no internet**, pre-installed libraries (pandas, matplotlib, …). Scoped to the workspace ([codeexec]).
- **Sub-tools:** `bash_code_execution` and `text_editor_code_execution`.
  - `bash_code_execution_tool_result{content:{type:"bash_code_execution_result",stdout,stderr,return_code,content:[{type:"bash_code_execution_output",file_id}]}}`
  - `text_editor_code_execution_tool_result{content: text_editor_code_execution_view_result{file_type,content,num_lines,start_line,total_lines} | …create_result{is_file_update} | …str_replace_result{old_start,old_lines,new_start,new_lines,lines}}`
  - Errors: `unavailable, execution_time_exceeded, invalid_tool_input, too_many_requests, output_file_too_large, file_not_found` ([codeexec]).
- **Legacy and PTC blocks:** `code_execution_tool_result{code_execution_result{stdout,stderr,return_code,content[code_execution_output{file_id}]}}`, or `encrypted_code_execution_result{encrypted_stdout,…}` when the code called web search ([create]).
- **Generated files:** only files at the top level of `$OUTPUT_DIR` are captured as `file_id`s. Download with `GET /v1/files/{file_id}/content`; only generated files are `downloadable:true`. Images, video and audio carry C2PA Content Credentials ([codeexec], [files]).
- **Inputs:** a `{"type":"container_upload","file_id":"file_…"}` content block. Files are preloaded, so execution time is billed even if the tool isn't called ([codeexec]).
- **Usage counter mismatch:** the pricing page shows `usage.server_tool_use.code_execution_requests`, but the API reference's `ServerToolUsage` lists only `web_search_requests` and `web_fetch_requests` ([pricing], [create]). **UNVERIFIED** which one is accurate. Parse leniently.
- **Retention:** containers keep data up to 30 days. Code execution isn't ZDR-eligible ([codeexec]).

---

## 4. Document citations

- **Enabling:** set `"citations":{"enabled":true}` on **all or none** of the `document` blocks (and `search_result` blocks). Only text can be cited, not images, and scanned PDFs without a text layer aren't citable ([citations]).
- **Response shape:** answers split into many `text` blocks, each with a `citations[]` array. `cited_text` doesn't count toward output tokens, nor toward input tokens when passed back. Optional `title` and `context` fields aren't cited ([citations]).

| Source | Block | Location object |
|---|---|---|
| Plain text | `{"type":"document","source":{"type":"text","media_type":"text/plain","data":"…"},"title":"…","context":"…","citations":{"enabled":true}}` | `{"type":"char_location","cited_text","document_index","document_title","start_char_index","end_char_index"}` (0-based, end exclusive) |
| PDF | `source:{type:"base64",media_type:"application/pdf",data}` \| `{type:"url",url}` \| `{type:"file",file_id}` | `{"type":"page_location",…,"start_page_number","end_page_number"}` (1-based, end exclusive) |
| Custom content | `source:{"type":"content","content":[{"type":"text","text":"chunk1"},…]}` (no further chunking) | `{"type":"content_block_location",…,"start_block_index","end_block_index"}` (0-based, end exclusive) |
| RAG `search_result` block (user content or `tool_result`) | `{"type":"search_result","source":"kb://1","title":"…","content":[{"type":"text","text":"…"}],"citations":{"enabled":true}}` | `{"type":"search_result_location","source","title","search_result_index","start_block_index","end_block_index","cited_text"}` |

Sources: [citations], [searchres], [create]. The `char_location`, `page_location` and `content_block_location` objects also carry `file_id` (nullable). `document_index` counts every document block across all messages.
- **Streaming:** each citation arrives as `content_block_delta` with `delta:{type:"citations_delta",citation:{…}}` ([citations]).
- **Incompatibilities:** `output_config.format` → 400. Under `2026-10-21`, a user-message `text` block carrying non-empty `citations` → 400; assistant replays are fine ([citations], [mig1021]).

---

## 5. Inputs

| Input | Request shape | Limits |
|---|---|---|
| Image | `{"type":"image","source":{"type":"base64","media_type":"image/jpeg\|png\|gif\|webp","data"}}` \| `{"type":"url","url"}` \| `{"type":"file","file_id"}`. Optional `transformations:{oversized_image:"downsize"\|"error"}` | ≤600 per request (100 on 200K-context models). ≤8000×8000 px. >20 images → stricter per-image limit (keep ≤3000 px). 10 MB base64 per image on 1P (5 MB on Bedrock). Animated GIF → first frame only. Tokens ≈ ⌈w/28⌉·⌈h/28⌉. Downscale above 2,576 px / 4,784 tokens (Claude 4.7+) or 1,568 px / 1,568 tokens (older) |
| PDF | `{"type":"document","source":{base64 \| url \| file}}` | 128 MB per request, ≤600 pages (100 if context < 1M). No encrypted PDFs. Each page = extracted text + page image (≈1.5–3K text tokens plus image tokens) |
| Plain text doc | `source:{"type":"text","media_type":"text/plain","data"}` or a Files API upload with `text/plain` | .docx/.xlsx unsupported: convert to text or PDF |
| Custom content doc | `source:{"type":"content","content":[text\|image blocks]}` | — |
| Files API | `POST /v1/files` (multipart `file`, optional `expires_in_seconds` 3600–7,776,000) → `{"id":"file_…","downloadable":false,…}`. Reference it in `document`/`image`/`container_upload` blocks | 500 MB per file, 1 TB per org. Workspace-scoped (never accept user-supplied ids). Not on Bedrock or Google-managed Vertex |
| Audio / video | **Not supported.** Current models take text and images in, text out | — |

Sources: [vision], [pdf], [files], [opus55], [models].
- **Platform restrictions:** Bedrock and Google-managed Vertex accept only base64 images and PDFs ([vision], [pdf]).
- **Assistant-side documents:** under `2026-10-21`, a `document` block directly in an assistant message → 400 ([mig1021]).

---

## 6. Streaming (SSE)

Event flow: `message_start` (Message with empty `content`; `usage` has input and cache fields; `stop_reason:null`) → for each block: `content_block_start` → `content_block_delta`* → `content_block_stop` → one or more `message_delta` (`delta:{stop_reason,stop_sequence,stop_details,container}`, `usage` **cumulative**) → `message_stop`. `ping` events can appear anywhere, and **unknown event types must be tolerated** ([streaming], [create]).

| Delta `type` | Payload |
|---|---|
| `text_delta` | `text` |
| `thinking_delta` | `thinking` (an empty string under `display:"omitted"`) |
| `signature_delta` | `signature`, sent once just before the thinking block's `content_block_stop` |
| `input_json_delta` | `partial_json`, for `tool_use` and `server_tool_use` blocks (buffered per key unless `eager_input_streaming`) |
| `citations_delta` | `citation` |
| `compaction_delta` | beta compaction |

Sources: [create], [beta-create], [streaming].
- **Thinking block start:** `{"type":"thinking","thinking":"","signature":""}` ([thinking]).
- **Trace under `display:"omitted"`** (from [thinking]):
```
event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EosnCkYICxIMMb3LzNrMu..."}}
event: content_block_stop
data: {"type":"content_block_stop","index":0}
event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}
```
- **Server tools:** `server_tool_use` streams like `tool_use`. Each **result block arrives whole in a single `content_block_start`** with no deltas, and there is a pause while the tool runs ([server-tools], [streaming]).
- **Server-side fallback:** a `fallback` block is a bare start/stop pair at the model boundary. `message_start.model` may name the fallback model ([refusals]).
- **Mid-stream error event:** `event: error` / `data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}`. The HTTP status was already 200 ([streaming]).
- **Stop reasons:** `end_turn`, `max_tokens`, `stop_sequence`, `tool_use`, `pause_turn`, `refusal`, `model_context_window_exceeded`, plus beta `compaction` ([create], [beta-create]).
  - `stop_details` is non-null only for `refusal`: `{type:"refusal",category:"cyber"|"bio"|"frontier_llm"|"reasoning_extraction"|"general_harms"|null,explanation}`, plus `recommended_model` when `fallbacks` is set ([refusals]).
  - A streamed refusal can arrive after partial text, delivered on `message_delta`. You must reset or rephrase the context before continuing ([stream-refusals]).
  - Refusals before any output are billed only in the bio, frontier_llm and reasoning_extraction categories ([refusals], [relnotes]).
- **Usage fields:**
  - `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `cache_creation:{ephemeral_5m_input_tokens,ephemeral_1h_input_tokens}`
  - `output_tokens_details:{thinking_tokens}` and `server_tool_use:{web_search_requests,web_fetch_requests}`
  - `service_tier` (`standard`|`priority`|`batch`), `speed`, `inference_geo`, and `iterations[]` (per-attempt records such as `message`, `fallback_message`, compaction)
  - Total input = `input_tokens + cache_creation_input_tokens + cache_read_input_tokens` ([create], [caching], [refusals]).
- **Accumulate deltas into the stored `content` array exactly as received.** On the bot this means one `ContentBlock` per `index`, including zero-text thinking blocks ([preserved]).

---

## 7. Prompt caching

- **Markers:** `cache_control:{type:"ephemeral",ttl:"5m"|"1h"}` on tools, system blocks, message blocks, images, documents, `tool_use`/`tool_result`. Up to **4 breakpoints**. Thinking blocks can't be marked directly; they are cached as part of earlier turns. Empty text blocks can't be cached ([caching]).
- **Automatic caching:** a top-level `cache_control` puts the breakpoint on the last cacheable block and moves it forward each turn. It uses one of the 4 slots. If the last block has an explicit marker with a different TTL → 400; if 4 explicit markers exist → 400. Available everywhere except legacy Bedrock InvokeModel ([caching]).
- **Prefix rules:**
  - Order is `tools` → `system` → `messages`, with exact byte match.
  - Writes happen only at breakpoints; reads look back **20 blocks**. A run of parallel `tool_use` or `tool_result` blocks counts as one position.
  - The 5-minute lifetime is measured from the start of the request, and each hit refreshes it for free.
  - A cache entry becomes available once the first response starts ([caching]).
- **Minimum cacheable length** (shorter prompts silently don't cache): 512 tokens on Fable/Mythos 5.x, Opus 5.5/5, Sonnet 5.5, Haiku 5.5; 1,024 on Opus 4.8, Sonnet 5/4.6/4.5; 2,048 on Opus 4.7; 4,096 on Opus 4.6/4.5 and Haiku 4.5 ([caching]).
- **Prices:**
  - 5m write = 1.25× input; 1h write = 2× input.
  - Read = 0.1× input, except 0.05× on Opus 5.5 and Sonnet 5.5, and 0.025× on Fable/Mythos 5.2 and 5.1.
  - These stack with the Batch discount and data-residency multipliers ([pricing]).
- **Mixed TTLs:** 1h entries must come before 5m entries ([caching]).
- **Invalidation (which cache layers break):**
  - Tool definitions → everything.
  - Web search toggle, citations toggle, speed → system and messages.
  - `tool_choice` or images → messages only.
  - Thinking config or top-level effort → messages always (system and tools for some models).
  - Dropped thinking blocks → messages from that point on ([caching]).
- **Server tools add breakpoints:** when a request already has a `cache_control`, the API adds a 5m breakpoint on each server-tool result. Those writes show up in `ephemeral_5m_input_tokens` even if every marker you set is 1h ([toolcache]).
- **Isolation:** per workspace on 1P, CPoAWS and Foundry; per org on Bedrock and Google-managed Vertex ([caching]).
- **Diagnostics:** `diagnostics:{previous_message_id}` → `diagnostics.cache_miss_reason` ([create]).

---

## 8. Errors, rate limits, retries

Error body: `{"type":"error","error":{"type":"…","message":"…"},"request_id":"req_…"}`. The response header `request-id` carries the same id. New `type` values may appear, so handle unknown ones ([errors]).

| HTTP | `error.type` | Notes |
|---|---|---|
| 400 | `invalid_request_error` | Also returned when a spend limit you set is hit |
| 401 | `authentication_error` | `CORS requests are not allowed…` when a browser `Origin` is sent |
| 402 | `billing_error` | |
| 403 | `permission_error` | |
| 404 | `not_found_error` | Threads: `error.details.error_code:"thread_not_found"` |
| 409 | `conflict_error` | |
| 413 | `request_too_large` | Returned by Cloudflare before the API |
| 429 | `rate_limit_error` | `retry-after` (seconds) when present. A tier spend-cap 429 has **no** `retry-after`. A sharp traffic ramp can trigger 429 "acceleration limits" |
| 500 | `api_error` (or `timeout_error`) | Retry with backoff |
| 504 | `timeout_error` | Use streaming for long requests |
| 529 | `overloaded_error` | Also sent mid-stream as an `error` event |

Sources: [errors], [threads], [ratelimits].
- **Rate-limit response headers:** `anthropic-ratelimit-{requests,tokens,input-tokens,output-tokens}-{limit,remaining,reset}` (reset is RFC 3339), `anthropic-priority-*` on Priority Tier, plus `anthropic-organization-id` and `anthropic-workspace-id` ([ratelimits], [apioverview]).
- **Retries:** the official SDKs retry transient failures (connection errors, rate limits, 5xx) twice by default with exponential backoff, honoring `retry-after`. Treat 529 and mid-stream `overloaded_error` as retryable, and 400 as not ([errors]).
- **Timeouts:** for requests over about 10 minutes or with large `max_tokens`, use streaming or Batches. Set TCP keep-alive in a raw client ([errors]).
- **HTTP 200 that still failed:** server-tool errors arrive as result blocks, refusals as `stop_reason`. Neither is an HTTP error ([websearch], [refusals]).

---

## 9. Async, resume, and re-attach

| Mechanism | What it gives | What it doesn't give | Src |
|---|---|---|---|
| Re-attach to an in-flight `/v1/messages` stream | — | **Doesn't exist.** No resume token or event replay, and there is no API to fetch a response by `msg_…` id. The docs don't say whether generation stops on disconnect (**UNVERIFIED**) | [streaming], [create] |
| Client-side continuation (official recipe) | Claude 4.5 and earlier: prefill the partial text as the assistant turn. Claude 4.6+ (no prefill): add a **user** message such as "Your previous response was interrupted and ended with [previous_response]. Continue from where you left off." | Partial `tool_use` or `thinking` blocks can't be recovered; resume from the last text block. The bot would be adding a message the user didn't write, so make it explicit and opt-in | [streaming] |
| Message Batches (`POST /v1/messages/batches`) | Durable async: poll `processing_status` and stream `results_url` (`.jsonl` keyed by `custom_id`). 50% price. Most batches finish under 1h, they expire at 24h, and results are kept **29 days**. Server tools run inside batches | No streaming. `stream`, `speed`, `fallbacks` and `max_tokens:0` are unsupported. Web search is throttled per org | [batch], [websearch] |
| Message Threads (beta `message-threads-2026-08-12`) | The server holds the conversation: `thread:{type:"create"}`, then `{type:"continue",previous_message_id}` sends only the new messages. Expires 5 min after the last request, deleted within 1h. After a dropped continue stream, resend the same request; `thread_already_continued` / `thread_continue_in_flight` → replay in full | **No API to list or read a thread.** Not ZDR-eligible. Can't be combined with compaction, most context editing, batches or count_tokens. The docs say the server may still be generating or may already have saved the turn when your connection drops, but you can't retrieve that output | [threads] |
| Bot-side recommendation | Persist every streamed delta locally. After a restart, show the partial answer and offer "retry" (re-send from the stored prefix) or "continue" (the opt-in continuation recipe). Use Batches only when the user asks for an async job | — | inference |

---

## 10. How OpenRouter exposes Claude

### 10.1 Surfaces, slugs, providers

- **Three request formats:** `/api/v1/chat/completions` (OpenAI), `/api/v1/responses` (OpenAI Responses), and **`/api/v1/messages`**, the Anthropic Messages "skin" used by Claude Code via `ANTHROPIC_BASE_URL=https://openrouter.ai/api` and `ANTHROPIC_AUTH_TOKEN` ([or-cc], [or-msgs]). Every Claude model lists all of them as `endpoints` ([or-models-api]).
- **Slugs** use dots: `anthropic/claude-opus-5.5`, `anthropic/claude-sonnet-5.5`, `anthropic/claude-haiku-5.5`, `anthropic/claude-fable-5.2`, `anthropic/claude-opus-4.8`. Canonical slugs carry a date (e.g. `anthropic/claude-opus-5.5-20260921`). There are `~anthropic/claude-*-latest` aliases and `:batch` variants ([or-models-api]).
- **Providers behind one slug** (from the live endpoints API):
  - `anthropic`, `anthropic/fast` (fast-mode tier at 2× price; Fable 5.2, Opus 5.5, Opus 5, Opus 4.8)
  - `claude-on-aws` (Haiku 5.5, Opus 5.5, Opus 4.8, Sonnet 5.5, Sonnet 4.6)
  - `amazon-bedrock[/region]`, `google-vertex[/global|us|europe]`, `azure[/global|us]`
  - Regional endpoints are priced 1.1× ([or-endpoints-api], [or-cc]).
- **Cancellation:** aborting the connection stops processing and billing on Anthropic and Azure, but **not** on AWS Bedrock or Google ([or-stream]).

### 10.2 Feature mapping (prefer `/api/v1/messages`)

| Anthropic feature | OpenRouter `/v1/messages` | OpenRouter `/v1/chat/completions` | Status and caveats |
|---|---|---|---|
| `thinking` adaptive / enabled / disabled / between_tools | ✔ all four in the schema | `reasoning:{enabled,effort,max_tokens,exclude}` | **Partial.** 5.5 models: `budget_tokens` / `reasoning.max_tokens` are "not forwarded" (the request is silently sent as adaptive). Disabling on Opus 5.5 or Sonnet 5.5 → OpenRouter's own 400 `Reasoning is mandatory for this endpoint and cannot be disabled.` On `~…-latest` aliases a disable request is coerced to the lowest effort ([or-c55]) |
| `thinking.display` | ✔ `summarized`/`omitted`/`updates` | `reasoning.exclude` | **Injected default:** OpenRouter sends `display:"summarized"` unless you set it. For Sonnet 5.5 with no setting it sends `{type:"adaptive",display:"summarized"}` ([or-reason], [or-c55]) |
| Thinking blocks and signatures | ✔ native `thinking`/`redacted_thinking` | `reasoning_details[]`: `reasoning.text{text,signature}`, `reasoning.encrypted{data}`, `reasoning.summary`, `format:"anthropic-claude-v1"`; replay `reasoning_details` on the assistant message | ✔. Chat Completions mapping detail **UNVERIFIED**. "For encrypted reasoning, the content may appear as `[REDACTED]` in streaming responses" ([or-reason]) |
| `block_binding` + `input_transformations` | ✔ schema, no beta header needed | ✗ | Requests through OpenRouter are "not subject to this enforcement" for Opus/Sonnet/Haiku 5.5 and Fable 5.1 (edits that would 400 directly succeed). Fable 5.2 isn't covered (**UNVERIFIED**) ([or-c55], [or-f51]) |
| Account-bound thinking (Sonnet/Haiku 5.5) | Handled by routing | same | Sonnet 5.5 replays stay within {Anthropic, Claude Platform on AWS, Azure}, never Bedrock or Vertex. Haiku 5.5 replays stay on the producing provider. If `provider.only`/`ignore` excludes those → 400. Fable 5.2 **UNVERIFIED** ([or-c55]) |
| `output_config.effort` | ✔ passthrough | `reasoning.effort` **or `verbosity`** → `output_config.effort` | `minimal`→`low`, `none` rejected. With no effort set, nothing is sent upstream (Opus 5.5 runs at Anthropic's `medium`). On models that take a budget, `reasoning.effort` becomes `budget_tokens = max(min(max_tokens × ratio, 128000), 1024)`, with ratio max/xhigh 0.95, high 0.8, medium 0.5, low 0.2, minimal 0.1 ([or-reason], [or-params], [or-c55]) |
| Per-message effort and mid-conversation system messages, `clear_at` | ✔ beta attached automatically | ✔ `configuration_update` (effort only) | ✔ ([or-f51], [or-reason]) |
| `tool_addition` / `tool_removal` (incl. `tool_definition`) | ✔ | ✗ "no representation" | ✔ Messages only ([or-opus5], [or-msgs]) |
| `output_config.format` / `task_budget` | ✔ | `response_format` json_schema | ✔ ([or-msgs]) |
| `temperature`/`top_p`/`top_k` | Accepted | Accepted | Omitted upstream when absent. Not listed in `supported_parameters` for 5.x models. With default routing, unsupported parameters are **silently ignored** unless `provider.require_parameters:true`. Which `anthropic-version` OpenRouter sends upstream is **UNVERIFIED** (it accepts temperature on Sonnet 4.6, which suggests `2023-06-01`) ([or-params], [or-provider], [or-models-api]) |
| `stop_sequences`, `system`, `metadata.user_id` | ✔ in schema | `stop`, system msgs, `user` | Whether `metadata.user_id` is forwarded: **UNVERIFIED** |
| `cache_control` (block + top-level, `ttl:"1h"`) | ✔ | ✔ on content parts | ✔. Top-level automatic caching works on Anthropic, Vertex, Azure, Bedrock and CPoAWS; on Bedrock it's translated to a trailing breakpoint. Sticky routing is per account+model+conversation, 60 min, `session_id` or `x-session-id`; it's disabled when `provider.order` is set ([or-cache]) |
| `speed:"fast"` | ✔ | ✔ (`speed` / `service_tier:"fast"\|"priority"`) | Routes to `anthropic/fast` but **falls back to standard endpoints** (billed standard) unless pinned. Response reports `service_tier:"priority"`, where Anthropic would say `standard` ([or-tiers], [or-cc]) |
| `service_tier` (`auto`/`standard_only`) | ✗ semantics replaced | ✗ | OpenRouter's `service_tier` means routing tiers (`default`, `flex`, `priority`/`fast`, `ultrafast`) ([or-tiers]) |
| `inference_geo` | ✗ not in schema | ✗ | OpenRouter has its own `eu.`/`us.openrouter.ai` in-region routing ([or-msgs], [or-servertools]) |
| `context_management` (clear_tool_uses, clear_thinking, `compact_20260112`) | ✔ schema (response `context_management.applied_edits`) | ✗ | Partial: on-demand `compaction` param is absent, `compaction` blocks are accepted as input ([or-msgs]) |
| `container` (code-exec reuse) | ✗ request field absent (the response schema has `container`) | ✗ | Unsupported ([or-msgs]) |
| `mcp_servers`, `thread`, `diagnostics`, `fallback_credit_token`, `inference_geo` | ✗ | ✗ | Unsupported ([or-msgs]) |
| Anthropic `fallbacks` | ✗ replaced | `models` | OpenRouter's `fallbacks` is OpenRouter multi-model routing ("if the primary model fails or refuses", ≤3 entries, `model` only), not Anthropic's server-side fallback ([or-msgs]) |
| Native `web_search` | ✔ `web_search_20250305` / `_20260209` in the tool schema; or `{"type":"openrouter:web_search","parameters":{"engine":"native",…}}` → `web_search_20260209` | `openrouter:web_search` (results surface as `url_citation` annotations) | **Partial.** `native_tools` exist only on the `anthropic` and `claude-on-aws` endpoints. `engine:"auto"` (the default) uses native only "if the provider supports it, otherwise falls back to Exa"; `native` also falls back to Exa "if the model doesn't support native search" (how that's decided per endpoint is **UNVERIFIED**). `_20260318` isn't in the schema. `max_uses` is forwarded to Anthropic ([or-websearch], [or-servertools], [or-endpoints-api]) |
| Native `web_fetch` | Only through `openrouter:web_fetch` engine `native` → `web_fetch_20260209`. No `web_fetch_*` type in the tool schema, and **`web_fetch_tool_result` isn't an accepted input block** | ✗ | **Partial.** Falls back to Exa or OpenRouter's own fetcher. `citations`, `use_cache` and `url_sources` aren't exposed ([or-webfetch], [or-msgs]) |
| Native `code_execution` | ✗ no `code_execution_*` tool type, no `native_tools` mapping. Code-execution result blocks are accepted **in responses only**, not as input | ✗ | **Unsupported.** `openrouter:bash` / `openrouter:shell` run in OpenRouter sandboxes, not Anthropic's tool. This also affects multi-turn replay of dynamic-filtering results (**UNVERIFIED** whether OpenRouter strips or rejects them) ([or-servertools], [or-containers], [or-msgs]) |
| Advisor, tool search | ✔ `advisor_20260301`, `tool_search_tool_*` | — | ✔ |
| Computer use | ✗ rejected at validation | ✗ | Unsupported ([or-c55]) |
| Document citations | ✔ schema (`citations` config on documents and `search_result`; all citation location types in responses) | ✗ no representation (**UNVERIFIED**) | Untested |
| Images | ✔ base64 / url / file | `image_url` | How OpenRouter fetches a URL for providers that need base64 (Bedrock, Vertex): **UNVERIFIED** |
| PDFs | ✔ `document` blocks | `file` part. The `file-parser` plugin defaults to the `native` engine for Claude, otherwise `mistral-ocr` ($2/1K pages) or `cloudflare-ai` | ✔. Don't enable plugins, so the PDF isn't transformed ([or-pdf]) |
| Anthropic Files API `file_id` | `GET/POST /api/v1/files?provider=anthropic` proxies to Anthropic **only with a BYOK key**. OpenRouter's own `or_file_…` files feed OpenRouter sandboxes | — | Partial ([or-files], [or-byok]) |
| Beta headers | Attached automatically when the feature is detected; routed only to supporting providers | `x-anthropic-beta` passthrough is documented only for `interleaved-thinking-2025-05-14` and `structured-outputs-2025-11-13` | Arbitrary betas: **UNVERIFIED** ([or-f51], [or-provider]) |
| 1M context | Listed as 1,000,000 for 4.6+ | same | Sonnet 4.5 is still listed at 1M, although Anthropic retired that beta (now 200K) ([or-models-api], [relnotes]) |
| Streaming | Anthropic SSE events, including `signature_delta`, `citations_delta`, `compaction_delta`, `ping`, `error`. Since 2026-10-21 no trailing `[DONE]` | OpenAI chunks, `: OPENROUTER PROCESSING` comments, final usage chunk | ✔ ([or-msgs], [or-changelog], [or-stream]) |
| Usage | Anthropic `usage` plus `cost`, `cost_details`, `is_byok`, `iterations`, `server_tool_use.{web_search_requests,web_fetch_requests,tool_calls_requested,tool_calls_executed}`. The response has `provider`; `openrouter_metadata` is returned with the `X-OpenRouter-Metadata: enabled` header | `prompt_tokens_details.{cached_tokens,cache_write_tokens}` | ✔ ([or-msgs], [or-cache]) |
| Errors | `{"type":"error","error":{"type":"overloaded_error","message":"…","error_type":"provider_overloaded"},"request_id":"gen-…"\|null}` | `{"error":{"code":429,"message":"…","metadata":{"error_type":"…","provider_code":"…"}}}` | **Normalized and lossy.** 500s get a generic message. The raw body is available via `GET /api/v1/generation/content?id=gen-…` → `error.raw` "when stored" ([or-errors], [or-gen-content]) |
| Batches | `POST /api/v1/batches` with `endpoint:"/v1/messages"` and a `:batch` model; results inline; deleted after 30 days | — | ✔ (`custom_id`; usually within 24h, Google Vertex up to 96h) ([or-batch]) |
| count_tokens | ✗ none documented | — | Unsupported |

Schema facts above come from the OpenAPI spec embedded in [or-msgs]. "Accepted in schema" means "documented", not tested live.

### 10.3 OpenRouter request settings that minimize rewriting (recommended)

```json
POST https://openrouter.ai/api/v1/messages
Authorization: Bearer $OPENROUTER_API_KEY
X-OpenRouter-Metadata: enabled
{
  "model": "anthropic/claude-opus-5.5",
  "max_tokens": 64000, "stream": true,
  "thinking": {"type": "adaptive", "display": "omitted"},
  "output_config": {"effort": "medium"},
  "provider": {"only": ["anthropic"], "allow_fallbacks": false, "require_parameters": true},
  "session_id": "tg-<chat>-<branch>",
  "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 5}],
  "messages": [ ...verbatim replay... ]
}
```
- Set `thinking.display` explicitly, so OpenRouter's injected `summarized` default doesn't apply ([or-reason]).
- Pinning `anthropic` keeps account-bound thinking, the prompt cache, native tools and cancellation-on-abort intact ([or-c55], [or-cache], [or-stream]).
- `require_parameters:true` stops silent parameter drops ([or-provider]).
- `x-api-key` auth on `/v1/messages` and Anthropic-native model ids (`claude-opus-5-5`): **UNVERIFIED**. Use Bearer and OpenRouter slugs.

---

## 11. OpenRouter gaps for Anthropic

- **Unrequested parameters.**
  - OpenRouter defaults `thinking.display` to `"summarized"` and, for Sonnet 5.5, sends `{type:"adaptive",display:"summarized"}` when no reasoning setting is given ([or-reason], [or-c55]).
  - OpenRouter's own Chat Completions debug example shows `"max_tokens": 65536` in the upstream body of a request that didn't set it. Whether OpenRouter injects a default `max_tokens` (it's optional in the `/messages` schema) is **UNVERIFIED** ([or-errors], [or-msgs]).
- **Silent rewrites instead of upstream errors.**
  - On 5.5 models, `budget_tokens` / `reasoning.max_tokens` are dropped, so adaptive thinking runs where Anthropic would return a 400 ([or-c55]).
  - Disabling reasoning returns OpenRouter's own 400 text, not Anthropic's ([or-c55]).
  - `~anthropic/*-latest` aliases coerce a disable request to the lowest effort ([or-c55]).
  - `minimal` is mapped to `low` ([or-reason]).
  - Unsupported parameters are ignored with default routing ([or-provider]).
- **The prefix check isn't enforced** for Opus/Sonnet/Haiku 5.5 and Fable 5.1 through OpenRouter. The same history that works there can 400 when sent directly, so the two backends differ for the same history ([or-c55], [or-f51], [preserved]).
- **Provider routing can break state.**
  - Account-bound thinking (Sonnet/Haiku 5.5, Fable 5.2) is lost on a provider switch. OpenRouter mitigates this for Sonnet and Haiku; Fable 5.2 is **UNVERIFIED** ([or-c55], [preserved]).
  - Caches are per provider and workspace; sticky routing is best-effort, lasts 60 min, and turns off when `provider.order` is set ([or-cache]).
  - Fast mode silently falls back to standard endpoints ([or-tiers]).
- **Official server tools only partially reachable.**
  - `openrouter:web_search` and `openrouter:web_fetch` default to `engine:"auto"`, which uses non-Anthropic engines (Exa, OpenRouter's fetcher) when the serving provider has no native tool (for Claude, Bedrock, Vertex and Azure endpoints list no native web tools). `native` also falls back to Exa or OpenRouter's fetcher ([or-websearch], [or-webfetch], [or-endpoints-api]).
  - Native code execution, `container` reuse, MCP connector, `web_search_20260318`, `web_fetch_*` tool types and computer use aren't available ([or-msgs], [or-servertools], [or-c55]).
  - Web-fetch and code-execution result blocks can't be sent back (input schema asymmetry) ([or-msgs]).
- **Anthropic request fields missing:** `inference_geo`, `container`, `mcp_servers`, `thread`, `diagnostics`, `compaction` (on-demand), `fallback_credit_token`. `service_tier` and `fallbacks` keep their names but mean something else ([or-msgs], [or-tiers]).
- **Beta passthrough is curated.** The docs list only 2 `x-anthropic-beta` values; other betas depend on OpenRouter auto-detecting the feature ([or-provider], [or-f51]).
- **Errors aren't verbatim.** They're normalized to OpenRouter's `error_type`, Anthropic `error.type` is lossy, 500 messages are masked, and `request_id` is a `gen-…` id or null, not Anthropic's `req_…` ([or-errors]).
- **The Chat Completions path loses fidelity:** no tool-change blocks, no `block_binding`, citations and server-tool blocks get flattened, and `reasoning_details` can show `[REDACTED]` while streaming ([or-opus5], [or-reason]). Use `/v1/messages`.
- **No resume path.**
  - Aborting the connection cancels generation on Anthropic and Azure. On Bedrock and Google it keeps running and keeps billing, with no way to re-attach ([or-stream]).
  - `GET /api/v1/generation/content` (needs Input & Output Logging, which retains data for at least 3 months) returns only flattened `completion` and `reasoning` strings, without signatures or blocks ([or-gen-content], [or-iolog]).
  - Batches are the only durable async option ([or-batch]).
- **Metadata mismatches.**
  - `/models` reports `reasoning.default_effort:"high"` for Opus 5.5, while Anthropic's default is `medium` and OpenRouter's own guide says none is sent ([or-models-api], [or-c55], [effort]).
  - Sonnet 4.6 is reported with `default_effort:"medium"` versus Anthropic's `high` ([or-models-api], [effort]).
  - Sonnet 4.5 is listed at 1M context although that beta is retired ([relnotes]).
- **Other gaps:**
  - No `count_tokens`.
  - Anthropic Files API only through BYOK proxy ([or-files]).
  - Fast-tier responses report `service_tier:"priority"` where Anthropic would say `standard` ([or-tiers]).

---

### References

[create]: https://platform.claude.com/docs/en/api/messages/create
[beta-create]: https://platform.claude.com/docs/en/api/beta/messages/create
[versioning]: https://platform.claude.com/docs/en/api/versioning
[mig1021]: https://platform.claude.com/docs/en/api/migration-2026-10-21
[models]: https://platform.claude.com/docs/en/models/overview
[models-api]: https://platform.claude.com/docs/en/api/models/retrieve
[opus55]: https://platform.claude.com/docs/en/models/opus-5-5/overview
[opus55-mig]: https://platform.claude.com/docs/en/models/opus-5-5/migration-guide
[thinking]: https://platform.claude.com/docs/en/build-with-claude/thinking
[ext-thinking]: https://platform.claude.com/docs/en/build-with-claude/extended-thinking
[preserved]: https://platform.claude.com/docs/en/build-with-claude/preserved-thinking
[effort]: https://platform.claude.com/docs/en/build-with-claude/effort
[midsys]: https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages
[ctxwin]: https://platform.claude.com/docs/en/build-with-claude/context-windows
[ctxedit]: https://platform.claude.com/docs/en/build-with-claude/context-editing
[compaction]: https://platform.claude.com/docs/en/build-with-claude/compaction
[compaction-threshold]: https://platform.claude.com/docs/en/build-with-claude/compaction-threshold
[so]: https://platform.claude.com/docs/en/build-with-claude/structured-outputs
[betas]: https://platform.claude.com/docs/en/api/beta-headers
[fallback-credit]: https://platform.claude.com/docs/en/build-with-claude/fallback-credit
[tool-ref]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-reference
[server-tools]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/server-tools
[websearch]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool
[webfetch]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-fetch-tool
[codeexec]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/code-execution-tool
[advisor]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool
[toolcache]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-use-with-prompt-caching
[mcp]: https://platform.claude.com/docs/en/agents-and-tools/mcp-connector
[citations]: https://platform.claude.com/docs/en/build-with-claude/citations
[searchres]: https://platform.claude.com/docs/en/build-with-claude/search-results
[vision]: https://platform.claude.com/docs/en/build-with-claude/vision
[pdf]: https://platform.claude.com/docs/en/build-with-claude/pdf-support
[files]: https://platform.claude.com/docs/en/build-with-claude/files
[streaming]: https://platform.claude.com/docs/en/build-with-claude/streaming
[stop]: https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons
[refusals]: https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback
[stream-refusals]: https://platform.claude.com/docs/en/test-and-evaluate/strengthen-guardrails/handle-streaming-refusals
[caching]: https://platform.claude.com/docs/en/build-with-claude/prompt-caching
[pricing]: https://platform.claude.com/docs/en/about-claude/pricing
[errors]: https://platform.claude.com/docs/en/api/errors
[apioverview]: https://platform.claude.com/docs/en/api/overview
[ratelimits]: https://platform.claude.com/docs/en/api/rate-limits
[threads]: https://platform.claude.com/docs/en/build-with-claude/message-threads
[batch]: https://platform.claude.com/docs/en/build-with-claude/batch-processing
[residency]: https://platform.claude.com/docs/en/manage-claude/data-residency
[tiers]: https://platform.claude.com/docs/en/api/service-tiers
[relnotes]: https://platform.claude.com/docs/en/release-notes/overview
[watermark]: https://platform.claude.com/docs/en/build-with-claude/text-watermark
[or-msgs]: https://openrouter.ai/docs/api/api-reference/anthropic-messages/create-a-message
[or-c55]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/claude-5-5
[or-f51]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/fable-5-1
[or-opus5]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/opus-5
[or-reason]: https://openrouter.ai/docs/guides/best-practices/reasoning-tokens
[or-cache]: https://openrouter.ai/docs/guides/best-practices/prompt-caching
[or-servertools]: https://openrouter.ai/docs/guides/features/server-tools
[or-websearch]: https://openrouter.ai/docs/guides/features/server-tools/web-search
[or-webfetch]: https://openrouter.ai/docs/guides/features/server-tools/web-fetch
[or-containers]: https://openrouter.ai/docs/guides/features/containers
[or-files]: https://openrouter.ai/docs/guides/features/files-api
[or-pdf]: https://openrouter.ai/docs/guides/overview/multimodal/pdfs
[or-params]: https://openrouter.ai/docs/api_reference/parameters
[or-provider]: https://openrouter.ai/docs/guides/routing/provider-selection
[or-errors]: https://openrouter.ai/docs/api_reference/errors-and-debugging
[or-stream]: https://openrouter.ai/docs/api_reference/streaming
[or-tiers]: https://openrouter.ai/docs/guides/features/service-tiers
[or-cc]: https://openrouter.ai/docs/cookbook/coding-agents/claude-code-integration
[or-batch]: https://openrouter.ai/docs/guides/features/batch-api
[or-byok]: https://openrouter.ai/docs/guides/overview/auth/byok
[or-changelog]: https://openrouter.ai/docs/changelog
[or-gen-content]: https://openrouter.ai/docs/api/api-reference/generations/get-stored-prompt-completion-and-error-content-for-a-generation
[or-iolog]: https://openrouter.ai/docs/guides/features/input-output-logging
[or-models-api]: https://openrouter.ai/api/v1/models
[or-endpoints-api]: https://openrouter.ai/api/v1/models/anthropic/claude-opus-5.5/endpoints
