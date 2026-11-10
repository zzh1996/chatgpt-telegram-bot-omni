# OpenRouter API — research notes for the Rust Telegram bot

Researched 2026-11-11. Sources: the live docs at openrouter.ai/docs (full dump from `https://openrouter.ai/docs/llms-full.txt`), the OpenAPI spec ([oas], `https://openrouter.ai/openapi.json`), and the public catalog endpoints (`/api/v1/models`, `/api/v1/models/{id}/endpoints`, `/api/v1/providers`), all fetched the same day. The model names below (GPT-6.x, Claude 5.5, Gemini 4, Grok 4.7, DeepSeek V4, GLM-5.3, Kimi K3, Qwen 3.7/3.8) are the ones those sources list today.

Conventions: CC = Chat Completions (`/api/v1/chat/completions`); RSP = Responses (`/api/v1/responses`); MSG = Anthropic Messages (`/api/v1/messages`); OR = OpenRouter. Citations are bracketed tags; the URLs are listed at the end. **UNVERIFIED** means no OR doc confirms the point; it is inference, memory, or third-party evidence.

---

## 0. TL;DR — decisions for the bot

* **The default request is not "clean".** OR may do all of the following unless you opt out:
  * choose a provider per request at random, weighted by its scoring, and fall back to another provider ([route]);
  * reorder providers for any request that has tools ("Auto Exacto") ([exacto]);
  * compress context on endpoints with ≤8k context ([mt]);
  * parse PDFs with paid Mistral OCR when the model has no native file support ([pdf]);
  * prepend `name:` to message content for non-OpenAI models ([ovw]);
  * inject a default `max_tokens` (65 536 seen for Claude and Gemini 4) ([err], [g4]);
  * set `thinking.display:"summarized"` and `eager_input_streaming` on Claude ([reason], [route]);
  * silently drop unsupported params, or map an effort level to the nearest one the model supports ([ovw], [latest]);
  * apply account/workspace default plugins, provider settings, service tier and guardrails ([plugins], [tiers]).

  §11.2 lists each item with its opt-out.
* **Pin per request:** `provider: {only:[<slug>], allow_fallbacks:false, require_parameters:true}`, `plugins:[{"id":"context-compression","enabled":false}]`, and an explicit `max_tokens`/`max_output_tokens`. Also send the header `X-OpenRouter-Metadata: enabled`; it returns the router pipeline (compression, guardrails, plugins, attempts) on the response ([rmeta]). For audits, `debug.echo_upstream_body:true` (stream only, CC and RSP) echoes the exact upstream body ([err]).
* **Which endpoint:** use CC as the universal path. It has the most knobs, normalized `reasoning_details`, audio/image output, and PDF annotation reuse. Add an RSP path for OpenAI GPT-5.4+/GPT-6 (`phase`, async tools, `configuration_update`, pro mode). MSG is optional, for Claude-only knobs such as `thinking.type:"between_tools"` and `display:"updates"`. All three skins are stateless (§1).
* **Reasoning round-trip:** echo `reasoning_details` back verbatim on each assistant message (CC). On RSP, echo the output items (`reasoning` items with `encrypted_content`/`signature`) back verbatim. This is mandatory for Claude and Gemini when tools are involved (Gemini otherwise returns 400 "missing a thought_signature") ([reason], [zed]). Never reorder or modify the blocks ([reason]).
* **Streams cannot be resumed.**
  * There is no background mode, no `GET /responses/{id}`, and no `starting_after`; `store:false` is forced ([rsp]).
  * A client disconnect cancels the upstream generation on supported providers (OpenAI, Anthropic, xAI, DeepSeek, …). It does not cancel on Google, Bedrock, Alibaba, Groq, Mistral, … ([stream]).
  * Afterwards you can still get cost and metadata via `GET /api/v1/generation?id=gen-…`, and the stored text via `/generation/content` only if Input & Output Logging is enabled ([logs]).
  * Gemini 4 "long decode" continuation tokens are the only resumable mechanism ([g4]). The Batch API (no streaming, ≤24h) is the only server-side async path ([batch]).
* **Errors after HTTP 200 are common.** Once the upstream sends headers, every failure arrives in-band: a CC chunk with `error` plus `finish_reason:"error"`, or RSP `response.failed`. A 200 whose body holds only `error` must be treated as a failure ([err]). Switch on `error_type`, not the HTTP status (§7).
* **Supporting new models without code changes** works through `/api/v1/models`:
  * `supported_parameters`, `architecture.input_modalities/output_modalities`;
  * `reasoning.{supported_efforts,default_effort,mandatory,supports_max_tokens}`;
  * `top_provider.max_completion_tokens`, `pricing`;
  * and per endpoint `/api/v1/models/{author}/{slug}/endpoints`: `supported_parameters`, `quantization`, `native_tools`, `supports_tool_choice`, `service_tier` (§8).
* **Open-weight models default to third-party quantized hosts.** On 2026-11-11, `moonshotai/kimi-k3` had 28 endpoints, mostly fp4/mxfp4, with the first-party `moonshotai` endpoint listed 24th. `deepseek/deepseek-v4-pro` had no `deepseek` endpoint at all. `z-ai/glm-5.3` had 47 endpoints ([api-ep]). To test the official model, pin the first-party slug: `openai`, `anthropic`, `google-ai-studio`, `google-vertex`, `xai`, `deepseek`, `z-ai`, `moonshotai`, `alibaba` ([api-prov]).

---

## 1. Endpoints

Base URL: `https://openrouter.ai/api/v1`. The regional hosts `https://eu.openrouter.ai` and `https://us.openrouter.ai` are for Business/Enterprise in-region routing; I/O logging, the Files API and most server tools are unavailable there ([route], [iolog], [files], [srv]). Authentication is `Authorization: Bearer <key>` ([auth]).

| | CC `POST /chat/completions` | RSP `POST /responses` | MSG `POST /messages` |
|---|---|---|---|
| Wire shape | OpenAI Chat Completions + OR extensions ([ovw]) | OpenAI Responses ("OpenResponses" in [oas]) | Anthropic Messages ([msgs-api]) |
| Status | stable | **GA since 2026-07-25**; was `beta.responses`, and the URL is unchanged ([changelog]) | stable |
| State | stateless | stateless: `store` is the constant `false`; `store:true` or a non-null `previous_response_id` gets a **400** ([rsp]). `background` is in the schema with no description and is not usable (§9) | stateless |
| Models | all text models | all models; OR translates to each provider | all models; OR maps `thinking`/`output_config` onto non-Claude models ([reason]) |
| Usage in a stream | an extra last chunk with `usage` and a non-empty `choices` | in `response.completed` / `response.incomplete` | in `message_delta`, before `message_stop` ([stream]) |
| Mid-stream error | a chunk with top-level `error` and `finish_reason:"error"` | `response.failed` (an `error` event is also possible) | SSE `error` event ([err]) |
| Where `error_type` sits | `error.metadata.error_type` | top-level `error_type` on the response | `error.error_type` ([err]) |
| `debug.echo_upstream_body` | yes (stream only) | yes (`response.debug` event) | not in schema / not documented ([err], [oas]) |
| Reasoning output | `message.reasoning` (string) + `message.reasoning_details[]` | `output[]` items `type:"reasoning"` | `thinking` / `redacted_thinking` blocks |
| `reasoning.exclude` | honored | **ignored** (reasoning items always returned) ([reason]) | n/a (`thinking.display:"omitted"`) |
| Only on this skin | audio output (`modalities`+`audio`); `input_audio.format` beyond wav/mp3; PDF `file` annotations reusable to skip re-parsing ([pdf]); `seed`, `logit_bias`, `logprobs`, `min_p`, `top_a`, `repetition_penalty`, `prediction`, `include_reasoning`, `reasoning_effort` ([oas]) | `phase` on assistant items (GPT-5.3-codex…GPT-6) ([g54]); `async:true` tools (GPT-6) ([g6]); `instructions`, `truncation`, `include`, `safety_identifier`; `openrouter:apply_patch` (RSP only); `openrouter:tool_search` and `openrouter:shell` (RSP and MSG) ([srv]) | `thinking.type:"between_tools"` (Sonnet 5.5), `thinking.display:"updates"`, mid-conversation `tool_addition`/`tool_removal`, `fallbacks` (≤3), `speed`, `context_management`, `safeguards`, `openrouter:bash` ([c55], [f51], [fallback]) |
| Missing on this skin | `phase` (cannot be represented); `async` flag stripped; `openrouter:tool_search` → 400 ([g6], [tool-search]) | `audio` output; `input_audio.format` limited to `wav`/`mp3`; `modalities` limited to `text`/`image` ([audio], [oas]) | video input ([video]); computer-use tools rejected ([c55]) |

Also present: legacy `/api/v1/completions` (text completion; router metadata supports it) ([rmeta]); `/api/v1/images` (§4.6); `/api/v1/audio/speech` and `/api/v1/audio/transcriptions` ([mm]); `/api/v1/videos` (async jobs); `/api/v1/batches` (§9); `/api/v1/embeddings`.

**Which skin for a multi-provider bot.** Use **CC as the canonical path**. It has the widest parameter surface, `reasoning_details` that OR normalizes across all families ("works identically across all supported reasoning models", [reason]), audio and image output, and it is what OR's PDF and annotation features are built around. Add **RSP for OpenAI GPT-5.4+/GPT-6**, because without `phase` there is "significant performance degradation … including early stopping" ([g54]), and async tools exist only on RSP ([g6]). MSG is only needed for Claude-exclusive controls; note that it has no `debug` echo. The plan to move to direct official APIs later fits this split: CC is the lingua franca for Gemini (OpenAI-compatible), xAI, DeepSeek, GLM, Kimi and Qwen, while OpenAI uses RSP and Anthropic uses MSG.

---

## 2. Request parameters

### 2.1 Core and OpenRouter-specific fields (CC; RSP/MSG differences noted)

| Field | Values / type | Semantics and notes |
|---|---|---|
| `model` | `author/slug[:variant…]`, `~author/family-latest`, `@preset/x`, `openrouter/auto` | Omitting it uses the account default ([ovw]). Variants are covered in §2.4, `~…-latest` in [latest]. The response `model` reports the concrete model that served the request ([latest]). |
| `messages` (CC) / `input`+`instructions` (RSP) | | Roles `system`, `developer`, `user`, `assistant`, `tool` ([oas]). On RSP, every replayed assistant message must carry `id` and `status` ([rsp-basic]). A trailing assistant message is a **prefill** (supported, but rejected by Claude Opus 5 and all 5.5 models) ([ovw], [c55]). |
| `name` on a message | string | **Prepended as `{name}: {content}` for non-OpenAI models** ([ovw]). Don't send it if you want clean prompts. |
| `stream` | bool | SSE (§6). |
| `max_tokens` (CC, marked deprecated) / `max_completion_tokens` (CC) / `max_output_tokens` (RSP) / `max_tokens` (MSG) | int ≥1 | Covers reasoning **and** visible output on most providers ([params]). "Some providers enforce a minimum of 16" ([oas]). Routing only picks providers that can produce that many tokens ([route]). If omitted, OR may inject its own value: `max_tokens: 65536` appears in the debug echo for Claude ([err]), and Gemini 4 gets 65 536 ([g4]). The in-flight credit hold also uses `max_tokens`, or a fixed cap when it is unset ([lim]). |
| `temperature`, `top_p`, `top_k`, `frequency_penalty`, `presence_penalty`, `repetition_penalty`, `min_p`, `top_a`, `seed`, `logit_bias`, `logprobs`, `top_logprobs` (0–20), `stop` (≤4) | | **An absent sampling param is omitted upstream; OR does not inject a default.** An explicitly sent value is forwarded even if it equals the default. Unsupported params are **silently ignored** unless `provider.require_parameters:true` ([params], [ovw], [route]). Mistral `safe_prompt` and Hyperbolic `raw_mode` pass through ([params]). RSP has only `temperature`, `top_p`, `top_k`, `frequency_penalty`, `presence_penalty`, `top_logprobs`, `stop` ([oas]). |
| `reasoning` | object | See §3. |
| `reasoning_effort` (CC) | `max\|xhigh\|high\|medium\|low\|minimal\|none` | Shorthand for `reasoning.effort`; a request that sends both with different values is rejected ([oas]). |
| `include_reasoning` (CC, legacy) | bool | `true` = `reasoning:{}`; `false` = `reasoning:{exclude:true}`. Not accepted on RSP ([reason]). |
| `verbosity` (CC) / `text.verbosity` (RSP) | CC: `low\|medium\|high\|xhigh\|max` (default `medium`); RSP schema: `low\|medium\|high` | On Anthropic models it **maps to `output_config.effort`** and overrides `reasoning.effort` ([params], [c46]). `xhigh` needs Claude Opus 4.7+, `max` needs Opus 4.6+ ([params]). |
| `response_format` (CC) / `text.format` (RSP) | `text`, `json_object`, `json_schema{name,schema,strict,description}`, `grammar{grammar}`, `python` ([oas]) | Acts as a soft routing preference (§2.5). For Claude, the structured-outputs beta header is added automatically ([route]). |
| `tools`, `tool_choice`, `parallel_tool_calls` | `tool_choice`: `none\|auto\|required\|{type:"function",function:{name}}\|{type:"openrouter:web_search"}`; `parallel_tool_calls` defaults to `true` | `tools` must be resent on every turn ([tools]). Function names: `[a-zA-Z0-9_-]`, ≤64 chars. Each tool may carry `cache_control` and `defer_loading` ([oas]). For providers without native tool calling, OR "transform[s] the tools into a YAML template" (**hidden prompt**) ([ovw]). `strict:true` is **stripped for Claude** unless you send the header `x-anthropic-beta: structured-outputs-2025-11-13` ([route]). Per-endpoint `supports_tool_choice.{auto,none,required,function}` flags exist ([api-ep]). |
| `modalities` | CC: `["text","image","audio"]`; RSP: `text`, `image` | Output modalities (§4.6, §4.4). |
| `image_config` | free-form object, e.g. `{"aspect_ratio":"16:9","quality":"high"}` | Provider-specific image-output options ([oas]); keys vary per model (**UNVERIFIED** list). |
| `audio` (CC only) | `{voice, format}` | Audio output; requires `stream:true` ([audio]). |
| `prediction` (CC) | `{type:"content",content}` | OpenAI predicted outputs ([ovw]). |
| `plugins` | array | §2.3. |
| `web_search_options` (CC) | `{search_context_size:"low\|medium\|high"}` | Native-search context size ([ws-plugin], [params]). Not in the CC OpenAPI schema, but listed in the `Parameter` enum ([oas]). |
| `provider` | object | §2.5. |
| `models` | string[] | Model fallback list (§2.6). |
| `route` | `fallback\|sort` | **Deprecated** alias of `provider.sort.partition` (`fallback`→`model`, `sort`→`none`) ([oas]). |
| `service_tier` | `auto\|default\|fast\|flex\|priority\|scale\|ultrafast` | `fast` = `priority`. `ultrafast` is opt-in only (OpenAI) ([tiers]). See §11 for routing interplay. |
| `user` / `safety_identifier` (RSP) | string | Per-end-user abuse isolation. OR **folds it into a hashed identity sent upstream and never forwards it raw**. If omitted, an account-level identity is sent instead, so one user's policy block can affect the whole account ([usertrack], [oas]). |
| `session_id` (or header `x-session-id`) | ≤256 chars | Sticky-routing key and Logs grouping. The body value wins over the header ([cache]). |
| `prompt_cache_key` | string | OpenAI field; also used as the sticky key when there is no `session_id` ([cache]). |
| `cache_control` (top level) | `{type:"ephemeral", ttl?:"5m"\|"1h"}` | Anthropic automatic caching. Also works on RSP ([cache]). §10. |
| `prompt_cache_options` | `{mode:"explicit", ttl?:"30m"\|"12h"}` | OpenAI GPT-5.6+ only ([cache]). |
| `metadata` | ≤16 pairs, keys ≤64 chars, values ≤512 chars | Whether it is forwarded upstream is **UNVERIFIED** ([oas]). |
| `trace` | `{trace_id, trace_name, span_name, generation_name, parent_span_id, …}` | Goes to Broadcast destinations only ([oas]). |
| `debug` | `{echo_upstream_body:true}` | Stream only. A first chunk (CC, empty `choices`) or `response.debug` event (RSP) carries the transformed upstream body; one is emitted per provider attempt. Redaction is "best effort" ([err]). |
| `stream_options.include_usage`, `usage:{include:true}` | | **Deprecated, no effect**: full usage is always returned ([usage], [oas]). |
| `max_tool_calls`, `stop_server_tools_when` | default 30, max 30 | Server-tool loop budget ([srv]). |
| `configuration_update` on a content-less `system`/`developer` message (CC) / `{type:"configuration_update"}` item (RSP) | `{reasoning:{effort}}` | Mid-conversation effort change that keeps the cache ([reason]). §3.4. |
| `deferred_tools`, `access_programs` (RSP), `prompt` (RSP stored prompt), `truncation` (RSP) | | Niche; `prompt` (an OpenAI stored prompt) is **UNVERIFIED** behind a stateless proxy ([oas]). |

`n` (multiple choices) is **not in the CC schema**; assume it is unsupported (**UNVERIFIED**) ([oas]).

### 2.2 Request headers

| Header | Purpose |
|---|---|
| `HTTP-Referer`, `X-OpenRouter-Title` (`X-Title` also accepted), `X-OpenRouter-Categories`, `X-OpenRouter-App-Visibility: hidden` | App attribution / rankings ([attr]) |
| `X-OpenRouter-Metadata: enabled` (legacy `X-OpenRouter-Experimental-Metadata`) | Adds `openrouter_metadata` (requested, strategy, endpoints, attempts, `pipeline[]`) to the response; when streaming it rides on the last chunk ([rmeta], [err]) |
| `x-session-id` | Same as body `session_id` ([cache]) |
| `X-OpenRouter-Cache`, `-Cache-TTL`, `-Cache-Clear` | OR response cache, **off by default** ([respcache]) |
| `x-anthropic-beta` | Pass-through allowlist: `interleaved-thinking-2025-05-14`, `structured-outputs-2025-11-13` ([route]) |

Response headers: `X-Generation-Id` on every endpoint, including streams ([stream]). `Retry-After` on 429, on 503, and on a 402 with `limit_source:openrouter_in_flight_budget` ([err]). `X-RateLimit-Limit/-Remaining/-Reset` appear only on OR-originated 429s for free-model and key-management limits; successful responses carry none ([lim]). `X-OpenRouter-Cache-Status/-Age/-TTL/-Source-Id` ([respcache]).

### 2.3 Plugins (`plugins:[{id, enabled?, …}]`)

Plugins "always run once when enabled". An **account default** set on the Settings > Plugins page applies to every request unless the request sends `{"id":…, "enabled":false}`. With **"Prevent overrides"** on, a request cannot disable it ([plugins]).

| `id` | Options | Default / notes |
|---|---|---|
| `web` (deprecated) | `engine` (`native\|exa\|firecrawl\|parallel\|perplexity`), `mode`, `max_results` (default 5), `max_uses`, `search_prompt`, `include_domains`, `exclude_domains`, `user_location`, `x_search` | Always searches once. **Injects results with a default prompt**: "A web search was conducted on `date`. Incorporate the following web search results into your response. IMPORTANT: Cite them using markdown links…" ([ws-plugin]) |
| `file-parser` | `pdf.engine`: `native\|mistral-ocr\|cloudflare-ai` (`pdf-text` is deprecated and redirects to `cloudflare-ai`) | Applies automatically to `file` parts; §4.3 ([pdf]) |
| `context-compression` | `engine:"middle-out"` (the only value) | **Default ON for every endpoint with ≤8 192 context.** Disable with `{"id":"context-compression","enabled":false}`. Skipped for image-only-output models ([mt]) |
| `response-healing` | — | Non-streaming only, requires `json_object`/`json_schema`; repairs JSON ([heal]) |
| `web-fetch` | `allowed_domains`, `blocked_domains`, `max_uses`, `max_content_tokens` | ([oas]) |
| `moderation` | — | Exists in the schema; semantics **UNVERIFIED** ([oas]) |
| `alignment` (beta), `armor` (beta), `auto-router`, `auto-beta-router`, `pareto-router`, `switchyard-router`, `jev-router`, `fusion` | | Routing and safety layers; avoid them in a transparent bot ([plugins], [oas]) |

`transforms` (the legacy `["middle-out"]`) is **gone** from both the current docs and the OpenAPI spec; `context-compression` replaced it ([mt], [oas]). Whether the old `transforms: []` still disables compression is **UNVERIFIED**.

### 2.4 Model suffixes (variants)

Catalog variants have their own `/models` entries; routing variants change ordering only ([variants]).

| Suffix | Kind | Effect |
|---|---|---|
| `:free` | catalog | Free endpoints with their own limits (20 rpm; 50/day, or 1000/day after ≥10 credits purchased) ([lim]). In a `models` list, a fallback to a paid model is billed ([fallback]). |
| `:batch` | catalog | Batch pricing (Batch API resolves it itself) ([variants]) |
| `:thinking` | catalog, **deprecated** | Use `reasoning` instead ([thinking]) |
| `:extended` | catalog, **deprecated** | No model offers it; requests fail ([variants]) |
| `:nitro` | routing | Sort by throughput **and admit priority-tier endpoints**, billed at the priority rate when one serves ([nitro]) |
| `:floor` | routing | Sort by price **and admit flex-tier endpoints** ([floor]) |
| `:exacto` | routing | Quality-first (tool-calling) provider sort ([variants]) |
| `:online` | routing, **deprecated** | `plugins:[{id:"web"}]` ([online]) |

Suffixes combine; the **last** sorting suffix wins. To resolve metadata, keep the catalog suffix and drop the routing ones; `/model/{id}` and `/models/{id}/endpoints` do this server-side ([variants]).

### 2.5 `provider` routing object

From [route] and [oas] (`additionalProperties:false`).

| Field | Default | Notes |
|---|---|---|
| `order` | — | Provider/endpoint slugs to try in order. Disables sticky routing and default routing. Falls back to the rest unless `allow_fallbacks:false`. |
| `only` / `ignore` | — | Merged with account-wide allow/ignore lists (the account list is the ceiling; nothing satisfying both → 404). If ≥2 remain, default routing picks among them. |
| `allow_fallbacks` | `true` | `false` = primary only; the upstream error is returned as-is. |
| `require_parameters` | `false` | `true` = only endpoints that support **every** param in the request. Otherwise unsupported params are ignored. |
| `data_collection` | `allow` | `deny` = no endpoints that store or train on data. |
| `zdr` | — | ZDR endpoints only (plus Safety-Retention endpoints unless the account is set to "ZDR only"). This is an OR with the account/guardrail settings; a request cannot turn ZDR off. |
| `enforce_distillable_text` | — | Only models whose author allows distillation. |
| `quantizations` | — | `int4, int8, fp4, mxfp4, nvfp4, fp6, fp8, mxfp8, fp16, bf16, fp32, unknown` |
| `sort` | — | `"price"\|"throughput"\|"latency"` or `{by, partition:"model"\|"none"}`. Bypasses default routing **and Auto Exacto**. |
| `preferred_min_throughput` / `preferred_max_latency` | — | A number (p50) or `{p50,p75,p90,p99}`. Soft preference only. |
| `max_price` | — | `{prompt, completion, image, audio, request}` in USD per million tokens / per unit. Hard filter. |
| `options` | — | `{<provider_slug>: {…}}` provider-specific passthrough, limited to an allowlist (`allowed_passthrough_parameters` on the images API) ([imggen], [prov]). |

Endpoint slug grammar: `provider[/variant][/region][/tier]`, e.g. `google-vertex/us-east5`, `baseten/fp4/us/priority`. A base slug matches every region/variant but **not** tier endpoints ([route]). The live slugs I checked: `openai`, `openai/flex`, `openai/fast`, `openai/ultrafast`, `anthropic`, `anthropic/fast`, `claude-on-aws`, `google-ai-studio`, `google-vertex/global`, `xai`, `xai/zdr`, `deepseek`, `z-ai/fp8`, `moonshotai`, `alibaba`, `amazon-bedrock/…`, `azure/…` ([api-ep]).

**Default routing**, when nothing pins it, picks a provider at random, weighted by measured effective price, throughput, TTFT, 429 rate, reliability and tool-call quality. Follow-ups stay on a provider while its cache is warm. Retries go to the remaining providers. Priority endpoints of **open-weight** models compete in default routing ([route], [tiers]). With `tools` present, OR "makes a best effort" to pick tool-capable providers ([route]).

**Soft parameter preferences**, applied even when `require_parameters:false`: `tools`, `response_format`/`text.format`, and `verbosity` filter providers *within* a model if some providers support them. They never remove a model ([route]).

### 2.6 Model fallbacks (`models`) and MSG `fallbacks`

`models:["a","b",…]`: on *any* error (context length, moderation, rate limit, downtime), OR tries the next model. Billing uses the model that served, reported in `model` ([fallback]). `provider.sort.partition` (default `model`) controls whether endpoints are grouped per model ([route]). MSG `fallbacks:[{model}]` has at most 3 entries, takes only `model`, and cannot be combined with `models`; violations are a 400 ([fallback]). A testing bot should never send these unless the user asks.

### 2.7 Minimal "clean" CC request

```json
POST /api/v1/chat/completions        (headers: Authorization, X-OpenRouter-Metadata: enabled)
{
  "model": "anthropic/claude-opus-5.5",
  "messages": [{"role": "user", "content": "Hello"}],
  "stream": true,
  "max_tokens": 16000,
  "reasoning": {"effort": "high"},
  "provider": {"only": ["anthropic"], "allow_fallbacks": false, "require_parameters": true},
  "plugins": [{"id": "context-compression", "enabled": false}],
  "session_id": "tg-<chat>-<thread>"
}
```

---

## 3. Reasoning

### 3.1 Request: `reasoning` object

| Field | CC | RSP | Meaning |
|---|---|---|---|
| `effort` | ✓ | ✓ | `max`, `xhigh`, `high`, `medium`, `low`, `minimal`, `none`. Only one of `effort` / `max_tokens` (or top-level `reasoning_effort`) may be set; otherwise "Only one of "reasoning.effort" and "reasoning.max_tokens" can be specified" ([reason], [oas]) |
| `max_tokens` | ✓ | ✓ | Thinking budget. Native on Anthropic (min 1024), Gemini (`thinkingBudget`) and some Qwen models (`thinking_budget`). For effort-only models it is converted to an effort ([reason]) |
| `enabled` | ✓ | ✓ | `true` = "medium effort, no exclusion"; the default is inferred from `effort`/`max_tokens` ([reason]) |
| `exclude` | ✓ | **ignored** | Model still reasons and is billed; the text is not returned ([reason]) |
| `summary` | ✓ | ✓ | `auto`, `concise`, `detailed` (OpenAI summaries) ([oas]) |
| `context` | ✓ | ✓ | `auto`, `all_turns`, `current_turn`; **GPT-5.6+ only** ([reason], [g56]) |
| `mode` | ✓ | ✓ | `standard` or `pro`. `pro` reroutes to the `*-pro` slug, OpenAI/Azure only, same per-token price ([reason]) |

The CC OpenAPI `reasoning` schema lists only `effort`, `max_tokens` and `summary`. `enabled`, `exclude`, `context` and `mode` are documented in the guides ([reason], [g56]); send them only when the user sets them.

Effort semantics ([reason]):
* **Effort-native families** (OpenAI o/GPT-5/6, Grok) receive the effort as-is.
* **Budget-only models** have the effort turned into a share of the output limit: `max`/`xhigh` ≈0.95, `high` 0.8, `medium` 0.5, `low` 0.2, `minimal` 0.1, `none` = off.
* **Anthropic, legacy budget mode:** `budget_tokens = max(min(max_tokens*ratio, 128000), 1024)`, and `max_tokens` must be strictly larger than the budget.
* **Gemini 3 `thinkingLevel`:** `minimal`/`low`/`medium`/`high` map 1:1; `xhigh`→`high`; anything unsupported → nearest level.
* **Every slug:** an unsupported non-`none` effort is **mapped to the nearest supported level** (e.g. `xhigh`→`high`). `none` sent to a model where reasoning is mandatory → **400** ("Reasoning is mandatory for this endpoint and cannot be disabled."). On `~…-latest` aliases, `none`/`enabled:false` is silently upgraded to the lowest effort ([latest], [c55]).

**Discover the allowed values from the models API, not from code** ([reason]). `GET /api/v1/models` → `reasoning: {supported_efforts (highest first; null = all), default_effort, default_enabled, mandatory, supports_max_tokens}`. Live examples (2026-11-11, [api-models]):

| Model | `supported_efforts` | `default_effort` | `mandatory` |
|---|---|---|---|
| `openai/gpt-6.1-sol` | max, xhigh, high, medium, low | medium | true |
| `anthropic/claude-opus-5.5` | max, xhigh, high, medium, low | high | true |
| `google/gemini-4-argon` | high, medium, low | medium | true |
| `x-ai/grok-4.7` | xhigh, high, medium, low | high | true |
| `deepseek/deepseek-v4-pro` | xhigh, high, none | high | false |
| `z-ai/glm-5.3` | max, high, low | max | true |
| `moonshotai/kimi-k3` | max, high, low, none | max | false |
| `qwen/qwen3.7-max` | (no effort list; `default_enabled:true`) | — | false |

Family-specific mappings ([c46], [c47], [s5], [o5], [c55], [g4]):
* **Claude 4.6+:** `reasoning.effort` and `verbosity` → `output_config.effort`, and `verbosity` wins. `minimal`→`low`.
* **Claude 4.7, 5 and 5.5:** adaptive thinking only. `reasoning.max_tokens` / `thinking.budget_tokens` are **accepted and silently ignored**; direct-to-Anthropic would be a 400 on 5.5.
* **Claude Sonnet 5, Opus 5 and 5.5:** reasoning is ON when `reasoning` is omitted.
* **Gemini 4:** `low`/`medium`/`high` only; `reasoning.max_tokens` does not set a budget.

MSG skin: `thinking:{type:"enabled",budget_tokens}` = `reasoning.max_tokens`; `{type:"adaptive"}` = `enabled:true`; `{type:"disabled"}` = `enabled:false`; `output_config.effort` = `reasoning.effort` and takes precedence over adaptive. Claude receives these fields unchanged; other models get their own vocabulary ([reason]).

### 3.2 Response shapes

**CC non-stream:** `choices[].message.reasoning` (plaintext string, may be absent) and `choices[].message.reasoning_details[]`. **CC stream:** `choices[].delta.reasoning` and `choices[].delta.reasoning_details[]`. Build the full sequence by concatenating chunks in order. Encrypted content "may appear as `[REDACTED]` in streaming" ([reason]).

`reasoning_details[]` element types ([reason], [oas]); common fields are `id` (string|null), `format`, `index`:

| `type` | Payload fields | Typical source |
|---|---|---|
| `reasoning.text` | `text`, `signature` (nullable) | Anthropic thinking blocks (with signature); open-weight plaintext reasoning |
| `reasoning.summary` | `summary` | OpenAI reasoning summaries |
| `reasoning.encrypted` | `data` | Anthropic `redacted_thinking`, OpenAI `encrypted_content`, Gemini thought signatures and continuation tokens |
| `reasoning.server_tool_call` | `tool_name`, `arguments`, `result`, `tool_call_id` | OR server-tool record (e.g. `openrouter:fusion`), replayed to rehydrate later turns ([oas]) |

`format` values: `unknown`, `openai-responses-v1`, `azure-openai-responses-v1`, `bedrock-openai-responses-v1`, `bedrock-xai-responses-v1`, `xai-responses-v1`, `meta-responses-v1`, `google-vertex-responses-v1`, `openrouter-stealth-v1`, `anthropic-claude-v1` (the default), `google-gemini-v1` ([reason]).

```json
"delta": {"reasoning_details": [{"type": "reasoning.text", "text": "Let me think…", "signature": null,
  "id": "reasoning-text-1", "format": "anthropic-claude-v1", "index": 0}]}
```

**RSP:** `output[]` items look like `{"type":"reasoning","id":"rs_…","summary":[{"type":"summary_text","text":…}],"content":[{"type":"reasoning_text","text":…}],"encrypted_content":"…","signature":"…","format":"…","status":…}` ([rsp-reason], [oas]). OR "automatically requests encrypted reasoning content from reasoning-capable providers", so `include:["reasoning.encrypted_content"]` is not needed ([reason]). Stream events: `response.reasoning_text.delta|done`, `response.reasoning_summary_part.added|done`, `response.reasoning_summary_text.delta|done` ([oas]). Usage: `usage.output_tokens_details.reasoning_tokens`.

**MSG:** native `thinking` (`thinking`, `signature`) and `redacted_thinking` (`data`) blocks. Gemini thought signatures and continuation tokens also arrive as `redacted_thinking` ([g4]). Claude's `thinking.display` is `summarized` (OR default), `omitted`, or `updates` (beta, MSG only) ([c55], [f51]).

**Budget trap:** reasoning shares the output limit. If it exhausts it, you get CC `finish_reason:"length"` with empty `content`, or RSP `status:"incomplete"` + `incomplete_details.reason:"max_output_tokens"` with no message item. Either way it is billed; detect it as `completion_tokens − reasoning_tokens ≈ 0` ([reason], [err]).

### 3.3 Passing reasoning back (stateless history)

General rules ([reason]):
* **CC:** put the assistant's `reasoning_details` array back on the same assistant message, unchanged. The plaintext `reasoning` field (alias `reasoning_content`) works only for raw-string reasoning. "The entire sequence of consecutive reasoning blocks must match the outputs generated by the model … you cannot rearrange or modify".
* **RSP:** append the previous `output` items in their original order (reasoning, `function_call`, `message` with `phase`), then your `function_call_output` items. Keep `content`, `summary`, `signature` and `encrypted_content` unchanged.
* Preservation is **essential with tool calls**: the model continues the same reasoning after the tool result.

| Upstream family | What comes back | What must go back | Failure if omitted / notes |
|---|---|---|---|
| **OpenAI** (GPT-5.x/6, o-series; Azure, Bedrock) | Summaries + encrypted content (`openai-responses-v1` / `azure-…` / `bedrock-…`); no raw CoT ([reason]) | CC: `reasoning_details`. RSP: reasoning items with `encrypted_content` + `phase` on assistant messages ([g54]) + `async` on pending calls ([g6]) | Losing `phase` → "significant performance degradation … early stopping" ([g54]). `reasoning.context:"current_turn"` makes the model ignore earlier reasoning (GPT-5.6+) ([reason]). |
| **Anthropic** (direct, Vertex, Bedrock, Azure, Claude Platform on AWS) | `reasoning.text` (+`signature`) and `reasoning.encrypted` (redacted), `anthropic-claude-v1` | Send them back unchanged, including on turns without tools ([c55]) | Blocks are bound to model and prefix. Sonnet 5.5 blocks are readable only within Anthropic / Claude-on-AWS / Azure; Haiku 5.5 blocks only by the producing account, so OR pins replay to that provider and returns **400** if `provider.only/ignore` excludes it. Direct-Anthropic prefix-binding enforcement (accounts created ≥2026-08-31) is "not enforced via OpenRouter". `thinking.block_binding.prefix_mismatch_behavior:"drop_block"` drops mismatched blocks and reports them in `input_transformations` ([c55], [f51]) |
| **Google Gemini** (AI Studio, Vertex) | Thought signatures as `reasoning.encrypted` (`google-gemini-v1`); their `id` links the signature to its tool call ([zed], [roo]) | `reasoning_details` on the assistant message that carries `tool_calls`. Do **not** put `thought_signature` on `tool_calls` (OR returns 400) ([zed]) | Gemini 3+: 400 "function call … is missing a `thought_signature`" ([zed]). Gemini 4 continuation item `{"type":"reasoning.encrypted","id":"gemini-continuation","format":"google-gemini-v1","data":…}` must be resent byte-for-byte to the same provider (§9) ([g4]). Agentic video navigation also comes back as encrypted reasoning that must be echoed ([video]). |
| **xAI Grok** | Encrypted reasoning (`xai-responses-v1`; `bedrock-xai-responses-v1` via Bedrock) ([reason]) | `reasoning_details` unchanged | Exact upstream requirement **UNVERIFIED** |
| **DeepSeek / Kimi / GLM / Qwen** (open weights, many hosts) | Plaintext `reasoning` + `reasoning.text` details (format likely `unknown`; **UNVERIFIED**) | `reasoning_details` (or `reasoning` string) on assistant tool-call messages | Upstream thinking + tool-call APIs require the reasoning to be echoed or they return 400. Whether OR maps `reasoning_details` → upstream `reasoning_content` on every host is **UNVERIFIED**; check with `debug.echo_upstream_body` |

**CC replay example: tool loop on Claude.** Turn 1 returned `tool_calls` and `reasoning_details`; turn 2 sends them back as-is:
```json
{"model": "anthropic/claude-sonnet-5.5", "reasoning": {"effort": "high"}, "tools": [ /* identical every turn */ ],
 "messages": [
  {"role": "user", "content": "Weather in Boston?"},
  {"role": "assistant", "content": null,
   "tool_calls": [{"id": "toolu_01", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Boston\"}"}}],
   "reasoning_details": [{"type": "reasoning.text", "text": "Need current weather…", "signature": "EqQB…",
                          "id": null, "format": "anthropic-claude-v1", "index": 0}]},
  {"role": "tool", "tool_call_id": "toolu_01", "content": "{\"temp_f\":62}"}
 ]}
```

**Switching models mid-conversation:** reasoning artifacts are model- and provider-bound. CC assistant messages accept a `model` field ("Model that generated this assistant message") ([oas]); whether OR uses it to drop foreign-format details is **UNVERIFIED**. Safe policy: when the next turn targets a different family or model, drop or keep `reasoning_details` according to `format`. Never send Claude-signed blocks to another account or provider group.

### 3.4 Mid-conversation effort changes (`configuration_update`)

Each skin has its own syntax; OR re-emits whichever form the selected provider expects ([reason], [g6]):
* **CC:** `{"role":"system","content":"","configuration_update":{"reasoning":{"effort":"low"}}}` placed right before the user turn it should affect (an OR extension).
* **RSP:** `{"type":"configuration_update","reasoning":{"effort":"low"}}`.
* **MSG:** `{"role":"system","content":[],"output_config":{"effort":"low"}}`.

Supported only on some models: Claude Fable 5.1/5.2, Opus 5, and GPT-6 / GPT-6.1 standard slugs. Unsupported models get a **400**, never a silent drop. Restrictions: no two adjacent updates, not with `truncation:"auto"`, not with pro mode, not in Batch ([reason], [g6]).

---

## 4. Multimodal

### 4.1 Input content parts

| Kind | CC part | RSP part | Notes |
|---|---|---|---|
| Text | `{"type":"text","text":…, "cache_control"?, "prompt_cache_breakpoint"?}` | `input_text` | System messages: text only ([img]) |
| Image | `{"type":"image_url","image_url":{"url":"https://…"\|"data:image/png;base64,…","detail":"auto\|low\|high\|original"}}` | `{"type":"input_image","image_url":…, "file_id"?, "detail"}` | `original` is an OR extension, **downgraded to `high`** where unsupported. Formats: png, jpeg, webp, gif. Count/size limits are per provider/model (not documented; errors `image_too_large`, `image_too_small`, `invalid_image`, `image_download_failed`) ([img], [oas], [err]). Put text first, then images ([img]) |
| File / PDF | `{"type":"file","file":{"filename":"x.pdf","file_data":"https://…"\|"data:application/pdf;base64,…","file_id"?}}` | `{"type":"input_file","filename":…,"file_url"\|"file_data"\|"file_id"}` | §4.3 ([pdf], [oas]) |
| Audio | `{"type":"input_audio","input_audio":{"data":"<base64>","format":"wav"}}` | same; `format` ∈ `wav`, `mp3` only | **No URLs**, base64 only. CC accepts any provider format (`aiff`, `aac`, `ogg`, `flac`, `m4a`, `pcm16`, `pcm24`, …) ([audio]) |
| Video | `{"type":"video_url","video_url":{"url":…,"processing":"agentic\|static"}}` (`input_video` = legacy CC form) | `{"type":"input_video","video_url":…,"processing"?}` | Formats: mp4, mpeg, mov, webm. **YouTube URLs only on Gemini via AI Studio**; Vertex accepts no URLs (use data URLs). OR "only sends video URLs to providers that explicitly support them". `processing` is Gemini-only and omitted upstream if unset. MSG has no video support ([video]) |

Check `architecture.input_modalities` (`text`, `image`, `file`, `audio`, `video`) before sending a part ([models-guide]).

### 4.2 Workspace files

`POST /api/v1/files` (multipart, ≤100 MiB per file, 10 GiB per workspace; PDF/PNG/JPEG/GIF/WebP/DOCX/XLSX/PPTX/MP3/WAV/FLAC/OGG/text) returns an `or_file_…` id. Uploaded files are not downloadable and are mainly meant for sandboxes ([files]). Using `file_id` in inference content parts is in the schema but its semantics are **UNVERIFIED** ([oas]).

### 4.3 PDFs and the `file-parser` plugin — avoid "local fakery"

* **Default:** "If you don't explicitly specify an engine, OpenRouter will default first to the model's native file processing capabilities, and if that's not available, we will use the `"mistral-ocr"` engine." Mistral OCR costs $2 per 1000 pages, or $2.2 on US-region requests, and is charged even with BYOK ([pdf]).
* **Engines:** `native` ("only available for models that support file input natively", charged as input tokens); `mistral-ocr`; `cloudflare-ai` (free markdown conversion). `pdf-text` is deprecated and redirects to `cloudflare-ai`. Mistral OCR forwards **at most 8 extracted images** per request, and strips images entirely for non-vision models ([pdf]).
* **Forcing native-only.** OR documents no "fail instead of parse" switch, so enforce it client-side:
  1. send `file` parts only when the model's `architecture.input_modalities` contains `"file"`;
  2. send `plugins:[{"id":"file-parser","pdf":{"engine":"native"}}]`. What it does when the model or endpoint lacks native support is **UNVERIFIED**; the plugin docs say it "will always attempt to use the provider's native" for web search, and something similar presumably applies here;
  3. treat any `annotations[].type=="file"` on the CC response as proof that OR parsed the file, since the native engine produces no annotations ([pdf]);
  4. with `X-OpenRouter-Metadata`, check `pipeline[]` for plugin stages ([rmeta]).

  `{"id":"file-parser","enabled":false}` exists ([oas]); what happens to the file then is **UNVERIFIED**.
* **Parsed annotations (CC):** `message.annotations:[{"type":"file","file":{"hash","name","content":[{type:"text"…},{type:"image_url"…}]}}]`. Sending them back skips re-parsing. On provider failure they are returned in `error.metadata.file_annotations`. RSP returns `file_citation` annotations instead, and those cannot be reused ([pdf]).

### 4.4 Audio output

CC only. Send `"modalities":["text","audio"]`, `"audio":{"voice":"alloy","format":"wav"}` and `stream:true` (required). Chunks carry `delta.audio:{data:<base64>, transcript}`; the final message has `audio:{id,data,expires_at,transcript}` ([audio], [oas]). Usage reports `completion_tokens_details.audio_tokens`. Separate endpoints exist for TTS (`/api/v1/audio/speech`, mp3/pcm) and STT (`/api/v1/audio/transcriptions`) ([mm]).

### 4.5 Video output

Video output uses the async `/api/v1/videos` job API (submit, poll, download, optional webhook) ([mm]). It is out of scope here.

### 4.6 Image output

* **In chat (CC/RSP):** `"modalities":["image","text"]` plus optional `image_config` (e.g. `aspect_ratio`, `quality`; the keys are provider-specific). The response carries `choices[].message.images:[{"image_url":{"url":"data:image/png;base64,…"}}]`. Usage reports `completion_tokens_details.image_tokens`, or `native_tokens_completion_images` in `/generation` ([oas], [ovw]). In streams, `delta.images` appears in the third-party client [go-or] (**UNVERIFIED** in official docs).
* **Dedicated Images API:** `POST /api/v1/images` with `{model, prompt, n≤10, resolution (512…4K), aspect_ratio, size, quality, output_format, background, output_compression, seed, input_references[], stream, provider}` → `{data:[{b64_json, media_type}], usage:{…,cost}}`. With `stream:true` the events are `image_generation.partial_image`, then `image_generation.completed`, then `error`. Billing is all-or-nothing. Capabilities are listed per endpoint via `/api/v1/images/models` and `…/endpoints` (`supported_parameters` descriptors, `allowed_passthrough_parameters`) ([imggen]).
* **Server tool** `openrouter:image_generation`: any model can call it; the default backend is `openai/gpt-image-2.6-flare` ([imgtool]).

---

## 5. Web search

### 5.1 Surfaces

| Surface | How to enable | Behaviour |
|---|---|---|
| **Server tool (recommended)** | `tools:[{"type":"openrouter:web_search","parameters":{…}}]` | The model decides when to search (0–N times). `engine` defaults to `auto` = native if the endpoint has it, else Exa ([ws]) |
| Plugin `web` (**deprecated**) | `plugins:[{"id":"web",…}]` or a `:online` suffix | Always searches once and **injects the results with a search prompt** ([ws-plugin]) |
| `web_search_options` (CC) | `{"search_context_size":"high"}` | Native search context size ([ws-plugin]) |
| Native OpenAI tool type | `{"type":"web_search"}` / `web_search_preview` in tools | **Hoisted to `openrouter:web_search`**, so it runs Exa on models without native search ([online], [oas]) |

Server-tool `parameters`: `engine` (`auto|native|exa|firecrawl|parallel|perplexity`), `mode`, `max_results` (5; 1–25), `max_uses` (forwarded natively only to Anthropic), `max_total_results`, `search_context_size` (low/medium/high), `max_characters`, `user_location{type:"approximate",city,region,country,timezone}`, `allowed_domains`, `excluded_domains`, `x_search{allowed_x_handles|excluded_x_handles,from_date,to_date,enable_image_understanding,enable_video_understanding}` ([ws]). The overall budget is set by `max_tool_calls` (default and max 30) ([srv]).

### 5.2 Native search coverage

From [ws] and the live `native_tools` field ([api-ep]):

| Provider | Models with native search | Domain filters | Notes |
|---|---|---|---|
| OpenAI | GPT-4.1 family, GPT-5+, o3, o3 Pro, o4-mini (**not** GPT-4o / 4o-mini / 4 Turbo → Exa) | `allowed_domains` only; `excluded_domains` silently ignored | |
| Anthropic | Claude 3.5 Haiku, 3.7 Sonnet, 4+ | both, but mutually exclusive | Native tool exists only on the `anthropic` and `claude-on-aws` endpoints, **not on Bedrock / Vertex / Azure**. If routing lands there, `auto` uses Exa ([api-ep]) |
| Google | Gemini 3 Flash/Pro, 3.1 Flash/Lite, 3.5 Flash (all Gemini 4 endpoints list `openrouter:web_search` live) | **Not supported**: with `auto` + filters → silently Exa; with `native` → 400 | |
| xAI (docs: "SpaceXAI") | Grok 4+ | both, mutually exclusive (≤5 each in the plugin) | X search is **opt-in** via `x_search` because of per-item billing since 2026-09-21; the legacy top-level `x_search_filter` still works |
| Perplexity | all | — | |

`engine:"native"` on a model without native search **falls back to Exa** (or to the workspace's fallback engine) instead of failing. Only a workspace engine list of just `native` produces a 403 ([ws]). Programmatically: endpoint `native_tools["openrouter:web_search"]` (e.g. `{"type":"web_search_20260209"}` for Anthropic) ([srv]).

### 5.3 Pricing

Native search is the provider's pass-through price; the model `pricing.web_search` shows it per search (e.g. $0.01 for GPT-6.1 / Claude 5.5, $0.014 for Gemini 4, $0.005 for Grok 4.7) ([api-models]). Third-party engines:

| Engine | Price |
|---|---|
| Exa | $0.007/request in instant, fast or auto mode; $0.012 in deep-lite or deep; $0.015 in deep-reasoning. Includes 10 results, then $0.001 each |
| Parallel | $0.001 (turbo, fast) or $0.005 (basic, advanced) |
| Perplexity | $0.005 |
| Firecrawl | $0.005 + $0.0125 per result; rejected under ZDR |

All engine fees come on top of tokens ([ws]). Zero-completion insurance does **not** refund search fees ([zci]).

### 5.4 Citations in responses

* **CC non-stream:** `message.annotations:[{"type":"url_citation","url_citation":{"url","title","content","start_index","end_index"}}]`. `content` is OR-added when available, e.g. Exa highlights separated by `[...]` ([ws-plugin], [ws]).
* **CC stream:** the chunk shape is **UNVERIFIED** in the official docs; the third-party client [go-or] models it as `delta.annotations` on stream chunks.
* **RSP:** `output_text.annotations:[{"type":"url_citation","url","title","start_index","end_index","content"?}]`. Streaming adds `response.output_text.annotation.added` (`annotation`, `annotation_index`, `content_index`, `item_id`, `output_index`), plus `response.web_search_call.in_progress|searching|completed` and output items of type `openrouter:web_search` ([rsp-ws], [oas]).
* **Usage:** `usage.server_tool_use_details:{web_search_requests, tool_calls_requested, tool_calls_executed}` ([ws], [oas]). `/generation` adds `num_search_results`, `web_search_engine` ([gen-api]).
* **Fidelity:** OR "standardized" all native results to the `url_citation` schema ([ws-plugin]). Provider-specific grounding detail (Gemini `groundingMetadata`/search-entry HTML, Anthropic `web_search_result_location` citation blocks and `encrypted_content`, OpenAI `web_search_call.action` sources) is **not documented as surviving on CC/RSP — assume it is lost (UNVERIFIED)**. The MSG skin defines Anthropic citation and search-result block types in [oas], so it is the most likely to preserve Claude's native shape.

---

## 6. Streaming

* **SSE framing:** `data: {json}\n\n` lines ending with `data: [DONE]`. OR sends **comment lines `: OPENROUTER PROCESSING`** as keep-alives (also as heartbeats while server tools run); skip lines starting with `:` ([stream], [subagent-cb]).
* **Generation id:** the `X-Generation-Id` response header plus the `id` on every chunk (`gen-…`). Persist it as soon as the headers arrive (§9) ([stream]).
* **Pre-commit errors:** before the upstream answers there is a normal HTTP status with a JSON body, and OR can still fail over silently. Once the provider returns headers, **HTTP 200 is committed**; any later failure, *including a provider error before the first token*, is in-band. If nothing has reached you yet, OR still tries backups, so a 200 can end with the last provider's error ([err]).

**CC chunk sequence (illustrative; field values abbreviated):**
```text
: OPENROUTER PROCESSING
data: {"id":"gen-…","object":"chat.completion.chunk","created":…,"model":"…","provider":"Anthropic","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":"…","reasoning_details":[…]},"finish_reason":null,"native_finish_reason":null}]}
data: {…"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"f","arguments":"{\"a\""}}]}}]}
data: {…"choices":[{"index":0,"delta":{"content":""},"finish_reason":"tool_calls","native_finish_reason":"tool_use"}]}
data: {…"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"tool_calls","native_finish_reason":"tool_use"}],"usage":{…}}
data: [DONE]
```
* **Final usage chunk:** always sent (no opt-in needed). Unlike OpenAI it **has a non-empty `choices`** that repeats `finish_reason`/`native_finish_reason`; treat it as an accounting frame, not a second terminal event. With `X-OpenRouter-Metadata`, `openrouter_metadata` also arrives on the last chunk ([stream], [rmeta]).
* **Debug chunk:** with `debug.echo_upstream_body`, the first chunk is `{"choices":[],"debug":{"echo_upstream_body":{…}}}`, sent once per provider attempt ([err]).
* **`finish_reason`** is normalized to `stop`, `length`, `tool_calls`, `content_filter` or `error`; the raw upstream value is in `native_finish_reason` ([ovw]). Upstream token-limit errors (`max_tokens_exceeded`, output-side `context_length_exceeded`, `token_limit_exceeded`, `string_too_long`) are **rewritten** to `finish_reason:"length"` on CC and `response.incomplete` on RSP ([stream], [err]). A model refusal on success comes back as `message.refusal` + `finish_reason:"content_filter"` on CC, and on RSP as `status:"incomplete"` + `incomplete_details.reason:"content_filter"` + a `refusal` part ([err]).
* **CC mid-stream error** (terminates the stream):
  ```text
  data: {"id":"gen-abc","object":"chat.completion.chunk","created":…,"model":"openai/gpt-4o","provider":"OpenAI","error":{"code":429,"message":"Rate limit exceeded","metadata":{"error_type":"rate_limit_exceeded","provider_code":"…"}},"choices":[{"index":0,"delta":{"content":""},"finish_reason":"error"}]}
  ```
  It can be the only event, so treat a 200 with an error chunk and no content as a failure. On 500-class errors the `message` is replaced by a generic string and `provider_code` is dropped ([err]).
* **RSP events** ([oas]):
  * lifecycle: `response.created`, `response.in_progress`;
  * output: `response.output_item.added|done`, `response.content_part.added|done`, `response.output_text.delta|done`, `response.refusal.delta|done`, `response.output_text.annotation.added`, `response.function_call_arguments.delta|done`, `response.custom_tool_call_input.delta|done`;
  * reasoning: `response.reasoning_text.*`, `response.reasoning_summary_part.*`, `response.reasoning_summary_text.*`;
  * tools: `response.web_search_call.*`, `response.image_generation_call.*` (including `partial_image`), `response.code_interpreter_call*`, `response.mcp_*`, `response.apply_patch_call_operation_diff.*`, `response.fusion_call.*`;
  * debug: `response.debug`;
  * terminal: `response.completed` / `response.incomplete` / `response.failed`, plus a plain `error` event.

  Each event has a `sequence_number`. A failed stream ends with `{"type":"response.failed","response":{…,"status":"failed","error":{"code":"server_error","message":…},"error_type":"provider_unavailable"}}` ([stream], [err]).
* **MSG:** standard Anthropic events. Usage comes in `message_delta`; errors are `{"type":"error","error":{"type":"overloaded_error","message":…,"error_type":"provider_overloaded"}}` ([err], [stream]).
* **Cancellation:** aborting the HTTP connection "immediately stops model processing and billing" **only for supported providers** ([stream]):

  | Supported | Not supported (keeps generating; "billed for the complete response") |
  |---|---|
  | OpenAI, Azure, Anthropic, Fireworks, Mancer, Recursal, AnyScale, Lepton, OctoAI, Novita, DeepInfra, Together, Cohere, Hyperbolic, Infermatic, Avian, xAI, Cloudflare, SFCompute, Nineteen, Liquid, Friendli, Chutes, DeepSeek | AWS Bedrock, Groq, Modal, **Google, Google AI Studio**, Minimax, HuggingFace, Replicate, Perplexity, Mistral, AI21, Featherless, Lynn, Lambda, Reflection, SambaNova, Inflection, ZeroOneAI, AionLabs, **Alibaba**, Nebius, Kluster, Targon, InferenceNet |

  Non-streaming requests are never cancelled. **Z.AI, Moonshot AI and many newer hosts appear in neither list (UNVERIFIED).** Conflicting docs: [broadcast] says that for a non-abortable upstream, the quantities OR billed are the "output delivered before the disconnect" (the provider may bill a BYOK key for more). [stream] says you are "billed for the complete response". Treat the cost as possibly the full generation.

---

## 7. Errors

**When the HTTP status is real:** OR returns the error's status code only for errors raised before the upstream commits (invalid request, auth, credits, routing). After that the status is 200 and the error is in the body or the stream (§6). A **non-streaming 200 can contain only `{"error":…}` and no `choices`**, so always check for `error` ([err]). A CC non-stream response can also carry partial content plus `choices[].error` and `finish_reason:"error"` ([err]).

**Body shapes:**
```json
// CC / generic:
{"error":{"code":403,"message":"…","metadata":{"error_type":"refusal","provider_code":"…","provider_name":"…","raw":{…}}},"openrouter_metadata":{…}?,"user_id":null}
// RSP:
{"error":{"code":"server_error","message":"Invalid credentials"},"error_type":"authentication","status":"failed","id":"resp_…"}   // pre-response: {"error":{…},"metadata":null}
// MSG:
{"type":"error","error":{"type":"authentication_error","message":"…","error_type":"authentication"},"request_id":null|"gen-…"}
```

| HTTP | `error_type` (canonical, stable across skins) | Cause | Retry policy |
|---|---|---|---|
| 400 | `invalid_request`, `invalid_prompt`, `context_length_exceeded`, `max_tokens_exceeded`, `token_limit_exceeded`, `string_too_long`, `invalid_image`, `image_too_large`, `image_too_small`, `unsupported_image_format`, `image_download_failed` | Bad params, prompt over context, mandatory-reasoning `none`, `store:true`, unsupported `configuration_update`, … | **Permanent.** Show it verbatim |
| 401 | `authentication` | Bad, disabled or expired key | Permanent |
| 402 | `payment_required` | `metadata.limit_source`: `openrouter_credits` (balance; or `reason:"weight_exceeds_budget"`), `openrouter_key_limit`, `openrouter_in_flight_budget` (`reason:"in_flight_budget_exhausted"`) | Retry **only** `openrouter_in_flight_budget`, after `Retry-After`. A 402 without the header is not retryable ([lim]) |
| 403 | `permission_denied`, `content_policy_violation`, `refusal` | Guardrail or prompt-injection block (`metadata.patterns`); budget block (`limit_source`: `openrouter_guardrail_budget`, `openrouter_workspace_budget`, `openrouter_key_limit` + `scope`, `limit_usd`, `spend_usd`, `resets_at`, `remedy_hint`); moderation; provider safety filter (e.g. Gemini `SAFETY`); refusal returned as an error (Anthropic usage policy, OpenAI `bio_policy`/`cyber_policy`) | Permanent (budgets: wait until `resets_at`). Policy blocks are never re-sent to another endpoint ([err], [g6]) |
| 404 | `not_found`, `image_not_found` | Unknown model, catalog-variant miss, `only` ∩ account allowlist empty ([route], [variants]) | Permanent |
| 408 | — | Request timeout | Retry with backoff |
| 412 / 413 / 422 | `precondition_failed` / `payload_too_large` / `unprocessable` | | Permanent |
| 429 | `rate_limit_exceeded` (+`provider_code`) | OR limits (free models, key management, per-IP) or upstream rate limit **after** OR tried other providers | Retry; honor `Retry-After` / `X-RateLimit-Reset` (ms epoch) ([lim]) |
| 500 | `server`, `unmapped` | Internal; **message masked**, no `provider_code` | Retry a few times |
| 502 | `provider_unavailable` | Model down or invalid/empty upstream response | Retry, or switch provider |
| 503 | `provider_overloaded` | No provider meets the routing requirements, or overloaded | Retry if transient; **permanent if caused by your `provider` pins** |
| 504 | `timeout` | Provider timeout | Retry |
| 524 / 529 | — | Edge timeout / provider overloaded ([oas]) | Retry |

* **RSP native `error.code` is lossy:** many types collapse to `server_error`, while `context_length_exceeded`, `invalid_request` and `refusal` become `invalid_prompt`, and `content_policy_violation` becomes `image_content_policy_violation`. Always read the top-level `error_type` ([err], [rsp-err]).
* **MSG mapping:** `authentication_error`, `permission_error`, `billing_error`, `not_found_error`, `rate_limit_error`, `overloaded_error` (also for `timeout`), `invalid_request_error`, `api_error` ([err]).
* **Moderation metadata:** `{reasons: string[], flagged_input: string (≤100 chars, middle-truncated), provider_name, model_slug}` ([err]). Moderated endpoints have `top_provider.is_moderated:true`; live, that covers almost all OpenAI and Anthropic models (and Amazon, Cohere, Meta) ([api-models]). This OR-side input moderation does not exist on the direct APIs; the exact classifier is **UNVERIFIED**.
* **Verbatim upstream detail:** `metadata.provider_code`, `metadata.provider_name` and `metadata.raw` (the provider's verbatim payload; documented for policy blocks) are present on non-500 provider errors. For 500s they are masked ([err]). For a testing bot, display `code`, `error_type`, `message`, `provider_name`, `provider_code` and `raw`, and send `X-OpenRouter-Metadata: enabled` to see `attempts[]` ([rmeta]).
* **No-content 200s:** possible on cold starts, or when reasoning used up `max_tokens`. Retrying doesn't help the latter ([err]). Zero-completion insurance means no token charge when `completion_tokens==0` and the finish reason is blank, or on an error finish ([zci]).

---

## 8. Accounting and model metadata

### 8.1 `usage` in responses

Always included; the opt-in flags are deprecated no-ops ([usage]). Token counts come from the model's native tokenizer ([ovw]).

CC non-stream response, composited from the documented shapes ([ovw], [reason], [ws-plugin], [usage]):
```json
{"id": "gen-…", "object": "chat.completion", "created": 1762800000, "model": "openai/gpt-6.1-sol", "provider": "OpenAI",
 "system_fingerprint": null, "service_tier": "default",
 "choices": [{"index": 0, "finish_reason": "stop", "native_finish_reason": "completed",
   "message": {"role": "assistant", "content": "…", "refusal": null,
     "reasoning": null, "reasoning_details": [{"type": "reasoning.summary", "summary": "…", "format": "openai-responses-v1", "index": 0},
                                             {"type": "reasoning.encrypted", "data": "gAAAA…", "id": "rs_…", "format": "openai-responses-v1", "index": 1}],
     "annotations": [{"type": "url_citation", "url_citation": {"url": "https://…", "title": "…", "content": "…", "start_index": 10, "end_index": 42}}]}}],
 "usage": {"prompt_tokens": 1200, "completion_tokens": 640, "total_tokens": 1840,
   "prompt_tokens_details": {"cached_tokens": 1024, "cache_write_tokens": 0},
   "completion_tokens_details": {"reasoning_tokens": 512},
   "cost": 0.0071, "is_byok": false,
   "cost_details": {"upstream_inference_cost": null, "upstream_inference_prompt_cost": 0.0008, "upstream_inference_completions_cost": 0.0063},
   "server_tool_use_details": {"web_search_requests": 1}}}
```

| CC field | RSP field | Meaning |
|---|---|---|
| `prompt_tokens` | `input_tokens` | Includes images, audio, tools |
| `prompt_tokens_details.cached_tokens` | `input_tokens_details.cached_tokens` | Cache **read** |
| `prompt_tokens_details.cache_write_tokens` | `input_tokens_details.cache_write_tokens` | Cache **write** (only for models with explicit caching and write pricing) ([cache]) |
| `prompt_tokens_details.audio_tokens`, `.video_tokens` | — | |
| `completion_tokens` | `output_tokens` | Includes reasoning |
| `completion_tokens_details.reasoning_tokens` | `output_tokens_details.reasoning_tokens` | |
| `completion_tokens_details.audio_tokens`, `.image_tokens`, `.accepted_prediction_tokens`, `.rejected_prediction_tokens` | | |
| `total_tokens` | `total_tokens` | |
| `cost` | `cost` | Credits charged (USD) |
| `is_byok` | `is_byok` | |
| `cost_details.upstream_inference_cost`, `.upstream_inference_prompt_cost`, `.upstream_inference_completions_cost`, `.server_tool_cost` | `cost_details.upstream_inference_cost`, `.upstream_inference_input_cost`, `.upstream_inference_output_cost` | Upstream cost; in `/generation`, `upstream_inference_cost` is filled **only for BYOK** ([usage]) |
| `server_tool_use_details.{web_search_requests,tool_calls_requested,tool_calls_executed}` | same | §5 |
| `deferred_tools_details.{prompt_tokens,completion_tokens,total_tokens,turns}` | | Totals over the internal calls of a tool-search run ([oas]) |

MSG reports Anthropic-style `usage` in `message_delta`; the fast tier adds `usage.speed:"fast"` ([tiers]). Response cache hits report zeroed usage ([respcache]). The top-level `service_tier` field on responses gives the tier the provider reported ([oas]).

### 8.2 `GET /api/v1/generation?id=gen-…`

Uses a normal inference API key; the docs example uses one, and [oas] does not mark it as needing a management key ([gen-api], [ovw]). The `data` fields:
* identity: `id`, `request_id`, `upstream_id`, `session_id`, `created_at`, `model`, `provider_name`, `router`, `api_type`;
* timing and tokens: `streamed`, `cancelled`, `latency` (ms), `generation_time` (ms), `moderation_latency`, `tokens_prompt` / `tokens_completion` (OR's own estimates), `native_tokens_prompt`, `native_tokens_completion`, `native_tokens_reasoning`, `native_tokens_cached`, `native_tokens_completion_images`;
* outcome: `finish_reason`, `native_finish_reason`;
* media and tools: `num_media_prompt`, `num_input_audio_prompt`, `num_media_completion`, `num_search_results`, `num_fetches`, `web_search_engine`;
* money: `total_cost`, `usage`, `upstream_inference_cost`, `cache_discount` (negative for Anthropic cache writes), `is_byok`;
* routing: `provider_responses[]` (`{provider_name, model_permaslug, endpoint_id, id, status, latency, is_byok}`, one per attempt including fallbacks), `service_tier`, `data_region`, `response_cache_source_id`;
* attribution: `app_id`, `http_referer`, `origin`, `user_agent`, `external_user`, `preset_id`, `workspace_id`.

How soon a record is available after a request is **not documented**. Third-party clients say "a few seconds" ([ocaml]); retry a 404 with backoff. Errors: 401, 403 (another user's generation), 404, 429, 5xx.

`GET /api/v1/generation/content?id=` returns `{data:{input:{messages|prompt}, output:{completion, reasoning}, error}}`. It only has content if **Input & Output Logging** (beta, an Observability setting) was on *when the request was made*. It is not populated for ZDR requests or for `eu.`/`us.` regional hosts ([logs], [iolog], [gen-content-api]). Retention is at least 3 months ([iolog]). Whether this endpoint needs a management key is **UNVERIFIED**: its 403 example reads "Only management keys can perform this operation" ([oas]).

### 8.3 Key and credits

`GET /api/v1/key` ([lim]) returns:
* `label`, `limit`, `limit_reset`, `limit_remaining`, `include_byok_in_limit`;
* `budgets[]` and `effective_budget`, each `{scope, guardrail_id, budget_id, limit_usd, spend_usd, remaining_usd, reset_interval, resets_at, include_byok}`;
* `usage`, `usage_daily`, `usage_weekly`, `usage_monthly` and the `byok_usage*` equivalents;
* `is_free_tier`, `free_model_daily_requests:{used, limit, remaining}`;
* a deprecated `rate_limit` object.

`GET /api/v1/credits` returns `{data:{total_credits,total_usage}}` and **requires a management key** ([oas]). Both endpoints are per-IP burst limited ([lim]). The in-flight spending budget: a request's estimated cost (prompt plus `max_tokens`, or a fixed cap) is held, so concurrent big requests can get a 402 even when the balance is positive ([lim]).

### 8.4 Models API: support new models with no code change

* `GET /api/v1/models` returns text models only by default. Filters: `?output_modalities=text,image,audio,…|all`, `supported_parameters=tools`, `endpoints=/api/v1/chat/completions`, `sort=newest|…`, `offset`/`limit`; there is also an RSS feed. `GET /api/v1/model/{author}/{slug}` gives one model and resolves aliases and variants ([models-guide]).
* **Model object** fields ([models-guide], [oas]):
  * identity: `id`, `canonical_slug`, `name`, `created`, `description`, `hugging_face_id`, `alias_target{slug,name}` (for `~latest`), `links.details`;
  * limits: `context_length`, `per_request_limits`, `top_provider{context_length, max_completion_tokens, is_moderated}`;
  * capabilities: `architecture{input_modalities, output_modalities, modality, tokenizer, instruct_type}`, `supported_parameters[]`, `reasoning{…}` (§3.1), `endpoints[]` (which API paths serve it);
  * pricing: `pricing{prompt, completion, request, image, image_token, image_output, audio, audio_output, input_audio_cache, web_search, internal_reasoning, input_cache_read, input_cache_write, input_cache_write_1h, discount, overrides[]}`. `overrides` carries conditional prices with `min_prompt_tokens`, `utc_start`/`utc_end`/`utc_days`; **skip entries with unknown condition keys**;
  * other: `default_parameters` (e.g. Grok 4.7 `{"temperature":0.7,"top_p":0.95}`; whether OR *applies* these when you omit them is **UNVERIFIED**, since [params] says absent sampling params are omitted), `expiration_date`, `knowledge_cutoff`, `benchmarks`, `supported_voices`.
* The `supported_parameters` vocabulary: `temperature, top_p, top_k, min_p, top_a, frequency_penalty, presence_penalty, repetition_penalty, max_tokens, max_completion_tokens, logit_bias, logprobs, top_logprobs, prediction, seed, response_format, structured_outputs, stop, tools, tool_choice, parallel_tool_calls, include_reasoning, reasoning, reasoning_effort, web_search_options, verbosity` ([oas]). Live example: `openai/gpt-6.1-sol` lists **no** `temperature`, `top_p`, `stop` or `logprobs` ([api-models]).
* `GET /api/v1/models/{author}/{slug}/endpoints` returns `data.endpoints[]`, each with:
  * `name`, `provider_name`, `tag` (routing slug), `model_id`, `quantization`, `status`, `service_tier` (`flex|priority|ultrafast|null`);
  * `context_length`, `max_completion_tokens`, `max_prompt_tokens`, `supported_parameters[]`, `supports_tool_choice{auto,none,required,function}`, `supports_implicit_caching`, `native_tools{…}`;
  * `pricing{…}`, `uptime_last_5m/30m/1d`, `latency_last_30m` / `throughput_last_30m` percentiles (these need an API key or cookie), `perf_last_30m_by_workload` ([oas], [api-ep]).
* **Recipe:**
  1. Build the model picker from `/models`, keeping `id` exactly as the user typed it, routing suffixes included.
  2. Gate every UI toggle on `supported_parameters`, `input_modalities` and `reasoning`.
  3. Clamp `max_tokens` to `top_provider.max_completion_tokens`.
  4. When the user pins a provider, re-gate on that endpoint's `supported_parameters` and send `require_parameters:true`.
  5. Show `quantization` and `provider_name`.
  6. Compute prices from `usage.cost`, not from a local table.

---

## 9. Async, background and resume — what is and isn't possible

| Capability | Status on OpenRouter | Evidence |
|---|---|---|
| OpenAI-style `background:true` + `GET /v1/responses/{id}` polling | **No.** `background` is in the RSP schema with no description; `store` is the constant `false`; there is no `GET`/`cancel` responses route in the API | [oas], [rsp] |
| Resume a dropped stream (`starting_after` / `sequence_number` replay) | **No.** Not documented or present. `sequence_number` exists on RSP events but there is no replay endpoint | [oas] |
| `previous_response_id` / server-side conversation | **No** (400) | [rsp] |
| Generation keeps running after the client disconnects | **Only on providers without cancellation support** (Google, Bedrock, Alibaba, Groq, Mistral, …), and even then the output is not retrievable. Supported providers are **aborted** on disconnect | [stream], [broadcast] |
| Cost / metadata after a drop | **Yes**, via `GET /api/v1/generation?id=` using the id from the `X-Generation-Id` header or the first chunk. `cancelled` shows whether OR aborted the upstream | [gen-api], [stream] |
| Text after a drop | Only with **Input & Output Logging** on (`/generation/content`), and only what was generated before the abort | [logs], [iolog] |
| Gemini 4 "long decode" continuation | **Yes (Gemini 4 only).** Checkpoints (`reasoning.encrypted`, `id:"gemini-continuation"`) stream in `delta.reasoning_details`; finish reasons are `length` + `native_finish_reason:"MAX_TOKENS"\|"CONTINUATION"`. To resume, resend the history plus an assistant message with `content:""` and the latest continuation item. After a dropped stream: "discard what came after the latest item and resend that item with the same `max_tokens`". `max_tokens` is cumulative across hops. The token works only on the provider that issued it, and a dropped stream returns no usage | [g4] |
| Survive a bot restart / long jobs | **Batch API**: `POST /api/v1/batches` with `{endpoint:"/v1/chat/completions"\|"/v1/responses"\|"/v1/messages", model, requests:[{custom_id, body}], callback_url?}` → 202. Poll `GET /api/v1/batches/{id}` (`validating→in_progress→finalizing→completed`). No streaming and no cancel; ≤24h (Vertex up to 96h); about 50% price; only models with `:batch`; no base64 images, audio, OR web search, ZDR, or `provider.order/sort`; results kept 30 days | [batch] |
| Cached replay | OR response cache (opt-in `X-OpenRouter-Cache: true`, TTL 300s by default) replays identical requests for free, even on streams. It is not a resume mechanism | [respcache] |

**Bot design implication.** Persist `gen-id`, model, provider and the partial text as the stream arrives. On a network drop or bot restart, mark the turn as interrupted, fetch `/generation` for the cost and `cancelled` flag, and offer a "regenerate" option (a full new request, billed again). For Gemini 4, offer "continue" from the last continuation item. Keep the SSE read timeout above the gap between `: OPENROUTER PROCESSING` keep-alives (the interval is **UNVERIFIED**); long reasoning phases can be silent for a long time.

---

## 10. Prompt caching via OpenRouter

Usage fields: `cached_tokens` (read) and `cache_write_tokens` (write), plus the `/generation` `cache_discount` ([cache]).

| Upstream | Mode | Write cost | Read cost | Notes |
|---|---|---|---|---|
| OpenAI | Automatic; prompts ≥1024 tokens | free before GPT-5.6; **1.25× on GPT-5.6+ even when automatic** | 0.25× or 0.5× | GPT-5.6+ explicit mode: per-block `prompt_cache_breakpoint:{mode:"explicit",ttl?:"30m"\|"12h"}` and root `prompt_cache_options:{mode:"explicit",ttl}`; 12h writes cost 2.5×. Explicit mode with no breakpoints **turns caching off**. Pair with `prompt_cache_key` |
| Anthropic | Explicit `cache_control:{type:"ephemeral",ttl?:"5m"\|"1h"}` on blocks (≤4 breakpoints), **or automatic top-level `cache_control`** (Anthropic, Vertex, Azure, Bedrock, Claude Platform on AWS; on Bedrock it becomes a trailing breakpoint) | 1.25× (5m), 2× (1h) | 0.1× | Minimum 512 tokens on 5.x, 1024–4096 on older models. RSP supports only the top-level form; per-block uses `prompt_cache_breakpoint`, which converts to a default 5m `cache_control` (TTL dropped) |
| Google Gemini 2.5+ | Implicit (no write cost; read 0.25×; TTL ~3–5 min) and explicit `cache_control` | explicit: input price + 5 min of storage | 0.25× | **Only the last breakpoint is used.** The first `system` message is immutable for caching, so put dynamic tail content in a later `user` message |
| xAI Grok | Automatic | free | 0.25× | |
| DeepSeek | Automatic | 1× | 0.1× | |
| Moonshot AI | Automatic | free | 0.25× | Groq (Kimi K2) also caches automatically at 0.5× |
| Z.AI (GLM) | Automatic | free (temporarily) | ≈0.2× | OR **sends Z.AI a session affinity key** derived from the account and `session_id` |
| Alibaba Qwen | **Explicit only** via `cache_control:{type:"ephemeral"}` (5m) on `qwen3-max`, `qwen-plus`, `qwen3.6-plus`, `qwen3-coder-plus`/`-flash`, `deepseek-v3.2`; not on snapshot slugs | 1.25× | 0.1× | |

Marker conversion: Anthropic-style block `cache_control` becomes `prompt_cache_breakpoint` on GPT-5.6+, and the reverse also happens; TTLs are **dropped** in both directions. `prompt_cache_options` is OpenAI-only ([cache]).

**Sticky routing** ([cache], [route]):
* After a cache hit (or on any successful request that carries `session_id`), later requests for the same account, model and conversation go to the **same provider endpoint**.
* Without `session_id`, the conversation key is a hash of the first system/developer message plus the first non-system message; `prompt_cache_key` is the fallback key.
* Stickiness expires after **60 min** idle. It applies only when cache reads are cheaper than prompt price, and an error from the sticky provider allows a re-route.
* It is **ignored when you set `provider.order`**.
* **Auto Exacto can override stickiness on tool requests**, causing mid-session cache misses. Opt out with `provider.sort:"price"` or `:floor` ([exacto]).
* For a bot: send a stable `session_id` per chat thread, and pin the provider explicitly anyway.

---

## 11. Known OpenRouter limitations vs. official APIs

Each entry below is marked **missing**, **degraded**, **renamed**, or **transformed**. "Default routing" means OR may choose a hosting provider other than the model's creator unless you pin one (§2.5). The live endpoint data is from 2026-11-11 ([api-ep]).

### 11.1 By upstream provider

**OpenAI** (first-party `openai`; also served by `azure`, `amazon-bedrock`; tier endpoints `openai/flex`, `openai/fast`, `openai/ultrafast`)
* **Missing — Responses state:** `store:true`, `previous_response_id` (400), background mode, `GET`/`cancel`/`input_items` routes, `starting_after` stream resume, Conversations API ([rsp], [oas]). `prompt` (stored prompt templates) behind a stateless proxy is **UNVERIFIED**.
* **Transformed:** OR auto-requests `reasoning.encrypted_content` ([reason]).
* **Transformed:** an OpenAI-native `{"type":"web_search"}` / `web_search_preview` tool is **hoisted to `openrouter:web_search`**. On models without native search it silently runs Exa ([online], [ws]). Pass-through of the other built-in tool types in the RSP schema (`file_search`, `code_interpreter`, `computer_use`, `mcp`, `image_generation`, `local_shell`, `custom`, `namespace`) is in [oas], but end-to-end support is **UNVERIFIED**.
* **Degraded on CC:**
  * `phase` cannot be represented, causing "early stopping" on GPT-5.4+ ([g54]);
  * `async:true` tools are stripped and run synchronously ([g6]);
  * `reasoning.exclude` on RSP is ignored ([reason]).
* **Transformed:**
  * `reasoning.mode:"pro"` reroutes to the `*-pro` slug and goes only to OpenAI/Azure ([reason]);
  * `reasoning.mode`, `prompt_cache_breakpoint` and `prompt_cache_options` are **stripped** when a fallback provider doesn't support them ([g56]);
  * an unsupported effort is mapped to the nearest one ([latest]).
* **Transformed:** `user`/`safety_identifier` are hashed. An account-level identity is sent when they are absent ([usertrack]).
* **Added:** OR-side input moderation on `is_moderated` endpoints (almost all OpenAI models), producing a 403 with `reasons`/`flagged_input` ([err], [api-models]).
* **Renamed:** policy refusals become HTTP 403 `error_type:"refusal"` with `provider_code` `bio_policy`/`cyber_policy` ([g6]); `service_tier:"fast"` is reported as `priority` ([tiers]).
* **Missing:** `n>1` (not in schema, **UNVERIFIED**). Audio output is CC-only with streaming required; RSP `input_audio` accepts only wav/mp3 ([audio]).
* **Transformed:** gpt-image models go through OR's normalized Images API (`resolution` tiers, `aspect_ratio`, `size`, `n≤10`) rather than OpenAI's own endpoints ([imggen]).
* **Degraded — routing:** a request may land on Azure or Bedrock. Bedrock silently ignores `reasoning.mode` (OR avoids it for pro requests) and Bedrock streams are **not cancellable** ([reason], [stream]).

**Anthropic** (first-party `anthropic`, `anthropic/fast`; also `claude-on-aws`, `google-vertex/*`, `amazon-bedrock/*`, `azure/*`; 11 endpoints for Opus 5.5)
* **Transformed — injected defaults:**
  * `max_tokens` (the debug echo shows `65536` for an unset value) ([err]);
  * `thinking.display:"summarized"` (Anthropic's own default on newer models is `"omitted"`) ([reason], [c55]);
  * on Sonnet 5.5 with no reasoning setting, `thinking:{type:"adaptive",display:"summarized"}` is added ([c55]);
  * `eager_input_streaming:true` on every tool of a streamed request ([route]).
* **Transformed:** tool `strict:true` is **stripped** unless `x-anthropic-beta: structured-outputs-2025-11-13` is sent. Only the `interleaved-thinking-2025-05-14` and `structured-outputs-…` beta headers pass through; OR manages other betas itself, such as structured outputs for `json_schema`, extended context, and `mid-conversation-tool-changes-2026-07-01` ([route], [f51]).
* **Transformed:** `reasoning.max_tokens` / `thinking.budget_tokens` on Claude 4.7/5/5.5 are **accepted and ignored** (sent as adaptive; Anthropic direct would 400) ([c55], [c47]). `verbosity` → `output_config.effort`, and it overrides `reasoning.effort`; `minimal`→`low`; `xhigh`/`max` fall back on models without them ([c46], [params]). Sampling params on Opus 4.7 and Sonnet 5 are silently ignored ([c47], [s5]).
* **Degraded — thinking semantics:**
  * prefix-binding enforcement (direct-API accounts created ≥2026-08-31) is "not enforced via OpenRouter", so edited histories behave differently from direct ([c55], [f51]);
  * account-bound Haiku 5.5 thinking pins replay to one provider (400 if your `only`/`ignore` excludes it) ([c55]).
* **Missing:** computer-use tools (rejected on MSG) ([c55]).
* **Degraded:** Anthropic native web search/fetch exists only on the `anthropic` and `claude-on-aws` endpoints; elsewhere `auto` uses Exa ([api-ep], [ws]). Citations, search-result blocks, the Files API (needs BYOK via `?provider=anthropic`), the MCP connector and code execution on CC/RSP are **UNVERIFIED**; the MSG schema includes some of these block types ([files], [oas]).
* **Renamed:** MSG errors get an extra `error.error_type`; `request_id` is the OR `gen-…` id ([err]). Fast mode reports `service_tier:"priority"` (Anthropic would say `standard`), plus `usage.speed:"fast"` ([tiers]). MSG `fallbacks` (≤3, `model` only) is OR routing, not Anthropic's server-side fallback ([fallback]).
* **Added:** OR moderation on `is_moderated` Claude endpoints ([api-models]).
* **Degraded:** on Vertex/Bedrock (the "Google" and "AWS Bedrock" providers) streams are not cancellable ([stream]). Endpoints differ in features: Bedrock endpoints don't list `structured_outputs` ([api-ep]).

**Google Gemini** (`google-ai-studio`, `google-vertex/*`, incl. `/flex` and `/priority` tiers)
* **Renamed:** thought signatures live in `reasoning_details` (`reasoning.encrypted`, `google-gemini-v1`) and must be echoed. Putting `thought_signature` on tool calls is a 400 ([zed], [roo]).
* **Transformed:** effort → `thinkingLevel` (`xhigh`→`high`; `minimal` passes through; unsupported → nearest). `reasoning.max_tokens` → `thinkingBudget`, which Google maps to a level on Gemini 3, so control is imprecise ([reason]). On Gemini 4, `reasoning.max_tokens` doesn't set a budget ([g4]).
* **Transformed (Gemini 4):** default `max_tokens` is 65 536. Continuation items from earlier turns are removed and the trailing continuation turn is dropped before forwarding; `cache_control` is skipped on continuation hops ([g4]).
* **Degraded:** video URLs work only as YouTube links on AI Studio; Vertex takes no URLs ([video]). Native search accepts no domain filters (`auto` silently switches to Exa; `native` → 400) ([ws]). Grounding metadata beyond `url_citation` is presumably lost (**UNVERIFIED**) ([ws-plugin]).
* **Degraded caching:** only the **last** `cache_control` breakpoint is used, and the first `system` message is immutable for caching ([cache]).
* **Degraded:** Google and AI Studio streams are **not cancellable**: an abort keeps generating and billing ([stream]).
* **Renamed:** safety blocks become a 403 `content_policy_violation` ([err]).
* **Missing:** Gemini-native features absent from OR docs: code execution, URL context, Maps grounding, `safetySettings`, `media_resolution`, the `cachedContents` API (all **UNVERIFIED**). OR does expose video `processing` (agentic/static) ([video]).

**xAI Grok** (docs brand: "SpaceXAI"; `xai`, `xai/zdr`, `xai/priority`; also `amazon-bedrock/global` for grok-4.7)
* **Transformed:** X search is **opt-in** (`x_search`; legacy `x_search_filter`) since per-item billing began on 2026-09-21. Native `web_search` only otherwise ([ws]).
* Reasoning comes back encrypted (`xai-responses-v1`, or `bedrock-xai-responses-v1` on Bedrock). Whether plaintext reasoning is available is **UNVERIFIED** ([reason]).
* **Degraded:** domain filters are mutually exclusive, ≤5 each in the plugin form ([ws-plugin]). Efforts are `xhigh…low`: `max` is mapped down, and `none` is a 400 because reasoning is `mandatory` ([latest]). Model `default_parameters` say temperature 0.7, top_p 0.95; whether OR applies them is **UNVERIFIED** ([api-models]).
* **Degraded — routing:** may land on Bedrock (not cancellable) ([api-ep], [stream]).

**DeepSeek** (`deepseek`, plus many hosts)
* **Degraded — availability:** on 2026-11-11 the canonical slugs `deepseek/deepseek-v4-pro`, `-v4-flash` and `-v3.2` had **no `deepseek` endpoint**. They are served by third-party hosts, mostly `fp8`/`fp4` (Relace, StreamLake, GMICloud, SiliconFlow, DeepInfra, Alibaba, Novita, Baidu, Azure, …). The first-party endpoint appears only on dated snapshots such as `deepseek/deepseek-v4.1-flash-1017` and `deepseek/deepseek-v4-pro-0813` ([api-ep]).
* **Degraded:** per-host parameter support varies (e.g. `logprobs` only on some hosts) ([api-ep]). Efforts are `xhigh, high, none`, so others are mapped to the nearest ([api-models], [latest]).
* **Transformed (possibly):** reasoning round-trip in thinking + tool-call loops relies on OR mapping `reasoning_details` to DeepSeek's `reasoning_content` (**UNVERIFIED**).
* **Missing:** DeepSeek-native beta features (FIM, chat prefix completion `prefix:true`) are not in OR docs (**UNVERIFIED**).
* Cancellation is supported for the `deepseek` provider only ([stream]).

**Zhipu GLM (Z.AI)** (`z-ai/fp8`, plus about 46 hosts for glm-5.3)
* **Degraded:** default routing favors third-party fp4/fp8 hosts. The first-party endpoint's `supported_parameters` omit `stop`, `logprobs`, `seed` and the penalties ([api-ep]).
* **Transformed:** OR **adds a session-affinity key** to Z.AI requests ([cache]).
* **Missing:** Z.AI-native controls (preserved/`clear_thinking` thinking, `tool_stream`, the built-in web search tool) are not documented on OR (**UNVERIFIED**).
* Efforts are `max, high, low`, and reasoning is `mandatory` (`none` → 400) ([api-models]). Cancellation support is unlisted (**UNVERIFIED**) ([stream]).

**Moonshot Kimi** (`moonshotai`; kimi-k3 has 28 endpoints, mostly `fp4`/`mxfp4`, with first-party listed 24th; the list order is not necessarily routing priority)
* **Degraded:** without pinning, you almost certainly test a quantized third-party deployment; tool-call quality varies, which is why Exacto exists ([api-ep], [exacto]).
* **Missing:** Kimi-native partial mode, the built-in `$web_search`, and the `reasoning_content` echo mapping are **UNVERIFIED** on OR. Cancellation support is unlisted (**UNVERIFIED**).
* Caching is automatic; Groq-hosted Kimi K2 also caches ([cache]).

**Alibaba Qwen** (`alibaba`, `alibaba/us-east-1`; commercial Qwen models are often first-party only)
* **Degraded:** prompt caching is explicit `cache_control` only, on a short model list (not snapshots) ([cache]). `reasoning.max_tokens` → `thinking_budget` only on some models. qwen3.7-max exposes no effort list ([reason], [api-models]).
* **Degraded:** Alibaba streams are **not cancellable** ([stream]). Native `enable_search` is not an OR native web-search provider, so search goes to Exa ([ws]). The `enable_thinking` mapping from `reasoning.enabled` is **UNVERIFIED**.

### 11.2 OpenRouter behaviours that violate the "clean request" principle

| # | Behaviour | Default? | How to disable / detect |
|---|---|---|---|
| 1 | Default routing picks the provider per request (weighted random) and may choose open-weight **priority** endpoints at a higher price ([route], [tiers]) | yes | `provider.only:[slug]` + `allow_fallbacks:false`; `service_tier:"default"` |
| 2 | Provider fallback, including **silent retry to another provider after HTTP 200 if no token was sent yet** ([err]) | yes (`allow_fallbacks:true`) | `allow_fallbacks:false`; check `openrouter_metadata.attempts` / `/generation` `provider_responses` |
| 3 | Auto Exacto reorders providers on every request with tools ([exacto]) | yes | Pin the provider, or `provider.sort` |
| 4 | Unsupported params silently ignored ([ovw], [route]) | yes | `provider.require_parameters:true` |
| 5 | Effort mapped to the nearest supported level; `~…-latest` upgrades `none`/`enabled:false` ([latest]) | yes | Validate against `reasoning.supported_efforts`/`mandatory`; never use `~` aliases |
| 6 | Context compression (middle-out) on endpoints with ≤8k context ([mt]) | yes | `plugins:[{"id":"context-compression","enabled":false}]` |
| 7 | Account / org **default plugins** (Settings > Plugins; "Prevent overrides" blocks per-request disabling) ([plugins]) | config | Keep the account defaults empty; send `{"id":…,"enabled":false}` for `web`, `file-parser`, `response-healing`, `context-compression`; inspect `openrouter_metadata.pipeline` |
| 8 | Workspace / account routing defaults: default provider sort, allowed/ignored providers, default service tier, allowed tiers, ZDR, data policy ([route], [tiers]) | config | Use a dedicated workspace and key with neutral settings |
| 9 | Guardrails (budgets, model allowlists, moderation, prompt-injection regex, sensitive-info redaction) block or modify requests ([err]) | config | No guardrails on the bot's key; check `pipeline[]` with `type:"guardrail"` |
| 10 | OR-side moderation on `is_moderated` endpoints (OpenAI, Anthropic, …) ([err], [api-models]) | yes | No documented opt-out; surface the 403 `reasons`/`flagged_input` verbatim |
| 11 | PDF parsing fallback to paid Mistral OCR when the model lacks native file support ([pdf]) | yes | §4.3 (gate on `input_modalities`, `engine:"native"`, detect `file` annotations) |
| 12 | `name` prepended as `{name}: ` for non-OpenAI models ([ovw]) | when `name` is set | Don't send `name` |
| 13 | Tools rendered as a **YAML prompt template** for providers without native tool calling ([ovw]) | when such a provider is chosen | Send `require_parameters:true`, or filter on `supported_parameters` containing `tools` |
| 14 | System-message concatenation / user-message merging for some providers ([err] debug section) | provider-dependent | Inspect `debug.echo_upstream_body` |
| 15 | Default `max_tokens` injection (65 536 seen for Claude and Gemini 4) ([err], [g4]) | yes | Always send `max_tokens` / `max_output_tokens` |
| 16 | Claude: `thinking.display:"summarized"`, an added `thinking` object (Sonnet 5.5), `eager_input_streaming`, `strict` stripping, auto beta headers, budgets ignored, `verbosity`→`effort` ([reason], [route], [c55]) | yes | Set `thinking.display` explicitly (MSG); send the structured-outputs beta header; don't send budgets or `verbosity` unless intended. The rest has no opt-out |
| 17 | OpenAI: encrypted reasoning auto-included; fields stripped on fallback; `async` stripped on CC; native `web_search` hoisted to the OR server tool ([reason], [g56], [g6], [online]) | yes | Use RSP for OpenAI; pin `openai`; check `native_tools` before offering search |
| 18 | Hashed end-user identity always sent upstream; Z.AI session-affinity key ([usertrack], [cache]) | yes | n/a (benign); send a stable `user` |
| 19 | Web plugin / `:online` injects results with a **search prompt** ([ws-plugin]) | only if used | Use the `openrouter:web_search` server tool, or none |
| 20 | Server tools with inner model calls (Fusion, Advisor, Subagent, image generation default `openai/gpt-image-2.6-flare`) ([srv], [imgtool]) | only if used | Don't enable them |
| 21 | Error and finish rewriting: token-limit errors → `length`/`incomplete`; `finish_reason` normalized; 500 messages masked ([err], [stream]) | yes | Show `native_finish_reason`, `error_type`, `provider_code`, `metadata.raw` |
| 22 | `detail:"original"` downgraded to `high`; Mistral OCR caps extracted images at 8 ([oas], [pdf]) | yes | — |
| 23 | Presets (`@preset/…`, `preset` field) can add system prompts, params, tools and caching ([presets]) | only if referenced | Never reference presets |
| 24 | Response cache returns verbatim old outputs regardless of `temperature` ([respcache]) | **off** | Don't send `X-OpenRouter-Cache` |
| 25 | Sticky routing can keep a conversation on one provider for 60 min ([cache]) | yes | Fine for transparency, as long as the provider is shown; pinning makes it moot |

**Audit loop for the bot:** when the user enables "/debug", send `debug.echo_upstream_body:true` (CC/RSP stream) plus `X-OpenRouter-Metadata: enabled`. Show the exact upstream JSON, `openrouter_metadata.pipeline`, `attempts`, the final `provider`, `model`, `service_tier` and `usage.cost`.

---

## Sources

[ovw]: https://openrouter.ai/docs/api_reference/overview
[params]: https://openrouter.ai/docs/api_reference/parameters
[stream]: https://openrouter.ai/docs/api_reference/streaming
[err]: https://openrouter.ai/docs/api_reference/errors-and-debugging
[lim]: https://openrouter.ai/docs/api_reference/limits
[auth]: https://openrouter.ai/docs/api_reference/authentication
[rsp]: https://openrouter.ai/docs/api_reference/responses/overview
[rsp-basic]: https://openrouter.ai/docs/api_reference/responses/basic-usage
[rsp-reason]: https://openrouter.ai/docs/api_reference/responses/reasoning
[rsp-err]: https://openrouter.ai/docs/api_reference/responses/error-handling
[rsp-ws]: https://openrouter.ai/docs/api_reference/responses/web-search
[changelog]: https://openrouter.ai/docs/changelog
[reason]: https://openrouter.ai/docs/guides/best-practices/reasoning-tokens
[cache]: https://openrouter.ai/docs/guides/best-practices/prompt-caching
[plugins]: https://openrouter.ai/docs/guides/features/plugins
[mt]: https://openrouter.ai/docs/guides/features/message-transforms
[heal]: https://openrouter.ai/docs/guides/features/plugins/response-healing
[ws-plugin]: https://openrouter.ai/docs/guides/features/plugins/web-search
[srv]: https://openrouter.ai/docs/guides/features/server-tools
[ws]: https://openrouter.ai/docs/guides/features/server-tools/web-search
[tool-search]: https://openrouter.ai/docs/guides/features/server-tools/tool-search
[imgtool]: https://openrouter.ai/docs/guides/features/server-tools/image-generation
[mm]: https://openrouter.ai/docs/guides/overview/multimodal/overview
[img]: https://openrouter.ai/docs/guides/overview/multimodal/image-understanding
[pdf]: https://openrouter.ai/docs/guides/overview/multimodal/pdfs
[audio]: https://openrouter.ai/docs/guides/overview/multimodal/audio
[video]: https://openrouter.ai/docs/guides/overview/multimodal/videos
[imggen]: https://openrouter.ai/docs/guides/overview/multimodal/image-generation
[route]: https://openrouter.ai/docs/guides/routing/provider-selection
[fallback]: https://openrouter.ai/docs/guides/routing/model-fallbacks
[variants]: https://openrouter.ai/docs/guides/routing/model-variants/overview
[online]: https://openrouter.ai/docs/guides/routing/model-variants/online
[nitro]: https://openrouter.ai/docs/guides/routing/model-variants/nitro
[floor]: https://openrouter.ai/docs/guides/routing/model-variants/floor
[thinking]: https://openrouter.ai/docs/guides/routing/model-variants/thinking
[exacto]: https://openrouter.ai/docs/guides/routing/auto-exacto
[tiers]: https://openrouter.ai/docs/guides/features/service-tiers
[latest]: https://openrouter.ai/docs/guides/features/latest-resolution
[rmeta]: https://openrouter.ai/docs/guides/routing/routers/router-metadata
[models-guide]: https://openrouter.ai/docs/guides/overview/models
[usage]: https://openrouter.ai/docs/cookbook/administration/usage-accounting
[usertrack]: https://openrouter.ai/docs/cookbook/administration/user-tracking
[tools]: https://openrouter.ai/docs/guides/features/tool-calling
[respcache]: https://openrouter.ai/docs/guides/features/response-caching
[zci]: https://openrouter.ai/docs/guides/features/zero-completion-insurance
[presets]: https://openrouter.ai/docs/guides/features/presets
[logs]: https://openrouter.ai/docs/guides/features/logs
[iolog]: https://openrouter.ai/docs/guides/features/input-output-logging
[broadcast]: https://openrouter.ai/docs/guides/features/broadcast
[batch]: https://openrouter.ai/docs/guides/features/batch-api
[files]: https://openrouter.ai/docs/guides/features/files-api
[attr]: https://openrouter.ai/docs/app-attribution
[prov]: https://openrouter.ai/docs/guides/providers/integration
[c55]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/claude-5-5
[f51]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/fable-5-1
[o5]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/opus-5
[s5]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/sonnet-5
[c47]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/claude-4-7
[c46]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/claude-4-6
[g4]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/gemini-4
[g6]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/gpt-6
[g56]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/gpt-5-6
[g54]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/gpt-5-4
[oas]: https://openrouter.ai/openapi.json
[gen-api]: https://openrouter.ai/docs/api/api-reference/generations/get-request-&-usage-metadata-for-a-generation
[gen-content-api]: https://openrouter.ai/docs/api/api-reference/generations/get-stored-prompt-completion-and-error-content-for-a-generation
[msgs-api]: https://openrouter.ai/docs/api/api-reference/anthropic-messages/create-a-message
[api-models]: https://openrouter.ai/api/v1/models
[api-ep]: https://openrouter.ai/api/v1/models/moonshotai/kimi-k3/endpoints
[api-prov]: https://openrouter.ai/api/v1/providers
[zed]: https://github.com/zed-industries/zed/blob/v0.214.7/crates/language_models/THOUGHT_SIGNATURES.md
[roo]: https://github.com/RooCodeInc/Roo-Code/issues/11629
[go-or]: https://github.com/revrost/go-openrouter/blob/main/chat.go
[ocaml]: https://ocaml.org/p/openrouter_api/0.1.0/openrouter_api/Openrouter_api/Generation/index.html
[subagent-cb]: https://openrouter.ai/docs/cookbook/building-agents/subagent-server-tool

Notes on sources:
* [api-models], [api-ep] and [api-prov] are live, unauthenticated catalog calls made on 2026-11-11. Besides the [api-ep] URL above, `/endpoints` was also fetched for `deepseek/deepseek-v4-pro`, `-v4-flash`, `-v3.2`, `-v4.1-flash(-1017)`, `-v4-pro-0813`, `z-ai/glm-5.3`, `qwen/qwen3.7-max`, `anthropic/claude-opus-5.5`, `google/gemini-4-argon`, `openai/gpt-6.1-sol` and `x-ai/grok-4.7`. Endpoint lists change daily.
* [zed], [roo], [go-or] and [ocaml] are third-party evidence, not OpenRouter docs.
