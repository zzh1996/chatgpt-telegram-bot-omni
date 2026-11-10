# xAI (Grok) & DeepSeek official APIs — and how OpenRouter maps them

Research date: **2026-11-11**. Everything below comes from live docs and JSON fetched on that date. Source keys like `[XRS]` resolve to URLs in §S at the end. **UNVERIFIED** marks claims the docs don't settle; check those with a live call (for OpenRouter, use `debug.echo_upstream_body`, §C6).

Naming: xAI's docs now say "SpaceXSI (xAI)" [XLLM]. OpenRouter labels the provider "SpaceXAI" [ORM]. The API host is still `api.x.ai`.

## 0. TL;DR for the Rust bot

| Topic | xAI direct | DeepSeek direct | Via OpenRouter |
|---|---|---|---|
| Best surface | **Responses API** (`POST /v1/responses`). Chat Completions is deprecated [XCMP][XBP] | Chat Completions is canonical (beta prefix/FIM, logprobs). Responses and Anthropic add server `web_search` (v4-pro; §B5) and mid-conversation tool insertion [DRSP][DTC] | Chat Completions or Responses. Both are **stateless** [ORRA] |
| Stateless replay | `store:false` + `include:["reasoning.encrypted_content"]`, then send back **all output items verbatim** [XBP] | Store `reasoning_content`. It **must** be sent back when `tools` are present, otherwise **400** [DTM] | Send back `reasoning_details` (or `reasoning`) unchanged [ORRT] |
| Reasoning knob | `reasoning.effort`: low/medium/high/xhigh (4.6/4.7). Cannot be disabled except on 4.3 (`none`) [XRS][XMP] | `thinking.type` + `reasoning_effort` none/low/high/**max** [DCC] | `reasoning.effort` (max…none), `reasoning.enabled` [ORRT] |
| Built-in tools | web_search, x_search, code_interpreter, image_generation, file_search/collections, attachment_search, remote MCP [XTO][XOAS] | Server `web_search` in Responses (**v4-pro only**) and Anthropic (v4-pro documented; flash **UNVERIFIED**). Standalone Search/Fetch REST APIs [DRSP][DMSG][DSRCH] | Only `openrouter:web_search` runs natively on xAI (with an `x_search` opt-in). Nothing native for DeepSeek [ORE][ORWS] |
| Recovery after disconnect | Deferred chat completions (poll, fetchable **once**, 24 h). Stored responses (30 d) [XDEF][XGT] | None. Stateless, no background, no batch [DRSP] | None. A dropped stream **cancels** xAI/DeepSeek generation [ORS] |
| Raw-model testing caveat | Pin provider `xai` (Amazon Bedrock also serves 4.6/4.7) [ORE] | **25–51 third-party hosts**, many fp8/fp4. The official endpoint exists only for `deepseek-v4-pro-0813` and `deepseek-v4.1-flash-1017` [ORE] | `provider:{only:[…],allow_fallbacks:false}` [ORPS] |

---

# Part A — xAI direct API

## A1. API surfaces

| Surface | Endpoint | Status / notes |
|---|---|---|
| **Responses API** | `POST https://api.x.ai/v1/responses`. Also `GET`/`DELETE /v1/responses/{id}`, `GET …/{id}/input_items`, `POST /v1/responses/compact` | Recommended. All new features ship here first. Native server-side tools, encrypted reasoning, `previous_response_id`, 30-day storage [XCMP][XOAS][XCOMP] |
| Chat Completions | `POST /v1/chat/completions` | **Deprecated / legacy**. Function tools only. No field for encrypted reasoning, so multi-turn runs drop the earlier thinking [XCMP][XBP][XOAS]. Multi-agent model **not** supported [XMA] |
| Deferred chat completions | `deferred:true` → `GET /v1/chat/deferred-completion/{request_id}` | REST or xAI SDK [XDEF] |
| Anthropic-compatible | `POST /v1/messages`, `/v1/complete` | "The Anthropic SDK compatibility is **fully deprecated**" [XLEG] |
| Legacy text completions | `POST /v1/completions` | Legacy; not supported by reasoning models [XOAS] |
| gRPC (xAI Python SDK `xai_sdk`) | gRPC API | "covers all of our features". `use_encrypted_content`, `include=["verbose_streaming"]`, `x-grok-conv-id` metadata [XGT][XSS][XPCH]. `code_interpreter`/`file_search` *names* don't work in gRPC (use `code_execution`/`collections_search`) [XPR] |
| WebSocket Responses | `wss://api.x.ai/v1/responses`, `{"type":"response.create",…}` | Same event stream. Per-socket in-memory `previous_response_id` (works with ZDR/`store:false`). Max 59 min per socket [XWSM] |
| Batch | `/v1/batches` (+ JSONL upload) | 20 % off text models. "most completing within 24 hours" [XPR][XBATCH] |
| Images / Videos | `/v1/images/generations`, `/v1/images/edits`, `GET /v1/images/{id}`. `/v1/videos/{generations,edits,extensions}`, `GET /v1/videos/{id}` | §A6 [XIMG][XVID] |
| Files / Collections | `/v1/files…` (+ `public-url`), `/v1/documents/search`, Collections API | §A6 [XOAS][XFILES] |
| Regional | `https://us.api.x.ai/v1` | US-only inference, **1.1×** price, currently grok-4.7 and grok-4.6 only [XPR][XREG] |

**Most capable surface: Responses** (or gRPC, which offers the same features). Chat Completions "understate[s] what Grok reasoning models can do, especially across multiple turns and tool calls" [XBP].

## A2. Models (text)

| Model id (aliases) | Ctx | $/1M in · cached · out (<200k prompt; ≥200k doubles) | Reasoning | Notes |
|---|---|---|---|---|
| `grok-4.7` | 500k | 2.00 · 0.50 · 6.00 | `low`/`medium`/`high` (default)/`xhigh`; **cannot be disabled** | Knowledge cutoff May 2026. "No text output limit". Always returns `reasoning.encrypted_content` on Responses [XM47][XRS] |
| `grok-4.6` | 500k | 2.00 · 0.50 · 6.00 | same as 4.7 | [XPR][XMP] |
| `grok-4.5` (`grok-4.5-latest`, `grok-build-latest`) | 500k | 2.00 · 0.30 · 6.00 | low/medium/high (default). `xhigh` is treated as `high` [XRS] (the model page lists xhigh [XMP]) | |
| `grok-4.3` (`-latest`) | 1M | 1.25 · 0.20 · 2.50 | `none`/low/medium/high/xhigh, **default `low`** | Target of the 2026-05-15 retirements [XMP][XRET] |
| `grok-4.20-0309-reasoning` (aliases `grok-4.20`, `grok-4.20-reasoning`, …) | 1M | 1.25 · 0.20 · 2.50 | yes (no effort list) | [XMP] |
| `grok-4.20-0309-non-reasoning` | 1M | same | **no** | separate slug [XMP] |
| `grok-4.20-multi-agent-0309` (`grok-4.20-multi-agent`) | 1M | same | `reasoning.effort` sets **agent count**: low/medium → 4, high/xhigh → 16 | No client/custom tools, no Chat Completions, no `max_tokens` [XMA] |
| `grok-build-0.1` (aliases `grok-code-fast-1`, …) | 256k | 1.00 · 0.20 · 2.00 | yes | No batch [XMP] |

* Retired on 2026-05-15 (grok-4-1-fast-*, grok-4-fast-*, grok-4-0709, grok-3). They redirect to `grok-4.3` with effort `low` (reasoning models) or `none` (non-reasoning models), billed at 4.3 prices [XRET].
* Long-context pricing: once the prompt reaches 200k tokens, **all** tokens in the request bill at the higher rate [XPR].
* Rate limits come in tiers by cumulative spend (T0…T4). Example: grok-4.7 T0 = 150 RPS / 50M TPM. Over the limit → `429` [XRL].

## A3. Request parameters (Responses ↔ Chat Completions)

| Responses field | CC field | Semantics / restrictions |
|---|---|---|
| `input` (string or items), `instructions` | `messages` | `instructions` "cannot be used alongside `previous_response_id`" [XOAS]. No role-order limitation [XM] |
| `reasoning.effort` (also non-standard `reasoning_effort`) | `reasoning_effort` | Values and defaults are per model (§A2). `reasoning.summary` and `generate_summary` exist "only for compatibility" and are always `detailed` [XOAS][XRESP] |
| `max_output_tokens` | `max_completion_tokens` (`max_tokens` deprecated) | **Visible output only.** Reasoning and function-call tokens don't count. Default **128,000** [XOAS][XBP] |
| `temperature` 0–2 | same | Values > 1 are sampled at 1. Responses echoes the value you sent [XOAS][XBP] |
| `top_p` (0,1] | same | Values above the model default (0.95) are sampled at 0.95 [XOAS]. xAI recommends temperature 0.7, top_p 0.95 [XBP] |
| `top_k`, `min_p` | — | Responses only [XOAS] |
| — | `stop` (≤4), `presence_penalty`, `frequency_penalty` | "cannot be used with reasoning models. Requests that include them **return an error**" [XRS]. `logit_bias` unsupported [XOAS] |
| `logprobs`, `top_logprobs` (0–8) | same | **Silently ignored for grok-4.20 and newer** [XM][XOAS] |
| `text.format` | `response_format` | `text` / `json_object` / `json_schema`. Tool arguments are always strict (`strict` is implicitly true) [XSO][XOAS] |
| — | `seed`, `n` | CC only [XOAS] |
| `tools`, `tool_choice`, `parallel_tool_calls`, `max_turns` | `tools` (functions only), `tool_choice`, `parallel_tool_calls` | Max 1000 tools. `max_turns` caps agentic loop turns (default is the server cap) [XOAS][XTUD] |
| `include` | — | `reasoning.encrypted_content`, `no_inline_citations`, `web_search_call.action.sources`, `code_interpreter_call.outputs`, `file_search_call.results` [XGT][XCIT][XSS] |
| `store` (default **true**), `previous_response_id` | — | §A4 |
| `background` | — | **"(Unsupported)"** [XOAS] |
| `prompt_cache_key` | `prompt_cache_key` / header `x-grok-conv-id` | Sticky routing for cache hits; "highly recommend" [XM47][XOAS] |
| `service_tier` | same | `default`/`priority`/`fast` (interchangeable). 2× price, billed only if the response reports `priority` [XPR][XOAS] |
| `safety_identifier`, `user` | same | End-user id for policy attribution. `user` is legacy [XRN] |
| `guidelines` | same | `[{"type":"policy","category":"nsfw\|violence\|profanity","behavior":"decline"}]` (`cyber` accepts `allow`). Fails with 400 on multi-agent models or with `stop` [XMOD][XOAS] |
| `search_parameters` | `search_parameters`, `web_search_options` | **Live Search removed.** Responses → `410 Gone`. CC → "requests that set this field fail" [XOAS] |
| `metadata`, `truncation` | — | compat only, not supported [XOAS] |

Stateless-replay request (the pattern xAI recommends) [XBP][XGT]:
```json
POST /v1/responses
{"model":"grok-4.7","store":false,"stream":true,"include":["reasoning.encrypted_content"],
 "reasoning":{"effort":"high"},"temperature":0.7,"top_p":0.95,"prompt_cache_key":"<chat-uuid>",
 "tools":[{"type":"web_search","allowed_domains":["arxiv.org"]},{"type":"x_search","from_date":"2026-11-01"},{"type":"code_interpreter"}],
 "input":[{"role":"user","content":[{"type":"input_text","text":"…"},
          {"type":"input_image","image_url":"data:image/png;base64,…","detail":"high"}]}]}
```

## A4. Multi-turn state rules

* **Server state:** responses are stored for **30 days** by default. Continue with `previous_response_id` [XGT]. Retrieve or delete with `GET`/`DELETE /v1/responses/{id}` [XOAS].
* **Local state (what the bot should do):** set `store:false` and `include:["reasoning.encrypted_content"]`. Append **every** output item (reasoning items with `encrypted_content`, messages, `function_call`s, and server-tool call items) unchanged to the next `input`, then your `function_call_output`s [XBP][XGT]. A reasoning item looks like `{"type":"reasoning","id":"rs_…","summary":[],"status":"completed","encrypted_content":"…"}` [XGT].
* `grok-4.7` returns `encrypted_content` **even without `include`**. "Your code still has to send it back." This default doesn't stop xAI from storing the trace for rehydration; only `store` controls `previous_response_id` storage [XRS][XBP].
* Leaving out the encrypted reasoning doesn't raise an error. The model loses its earlier thinking, and it is "the most common cause of cache misses" [XBP]. No error is documented for replaying it to a different model (**UNVERIFIED**).
* Chat Completions has **no field for the ciphertext** [XRS][XBP].
* Image inputs combined with server-side history storage: "the request may fail" (store locally instead) [XIU].
* Multi-agent: sub-agent state is returned encrypted only with `use_encrypted_content` (xAI SDK) [XMA].
* WebSocket mode: an unknown or evicted `previous_response_id` → `400 {"error":{"code":"previous_response_not_found",…}}`. A failed turn evicts its id [XWSM].

## A5. Server-side tools, citations, pricing

| Tool (Responses `type`) | Key params | Price [XPR] |
|---|---|---|
| `web_search` | `allowed_domains` **or** `excluded_domains` (≤5 each); `enable_image_understanding` (adds `view_image`); `enable_image_search` (Markdown `![alt](url)` embeds). `search_context_size`, `user_location`, `external_web_access` are **rejected** [XWS][XOAS] | $5 / 1k calls (image search included) |
| `x_search` | `allowed_x_handles` **or** `excluded_x_handles` (≤20), `from_date`/`to_date` (YYYY-MM-DD), `enable_image_understanding`, `enable_video_understanding` [XXS] | **$5 / 1k posts + $10 / 1k profiles fetched** (since 2026-09-21) [XTUD] |
| `code_interpreter` (gRPC: `code_execution`) | sandboxed Python. `container` is rejected [XOAS][XCE] | $5 / 1k |
| `image_generation` | `action`: `auto`/`generate`/`edit`. Output item `image_generation_call` carries `result` (base64) and `prompt`. Item ids `ig_`/`ie_`. No partial previews [XTIG] | Imagine rates |
| `file_search` (collections) | `vector_store_ids`, `max_num_results` [XOAS] | $2.50 / 1k |
| `attachment_search` | added **implicitly** when `input_file` is attached [XFILES] | $5 / 1k |
| `mcp` | `server_url` (Streamable HTTP/SSE), `server_label`, `allowed_tools`, `authorization`, `headers`, `defer_loading`. `require_approval` and `connector_id` unsupported [XRMCP][XOAS] | tokens only |
| `view_image` / `view_x_video` | enabled via the search flags | tokens only |
| `tool_search`, `shell` | in OpenAPI `ModelTool` [XOAS] | **UNVERIFIED** |

* Built-in and function tools can be mixed. Execution pauses at client tool calls [XAU].
* Server-side tool **outputs aren't returned by default**, only the invocations. Opt in with the `include` values in §A3 [XTUD][XSS].
* Output item types: `web_search_call`, `custom_tool_call` (X search when `call_id` starts `xs_`, attachment search `as_`), `code_interpreter_call`, `file_search_call`, `mcp_call`, `function_call` [XTUD].
* Only successful calls are billed. Usage reports `usage.server_side_tool_usage_details{web_search_calls, x_search_calls, x_posts_fetched, x_users_fetched, code_interpreter_calls, file_search_calls, mcp_calls, document_search_calls, image_generation_calls}` [XOAS][XTUD].
* Citations [XCIT]:
  * `citations`: an array of **all** source URLs encountered, returned by default and populated on the last CC chunk [XOAS].
  * **Inline citations** are on by default in Responses as `[[N]](url)`; disable with `include:["no_inline_citations"]` (the gRPC SDK is opt-in via `inline_citations`).
  * Structured inline data sits in `output_text.annotations[]`: `{"type":"url_citation","url":…,"start_index":…,"end_index":…,"title":"1"}`.
* The Chat Completions `tools` field is "Currently, only functions" [XOAS]. Use Responses for server tools.

## A6. Multimodal

| Capability | Details |
|---|---|
| Image **input** | Responses: `{"type":"input_image","image_url":"https://…\|data:…","detail":"high"}`. CC: `image_url` parts. **≤20 MiB, jpg/jpeg and png only**, no count limit, any order [XIU][XM] |
| Files / PDF | `{"type":"input_file","file_url":…}` or `{"file_id":…}`. Triggers agentic `attachment_search`. txt/md/code/csv/json/pdf. Needs an agentic model (4.20+). Max **512 MB** per the guide (the OpenAPI upload text says 50 MB, a conflict). No `n>1` [XFILES][XCWF][XMF][XOAS] |
| Image **generation** | `POST /v1/images/generations` `{model:"grok-imagine-image-2.0", prompt, n:1–10, aspect_ratio: 1:1…21:9,5:2,auto, resolution:"1k"\|"2k", quality:"low"\|"medium"\|"auto" (2.0 only; default auto), response_format:"url"\|"b64_json", deferred, storage_options, output.upload_urls, user}` → `data[]{url\|b64_json, mime_type}`, `usage.cost_in_usd_ticks`. URLs are temporary [XIG][XIMG] |
| Image **editing** | `POST /v1/images/edits` with `image` or `images[]` (≤5) (`url` or `file_id`) [XIMG][XRN] |
| Video | `POST /v1/videos/generations` `{model, prompt, duration 1–15 (default 8), resolution 480p\|720p\|1080p, aspect_ratio, image (I2V), reference_images, reference_audios, keyframes}` → `{request_id}`, poll `GET /v1/videos/{request_id}`. Plus `/edits`, `/extensions` [XVID] |
| Pricing | grok-imagine-image $0.02/img. grok-imagine-image-2.0 $0.04/img (low 1k; quality/resolution variants cost more). video-1.5-lite $0.02/s, video $0.05/s, video-1.5 $0.08/s [XPR][ORIM] |
| Image output inside chat | CC delta has an `images[]` field [XOAS]. The Responses `image_generation` tool returns base64 [XTIG] |

## A7. Streaming format

* CC: SSE `data:` chunks ending with `data: [DONE]`. `delta.reasoning_content` deltas come before `delta.content`. The documented example carries a `usage` object on **every chunk**. `stream_options.include_usage` adds a final usage chunk. `citations` appear on the last chunk [XST][XOAS].
  ```
  data: {"object":"chat.completion.chunk","model":"grok-4.7","choices":[{"index":0,"delta":{"reasoning_content":"The","role":"assistant"}}],"usage":{"prompt_tokens":41,"completion_tokens":1,…}}
  ```
* Contradiction: [XCMP] says Chat Completions returns "No reasoning content", while [XST][XDEF][XOAS] show or describe `reasoning_content`. For grok-4.7, xAI exposes **summarized** reasoning [XRS]. Treat the presence and semantics of CC `reasoning_content` as **UNVERIFIED** per model.
* Responses: OpenAI-style semantic events (`response.output_text.delta`, `response.reasoning_text.delta` / `response.reasoning_summary_text.delta`, `response.function_call_arguments.done`, `response.output_item.added/done`, `response.completed` / `response.incomplete`) [XRS][XBP][XTIG]. The OpenAPI text claims a `data: [DONE]` terminator (likely boilerplate; handle both) [XOAS].
* CC `finish_reason` is documented as `stop`, `length`, or `end_turn`/`null` [XOAS]. A `tool_calls` value is **UNVERIFIED**. Responses `status` is `completed`/`in_progress`/`incomplete`, and `incomplete` means truncated [XRESP][XBP].
* No keep-alive comments are documented (**UNVERIFIED**). xAI says to set client timeouts **≥900 s** (≥1800 s for long evals) and prefer streaming [XBP].

## A8. Usage, billing, caching

* CC `usage`: `prompt_tokens`, `completion_tokens`, `total_tokens`, `prompt_tokens_details{text_tokens,audio_tokens,image_tokens,cached_tokens}`, `completion_tokens_details{reasoning_tokens,audio_tokens,accepted_prediction_tokens,rejected_prediction_tokens}`, `num_sources_used` (legacy Live Search), `server_side_tool_usage_details`, **`cost_in_usd_ticks`** [XOAS].
* Responses `usage`: `input_tokens`, `input_tokens_details.cached_tokens`, `output_tokens`, `output_tokens_details.reasoning_tokens`, `num_server_side_tools_used`, `num_sources_used`, `context_details`, `cost_in_usd_ticks`, `cost_in_nano_usd` [XRESP].
* `cost_in_usd_ticks` is the exact billed cost after discounts, including tool costs. **1 USD = 10^10 ticks** [XCOST].
* Caching is automatic and matches the prefix from the start of input. Cached tokens bill at the cached rate and still count toward TPM. Hits are not guaranteed. Use `prompt_cache_key` / `x-grok-conv-id` [XPCH][XPC][XRL].
* Reasoning tokens bill at the output rate [XPC]. Usage-policy violations caught before generation on Responses cost **$0.05** [XPR].

## A9. Errors

| HTTP | Meaning [XDBG][XOAS] | Retry? |
|---|---|---|
| 400 | bad request (also an invalid key, per OpenAPI), `guidelines` not applicable, WebSocket `previous_response_not_found` | no |
| 401 / 403 / 404 / 405 / 415 | auth / permission / not found / method / media type | no |
| 410 | `search_parameters` on Responses (Live Search removed) | no |
| 422 | invalid field format / missing fields | no |
| 429 | rate limit (RPS/TPM, per model) | **yes**, exponential backoff [XRL] |
| 5xx | not enumerated. "Retry `429` and `5xx` … with exponential backoff" [XBP] | **yes** |
| 202 | deferred result not ready | poll |

The error body schema for text endpoints is **not documented** (**UNVERIFIED**), so show it verbatim. Image/video async errors use `{code: invalid_argument|permission_denied|failed_precondition|service_unavailable|internal_error, message}` [XOAS].

## A10. Async / resume

* **Deferred CC:** `{"deferred":true,…}` → `{"request_id":"…"}`. `GET /v1/chat/deferred-completion/{id}` returns `202` (empty body) until ready, then `200` with a normal `chat.completion` (incl. `reasoning_content`). The result "would be available to be requested **exactly once within 24 hours**". No streaming. Same rate limit as CC [XDEF]. Survives bot restarts if you persist `request_id`.
* Deferred images: `deferred:true`, then poll `GET /v1/images/{id}` (`pending`/`done`/`failed`), retained 24 h, URL output only [XIG]. Videos are always async [XVID].
* Responses: `background` is unsupported [XOAS]. A stored (`store:true`) response can be fetched by id for 30 days [XGT]. Whether a stream interrupted by a client disconnect keeps running server-side and becomes retrievable is **UNVERIFIED**. **No stream-resume** (`starting_after`/sequence replay) is documented.
* Batch API for 24-hour jobs [XPR].

---

# Part B — DeepSeek direct API

## B1. API surfaces

| Surface | Base / endpoint | Notes |
|---|---|---|
| Chat Completions (OpenAI format) | `POST https://api.deepseek.com/chat/completions` | Canonical [DQS][DCC] |
| **Beta** | `https://api.deepseek.com/beta` | Chat prefix completion (`prefix:true`), FIM `POST /beta/completions`, `strict` function tools [DPFX][DFIM][DTC] |
| Responses API | `POST https://api.deepseek.com/responses` | Built for Codex. **Stateless** (`previous_response_id`, `conversation`, `store`, `background`, `include` unsupported). Unsupported params are **silently ignored** [DRSP] |
| Anthropic Messages | `POST https://api.deepseek.com/anthropic/v1/messages` | `claude-opus*` → `deepseek-v4-pro`, other `claude*` → `deepseek-flash` [DANT][DMSG] |
| Files API | `/files` (OpenAI shape), `/anthropic/v1/files` (header `anthropic-beta: files-api-2025-04-14`) | **Images only** [DFILES] |
| Search / Fetch REST | `POST /v1/tools/search`, `POST /v1/tools/fetch` | Standalone; billed per request [DSRCH] |
| Misc | `GET /models` (capability metadata), `GET /user/balance` | [DLM] |

**Most capable surface:** it depends on the feature.
* CC is the only one with prefix completion, FIM, and token logprobs (incl. reasoning logprobs).
* Responses and Anthropic support inserting tool calls (and `system` messages) mid-conversation; CC supports only mid-conversation `system` [DTC]. They also support the server-side `web_search` tool on `deepseek-v4-pro`.
* Responses alone supports `json_schema` output [DRSPR].

## B2. Models

| API name | Serves | Ctx / max out | Vision | $/1M off-peak (peak = 2×): hit · miss · out | Concurrency |
|---|---|---|---|---|---|
| `deepseek-flash` | **DeepSeek-V4.1-Flash-1017** | 1M / 384K (393,216) | ✓ | 0.003 · 0.15 · 0.60 | 2500 |
| `deepseek-v4-pro` | **DeepSeek-V4-Pro-0813** | 1M / 384K | ✗ | 0.022 · 0.66 · 1.98 | 500 |

* **Peak** is 01:00–04:00 and 06:00–10:00 UTC, Mon–Fri, excluding Chinese public holidays [DPR].
* The legacy names `deepseek-v4-flash` and `deepseek-v4-flash-vision-exp` are still accepted and are served by V4.1-Flash-1017 at the Flash price [DPR].
* `deepseek-chat` and `deepseek-reasoner` were fully retired after 2026-07-24 15:59 UTC [DLOG].
* A plan to route `deepseek-v4-pro` to V4.1-Flash [DN0910] was reversed: V4-Pro service continues past 2026-09-14 [DLOG].
* Both models default to thinking mode. `/models` reports effort levels `low`/`high`/`max` [DLM].

## B3. Request parameters (Chat Completions)

| Field | Semantics [DCC][DTM] |
|---|---|
| `thinking: {"type":"enabled"\|"disabled"}` | Mode toggle. Takes precedence over `reasoning_effort` for mode selection. If both are omitted, thinking is **on**. (OpenAI SDK: pass via `extra_body`.) |
| `reasoning_effort` | `none` (disables thinking) / `low` / `high` (default) / **`max`**. Aliases: `minimal`→low, `medium`/`xhigh`→high (Anthropic also maps `ultra`→max) |
| `max_tokens` | 1…393,216. **Default 8K non-thinking, 64K thinking, 128K with `max`** |
| `temperature` (0–2), `presence_penalty`, `frequency_penalty` | Temperature has **no effect in thinking mode** (no error). The penalties are "no longer supported" (no effect) |
| `top_p` | `deepseek-flash`: applies only in thinking mode, clamped to ≥0.95, fixed at 1.0 in non-thinking. `deepseek-v4-pro`: ignored in thinking mode (0.95 with tools, otherwise 1.0); applies in non-thinking |
| `response_format` | `{"type":"text"\|"json_object"}`. In JSON mode, non-JSON output is **replaced with spaces** (content can be blank with `finish_reason:"stop"`). The prompt must mention "json" [DJSON] |
| `stop` | string or ≤16 strings |
| `stream`, `stream_options.include_usage` | `stream_options` without `stream:true` → 400 |
| `tools` (functions; `strict` beta), `tool_choice` | `required` and named choice → **400 in thinking mode**. `tool_choice:"none"` disables tools even when `tools` is set |
| `logprobs`, `top_logprobs` (0–20) | Tokens outside the top 20 get `-9999.0`. Response carries `logprobs.content[]` **and** `logprobs.reasoning_content[]` |
| `user_id` | ≤512 bytes. Used for content-safety attribution, **KV-cache isolation**, and per-user scheduling/concurrency [DRL] |
| assistant `prefix:true` (+ `reasoning_content` as a CoT prefix in thinking mode) | **Beta** base URL only. The last message must be assistant [DPFX][DCC] |

* Images go only in `user` messages (CC), as `image_url` or `file` (`file_id` | `file_data`) parts [DVIS].
* Responses equivalents [DRSP][DRSPR]:
  * `reasoning.effort` (none/low/high/max) toggles thinking and sets effort.
  * `text.format` supports `json_schema`.
  * `top_logprobs`, `max_output_tokens`, `user`.
  * `reasoning.summary` is accepted, but no summary is generated.
* Anthropic equivalents [DANT][DMSG]:
  * `thinking.type` enabled/disabled/adaptive(→enabled); `budget_tokens` is ignored.
  * `output_config.effort`.
  * `metadata.user_id`.
  * `tool_choice:any` is treated as `auto`; `tool` → 400 in thinking mode.
  * `top_k`, `container`, `mcp_servers`, `service_tier` are ignored.
  * The client tool `type:"custom"` → 422.

```json
POST https://api.deepseek.com/chat/completions
{"model":"deepseek-v4-pro","thinking":{"type":"enabled"},"reasoning_effort":"max","stream":true,
 "stream_options":{"include_usage":true},"tools":[{"type":"function","function":{"name":"get_date","parameters":{"type":"object","properties":{}}}}],
 "messages":[{"role":"user","content":"…"},
  {"role":"assistant","content":"","reasoning_content":"<verbatim from previous response>","tool_calls":[{"id":"call_00_…","type":"function","function":{"name":"get_date","arguments":"{}"}}]},
  {"role":"tool","tool_call_id":"call_00_…","content":"2026-11-11"}]}
```
Beta features [DPFX][DFIM][DFIMR]:
```json
POST https://api.deepseek.com/beta/chat/completions  {"model":"deepseek-flash","messages":[{"role":"user","content":"Write quicksort"},{"role":"assistant","content":"```python\n","prefix":true}],"stop":["```"]}
POST https://api.deepseek.com/beta/completions       {"model":"deepseek-flash","prompt":"def fib(a):","suffix":"    return fib(a-1) + fib(a-2)","max_tokens":128}
```
FIM limits: **non-thinking only**, max 4K tokens. `echo` can't be combined with `suffix` or `logprobs`. `logprobs` ≤20. `finish_reason` adds `insufficient_system_resource` and `aborted` [DFIM][DFIMR][DPR].

## B4. Multi-turn state rules

* All three APIs are stateless; resend the full history [DMR][DRSP][DMSG].
* **No `tools` in the request:** past `reasoning_content` "does not need to be passed back; even if passed … it will be ignored" [DTM].
* **`tools` present:** the `reasoning_content` of **all previous turns** (even turns without tool calls) "must be fully passed back". Otherwise the API **returns 400** [DTM].
* Anthropic: send back the complete assistant `content` (thinking + tool_use blocks). The service *may* recover omitted thinking from tool-call ids; if recovery fails → 400 [DANT][DMSG].
* Responses: `reasoning` input items carry plain-text `content` that is merged into the adjacent assistant message. **`summary`/`encrypted_content` are unsupported.** `web_search_call` items replayed in `input` restore their search results [DRSP].
* `/models` reports an Anthropic `system_prompt_update` capability: `in-history` for flash, `leading-only` for v4-pro [DLM].

## B5. Search / server-side tools / citations

* **Server-side `web_search`:**
  * Responses: `{"type":"web_search"}`, **`deepseek-v4-pro` only**. With `deepseek-flash` it is **ignored** [DRSP].
  * Anthropic: `{"type":"web_search_20250305"|"web_search_20260209","name":"web_search","max_uses":N}` (default/cap 5) [DMSG]. The documented example uses v4-pro. Whether flash supports it on this endpoint isn't stated (**UNVERIFIED**) [DANT].
  * Results come back as `server_tool_use` + `web_search_tool_result` blocks with `web_search_result{title,url,encrypted_content,page_age}`. Error codes: `invalid_tool_input`, `unavailable`, `max_uses_exceeded`, `too_many_requests`, `query_too_long`, `request_too_large` [DMSG].
  * Usage: Responses `usage.server_tool_use{tool_search,tool_fetch}`; Anthropic `{web_search_requests,tool_search}` [DRSP][DANT].
  * Responses `output_text.annotations` appear only as `[]` in examples. A citation format is **not documented** (**UNVERIFIED**).
  * Other Responses tool types (file_search, code_interpreter, mcp, computer_use) are ignored. `custom` works only as `apply_patch` [DRSP].
* **Standalone Search API** (`/v1/tools/search`):
  * Request `{query (1–128 chars; site:, -site:, "…", lang:, after:, before: operators), count 1–50, ori_question, localization, need_content}`.
  * Result items `{rerank_score 0–3, title, url, snippet, summary, content, site_name, site_icon, date, authority 0–4}`.
* **Fetch API** (`/v1/tools/fetch`): `{url}` → `{title, content(md)}`.
* Search/Fetch pricing is **$0.75 / 1k successful requests each**. Tool-triggered searches are billed the same way. QPS limits: 100 (search) and 25 (fetch) [DSRCH][DPR].
* For the bot's "official built-in tools only" rule: the standalone Search/Fetch APIs are official, but using them as a *client-side* function tool means local orchestration. Only the v4-pro server tool is truly built-in.

## B6. Multimodal

* Image input works with **`deepseek-flash` only** (v4-pro rejects it; on Anthropic, images are replaced with `[Unsupported Image]`) [DVIS][DANT].
* Formats: JPEG, PNG, GIF, WebP, detected from content.
* Limits:
  * URL ≤8192 chars; URL download ≤32 MiB within 60 s.
  * Body ≤48 MiB; inline image ≤32 MiB; `file_id` image ≤64 MiB.
  * ≤600 images per request; ≤8192 px per side (4096 px with 15+ images).
* `detail`: `low` downsamples to 512², while `high`/`original`/`auto` keep the original.
* Token cost: images are resized to about 1300² pixels, capped at **1024 tokens per image** [DVIS].
* Files API: images only, ≤64 MiB, `purpose:"user_data"`, optional expiry from 1 h to 30 d [DFILES]. Responses "File inputs are not supported" [DRSP]. **No PDF/document input. No image/video generation.**

## B7. Streaming format

* CC SSE: `data: {…"object":"chat.completion.chunk"…}`, terminated by `data: [DONE]`. `delta.reasoning_content` streams first, then `delta.content`.
* The **final** chunk carries `usage` alongside `finish_reason` (no separate usage-only chunk). With `include_usage:true`, every other chunk has `usage:null` [DCC].
* Tool-call deltas: the first chunk has `id`, `type`, `function.name`; later chunks carry only `arguments` [DCC].
* **Keep-alive:** while waiting, non-stream responses send empty lines and stream responses send **`: keep-alive`** SSE comments. If inference hasn't started after **10 min**, the connection is closed [DRL].
* `finish_reason` ∈ `stop`, `length`, `content_filter`, `tool_calls`, **`insufficient_system_resource`**, **`aborted`** [DCC].
* Responses SSE: events from `response.created` through `response.completed` / `response.incomplete` / `response.failed`. Includes `response.reasoning_text.delta`, `response.web_search_call.{in_progress,searching,completed}`, `response.custom_tool_call_input.delta`. Monotonic `sequence_number`. **No `[DONE]`**. `incomplete_details.reason` ∈ `max_output_tokens`, `content_filter` [DRSP][DRSPR].
* Anthropic SSE: `message_start` … `message_stop`, with `ping` and keep-alive comments. If waiting times out, the stream may get an **unnamed** `data: {"error":{"message":…}}` frame before close. A close before `message_stop` means the response is incomplete [DANT][DMSG].

## B8. Usage & context caching

* CC usage: `prompt_tokens = prompt_cache_hit_tokens + prompt_cache_miss_tokens`, `prompt_tokens_details.cached_tokens` (= hit), `completion_tokens`, `completion_tokens_details.reasoning_tokens`, `total_tokens`, plus `system_fingerprint` [DCC].
* Anthropic usage: `input_tokens` excludes hits, `cache_read_input_tokens` counts hits, and `cache_creation_input_tokens` is always 0. Read the **final** `message_delta.usage` (cumulative) [DANT].
* Disk KV cache is on by default and best-effort. Prefix units are persisted at the end of user input and the end of model output, at detected common prefixes, and at fixed intervals. A hit needs a **full** prefix-unit match. Builds take seconds; entries are evicted within hours to days [DKV].
* V4.1-Flash's KV cache needs 1/4 the HBM and 1/8 the SSD of the previous generation [DN0910].
* Billing is tokens × price, with the granted balance used first [DPR]. The tokenizer is downloadable [DTU].

## B9. Errors

| HTTP | Meaning [DERR][DSRCH] | Retry? |
|---|---|---|
| 400 | invalid format; also missing `reasoning_content` with tools, `required`/named tool_choice in thinking mode, `stream_options` without stream, images in system/assistant messages | no (fix the request) |
| 401 | bad API key | no |
| **402** | **insufficient balance** | no (top up) |
| 422 | invalid parameters | no |
| 429 | rate / **concurrency** limit (2500 flash / 500 v4-pro per account; per-`user_id` for raised quotas) [DRL] | yes, after backoff |
| 500 | server error | yes |
| 503 | server overloaded | yes |
| (Search/Fetch) 403 / 404 / 408 / 413 / 504 | security policy / no content / body read timeout / too large / downstream timeout | 408 and 5xx: retry |

Error body: `{"error":{"message","type","param","code"}}`. The Anthropic endpoint also uses this shape, without Anthropic's top-level `type:"error"` [DMSG][DSRCH].

## B10. Async / resume

None documented. There is no deferred mode, background mode, batch API, stored responses, or stream resume, and every API is stateless [DRSP][DMR]. In-flight work is lost on disconnect. Whether tokens generated before the disconnect are billed is **UNVERIFIED**. The only liveness aids are keep-alives and the 10-minute pre-inference timeout [DRL].

---

# Part C — How OpenRouter exposes xAI and DeepSeek

## C1. Catalog snapshot (`GET /api/v1/models` + `/models/{id}/endpoints`, fetched 2026-11-11) [ORM][ORE]

**xAI text models.** The common supported_parameters are `include_reasoning, reasoning, reasoning_effort, max_tokens, temperature, top_p, seed, logprobs, top_logprobs, response_format, structured_outputs, tools, tool_choice`.

| OR id | Ctx / max out | `reasoning` metadata | Endpoints (provider tag) | Notes |
|---|---|---|---|---|
| `x-ai/grok-4.7` | 500k / 450k | mandatory; efforts xhigh/high/medium/low; default high | `xai`, `xai/zdr`, `xai/zdr/us` (1.1×), `xai/priority`, `xai/zdr/priority` (2×), Amazon Bedrock `global` | `default_parameters {temperature:0.7, top_p:0.95}`. `stop`/`top_k` in the model-level list come **only from the Bedrock endpoint** |
| `x-ai/grok-4.6` | 500k / 450k | same | same + Bedrock `us-west-2` | |
| `x-ai/grok-4.5` | 500k / 450k | mandatory; high/medium/low | xai ×4 | no `stop` |
| `x-ai/grok-4.3` | 1M / 900k | optional; high/medium/low/**none**; default **low** | xai ×4 | **no `xhigh`** (xAI lists it [XMP]) |
| `x-ai/grok-4.20` | 2M / 1.8M (xAI docs say **1M** [XMP]) | optional, **default_enabled:false**; no `reasoning_effort` | xai ×4 | Defaults to non-reasoning. xAI's alias `grok-4.20` is the *reasoning* slug [XMP] |
| `x-ai/grok-4.20-multi-agent` | 2M / 1.8M (xAI: 1M) | mandatory; xhigh/high/medium/low (default medium) | xai ×4 | **no `tools`/`tool_choice`** (matches [XMA]) |
| `x-ai/grok-build-0.1` | 256k / 230k | mandatory; no efforts | xai ×4 | |
| `…:batch` variants (4.7, 4.6, 4.3) | — | — | `/api/v1/batches` only | OR Batch API, ~20 % cheaper |

* Pricing equals xAI list prices, including the ≥200k override. `web_search` costs $0.005 per call on xAI endpoints and $0.01 on Bedrock.
* The `xai` endpoints are `supports_implicit_caching:true` and `native_tools:{"openrouter:web_search":{"type":"web_search"}}`. **`xai/zdr/us` and Bedrock have no native tools.**
* Image/video catalog [ORIM][ORVM]:
  * `x-ai/grok-imagine-image-2.0` (`/api/v1/images`): resolution 1K/2K; aspect ratios 1:1…20:9, auto (**no 21:9/5:2**); quality low/medium (**no auto**); **n max 1**; input_references 0–3.
  * `x-ai/grok-imagine-video`, `-1.5`, `-1.5-lite` (`/api/v1/videos`): 1–15 s, `first_frame` only, `allowed_passthrough_parameters: []`.
  * `grok-imagine-image` 1.0 is not listed.

**DeepSeek models**

| OR id | ↔ official | Official `DeepSeek` endpoint? | # endpoints | Third-party quantizations |
|---|---|---|---|---|
| `deepseek/deepseek-v4.1-flash-1017` | `deepseek-flash` (description says so) | **yes**: ctx 1M, max out 393,216, params `reasoning, include_reasoning, reasoning_effort, max_tokens, temperature, top_p, stop, frequency_penalty, presence_penalty, logprobs, top_logprobs, tools, tool_choice, response_format`; `supports_tool_choice.required/function = false`; peak/off-peak UTC price overrides | 51 | fp8, fp4, mxfp4, unknown (Fireworks, Together, DeepInfra, Baseten, Google Vertex, Mistral, Cohere, …) |
| `deepseek/deepseek-v4-pro-0813` | `deepseek-v4-pro` | **yes** (same params; required/function tool_choice true) | 25 | fp8, fp4, mxfp4 |
| `deepseek/deepseek-v4.1-flash` (0910), `deepseek-v4-flash-0731`, `deepseek-v4-flash-vision-exp`, `deepseek-v4-pro` (**0423 preview**), `deepseek-v4-flash` (0423), `v3.2`, `v3.2-exp`, `v3.1-terminus`, `chat-v3.1`, `r1-0528`, `chat-v3-0324`, `chat` | retired or older weights | **no** (third-party only) | 1–38 | fp8/fp4/… |

* Reasoning metadata: `supported_efforts ["max","high","low","none"]`, default `high` (0423 slugs: `xhigh/high/none`).
* Third-party endpoints often add `structured_outputs`, `seed`, `top_k`, `min_p`, `logit_bias`. Max output ranges from 131K to 943K (beyond the official 384K). `top_provider.max_completion_tokens: 943718` is therefore **not** the official limit.

## C2. Reasoning mapping

* Request [ORRT][ORP]:
  * `reasoning: {effort: max|xhigh|high|medium|low|minimal|none, max_tokens, enabled, exclude}`, plus a top-level `reasoning_effort` alias. `enabled:true` → effort "medium".
  * Per-model discovery: `reasoning.{supported_efforts, default_effort, default_enabled, mandatory}`. When `mandatory`, don't send `none`.
  * The legacy `include_reasoning` is CC-only.
* Response:
  * CC: `message.reasoning` (string) + `message.reasoning_details[]` (stream: `delta.reasoning`, `delta.reasoning_details`).
  * Detail types: `reasoning.text{text,signature}`, `reasoning.summary{summary}`, `reasoning.encrypted{data}`, each with `id`, `format`, `index`. xAI formats are **`xai-responses-v1`** and `bedrock-xai-responses-v1` [ORRT].
* Replay: send back `reasoning_details` **unchanged and in order** (the sequence can't be modified), or `reasoning` / `reasoning_content` (alias) for plain text.
* On Responses, OR auto-requests encrypted reasoning; replay `encrypted_content` unchanged [ORRT].
* xAI: the `xai-responses-v1` format and multi-agent availability (which xAI doesn't support on CC [XMA]) imply that **OR calls xAI's Responses API upstream** (inference, **UNVERIFIED**). Encrypted reasoning should therefore survive replay via `reasoning_details`.
* xAI effort values pass through as `reasoning.effort` (xhigh on 4.6/4.7). On `x-ai/grok-4.20`, `reasoning.enabled` presumably switches between the `-reasoning` and `-non-reasoning` slugs (**UNVERIFIED**).
* DeepSeek: `reasoning.effort:"max"` is listed as supported. How OR maps it onto `thinking` + `reasoning_effort` is **not documented (UNVERIFIED)**; check with `debug.echo_upstream_body`. With tools + thinking, the earlier `reasoning` must be replayed or DeepSeek returns 400 [DTM]. Whether OR forwards the `reasoning`/`reasoning_details` you send as DeepSeek `reasoning_content` is **UNVERIFIED**, but required.

## C3. Feature support via OpenRouter — xAI

| Feature | Via OR | Evidence |
|---|---|---|
| Text, streaming, function tools, `response_format`/json_schema | ✅ | [ORE] |
| Reasoning effort (low…xhigh), encrypted reasoning replay | ✅ (`reasoning_details` round-trip) / summarized-text exposure **UNVERIFIED** | [ORE][ORRT] |
| `reasoning: none` on grok-4.3; xhigh on grok-4.3 | ✅ / ❌ (not in supported_efforts) | [ORM][XMP] |
| `stop`, penalties | ❌ on xAI endpoints (xAI errors on reasoning models anyway) | [ORE][XRS] |
| `logprobs` | listed, but xAI ignores it for ≥4.20 → effectively ❌ | [ORE][XM] |
| `top_k`, `min_p`, `n`, `max_turns`, `guidelines`, `safety_identifier` | ❌ not in xAI endpoint params | [ORE][XOAS] |
| `seed` | listed; xAI Responses has no `seed` → **UNVERIFIED** | [ORE][XOAS] |
| Web search (native xAI `web_search`) | ✅ `openrouter:web_search` with `engine:"auto"\|"native"`. A `{"type":"web_search"}` tool is auto-hoisted. Plugin `web` / `:online` are **deprecated** | [ORWS][OROV] |
| ↳ domain filters | ✅ allowed/excluded (mutually exclusive). `max_results`, `search_context_size`, `max_uses` are ignored with native search | [ORWS] |
| ↳ `enable_image_search`, `enable_image_understanding` | ❌ not exposed | [ORWS] |
| X search | ⚠️ opt-in `parameters.x_search{allowed_x_handles, excluded_x_handles, from_date, to_date, enable_image_understanding, enable_video_understanding}`, only alongside native web search (legacy `x_search_filter` still works) | [ORWS][ORWP] |
| Code execution, collections/file_search, attachment_search, remote MCP, xAI `image_generation` tool | ❌ not native: `native_tools` lists only web_search. OR's own `openrouter:shell` / `openrouter:image_generation` are **OR-run substitutes** | [ORE][ORST] |
| Citations | ⚠️ normalized `annotations[{type:"url_citation",url_citation:{url,title,content,start_index,end_index}}]`. xAI `citations[]` and inline `[[N]]` behaviour **UNVERIFIED** | [ORWP][XCIT] |
| Image input | ✅ (modality text+image+file). OR accepts png/jpeg/webp/gif, but xAI documents jpg/png only | [ORIMG][XIU] |
| File/PDF input | ⚠️ "file" modality listed. Native passthrough to xAI `attachment_search` is **UNVERIFIED**. OR may parse PDFs itself (`file-parser` plugin) | [ORM][ORPDF] |
| Image generation | ⚠️ `/api/v1/images` with `x-ai/grok-imagine-image-2.0`: n=1 only, ≤3 refs, no 21:9/5:2, no `auto` quality | [ORIM] |
| Video generation | ⚠️ T2V + first-frame I2V only. No reference-to-video, keyframes, edit, extend | [ORVM] |
| Stored responses / `previous_response_id` / `store:true` | ❌ → 400 | [ORRA][ORCR] |
| Deferred completions, context compaction, WebSocket mode | ❌ | [ORRA] |
| Priority tier / ZDR / US region | ✅ `service_tier:"priority"`, `provider.zdr`, endpoint `xai/zdr/us` | [ORE][ORSTier][ORPS] |
| Prompt caching | ✅ implicit; `cached_tokens`; sticky routing via `session_id`/`prompt_cache_key`. Forwarding as `x-grok-conv-id` is **UNVERIFIED** | [ORPC][ORCC] |
| Usage / cost | ⚠️ OR `usage.cost`, `cost_details.upstream_inference_cost`, `web_search_requests`. xAI `cost_in_usd_ticks` and `server_side_tool_usage_details` are not exposed (**UNVERIFIED**) | [ORUA][ORWS] |
| Batch | ✅ OR Batch API (`:batch` ids) | [ORM] |

## C4. Feature support via OpenRouter — DeepSeek

| Feature | Via OR | Evidence |
|---|---|---|
| Thinking on/off + effort low/high/max | ✅ `reasoning.effort` / `reasoning.enabled`. Exact upstream mapping **UNVERIFIED** | [ORM][ORRT] |
| `reasoning_content` replay for tool calls | ✅ via `reasoning` / `reasoning_details` (alias `reasoning_content`) **UNVERIFIED end-to-end** | [ORRT][DTM] |
| Tools; `tool_choice` | ✅. Flash official endpoint: `required`/named = false | [ORE][DCC] |
| `response_format` json_object | ✅ official. json_schema (`structured_outputs`) only on third-party hosts | [ORE] |
| `logprobs`/`top_logprobs` | ✅ official endpoint. `logprobs.reasoning_content` passthrough **UNVERIFIED** | [ORE][DCC] |
| `user_id` (KV-cache isolation) | ❌/**UNVERIFIED**: OR `user` is "never forward[ed] raw" (hashed identity) | [ORCC][DRL] |
| Chat prefix completion (`prefix:true`), FIM (`/beta/completions`, `suffix`), `strict` tools (beta) | ❌ no `prefix` field in OR's assistant message schema, no beta base URL, `suffix` undocumented | [ORCC][DPFX][DFIM] |
| Native web search (Responses/Anthropic, v4-pro) | ❌ `native_tools: {}` on the DeepSeek endpoint. `openrouter:web_search` falls back to **Exa** (not DeepSeek search) | [ORE][ORWS] |
| DeepSeek Search/Fetch REST | ❌ | [DSRCH] |
| Image input (flash) | ✅ model-level modality text+image. Per-host support **UNVERIFIED**. DeepSeek Files API `file_id` ❌ | [ORM][DVIS] |
| PDF/files | ⚠️ DeepSeek has no native file input, so OR **parses PDFs itself** (default `mistral-ocr`, $2/1k pages, or `cloudflare-ai`). This is a hidden transform | [ORPDF][DRSP] |
| Caching | ✅ implicit + sticky routing. OR's guide says "0.1×" generically, but the official endpoint's listed price is DeepSeek's own (flash hit $0.003 vs miss $0.15). `prompt_cache_hit/miss_tokens` are replaced by OR `cached_tokens` | [ORPC][ORE][ORUA] |
| Peak/off-peak pricing | ✅ time-of-day `overrides` on the official endpoint. Chinese public-holiday off-peak is **UNVERIFIED** | [ORE][DPR] |
| `finish_reason` `insufficient_system_resource`/`aborted` | ⚠️ normalized to {tool_calls, stop, length, content_filter, error}. Raw value in `native_finish_reason` | [ORAPI] |
| Responses / Anthropic skins | ✅ OR's own `/api/v1/responses` and `/messages` (stateless), translated by OR. DeepSeek-native `apply_patch`/`web_search` are not reachable | [ORRA][ORM] |
| Batch | ✅ OR `:batch` variants (DeepSeek itself has none). Which host runs them is **UNVERIFIED** | [ORM] |

## C5. Upstream hosts and pinning (critical for "raw" testing)

* By default OR load-balances by price/latency across all endpoints. With tools, **Auto Exacto** reorders providers using tool-call error rate and benchmarks [ORPS][ORAE][ORTR]. A "DeepSeek V4.1 Flash" request can land on fp4/fp8/mxfp4 third-party deployments with different max outputs and parameter support [ORE].
* Pin with `"provider":{"only":["deepseek"],"allow_fallbacks":false,"require_parameters":true}`.
  * `require_parameters` drops providers that would silently ignore params. Default routing lets them ignore unknown params [ORPS].
  * Endpoint slugs follow `provider[/variant][/region][/tier]`. The base slug `xai` matches `xai`, `xai/zdr`, `xai/zdr/us`, but not priority tiers [ORPS].
  * `provider.quantizations:[…]` filters by `fp8`/`fp4`/`bf16`/… [ORPS].
* For xAI: `"only":["xai"]` excludes Amazon Bedrock (Bedrock has no native search, no implicit caching, and `web_search` at $0.01). Add `"ignore":["xai/zdr/us"]` to avoid the 1.1× US endpoint, which also has no native search [ORE].

## C6. OpenRouter behaviours that affect transparency

* **`debug: {"echo_upstream_body": true}`** (streaming only) returns the transformed upstream body as the first chunk (CC `debug` field, Responses `response.debug` event), one per attempted provider. It shows "default values OpenRouter applies" (e.g. an injected `max_tokens`) [ORERR]. Use it to verify DeepSeek/xAI reasoning mapping and any `default_parameters` [ORM][ORMOD].
* OR says absent sampling params are omitted upstream [ORP]. The models list nonetheless carries `default_parameters` (grok-4.7: temperature 0.7 / top_p 0.95; deepseek-v4-pro-0813: 1 / 1). Whether these get injected is **UNVERIFIED**; check with the debug echo.
* SSE: comments **`: OPENROUTER PROCESSING`**. On CC the **final usage chunk repeats `finish_reason`** (one content-free choice) [ORS]. Usage is always included; `stream_options.include_usage` is a no-op [ORUA].
* Errors: shape `{error:{code,message,metadata}}`, with typed `error.metadata.error_type`. After headers are sent, errors arrive in-stream (`finish_reason:"error"` / `response.failed`) with HTTP 200. **500-class messages are masked**; `provider_code`/`raw` are available only for non-500 errors [ORERR]. Codes: 400, 401, **402 = OR credits**, 403 (guardrail/moderation/budget), 408, 429 (Retry-After), 502 (provider down), 503 (no provider meets requirements) [ORERR].
* **Stream cancellation:** aborting the connection stops generation and billing on supported providers, which include **XAI and DeepSeek**. Non-stream requests keep running and are billed [ORS].
* Stateless only: no `previous_response_id`/`store` [ORRA]. The `X-Generation-Id` header comes on every request [ORS]. With I/O logging enabled, stored prompt/completion can be fetched via `GET /api/v1/generation/content?id=` (management key; retention ≥3 months) [ORGEN][ORIO]. A possible restart-recovery path for *non-streaming* requests (**UNVERIFIED**).
* Opt-in features to keep **off** for a transparent bot: response caching (`X-OpenRouter-Cache`) [ORRC], plugins (`web`, `file-parser`, `response-healing`, `context-compression`, which is auto-on only for ≤8k-context models) [ORMT], presets.

---

# Part D — Gaps

## D1. OpenRouter gaps for xAI

* **No stateful Responses features.** `store:true` / `previous_response_id` → 400 [ORRA]. Context compaction, WebSocket mode, and deferred completions are unreachable [XCOMP][XWSM][XDEF]. **So no OR-side recovery for long Grok runs.**
* **Server tools reduced to web search.** Only `openrouter:web_search` maps to xAI's native tool [ORE]. X search needs an explicit `x_search` opt-in [ORWS]. **code_interpreter, collections/file_search, attachment_search, remote MCP, and the xAI `image_generation` tool are not exposed natively** [ORE][XTO]. OR's `openrouter:shell` / `openrouter:image_generation` run on OR, not xAI [ORST], which would violate "only official built-in tools".
* **Web-search knobs missing:** `enable_image_search` and `enable_image_understanding` are not exposed [ORWS][XWS]. Native search forwards no `max_results`/`max_uses` [ORWS].
* **Tool-output include flags** (`web_search_call.action.sources`, `code_interpreter_call.outputs`, `no_inline_citations`) are not documented as forwarded to xAI (**UNVERIFIED**) [XSS][ORCR].
* **Citations are normalized** to `url_citation` annotations. xAI's full `citations[]` list and `[[N]](url)` inline handling are **UNVERIFIED** [ORWP][XCIT].
* **Params not exposed** on xAI endpoints: `top_k`, `min_p`, `n`, `max_turns`, `guidelines`, `safety_identifier`, `stop` [ORE][XOAS]. `logprobs` is listed but ignored by xAI ≥4.20 [XM]. `xhigh` is missing for grok-4.3 [ORM][XMP].
* **`x-ai/grok-4.20` defaults to non-reasoning** (`default_enabled:false`), unlike xAI's `grok-4.20` alias (reasoning) [ORM][XMP].
* **Exact cost/tool counters lost:** `cost_in_usd_ticks`, `server_side_tool_usage_details` (x_posts_fetched/x_users_fetched), `num_server_side_tools_used` are replaced by OR `cost`/`web_search_requests` (**UNVERIFIED** passthrough) [ORUA][XOAS].
* **Imagine API is restricted:** `n=1` (xAI allows 1–10), ≤3 reference images (xAI: 5), no `21:9`/`5:2`, no `quality:auto`, no `deferred`/`storage_options` [ORIM][XIMG]. grok-imagine-image 1.0 is absent. Video has no R2V, keyframes, edits, or extensions [ORVM][XVID].
* **Routing pitfalls:** Amazon Bedrock serves grok-4.6/4.7 with a different param set and no native search. `xai/zdr/us` has no native search and costs 1.1× [ORE].
* **Error fidelity:** 500s are masked. Mid-stream errors arrive as HTTP 200 with OR's typed `error_type`, not xAI's verbatim body [ORERR].
* **Files/PDF:** native routing to xAI `attachment_search` vs OR's own parsing is **UNVERIFIED** [ORPDF][XFILES].
* `prompt_cache_key` → `x-grok-conv-id` forwarding is **UNVERIFIED**. Cache hit rates may suffer [XM47][ORCC].

## D2. OpenRouter gaps for DeepSeek

* **Host ambiguity:** official DeepSeek serves only `deepseek-v4-pro-0813` and `deepseek-v4.1-flash-1017` on OR. Every other DeepSeek slug, including `deepseek/deepseek-v4-pro` (the 0423 *preview* weights) and `deepseek/deepseek-v4-flash`, is **third-party only**, often fp8/fp4/mxfp4, with different limits and params [ORE]. You must pin `provider.only:["deepseek"]` + `allow_fallbacks:false` [ORPS].
* **Beta features unreachable:** chat prefix completion (`prefix:true`, `reasoning_content` prefix), FIM `/beta/completions` with `suffix`/`echo`, and `strict` function tools [ORCC][DPFX][DFIM][DTC].
* **No DeepSeek search:** the Responses/Anthropic server `web_search` (v4-pro) isn't native on OR (`native_tools:{}`). `openrouter:web_search` would silently use **Exa** [ORE][ORWS]. The Search/Fetch REST APIs are unavailable [DSRCH].
* **Responses/Anthropic-specific DeepSeek semantics are lost** (`apply_patch` custom tool, `web_search_call` replay, Anthropic `output_config.effort`, model aliasing) because OR translates through its own skins [ORRA][DRSP][DANT].
* **Reasoning mapping is undocumented** (`effort` → `thinking` + `reasoning_effort`, `max` → 128K default `max_tokens`?). So is the `reasoning`/`reasoning_details` → `reasoning_content` replay for tool calls (**UNVERIFIED**, verify with `echo_upstream_body`) [ORRT][DTM][DCC].
* **`user_id` semantics** (KV-cache isolation, per-user concurrency) are likely lost: OR hashes `user` [ORCC][DRL].
* **Usage detail:** DeepSeek's `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens` and `system_fingerprint` are replaced by OR usage fields (**UNVERIFIED** passthrough) [ORUA][DCC].
* **finish_reason fidelity:** `insufficient_system_resource`/`aborted` are normalized (raw value only in `native_finish_reason`) [ORAPI][DCC].
* **PDF/document input is silently OCR'd by OR** (mistral-ocr by default, extra cost), not by DeepSeek. This is a hidden transform [ORPDF].
* **Files API (`file_id`) unavailable.** Image-input support per third-party host is unknown [DFILES][ORE].
* **Error fidelity:** DeepSeek 402 (balance) never surfaces (OR's 402 means OR credits). 500s are masked. DeepSeek's 400 text (e.g. missing `reasoning_content`) arrives inside OR's envelope [ORERR][DERR].
* **No recovery story:** DeepSeek is stateless and OR cancels on disconnect (DeepSeek is on the cancellation list) [ORS][DRSP].

---

# Part E — Implementation notes for the Rust bot (from the above)

1. **Provider abstraction.** Use three wire formats: xAI Responses (direct), DeepSeek Chat Completions (direct, with optional Anthropic/Responses for v4-pro web_search), and OR Chat Completions/Responses. Persist **opaque replay blobs** per assistant turn: xAI output items incl. `encrypted_content`; DeepSeek `reasoning_content`; OR `reasoning_details` [XBP][DTM][ORRT].
2. **SSE parser.** Ignore `:`-comment lines (`: keep-alive`, `: OPENROUTER PROCESSING`) and blank lines, and support both `[DONE]` and event-only terminators [DRL][ORS][DRSP]. Treat HTTP-200 streams that carry error events as failures and show the body verbatim [ORERR].
3. **Recovery.** For xAI direct, offer a "deferred" mode (persist `request_id`; results are fetchable **once**) [XDEF]. DeepSeek and OR cannot resume, so mark interrupted turns as such.
4. **Expose every official toggle per model** from the tables above. Don't send params a model rejects: xAI `stop`/penalties on reasoning models [XRS]; DeepSeek `tool_choice:"required"` in thinking mode [DCC]. Display "ignored" params (DeepSeek temperature in thinking mode, xAI logprobs ≥4.20) as no-ops, not as errors.

---

## S. Sources (all fetched 2026-11-11)

xAI — base `https://docs.x.ai` (every page is also available as `.md`):
[XLLM] /llms.txt · [XM] /developers/models · [XM47] /developers/grok-4-7 · [XMP] /developers/models/{grok-4.7,grok-4.6,grok-4.5,grok-4.3,grok-4.20-0309-reasoning,grok-4.20-0309-non-reasoning,grok-4.20-multi-agent-0309,grok-build-0.1} · [XPR] /developers/pricing · [XRN] /developers/release-notes · [XRS] /developers/model-capabilities/text/reasoning · [XGT] /developers/model-capabilities/text/generate-text · [XCMP] /developers/model-capabilities/text/comparison · [XBP] /developers/model-capabilities/text/best-practices · [XST] /developers/model-capabilities/text/streaming · [XSO] /developers/model-capabilities/text/structured-outputs · [XMA] /developers/model-capabilities/text/multi-agent · [XIU] /developers/model-capabilities/images/understanding · [XIG] /developers/model-capabilities/images/generation · [XIMG] /developers/rest-api-reference/inference/images · [XVID] /developers/rest-api-reference/inference/videos · [XRESP] /developers/rest-api-reference/inference/responses · [XLEG] /developers/rest-api-reference/inference/legacy · [XFILES] /developers/files · [XCWF] /developers/model-capabilities/files/chat-with-files · [XMF] /developers/files/managing-files · [XTO] /developers/tools/overview · [XWS] /developers/tools/web-search · [XXS] /developers/tools/x-search · [XCE] /developers/tools/code-execution · [XTIG] /developers/tools/image-generation · [XRMCP] /developers/tools/remote-mcp · [XCIT] /developers/tools/citations · [XSS] /developers/tools/streaming-and-sync · [XTUD] /developers/tools/tool-usage-details · [XAU] /developers/tools/advanced-usage · [XRL] /developers/rate-limits · [XPCH] /developers/advanced-api-usage/prompt-caching/how-it-works · [XPC] /developers/advanced-api-usage/prompt-caching/usage-and-pricing · [XDEF] /developers/advanced-api-usage/deferred-chat-completions · [XWSM] /developers/advanced-api-usage/websocket-mode · [XREG] /developers/advanced-api-usage/regions · [XMOD] /developers/advanced-api-usage/custom-moderation-controls · [XCOMP] /developers/advanced-api-usage/context-compaction · [XBATCH] /developers/advanced-api-usage/batch-api · [XCOST] /developers/cost-tracking · [XDBG] /developers/debugging · [XRET] /developers/migration/may-15-retirement · [XOAS] /openapi.json

DeepSeek — base `https://api-docs.deepseek.com` (pages also available as `.md`):
[DQS] / · [DPR] /quick_start/pricing · [DTM] /guides/thinking_mode · [DCC] /api/create-chat-completion · [DRSP] /guides/responses_api · [DRSPR] /api/create-response · [DANT] /guides/anthropic_api · [DMSG] /api/create-message · [DVIS] /guides/vision · [DFILES] /guides/files_api · [DSRCH] /guides/search · [DTC] /guides/tool_calls · [DJSON] /guides/json_mode · [DPFX] /guides/chat_prefix_completion · [DFIM] /guides/fim_completion · [DFIMR] /api/create-completion · [DKV] /guides/kv_cache · [DMR] /guides/multi_round_chat · [DERR] /quick_start/error_codes · [DRL] /quick_start/rate_limit · [DTU] /quick_start/token_usage · [DLM] /api/list-models · [DLOG] /updates · [DN0910] /news/news260910

OpenRouter:
[ORM] https://openrouter.ai/api/v1/models · [ORE] https://openrouter.ai/api/v1/models/{id}/endpoints (all x-ai/* and deepseek/* ids) · [ORIM] https://openrouter.ai/api/v1/images/models (+ /images/models/x-ai/grok-imagine-image-2.0/endpoints) · [ORVM] https://openrouter.ai/api/v1/videos/models · [ORRT] https://openrouter.ai/docs/guides/best-practices/reasoning-tokens · [ORST] https://openrouter.ai/docs/guides/features/server-tools · [ORWS] https://openrouter.ai/docs/guides/features/server-tools/web-search · [ORWP] https://openrouter.ai/docs/guides/features/plugins/web-search · [OROV] https://openrouter.ai/docs/guides/routing/model-variants/online · [ORPS] https://openrouter.ai/docs/guides/routing/provider-selection · [ORAE] https://openrouter.ai/docs/guides/routing/auto-exacto · [ORTR] https://openrouter.ai/docs/guides/providers/traffic-routing · [ORP] https://openrouter.ai/docs/api_reference/parameters · [ORS] https://openrouter.ai/docs/api_reference/streaming · [ORERR] https://openrouter.ai/docs/api_reference/errors-and-debugging · [ORAPI] https://openrouter.ai/docs/api_reference/overview · [ORRA] https://openrouter.ai/docs/api_reference/responses/overview · [ORCC] https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion · [ORCR] https://openrouter.ai/docs/api/api-reference/responses/create-a-response · [ORPC] https://openrouter.ai/docs/guides/best-practices/prompt-caching · [ORUA] https://openrouter.ai/docs/cookbook/administration/usage-accounting · [ORIMG] https://openrouter.ai/docs/guides/overview/multimodal/image-understanding · [ORPDF] https://openrouter.ai/docs/guides/overview/multimodal/pdfs · [ORSTier] https://openrouter.ai/docs/guides/features/service-tiers · [ORMT] https://openrouter.ai/docs/guides/features/message-transforms · [ORRC] https://openrouter.ai/docs/guides/features/response-caching · [ORMOD] https://openrouter.ai/docs/guides/overview/models · [ORIO] https://openrouter.ai/docs/guides/features/input-output-logging · [ORGEN] https://openrouter.ai/docs/api/api-reference/generations/get-stored-prompt-completion-and-error-content-for-a-generation
