# GLM (Zhipu / Z.ai), Kimi (Moonshot), Qwen (Alibaba Model Studio): official APIs and how OpenRouter maps them
**How this was researched**
- Research snapshot: **2026-11-11**. Every claim comes from live documentation fetched that day.
- Citations use labels such as `[glm-…]`, `[k-…]`, `[q-…]`. Labels used more than once are reference-style links defined at the end of each vendor section; single-use labels and all OpenRouter links are inline.
- **UNVERIFIED** means a fetched page did not confirm the claim: it was inferred, or it is only stated by third parties.
- OpenRouter facts come from the public `GET https://openrouter.ai/api/v1/models`, `/api/v1/models/{id}/endpoints` (all 73 `z-ai/*`, `moonshotai/*`, `qwen/*` ids), `/api/v1/providers`, and the docs' `.md` pages.
- The research agents made unauthenticated live probes only, and only where noted.

## 0. Cross-provider summary
| Topic | GLM (Zhipu / Z.ai) | Kimi (Moonshot) | Qwen (Alibaba) |
|---|---|---|---|
| Chat base URL (CN / intl) | `https://open.bigmodel.cn/api/paas/v4` / `https://api.z.ai/api/paas/v4` | `https://api.moonshot.cn/v1` / `https://api.moonshot.ai/v1` (docs moved to platform.kimi.com / platform.kimi.ai) | `https://{ws}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1` / `https://{ws}.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1`. The legacy `dashscope(-intl).aliyuncs.com` is frozen after 2026-09-30 |
| Other official surfaces | Anthropic `/api/anthropic`; Responses (CN; stateful, opt-in `store`); CN async chat + Batch; Web Search / Reader APIs | Responses + Anthropic (**K3 only**, stateless); Batch; Formula tools; REST search/fetch | Responses (stateful, `store` on by default, built-in tools); Anthropic `/apps/anthropic`; DashScope native; Batch |
| Thinking switch | `thinking.type` `enabled` (default) / `disabled` (5.3: enabled only) | K3: always on, no switch; k2.6: `thinking.type` (+`keep`) | `enable_thinking` (hybrid; qwen3.8 on by default) |
| Effort / budget | `reasoning_effort` (5.2+): 5.3 `low\|high\|max`; default `max` | `reasoning_effort` `low\|high\|max` (K3), default `max` | `reasoning_effort` `low\|medium\|xhigh` (qwen3.8) **or** `thinking_budget` |
| Reasoning field | `reasoning_content` | `reasoning_content` | `reasoning_content` |
| Replaying reasoning | default `clear_thinking:true` drops it; `false` + byte-exact echo preserves it; always echo in tool loops | **required** on K3; k2.6 `keep:"all"` | qwen3.8 `preserve_thinking` default `true`, so echo it; missing gives no error, only degradation |
| Official web search | chat tool `{"type":"web_search","web_search":{"enable":true,"search_engine":…}}`, **default off**; results in top-level `web_search[]`; CN ¥0.01–0.05, intl $0.01/use | Responses `web_search` (K3) $0.005/search; Formula `moonshot/web-search`; REST `/v1/tools/search*`. `$web_search` builtin is undocumented/legacy | `enable_search` + `search_options`; sources (`search_info`) only via DashScope native / Responses, not compatible mode |
| Other official tools | `retrieval` (CN KB), `mcp` (CN), Web Reader | Formula tools (fetch, memory, excel, date, …), REST fetch | code interpreter (`enable_code_interpreter`; Responses `code_interpreter`), web extractor, image search, `file_search`, MCP |
| Files | `file` part (`file_id`/`file_url`/`file_data`), parsed by Zhipu | `file-extract` returns Markdown; the client inlines it | Qwen-Long `fileid://` (BJ only); Responses `input_file` PDF |
| Prefill | not documented | `partial:true` (+`reasoning_content`) | `partial:true` (not qwen3.8, not thinking) |
| Cache | implicit; `prompt_tokens_details.cached_tokens` | implicit; TTL `5m`/`1h`; K3 writes billed; `cache_write_tokens` | implicit (hit 20%) **or** explicit `cache_control` (write 125%, hit 10%) |
| Usage in stream | always in the last chunk | `stream_options.include_usage` → `choices:[]` chunk | `include_usage` → `choices:[]` chunk |
| Abnormal stream end | `finish_reason` `sensitive` / `network_error` / `model_context_window_exceeded` | `content_filter` is a 400 (input or output); mid-stream form UNVERIFIED | `DataInspectionFailed` (input or output); mid-stream form UNVERIFIED |
| Async / resume | CN async chat + polling; no stream resume | none (a disconnect is still billed in full); Partial-Mode continuation | none; Batch; `partial` continuation (non-thinking only) |
| OR: official host | `z-ai` (labelled fp8/fp4) plus up to 36 third-party hosts | `moonshotai` for K3.1-pro/K3/K2.7-code/K2.6; **none** for K2.5 and older | `alibaba` only, for proprietary models; none for open weights |
| OR: official built-in search | ✗ (`native_tools:{}`; Exa instead) | ✗ | ✗ |

### Where the old Python bot's assumptions no longer hold
1. **GLM web search is off by default.** `web_search.enable` defaults to `false`, and no page says any model searches without the tool. The old `{type:"web_search",web_search:{enable:false}}` is redundant and schema-invalid (`search_engine` is `required`). Through OpenRouter it is actively harmful (§4.7). Just omit `tools` [glm-zchat].
2. **GLM thinking and effort defaults.**
   - `thinking` defaults to `enabled`, and `disabled` is an error on 5.3.
   - Omitting `reasoning_effort` means `max` on 5.2+.
   - `clear_thinking` defaults to `true`, so the server **drops replayed `reasoning_content`** unless you send `clear_thinking:false` [glm-zchat], [glm-zmode].
   - GLM also has narrower ranges: temperature ≤1.0, `tool_choice` auto only, one stop word on intl, no `json_schema`/`seed`/`n`/penalties.
   - `glm-5.3` is text-only; use `glm-5.3-flash*` for media.
3. **`kimi-k2.5` returns 404 since Aug 2026, and `kimi-k2.6` retires end of Dec 2026** [k-changelog].
   - K3 always thinks (`reasoning_effort` low/high/max, default max).
   - Its temperature/top_p/n/penalties are fixed (other values error).
   - Replaying `reasoning_content` is **required** [k-params], [k-thinking].
4. **Kimi search, images and tokens.**
   - `$web_search` is undocumented/legacy. The official built-in search is the Responses-API `web_search` (K3 only), which Moonshot currently calls "not recommended for production workflows in the near term" [k-resp], [k-k31].
   - Public image URLs are rejected (use base64 or `ms://`).
   - `max_tokens` is deprecated in favour of `max_completion_tokens`.
5. **qwen3.8 reasoning replay.** `preserve_thinking` defaults to `true`, so `reasoning_content` must be echoed in its own field. A missing echo produces no error, only silent degradation. The generic multi-turn page still says the opposite [q-occ], [q-mt].
6. **DashScope endpoint and search.**
   - The legacy `dashscope(-intl).aliyuncs.com` domains are frozen after 2026-09-30 (600 s timeout, 16 MiB body) [q-regions].
   - OpenAI-compatible mode never returns search sources. qwen3.8 can't use `search_strategy:"agent"` there [q-ws].
   - DashScope silently skips search above 15 RPS, and silently truncates non-stream responses after 300 s [q-ws-zh], [q-occ].
7. **Errors on HTTP 200.** All three vendors end some failures inside an HTTP 200 stream (GLM `finish_reason` `sensitive`/`network_error`), or as moderation 400s triggered by the *output* (Kimi `content_filter`, Qwen `DataInspectionFailed`). The bot must show them verbatim and keep the partial text clearly marked.

### Implications for the Rust bot
- **What OpenRouter gives you.** It is a transport for the raw model (when pinned to the official host) with OR-normalised semantics. It cannot reach any vendor built-in tool: search, code interpreter, `file-extract`, Qwen-Long, retrieval or MCP. Async APIs and vendor-stateful Responses features are also out of reach. Keep the direct-API backends for those.
- **Pinning the official host.** Use `provider: {"only":["z-ai"|"moonshotai"|"alibaba"], "allow_fallbacks": false, "require_parameters": true}`.
  - Expose `debug: {"echo_upstream_body": true}` (streaming only) as a transparency/debug toggle to show exactly what OR sent upstream.
  - Check every **UNVERIFIED** mapping below with it before relying on it.
- **Store reasoning verbatim.** Store `reasoning_content` (and OR `reasoning_details`) per assistant message, *including tool-call turns*, and replay it.
  - Required: K3.
  - Expected by default: qwen3.8 (`preserve_thinking`).
  - Needed for GLM preserved thinking (`clear_thinking:false`) and tool loops.
- **Never send vendor-native tool objects to OR.** OR rewrites any `{"type":"web_search",…}` into its own Exa-backed server tool (§4.7).
- **Treat HTTP 200 as possibly failed.**
  - GLM: `finish_reason` `sensitive`/`network_error`.
  - OR: `finish_reason:"error"` + top-level `error`, and `native_finish_reason`.
  - DashScope: the non-stream partial-response header.
  - Show these verbatim.

## 1. Zhipu GLM: BigModel (CN, `open.bigmodel.cn`) / Z.ai (intl, `api.z.ai`)
### 1.1 API surfaces and models
| Surface | China | International | Notes |
|---|---|---|---|
| Chat Completions (OpenAI-style, native) | `POST https://open.bigmodel.cn/api/paas/v4/chat/completions` | `POST https://api.z.ai/api/paas/v4/chat/completions` | `Authorization: Bearer <key>` [glm-cnapi](https://docs.bigmodel.cn/cn/api/introduction), [glm-zapi]; intl accepts optional `Accept-Language: en-US,en` [glm-zchat] |
| Coding Plan chat | `…/api/coding/paas/v4` | `https://api.z.ai/api/coding/paas/v4` | Since 2026-10-16, Standard and Coding Plan keys are separate; the **key type decides billing** [glm-keys]. Coding Plan "may only be used within officially supported tools", so it is not usable for a bot [glm-policy](https://docs.z.ai/devpack/usage-policy) |
| Anthropic Messages | `https://open.bigmodel.cn/api/anthropic` (`/v1/messages`, `x-api-key`) | `https://api.z.ai/api/anthropic` | "differences in some scenarios" [glm-claude](https://docs.bigmodel.cn/cn/guide/develop/claude/introduction), [glm-keys]; thinking/signature behaviour **UNVERIFIED** |
| Responses API | `https://open.bigmodel.cn/api/v1/responses` (+`GET/DELETE /v1/responses/{id}`, input items) | `https://api.z.ai/api/v1` listed, no intl reference | `store` defaults to `false`; no `data: [DONE]`; no cancel; `previous_response_id` lives 7 days; timeout 7200 s [glm-resp], [glm-respref], [glm-z53]. Accounts that ever had a Coding Plan can "currently only" use Chat Completions [glm-c53](https://docs.bigmodel.cn/cn/guide/models/text/glm-5.3), [glm-z53] |
| Async chat / Batch | `/api/paas/v4/async/chat/completions` + `/async-result/{id}`; `/api/paas/v4/batches` | not documented (intl OpenAPI has only image/video async) [glm-zoas] | §1.8 |
| Standalone tools | `/paas/v4/web_search`, `/paas/v4/reader` | same | separate tool APIs, not chat tools [glm-zoas] |

Doc inconsistency: the Z.ai GLM-5.3 page gives `/api/coding/paas/v4` as the "OpenAI Chat Completion Protocol" URL [glm-z53], while the API intro uses `/api/paas/v4` [glm-zapi].

Models [glm-cnover](https://docs.bigmodel.cn/cn/guide/start/model-overview), [glm-cnprice], [glm-zprice], [glm-cnparam]; "Thinking" column per [glm-cnchat], [glm-zthink]:

| model id | ctx / max out | input | thinking | CN ¥/M in·out·cache-hit | Intl $/M in·cached·out |
|---|---|---|---|---|---|
| `glm-5.3` | 1M / 128K | text only | forced; `reasoning_effort` low/high/max | 8·28·2 | 1.4·0.26·4.4 |
| `glm-5.3-flash` / `-flashx` / `-flashx-legal` | 1M / 128K | text, image, video, file | forced | 0.8·2.8·0.23 / 2·7·0.57 / 2·7·0.57 | 0.15·0.03·0.50 / 0.37·0.075·1.25 / = flashx |
| `glm-5.2` | 1M / 128K | text | default on, can disable | 8·28·2 | 1.4·0.26·4.4 |
| `glm-5.1`, `glm-5`, `glm-5-turbo` (CN only) | 200K / 128K | text | auto | 6–8·24–28 / 4–6·18–22 / 5–7·22–26 (tiered at 32K input) | 5.1: 1.4·0.26·4.4; 5: 1·0.2·3.2 |
| `glm-4.7` (+`-flash` free, `-flashx`) | 200K / 128K | text | forced | tiered 2–4·8–16 | 0.6·0.11·2.2 |
| `glm-4.6` | 200K / 128K | text | hybrid/auto | not on CN price page | 0.6·0.11·2.2 |
| `glm-4.5-air` / `-airx` / `-flash` | 128K / 96K | text | auto | air 0.8·2–8 | air 0.2·0.03·1.1 |
| `glm-5v-turbo` (CN), `glm-4.6v` (+flash/flashx), `glm-4.5v`, `glm-4.1v-thinking-*` | 200K/128K; 128K/32K; 64K/16K; 64K/16K | vision | 4.6V auto, 4.5V forced | [glm-cnprice] | 4.6V 0.3·0.05·0.9 |

- **Cache storage** on CN is "限时免费" (free for a limited time).
- **`max_tokens`** defaults to 65536 on 4.6+ text models and 5.3-flash; max 131072 (98304 on 4.5) [glm-cnparam].
- **Plain `glm-4.5`** is on the Z.ai price list but not in the CN model enum, overview or pricing; possibly retired on CN (**UNVERIFIED**).

### 1.2 Thinking / reasoning

```json
{"model":"glm-5.2","thinking":{"type":"enabled","clear_thinking":false},"reasoning_effort":"high"}
```

**`thinking.type`** takes `enabled`|`disabled`; the default is **`enabled`** (verified in [glm-zchat]; also [glm-cnparam]).
- `GLM-5.3`/`5.3-FLASH` "can only be enabled". `disabled` is an error [glm-zthink], [glm-z53].
- GLM-4.7 "will think compulsorily". 5.2/5.1/5/4.6 "automatically determine whether to think" [glm-zchat].
- The thinking-mode guide says thinking is "activated by default" for 5.x/4.7, unlike hybrid 4.6 [glm-zmode].

**`reasoning_effort`** is top-level: "takes effect when `thinking` is enabled. Default is `max`, supported by `GLM-5.2` and above" (verified) [glm-zchat].
- 5.3 family: `low|high|max` only; other values error [glm-zthink].
- 5.2: `none`/`minimal` skip thinking, `low`/`medium`→`high`, `xhigh`→`max`.
- The Coding Plan endpoint maps differently [glm-zthink]. Responses API: `reasoning.effort`, same mapping [glm-respref].

**Output:**
- `message.reasoning_content` (non-stream) / `delta.reasoning_content` (stream) [glm-zthink], [glm-zstream].
- GLM-4.5V may put `<think></think>` and `<|begin_of_box|>…<|end_of_box|>` inside `content` [glm-cnchat].
- Usage: `completion_tokens_details.reasoning_tokens` [glm-cncache]; Responses: `output_tokens_details.reasoning_tokens` [glm-respref].

**Multi-turn: `thinking.clear_thinking`** (bool, "Default value is True", verified) [glm-zchat]
- `true`: "the system ignores/removes `reasoning_content` from prior turns, and only keeps non-reasoning context (… tool calls, and tool results)".
- `false`: retains it, and "you must forward the full, unmodified, and correctly ordered historical `reasoning_content`". Missing or edited blocks "may degrade performance or prevent the feature"; **no error is documented**.
- It only affects cross-turn history, not whether the current turn thinks.
- Preserved thinking defaults **on for the Coding Plan endpoint, off for the standard endpoint** [glm-zmode].
- GLM-5.3-Flash recommends `clear_thinking:false` + `stream` + `tool_stream` [glm-zflash](https://docs.z.ai/guides/vlm/glm-5.3-flash).

**Interleaved thinking with tools** is on by default. "Thinking blocks should be explicitly preserved and returned together with the tool results" [glm-zmode]. The official example echoes `reasoning_content` on the assistant tool-call message, even though the request schema doesn't list the field:

```json
{"role":"assistant","content":"","reasoning_content":"<exact streamed reasoning>",
 "tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Beijing\"}"}}]},
{"role":"tool","tool_call_id":"call_1","content":"{\"weather\":\"Sunny\"}"}
```

GLM-4.7+ allows a per-request `thinking.type` toggle within one conversation [glm-zmode]. Responses replay item: `{"type":"reasoning","content":{"type":"reasoning_text","text":"…"}}` [glm-respref].

### 1.3 Built-in (server-side) tools

```json
"tools":[{"type":"web_search","web_search":{"enable":true,"search_engine":"search_pro","search_result":true,
  "count":10,"search_domain_filter":"www.example.com","search_recency_filter":"oneWeek","content_size":"medium",
  "result_sequence":"after","require_search":false,"search_prompt":"…{{search_result}}…"}}]
```

| `web_search` field | default / values | src |
|---|---|---|
| `enable` | **`false`** ("Default is `false`", verified) | [glm-zchat], [glm-cnchat] |
| `search_engine` (**`required`** in schema, verified) | CN: `search_std` (default), `search_pro`, `search_pro_sogou`, `search_pro_quark`. Intl chat schema: only `search_pro_jina` (default). The intl guide example uses `search-prime` (the standalone intl Web Search API engine) | [glm-cnchat], [glm-zchat], [glm-zws], [glm-zwsapi](https://docs.z.ai/api-reference/tools/web-search) |
| `search_query` | "Force trigger a search" | [glm-zchat] |
| `search_intent` (CN) | default runs intent detection; `false` always searches | [glm-cnchat] |
| `count` | 1–50, default 10 (sogou 10/20/30/40/50) | [glm-cnchat] |
| `search_domain_filter` | one whitelist domain | [glm-cnchat] |
| `search_recency_filter` | `oneDay\|oneWeek\|oneMonth\|oneYear\|noLimit` (default) | [glm-cnchat] |
| `content_size` | `medium` (default, 400–600 chars) or `high` (~2500 chars) | [glm-zchat] |
| `result_sequence` | `before` or `after` (default) | [glm-cnchat] |
| `search_result` | `false`: return sources in the response | [glm-cnchat] |
| `require_search` | `false`: force the answer to be based on search results | [glm-cnchat] |
| `search_prompt` | default ends "State the answer directly without citing data sources", so there are no inline citations unless overridden | [glm-zchat] |

- **Response:** top-level `web_search: [{title, content, link, media, icon, refer:"ref_1", publish_date}]` [glm-cnchat], [glm-zws]. Its position in a *stream* is not in the chunk schema (**UNVERIFIED**).
- **Model support:** examples use `glm-4-air`. GLM-5.x model pages don't list web search, so chat web_search on GLM-5.x is **UNVERIFIED** [glm-cnws], [glm-c52].
- **Doc contradiction:** Z.ai's `tools` text says "Currently, only functions are supported" while listing the retrieval and web_search schemas [glm-zchat].
- **Pricing:** CN `search_std` ¥0.01, `search_pro` ¥0.03, `search_pro_sogou`/`search_pro_quark` ¥0.05 per search [glm-cnprice], [glm-cnws]; intl "Web Search $0.01 / use" [glm-zprice].
- **Responses API search:** bare `{"type":"web_search"}`. Output item `web_search_call{action:{type:"search",queries[],source[]},result[]}`. Stream events `response.web_search_call.in_progress|searching|completed` [glm-respref].
- **Other official tools:**
  - `{"type":"retrieval","retrieval":{"knowledge_id","prompt_template":"…{{knowledge}}…{{question}}…"}}` (CN knowledge base) [glm-cnret](https://docs.bigmodel.cn/cn/guide/tools/knowledge/retrieval), [glm-cnchat].
  - CN-only `{"type":"mcp","mcp":{"server_label","server_url","transport_type":"streamable-http|sse","allowed_tools":[],"headers":{}}}`, whose results come as `tool_calls[].type:"mcp"` (`mcp_list_tools|mcp_call`, `output`/`error`) [glm-cnchat].
  - **No code-interpreter chat tool** (a `code-interpreter` Files purpose exists) [glm-cnfile].

### 1.4 Multimodal input
Vision models: `glm-5.3-flash*`, `glm-5v-turbo`, `glm-4.6v*`, `glm-4.5v`, `glm-4.1v-thinking-*`, `glm-4v-flash`. `glm-5.3` and `glm-5.2` are text-only [glm-z53], [glm-c52].

| part | shape | limits | src |
|---|---|---|---|
| image | `{"type":"image_url","image_url":{"url":"https://…"\|base64}}` | ≤5 MB, ≤6000×6000, jpg/png/jpeg. CN ≤50 images (5.3-Flash/5V-Turbo/4.6V/4.5V); 1 for 4V-Flash (no base64). Intl: "GLM-5V limited to 150" | [glm-cnchat], [glm-zchat] |
| video | `{"type":"video_url","video_url":{"url":"…"}}` | ≤200 MB, mp4/mkv/mov; URL (base64 **UNVERIFIED**) | [glm-cnchat] |
| file | `{"type":"file","file":{"file_id"\|"file_url"\|"file_data":"data:<MIME>;base64,…","filename":"…"}}` | ≤50 MB each, ≤50 files; pdf/txt/word/jsonl/xlsx/pptx. Intl: can't be mixed with image/video | [glm-cnchat], [glm-zchat] |
| audio (CN `glm-4-voice`) | `{"type":"input_audio","input_audio":{"data":"<b64>","format":"wav"\|"mp3"}}` | ≤10 min | [glm-cnchat] |

- **Files:** `file_id` comes from `POST /paas/v4/files` (CN purposes `batch|code-interpreter|agent|voice-clone-input|user_data`; intl `user_data|agent`). Zhipu parses files server-side [glm-cnfile], [glm-zoas].
- **Tools on vision models:** only 5.3-Flash, 4.6V and AutoGLM-Phone accept `tools` [glm-cnchat].
- **Responses parts:** `input_text`, `input_image{image_url}`, `input_file{file_data|file_url}` [glm-respref].

### 1.5 Other parameters, JSON mode, caching
| param | values / default | src |
|---|---|---|
| `temperature` | [0.0, **1.0**], 2 decimals. Default 1.0 (5.x/4.7/4.6), 0.6 (4.5), 0.8 (4.6V/4.5V) | [glm-cnchat] |
| `top_p` | [0.01, 1.0], default 0.95 | [glm-cnchat] |
| `do_sample` | `true`. `false` = greedy, ignores temperature/top_p; the OpenAI-SDK note says temperature=0 "does not apply" | [glm-cnchat], [glm-cnoai](https://docs.bigmodel.cn/cn/guide/develop/openai/introduction) |
| `max_tokens` | 1–131072 | [glm-cnparam] |
| `stop` | array, maxItems 4. Intl: "currently only one stop word is supported" | [glm-zchat] |
| `tool_choice` | only `"auto"` (Responses: `none\|auto`) | [glm-zfc](https://docs.z.ai/guides/capabilities/function-calling), [glm-respref] |
| `tools` | ≤128 functions | [glm-zchat] |
| `tool_stream` | `false`. Streams `tool_calls[].function.arguments` incrementally; needs `stream:true`. API ref: GLM-4.6…5.3; guide: "limited to glm-5" | [glm-zchat], [glm-ztool](https://docs.z.ai/guides/capabilities/stream-tool) |
| `response_format` | `{"type":"text"\|"json_object"}`; **no `json_schema`** | [glm-zchat], [glm-zstruct] |
| `request_id` / `user_id` | 6–64 chars, client-unique (auto if absent) / 6–128 chars | [glm-zchat] |

- **Not documented** in either doc set: `seed`, `n`, `top_k`, `presence/frequency/repetition_penalty`, `logprobs`, `parallel_tool_calls`, `stream_options`, and any partial/prefill mode.
- **Context cache:** implicit/automatic only, reported as `usage.prompt_tokens_details.cached_tokens` [glm-zcache](https://docs.z.ai/guides/capabilities/cache).
  - CN advises an identical prefix of ≥500 tokens and says hits are billed "通常 50%" (usually 50%); the price tables show lower ratios [glm-cncache].
  - No explicit cache API; Responses has `prompt_cache_key` [glm-respref].

### 1.6 Streaming
SSE `data: {json}` lines ending with `data: [DONE]` [glm-zstream]:

```
data: {"id":"1","created":1677652288,"model":"glm-5.3","choices":[{"index":0,"delta":{"reasoning_content":"Let me"},"finish_reason":null}]}
data: {"id":"1",…,"choices":[{"index":0,"delta":{"content":"Spring"},"finish_reason":null}]}
data: {"id":"1",…,"choices":[{"index":0,"finish_reason":"stop","delta":{"role":"assistant","content":""}}],"usage":{"prompt_tokens":8,"completion_tokens":262,"total_tokens":270,"prompt_tokens_details":{"cached_tokens":0}}}
data: [DONE]
```

- **`delta` fields:** `role`, `content`, `reasoning_content`, `tool_calls[{index,id,type:"function",function:{name,arguments}}]`, `audio{id,data,expires_at}`. Top-level `content_filter[{role:"assistant|user|history",level:0-3}]` [glm-cnchat].
- **Usage** comes **automatically in the last chunk** with `finish_reason`; no `stream_options` needed [glm-zstream].
- **`finish_reason`:** `stop`, `length`, `tool_calls`, `sensitive` (moderation block; developer "should retract published content"), `network_error` (inference failure), `model_context_window_exceeded` [glm-cnchat].
- **Mid-stream failures:** "if the API terminates abnormally during inference, the above error codes will not be returned… the reason… in `finish_reason`" [glm-zerr].
  - CN content safety: a stream may end with code 1301 or `finish_reason:"sensitive"` + `content_filter`, and already-shown text should be withdrawn [glm-cnsafe].
  - A JSON error object inside SSE is **UNVERIFIED**.
- **Responses API stream:** typed events with `sequence_number`; **no `[DONE]`** [glm-respref].
  - Lifecycle: `response.created|in_progress|completed|failed|incomplete`.
  - Items and deltas: `response.output_item.*`, `response.content_part.*`, `response.output_text.delta/done`, `response.function_call_arguments.delta/done`, `response.reasoning_text.delta/done`.
  - Tools and errors: `response.web_search_call.*`, `error`.

### 1.7 Errors
**Envelope:** `{"error":{"code":"1214","message":"…"}}`. `code` is a **string** (live probe; the intl schema says int32) [glm-zerr].
- 1301 also returns top-level `"contentFilter":[{"level":1,"role":"user"}]` [glm-cnsafe].
- Responses API uses string codes: `invalid_request`, `model_not_found`, `context_length_exceeded`, `rate_limit_exceeded`, `insufficient_quota`, `server_error`, `server_is_overloaded|overloaded|slow_down`, `content_filter|cyber_policy`, … [glm-respref].

| code | HTTP | meaning | retry? |
|---|---|---|---|
| 1000/1001/1003/1005 | 401 | auth failed / no header / token expired / 2FA | no |
| 1113 | 429 | CN arrears; intl no balance/resource package | no (top up) |
| 1210–1215, 1261 | 400 | bad params / unknown model / unsupported method / missing-invalid field / conflict / prompt too long | no |
| 1220–1222 | 403/400 | no permission / API offline / missing | no |
| 1200, 1230, 1234 | 500 | API call / process / network error ("try again later") | yes, backoff |
| 1301 | 400 | unsafe input/output content | no |
| 1302 | 429 | account rate limit (CN limits are per-model **concurrency**) | yes, backoff + less concurrency [glm-cnrl] |
| 1305 | 429 | platform/model overloaded | yes, longer intervals [glm-cnrl] |
| 1308, 1310, 1316–1321 | 429 | quota window exhausted (message has `next_flush_time`) | after reset |
| 1309, 1311, 1313–1315 | 429 | Coding Plan expired / model not in plan / fair-use / enterprise misuse | no |

Sources for the table: [glm-zerr], [glm-cnerr](https://docs.bigmodel.cn/cn/api/api-code). The retry column is inferred; only 1302 and 1305 have explicit guidance [glm-cnrl].

Live probes by the research agent (no or invalid key, 2026-11-11):
- `api.z.ai` returned `{"error":{"code":"1000","message":"身份验证失败。"}}`, i.e. Chinese text even with `Accept-Language: en-US`.
- Anthropic endpoint: `{"error":{"message":"…","type":"1001"}}`.
- `POST https://api.z.ai/api/v1/responses` without a key → **HTTP 200** + `{"code":1001,"msg":"…","success":false}`.

### 1.8 Async, batch, resume
- **CN async chat:** `POST /api/paas/v4/async/chat/completions` (sync body minus `stream`) → `{"model","id","request_id","task_status":"PROCESSING"}`.
  - Poll `GET /api/paas/v4/async-result/{id}`, which returns the full completion + `task_status` `PROCESSING|SUCCESS|FAIL` [glm-cnasync](https://docs.bigmodel.cn/api-reference/模型-api/对话补全异步), [glm-cnasyncres](https://docs.bigmodel.cn/api-reference/模型-api/查询异步结果).
  - The spec also lists a `text/event-stream` response with no parameter to request it, and result retention is undocumented (both **UNVERIFIED**). Intl: not documented.
- **CN batch:** `.jsonl` lines `{custom_id, method:"POST", url:"/v4/chat/completions", body}`, ≤50,000 requests / ≤100 MB, 50% price. Statuses include `completed|failed|expired|cancelled` [glm-cnbatch](https://docs.bigmodel.cn/cn/guide/tools/batch).
- **Responses:** `store:true` then `GET /v1/responses/{id}`. No stream replay (`starting_after`), no background mode, no cancel [glm-respget](https://docs.bigmodel.cn/api-reference/response/查询-response), [glm-resp].
- **No GLM surface documents stream resume.** Fallbacks: CN async chat (non-streamed) or re-send; `request_id` idempotency is **UNVERIFIED**.

[glm-zchat]: https://docs.z.ai/api-reference/llm/chat-completion
[glm-cnchat]: https://docs.bigmodel.cn/api-reference/模型-api/对话补全
[glm-zapi]: https://docs.z.ai/api-reference/introduction
[glm-zoas]: https://docs.z.ai/openapi.json
[glm-zmode]: https://docs.z.ai/guides/capabilities/thinking-mode
[glm-zthink]: https://docs.z.ai/guides/capabilities/thinking
[glm-zstream]: https://docs.z.ai/guides/capabilities/streaming
[glm-zstruct]: https://docs.z.ai/guides/capabilities/struct-output
[glm-cncache]: https://docs.bigmodel.cn/cn/guide/capabilities/cache
[glm-zws]: https://docs.z.ai/guides/tools/web-search
[glm-cnws]: https://docs.bigmodel.cn/cn/guide/tools/web-search
[glm-zprice]: https://docs.z.ai/guides/overview/pricing
[glm-cnprice]: https://docs.bigmodel.cn/cn/guide/start/pricing
[glm-cnparam]: https://docs.bigmodel.cn/cn/guide/start/concept-param
[glm-resp]: https://docs.bigmodel.cn/cn/guide/develop/responses/introduction
[glm-respref]: https://docs.bigmodel.cn/api-reference/response/创建-response
[glm-z53]: https://docs.z.ai/guides/llm/glm-5.3
[glm-c52]: https://docs.bigmodel.cn/cn/guide/models/text/glm-5.2
[glm-zerr]: https://docs.z.ai/api-reference/api-code
[glm-cnrl]: https://docs.bigmodel.cn/cn/api/rate-limit
[glm-cnsafe]: https://docs.bigmodel.cn/cn/guide/platform/securityaudit
[glm-cnfile]: https://docs.bigmodel.cn/api-reference/文件-api/上传文件
[glm-keys]: https://docs.z.ai/devpack/notice/api-key-management

## 2. Moonshot Kimi (Kimi Open Platform)
**Docs have moved.**
- Docs now live at **platform.kimi.com/docs** (CN) and **platform.kimi.ai/docs** (intl). `platform.moonshot.{cn,ai}/docs/*` 301-redirects there (checked: `platform.moonshot.ai/docs/api/chat` → `platform.kimi.ai/docs/api/chat`).
- **API hosts are unchanged:** `api.moonshot.cn` / `api.moonshot.ai` [k-overview-cn](https://platform.kimi.com/docs/api/overview.md), [k-overview].
- Accounts, balances and keys are per site. A key used on the other site gets 401 [k-errors-cn](https://platform.kimi.com/docs/api/errors.md), [k-trouble].

### 2.1 API surfaces and models
| Surface | CN base | Intl base | Endpoint | Models |
|---|---|---|---|---|
| OpenAI Chat Completions | `https://api.moonshot.cn/v1` | `https://api.moonshot.ai/v1` | `POST /chat/completions` | all current [k-chat] |
| OpenAI Responses (**stateless**) | same `/v1` | same | `POST /responses` | **K3 series only** [k-resp] |
| Anthropic Messages | `https://api.moonshot.cn/anthropic` | `https://api.moonshot.ai/anthropic` | `POST /v1/messages` | **K3 series only** [k-msgs] |
| Models / token estimate / balance | `/v1/models`, `/v1/tokenizers/estimate-token-count`, `/v1/users/me/balance` | same | | [k-overview] |
| Files / Batch | `/v1/files[/{id}[/content]]`; `/v1/batches[/{id}[/cancel]]` | same | | [k-overview] |
| REST search/fetch tools | `/v1/tools/search`, `/v1/tools/search_pro`, `/v1/tools/fetch` | same | | [k-overview] |
| Formula (official tools) | `GET /v1/formulas/{uri}/tools`, `POST /v1/formulas/{uri}/fibers` | same | | [k-official] (not in openapi.json [k-openapi]) |
| Response signature | `POST /v1/signatures/verify` | same | | [k-sig] |

`/v1/models` returns `id`, `context_length`, `supports_image_in`, `supports_video_in` and `supports_reasoning`, which the bot can use as runtime capability flags [k-listmodels](https://platform.kimi.ai/docs/api/list-models.md).

Models [k-models](https://platform.kimi.ai/docs/models.md), [k-models-cn](https://platform.kimi.com/docs/models.md), [k-price], [k-changelog]:

| Model | Status | Context | Thinking | Vision |
|---|---|---|---|---|
| `kimi-k3.1-pro` | current flagship | 1,048,576 | always on | image + video [k-vision] |
| `kimi-k3.1-pro-highspeed` | current, 2–3× faster | 1,048,576 | always on | **UNVERIFIED** (not in vision guide; check `supports_image_in`) |
| `kimi-k3` | current (previous flagship) | 1,048,576 | always on | image + video |
| `kimi-k2.7-code` (+`-highspeed`) | closed to users registered after Nov 1; retiring end of Dec 2026 | 262,144 | always on | image + video |
| `kimi-k2.6` | same status as k2.7-code | 262,144 | on by default, can be disabled | image + video |
| `kimi-k2.5`, all `moonshot-v1-*` | **retired Aug 2026; calls return 404 "model not found"** (verified in [k-changelog]) | — | — | — |
| `kimi-k2-*` (incl. `kimi-k2-thinking(-turbo)`), `kimi-latest` | retired (K2 series May 2026) [k-changelog] | — | — | — |

### 2.2 Request parameters (Chat Completions)
"Fixed" means any other value is an error, so omit the field [k-params], [k-chat].

| Field | K3 series | `kimi-k2.7-code` | `kimi-k2.6` |
|---|---|---|---|
| `reasoning_effort` | `"low"`/`"high"`/`"max"`, default `"max"`. Changing it mid-session breaks the prefix cache | not supported | not supported |
| `thinking` | not in schema; don't pass it (behaviour if passed **UNVERIFIED**) | only `{"type":"enabled","keep":"all"}`; `"disabled"` → error | `{"type":"enabled"}` (default) \| `{"type":"disabled"}`, plus `keep: null` (default) \| `"all"` |
| `temperature` | fixed 1.0 | fixed 1.0 | 1.0 thinking / 0.6 non-thinking |
| `top_p` / `n` / `presence_penalty` / `frequency_penalty` | fixed 0.95 / 1 / 0 / 0 | same | same |
| `tool_choice` | `auto`/`none`/`required`/function object | `required` → error | `required` → error |
| `max_completion_tokens` | default 131072, max 1048576. Lowered automatically if prompt + max > context. `max_tokens` is deprecated | | |

All models accept [k-chat]:
- `messages[]` (roles `system|user|assistant|tool`, non-empty `content`, optional `name`, `partial` on the last assistant message only).
- `stop` (string or ≤5 strings, each ≤32 bytes), `stream`, `stream_options.include_usage`.
- `response_format` `text`|`json_object`|`json_schema{name,strict (default true),schema}`.
- `tools[]` of `type:function`. Names match `^[a-zA-Z_][a-zA-Z0-9-_]{0,127}$`; `strict` defaults to true; duplicate names → 400 [k-official].
- `prompt_cache_key`, `prompt_cache_options{mode:"implicit",ttl:"5m"|"1h"}`, `safety_identifier`, `logprobs`, `top_logprobs` (0–20), `prediction{type:"content",content}`.
- K3 only: dynamic tool messages `{"role":"system","tools":[...]}` with no `content` [k-dyn](https://platform.kimi.ai/docs/guide/use-dynamic-tool-loading.md).

Not available:
- `seed`, `top_k`, `repetition_penalty` and `user` are absent from the schema [k-openapi]. Whether they are rejected or ignored is **UNVERIFIED**.
- `n>1` → 400 `invalid n: only 1 is allowed for this model` [k-stream].
- A forced specific function while thinking → 400 `tool_choice 'specified' is incompatible with thinking enabled` [k-toolchoice](https://platform.kimi.ai/docs/guide/use-tool-choice.md).

Headers:
- `X-Msh-Request-Nonce` → the response carries `Msh-Request-Timestamp` + `Msh-Request-Signature` (`reqsigv1_…`), which `/v1/signatures/verify` checks. This proves the official API served the named model [k-sig].
- `X-Msh-Client-Request-Id` → stored as `client_request_id` in billing records [k-interrupted].

### 2.3 Thinking and `reasoning_content` across turns
- Reasoning arrives in `message.reasoning_content` (deltas when streaming), always before `content`/`tool_calls`. It counts toward `max_completion_tokens` [k-thinking].
- **K3:** sending back the complete assistant message unchanged (`reasoning_content` + `tool_calls`) is **required** in multi-turn chats and tool loops [k-thinking], [k-effort](https://platform.kimi.ai/docs/guide/use-reasoning-effort.md), [k-k31].
- **k2.7-code:** Preserved Thinking is always on.
- **k2.6:** `keep:null` (default) means the server ignores historical reasoning; `keep:"all"` keeps it. Within one tool loop, always send every `reasoning_content` back [k-thinking].
- The error text when reasoning is missing is **UNVERIFIED** (undocumented). One doc inconsistency: the official-tools sample rebuilds assistant messages without `reasoning_content` [k-official].
- **Tool calls:** every `tool_call` needs a `role:"tool"` reply with the same `tool_call_id`, in any order. Otherwise you get an error (`tool_call_id not found`) [k-tools](https://platform.kimi.ai/docs/guide/use-kimi-api-to-complete-tool-calls.md).
- **Responses API:** `reasoning.effort`. `reasoning` items carry text in `summary[].summary_text`, and `encrypted_content` is always null. Replay as input `reasoning` items with `content[]` (`reasoning_text`) or `summary` [k-resp].
- **Messages API:** `output_config.effort`. Return `thinking` blocks with `signature` unchanged [k-msgs].

```json
POST https://api.moonshot.ai/v1/chat/completions
{"model":"kimi-k3.1-pro","reasoning_effort":"high","stream":true,"stream_options":{"include_usage":true},
 "messages":[
  {"role":"user","content":"What is the weather in Beijing today?"},
  {"role":"assistant","content":"","reasoning_content":"...",
   "tool_calls":[{"id":"get_weather_0_4d9e2559","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Beijing\"}"}}]},
  {"role":"tool","tool_call_id":"get_weather_0_4d9e2559","content":"Sunny, 25°C"}],
 "tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]}
```

For k2.6 the switch is `"thinking":{"type":"enabled","keep":"all"}` (`extra_body` in the OpenAI SDK) [k-params].

### 2.4 Built-in / official tools and web search
**`$web_search` builtin_function is gone from the official docs.**
- The old `/docs/guide/use-web-search` page 308-redirects to "Best Practices for Web Search". Neither that page, the API reference nor openapi.json mentions `builtin_function`/`$web_search` [k-oldws](https://platform.kimi.ai/docs/guide/use-web-search), [k-wsbp](https://platform.kimi.ai/docs/guide/best-practices-for-web-search.md), [k-openapi]. Only one stray line in the streaming guide still lists `builtin_function` as a `tool_call.type` [k-stream].
- A third-party article quotes the old page as "retires on October 20, 2026" (**UNVERIFIED**) [k-3p-parallel](https://parallel.ai/articles/kimi-k3-web-search).

**Legacy echo flow** (third-party code only, **UNVERIFIED**) [k-3p-leanpub](https://leanpub.com/read/clojureai/using-moonshots-kimi-k2-model-with-built-in-web_search-tool-support):
1. Declare `{"type":"builtin_function","function":{"name":"$web_search"}}`.
2. The model returns `finish_reason:"tool_calls"`.
3. Append the assistant message verbatim plus `{"role":"tool","tool_call_id":<id>,"name":"$web_search","content":<function.arguments echoed verbatim>}`, and re-send.
4. The server performs the search and answers. Search tokens are reported inside `arguments`.

**Status:**
- K3/K3.1 quickstarts: "Web search is being updated and is not recommended for production workflows in the near term" [k-k31], [k-k31-cn](https://platform.kimi.com/docs/guide/kimi-k3-1-pro-quickstart.md).
- "Web search is not enabled automatically for every API request" [k-trouble].

| Channel | How | Executed by | Price intl / CN |
|---|---|---|---|
| Responses API `web_search` tool (K3 only) | `tools:[{"type":"web_search","filters":{"allowed_domains":[≤100]},"search_content_types":["text"\|"image"],"image_settings":{"max_results":1-10,"caption":false}}]`, `include:["web_search_call.action.sources","web_search_call.results"]` | **server-side built-in** | $0.005 / ¥0.03 per executed search [k-wsprice], [k-wsprice-cn](https://platform.kimi.com/docs/pricing/websearch) |
| Formula `moonshot/web-search:latest` | Client fetches `/tools`; the model emits a normal `function` call; the client POSTs `{"name","arguments"}` to `/fibers` and returns `context.encrypted_output` as the tool message | Moonshot server, client-triggered | $0.005 / ¥0.03 per execution |
| REST `/v1/tools/search` | `text_query`, `limit` 1–20 (default 5), `timeout_seconds`, `include_content` | client-orchestrated | $0.002 / ¥0.01 |
| REST `/v1/tools/search_pro` | + `sites` (≤5), `time_window{start,end}`; returns `chunks[]{text,score}`, `authority` | client-orchestrated | $0.003 / ¥0.015 |
| REST `/v1/tools/fetch` | `url` → `markdown` | client-orchestrated | $0.002 / ¥0.01 |

- **Billing:** failed or empty calls are not billed. Search QPS is a separate quota (Tier0 1 … Tier5 50); exceeding it → 429 + `X-RateLimit-*` [k-wsprice], [k-limits].
- **Responses `web_search`** [k-resp]:
  - At most one per request. `search_context_size`/`blocked_domains` → `invalid_request_error`; `user_location` is ignored.
  - `web_search_call` comes first in `output`. Stream events: `response.web_search_call.in_progress|searching|completed`.
  - **Replayed `web_search_call` input items are ignored** ("search results are already carried by the assistant message that follows"). A stateless replay keeps only the answer text.
  - The `annotations` citation format is unspecified (**UNVERIFIED**). `tool_choice` accepts `auto` only.
- **Formula catalogue** [k-official]: `convert`, `web-search`, `rethink`, `random-choice`, `mew`, `memory`, `excel`, `date`, `base64`, `fetch` (`moonshot/<name>:latest`).
  - All are free for a limited time except `web-search`.
  - `web-search` output is opaque `----MOONSHOT ENCRYPTED BEGIN----…`, so it cannot be shown to users.
  - There is no code-execution tool.

```json
POST /v1/formulas/moonshot/web-search:latest/fibers   {"name":"web_search","arguments":"{\"query\":\"...\"}"}
→ {"id":"fiber-…","object":"fiber","status":"succeeded","context":{"input":"…","encrypted_output":"----MOONSHOT ENCRYPTED BEGIN----…"},"formula":"moonshot/web-search:latest"}
then {"role":"tool","tool_call_id":"web_search:0","content":"----MOONSHOT ENCRYPTED BEGIN----…"}
```

### 2.5 Multimodal and files
[k-vision], [k-chat], [k-resp], [k-msgs]

- **Images:** `{"type":"image_url","image_url":{"url":"data:image/png;base64,…" | "ms://<file_id>"}}`; a bare-string `image_url` also works.
  - **Public http(s) image URLs are not supported** on any API [k-k31].
  - Formats: jpeg/png/gif/webp/bmp/heic/heif. Animated GIF/WebP may be billed as video; SVG is rejected.
  - Size: ≤4096×2160 recommended. No count limit, but the body must be ≤100M.
- **Video:** `{"type":"video_url","video_url":{"url":"data:video/mp4;base64,…" | "ms://<file_id>"}}`. Formats mp4/mpeg/mov/avi/x-flv/mpg/webm/wmv/3gpp, ≤1080p recommended. Large videos must be uploaded with `purpose:"video"`.
- **Other APIs:** Responses `input_image.image_url` must be a data URL (`detail` `auto|low|high|original`). Messages image `source` is `base64` or `{"type":"url","url":"ms://<file_id>"}`.
- **Files** [k-upload](https://platform.kimi.ai/docs/api/files-upload.md), [k-content](https://platform.kimi.ai/docs/api/files-content.md), [k-fileqa](https://platform.kimi.ai/docs/guide/use-kimi-api-for-file-based-qa.md):
  - `purpose` ∈ `file-extract|image|video|batch`. Limits: ≤100 MiB/file and 10 GiB/org. Ids start with `file_`.
  - `file-extract` handles pdf/doc(x)/xls(x)/ppt(x)/txt/md/epub/html/json/code and returns **Markdown**. **Images are no longer OCR'd** (since Aug 2026).
  - The client puts the text into a `system` message (no file-id reference), so the extracted text stays visible in the request. Upload and extraction are free; the inlined text is billed as input.

```text
POST /v1/files (multipart file=@paper.pdf, purpose=file-extract) → {"id":"file_…","object":"file","purpose":"file-extract","status":"ready",…}
GET  /v1/files/file_…/content → text/plain (Markdown)
messages: [{"role":"system","content":"<extracted markdown>"},{"role":"user","content":"Summarize paper.pdf"}]
```

### 2.6 Partial mode, caching, JSON mode
**Partial mode** [k-partial](https://platform.kimi.ai/docs/guide/use-partial-mode-feature-of-kimi-api.md), [k-chat], [k-features]
- The last message is `{"role":"assistant","content":"<prefix>","partial":true}`. The returned `content` excludes the prefix; `name` counts as part of the prefix.
- With thinking, include the earlier `reasoning_content` on the partial message. If truncation happened mid-thinking, `content` is empty and generation restarts; continuing partial *reasoning* is undocumented.
- Do not combine it with `json_object`. Recommended for resuming interrupted output.

```json
{"model":"kimi-k3.1-pro","max_completion_tokens":86400,"messages":[
  {"role":"user","content":"Please recite the complete Chu Shi Biao."},
  {"role":"assistant","content":"<content so far>","partial":true,"reasoning_content":"<reasoning so far>"}]}
```

**Context cache** [k-cache], [k-chat]
- Automatic and implicit; the old explicit cache API is gone. `prompt_cache_options.ttl` is `5m` (default) or `1h`, locked at first write.
- **Cache write is billed on K3** (5m = input price, 1h = 2× input); K2.x has no write charge. Per-org, cannot be cleared, and `prompt_cache_breakpoint` → 400.
- Usage: `usage.prompt_tokens_details.cached_tokens` / `.cache_write_tokens`, plus headers `Msh-Usage-Cache-Write-Tokens-5m/-1h`. Messages API: top-level `cache_control{type:"ephemeral",ttl}`.

**JSON** [k-rf](https://platform.kimi.ai/docs/guide/response_format.md)
- `json_object` needs the fields described in the prompt.
- `json_schema` + `strict:true` uses constrained decoding. Non-conforming schemas may return 200 without warning.

### 2.7 Streaming and usage
[k-chat], [k-stream]

- `data: {json}` frames, ending with `data: [DONE]`. **Only `[DONE]` marks completion.**
- First chunk is `delta:{"role":"assistant","content":""}`. Chunks carry `system_fingerprint`.
- `delta.reasoning_content` comes first, then `content`/`tool_calls`. Tool-call fragments share an `index`; `id`/`type`/`name` appear only in the first fragment.
- With `stream_options.include_usage`, the last chunk before `[DONE]` has `choices:[]` + top-level `usage`. The guide example *also* shows `choices[0].usage` on the finish chunk, so read both.
- `finish_reason`: `stop|length|tool_calls`. `client_disconnected` appears only in Admin billing data [k-interrupted].
- Chat has no `content_filter` finish reason. Filtering is a 400 error, possibly triggered by the *output*. Its mid-stream form and the SSE error-frame format are **UNVERIFIED**.
- Responses API: `error`, `response.failed`, `response.incomplete` (`incomplete_details.reason ∈ {max_output_tokens, content_filter}`). Messages `stop_reason`: `end_turn|max_tokens|tool_use|refusal` [k-resp], [k-msgs].

```text
data: {"id":"chatcmpl-…","object":"chat.completion.chunk","created":1792741110,"model":"kimi-k3.1-pro","choices":[{"index":0,"delta":{"reasoning_content":"The"},"finish_reason":null}],"system_fingerprint":"fpv0_e7278693"}
data: {"id":"chatcmpl-…","object":"chat.completion.chunk","created":1792741110,"model":"kimi-k3.1-pro","choices":[],"usage":{"prompt_tokens":373,"completion_tokens":93,"total_tokens":466,"completion_tokens_details":{"reasoning_tokens":71},"prompt_tokens_details":{"cache_write_tokens":256}}}
data: [DONE]
```

When a stream breaks, the usage chunk is lost. The docs suggest counting the partial text with `/v1/tokenizers/estimate-token-count` [k-stream], [k-estimate](https://platform.kimi.ai/docs/api/estimate.md).

### 2.8 Errors
`{"error":{"type","message"[,"code"]}}` [k-errors](https://platform.kimi.ai/docs/api/errors.md), [k-chat]

| HTTP | `error.type` | Meaning | Retry? |
|---|---|---|---|
| 400 | `content_filter` | "The request was rejected because it was considered high risk" (input **or output**) | no (show verbatim) |
| 400 | `invalid_request_error` | bad params, "Input token length too long", bad `purpose`, file >100MB/empty | no |
| 401 | `invalid_authentication_error` / `incorrect_api_key_error` | bad key / key from the other site | no |
| 403 | `permission_denied_error` | API not enabled for account, IP allowlist | no |
| 404 | `resource_not_found_error` | missing/retired model (e.g. `kimi-k2.5`) | no |
| 429 | `engine_overloaded_error` | server capacity | **yes**, with `Retry-After` + exponential backoff |
| 429 | `rate_limit_reached_error` | org concurrency/RPM/TPM/TPD | yes, after waiting (TPD resets next day) |
| 429 | `exceeded_current_quota_error` | balance ≤ 0 / account disabled | no |
| 499 | `client_closed_request` | client/proxy disconnected | — |
| 500 | `server_error` / `unexpected_output` | internal | yes |
| 503 | `server_unavailable` | scaling/maintenance | yes |
| 504 | HTML "504 Gateway Time-out" | non-streaming request with no bytes for 900 s | use `stream:true` |

- **Search/fetch tool errors:** `invalid_request`, `rate_limited`, `rate_limit_unavailable`, `internal_error`, `timeout`, `security_risk` (403), `markdown_not_found` (404) [k-searchpro](https://platform.kimi.ai/docs/api/tools-search-pro.md), [k-fetch](https://platform.kimi.ai/docs/api/tools-fetch.md).
- **Hidden retries:** the OpenAI SDK's 2 automatic retries count toward RPM (Tier0 = 3 RPM) [k-trouble].

### 2.9 Async, batch, resume
- **No async chat and no stream resume.** Responses keeps no server state (`background`/`store`/`previous_response_id` are always false/null) [k-resp].
  - The official "auto-reconnect" pattern retries the whole request [k-reconnect](https://platform.kimi.ai/docs/guide/auto-reconnect.md). Partial Mode (re-sending the saved prefix) is suggested for resuming [k-features].
  - **A client disconnect still finishes and bills the request** [k-trouble]. The Admin interrupted-requests feed returns billing rows only [k-interrupted].
- **Batch** [k-batch](https://platform.kimi.ai/docs/guide/use-batch-api.md), [k-batchcreate](https://platform.kimi.ai/docs/api/batch-create.md), [k-batchprice](https://platform.kimi.ai/docs/pricing/batch):
  - JSONL input (`purpose:"batch"`, ≤100MB, one model per file, unique `custom_id`).
  - `POST /v1/batches {"input_file_id","endpoint":"/v1/chat/completions","completion_window":"24h"}` (12h–7d).
  - Statuses `validating → in_progress → completed|failed|expired|cancelled`.
  - **60%** of standard price; available for k3.1-pro, k3, k2.7-code and k2.6.

### 2.10 Pricing (per 1M tokens) and CN vs intl
[k-price], [k-price-cn](https://platform.kimi.com/docs/pricing/chat)

| Model | Input (miss) | Output | Cached | Write 5m | Write 1h | CN in / out / cached / 5m / 1h |
|---|---|---|---|---|---|---|
| kimi-k3.1-pro | $2.00 | $10.00 | $0.20 | $2.00 | $4.00 | ¥13 / 68 / 1.30 / 13 / 26 |
| kimi-k3.1-pro-highspeed | $4.00 | $20.00 | $0.20 | $4.00 | $8.00 | ¥26 / 136 / 1.30 / 26 / 52 |
| kimi-k3 | $3.00 | $15.00 | $0.30 | $3.00 | $6.00 | ¥20 / 100 / 2 / 20 / 40 |
| kimi-k2.7-code (-highspeed) | $0.95 ($1.90) | $4.00 ($8.00) | $0.19 ($0.38) | — | — | ¥6.50 / 27 / 1.30 (¥13 / 54 / 2.60) |
| kimi-k2.6 | $0.95 | $4.00 | $0.16 | — | — | ¥6.50 / 27 / 1.10 |

- **Tiers:** thresholds are CN ¥0/50/100/500/5,000/20,000 vs intl $1/10/20/100/1,000/3,000. Limits are the same on both sites (concurrency 1/15/40/50/60/100, RPM 3/100/100/200/200/300) [k-limits], [k-limits-cn](https://platform.kimi.com/docs/pricing/limits).
- **Balance:** CNY on CN, USD on intl [k-balance-cn](https://platform.kimi.com/docs/api/balance.md), [k-balance](https://platform.kimi.ai/docs/api/balance.md). Hosted Agents beta is CN-only [k-changelog-cn](https://platform.kimi.com/docs/changelog/index.md).

[k-overview]: https://platform.kimi.ai/docs/api/overview.md
[k-params]: https://platform.kimi.ai/docs/api/models-overview.md
[k-chat]: https://platform.kimi.ai/docs/api/chat.md
[k-resp]: https://platform.kimi.ai/docs/api/responses.md
[k-msgs]: https://platform.kimi.ai/docs/api/messages.md
[k-thinking]: https://platform.kimi.ai/docs/guide/use-thinking-models.md
[k-k31]: https://platform.kimi.ai/docs/guide/kimi-k3-1-pro-quickstart.md
[k-stream]: https://platform.kimi.ai/docs/guide/utilize-the-streaming-output-feature-of-kimi-api.md
[k-vision]: https://platform.kimi.ai/docs/guide/use-kimi-vision-model.md
[k-cache]: https://platform.kimi.ai/docs/guide/context-caching.md
[k-official]: https://platform.kimi.ai/docs/guide/use-official-tools.md
[k-wsprice]: https://platform.kimi.ai/docs/pricing/websearch
[k-price]: https://platform.kimi.ai/docs/pricing/chat
[k-limits]: https://platform.kimi.ai/docs/pricing/limits
[k-features]: https://platform.kimi.ai/docs/features.md
[k-trouble]: https://platform.kimi.ai/docs/guide/troubleshooting.md
[k-changelog]: https://platform.kimi.ai/docs/platform-changelog.md
[k-sig]: https://platform.kimi.ai/docs/api/signatures-verify.md
[k-interrupted]: https://platform.kimi.ai/docs/admin-api/billing-interrupted-requests.md
[k-openapi]: https://platform.kimi.ai/docs/openapi.json

## 3. Alibaba Qwen: Model Studio / Bailian (DashScope)
### 3.1 API surfaces, regions, models
| Surface | Path (on the base host) | Notes |
|---|---|---|
| OpenAI-compatible Chat Completions | `POST /compatible-mode/v1/chat/completions` | Non-OpenAI params go **top-level** in the HTTP JSON (`extra_body` is only an SDK construct). Also serves Bailian-hosted DeepSeek/Kimi/GLM/MiniMax [q-occ] |
| OpenAI-compatible Responses | `POST /compatible-mode/v1/responses`, `GET /responses/{id}`, delete, input items | Built-in tools; `previous_response_id`; `conversation`; **`store` defaults to `true`**. The old `/api/v2/apps/protocols/compatible-mode/v1/responses` is unmaintained [q-resp], [q-respget] |
| Anthropic-compatible | `POST /apps/anthropic/v1/messages` (`x-api-key` or Bearer) | `thinking`, `output_config.effort`, server `web_search`/`web_fetch` [q-anth] |
| DashScope native | `POST /api/v1/services/aigc/text-generation/generation` (text-only models); `…/multimodal-generation/generation` (qwen3.x incl. qwen3.8-max, VL) | "Most complete set of features and parameters" [q-apiref](https://www.alibabacloud.com/help/en/model-studio/qwen-api-reference/). A multimodal model on text-generation → `400 url error, please check url` [q-ws] |
| Files / Batch | `/compatible-mode/v1/files`; OpenAI-style Batch | §3.4, §3.8 [q-long], [q-batch] |

Regions [q-regions]:

| Region | Workspace domain (recommended) | Legacy DashScope domain |
|---|---|---|
| China (Beijing) | `{WorkspaceId}.cn-beijing.maas.aliyuncs.com` | `dashscope.aliyuncs.com` |
| Singapore | `{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com` | `dashscope-intl.aliyuncs.com` |
| China (Hong Kong) | `{WorkspaceId}.cn-hongkong.maas.aliyuncs.com` | `cn-hongkong.dashscope.aliyuncs.com` |
| US (Virginia) / Frankfurt / Tokyo | `{WorkspaceId}.us-east-1 / eu-central-1 / ap-northeast-1.maas.aliyuncs.com` | — |

- **Legacy domains are frozen:** "The DashScope domain (dashscope.aliyuncs.com) will no longer support new features after September 30, 2026" (verified).
- **Limits by domain:** timeout 3600 s (workspace) vs 600 s (legacy); body 64 MB vs 16 MiB [q-regions], [q-vis].
- **Keys are region-bound:** a key used in the wrong region → 401 `invalid_api_key` [q-compat].
- **Region-only features:** Batch is SG/BJ only; the guardrail header and Qwen-Long are BJ only; the free quota is SG only [q-guard], [q-long], [q-price].
- **Use the Chinese reference:** the English DashScope reference no longer documents `enable_search`/`search_options`; the Chinese one does [q-ds], [q-ds-zh].

Models [q-models](https://www.alibabacloud.com/help/en/model-studio/models), [q-tgm](https://www.alibabacloud.com/help/en/model-studio/text-generation-model), [q-think], [q-price], [q-omni], [q-long]:

| Model | In → out | Thinking | Context / max out | $/M in/out (SG; BJ) |
|---|---|---|---|---|
| `qwen3.8-max` (snapshot `-0902`) | text+image+video → text | hybrid, **on by default** | 1M / 128K | $2/$6; BJ $1.65/$4.951 |
| `qwen3.8-flash` | text+image+video → text | hybrid, on | 1M / 128K | $0.15/$0.47 |
| `qwen3.7-plus` | text+image+video → text | hybrid, on | 1M / 128K | $0.4/$1.6 (≤256K), $1.2/$4.8 (≤1M) |
| `qwen3.8-2.4t-a95b` | open weights | thinking only | – | – |
| `qwen3.8-omni-flash` | text+image+audio+video → **text** | hybrid, on (`reasoning_effort` xhigh) | 1M | – |
| `qwen-plus` / `qwen-flash` | text | hybrid, **off by default** | – | – |
| `qwen-long` (BJ only) | text + files | – | 10M / 32,768 | $0.072/$0.287 |

### 3.2 Thinking / reasoning
| Param | Semantics |
|---|---|
| `enable_thinking` (bool) | Chat: top-level. DashScope: in `parameters`. Responses: top-level, being deprecated in favour of `reasoning.effort`. Toggles thinking on hybrid models; output goes to `reasoning_content` [q-occ], [q-ds], [q-resp] |
| `reasoning_effort` (Chat/DashScope), `reasoning.effort` (Responses), `output_config.effort` (Anthropic) | qwen3.8: `low`, `medium`, `xhigh` (default). `minimal`→low; `high`/`max`→xhigh; `none`→`enable_thinking=false`. Other values error; combining with `thinking_budget` errors. low = 4096, medium = 16384, xhigh = 262144 tokens; default budget 131072 [q-occ], [q-ds] |
| `thinking_budget` (int) | Max CoT tokens; defaults to the model maximum. Out of range → `The thinking_budget parameter must be a positive integer and not greater than xxx` [q-occ], [q-err] |
| `preserve_thinking` (bool) | Appends historical `reasoning_content` to the input, billed as input. "Default value: false (Default value for qwen3.8-max/qwen3.8-flash: true)" (verified) [q-occ], [q-ds] |
| `max_completion_tokens` vs `max_tokens` | `max_completion_tokens` caps CoT + answer. `max_tokens` (being deprecated) caps only the answer on Qwen. Responses `max_output_tokens` and Anthropic `max_tokens` include CoT on qwen3.8 [q-occ], [q-resp], [q-anth] |
| `/think`, `/no_think` tags | Only Qwen3 open-source hybrids and `qwen-plus-2025-04-28` [q-think] |

**Multi-turn**
- qwen3.8: "You must send back all historical reasoning_content in the reasoning_content field. Do NOT concatenate reasoning_content into the content field". Leaving it out gives **no error**, only degraded output [q-occ], [q-think].
- Tool turns: echo the assistant message with reasoning + content + `tool_calls`, followed by the tool results [q-fc].
- **Doc contradiction:** the generic multi-turn page (updated Oct 27) still says "retain only content and ignore reasoning_content" [q-mt]; the per-model guidance (Nov 6) says the opposite.
- Responses API: replay `reasoning` items (`summary[].type:"summary_text"`), `function_call`/`function_call_output` and `web_search_call` items, or use `previous_response_id` (7 days) [q-resp]. Anthropic-compatible `thinking` blocks carry `signature: ""` [q-anth].

**Errors you can trigger**
- `tool_choice` `required`/object while thinking → `The tool_choice parameter does not support being set to required or object in thinking mode` [q-fc].
- `parameter.enable_thinking only support stream call` (open-source models, non-stream). `InvalidParameter.NotSupportEnableThinking` (no toggle) [q-err].
- DashScope + Qwen3 thinking needs `incremental_output:true` + `result_format:"message"` [q-ds].

```json
{"model":"qwen3.8-max","stream":true,"stream_options":{"include_usage":true},
 "enable_thinking":true,"reasoning_effort":"medium","preserve_thinking":true,
 "messages":[{"role":"user","content":"Q1"},
  {"role":"assistant","content":"A1","reasoning_content":"<verbatim prior reasoning>"},
  {"role":"user","content":"Q2"}]}
```

### 3.3 Built-in tools
**Web search: `enable_search`** (Chat Completions + DashScope). Default `false`; the model decides whether to search [q-occ].

| `search_options` field | Default | Notes |
|---|---|---|
| `forced_search` | false | always search [q-ds-zh] |
| `search_strategy` | `turbo` | `turbo`/`max`/`agent`/`agent_max`. `agent` = multi-round (text endpoint needs streaming; only `enable_source` works with it). **qwen3.8 does not support `agent` on Chat/DashScope** (use Responses `web_search`). qwen3.8-omni-flash *requires* `agent`. `agent_max` = agent + web extractor (thinking, streaming). Legacy names: `standard`/`pro`→turbo, `pro_max`/`pro_ultra`→max [q-ws], [q-ws-zh], [q-wx] |
| `enable_source` | false | returns `search_info`. **DashScope only** [q-ds-zh] |
| `enable_citation` | false | `[1]`-style markers; needs `enable_source` [q-ds-zh] |
| `citation_format` | `"[<number>]"` | or `"[ref_<number>]"` [q-ds-zh] |
| `enable_search_extension` | false | vertical sources (FX/weather/stocks/oil/calendar/gold) → `search_info.extra_tool_info[{tool,result}]` [q-ws-zh] |
| `prepend_search_result` | false | first stream chunk carries only the sources (DashScope only) [q-ds-zh] |
| `freshness` / `assigned_site_list` | unset / `[]` | 7/30/180/365 days / ≤25 sites; `turbo` only; qwen-plus/flash [q-ws-zh] |
| `intention_options.prompt_intervene` | – | natural-language scoping [q-ws-zh] |
| `include_tool_usage` (top-level, Chat) | false | adds `usage.x_tools.search.{count,strategy}` [q-occ] |

- **Chat Completions returns no sources or citations:** "OpenAI-compatible protocol does not support returning search sources" [q-ws].
  - Responses supports basic search + `action.sources` and silently ignores `search_strategy` [q-ws], [q-ws-zh].
  - Multimodal models search only when streaming (`Non-streaming mode does not support Web Search`) [q-ws].
- **Billing:** injected pages count as input tokens.
  - BJ turbo ¥3 / max ¥4 per 1000 calls (from 2026-02-27) [q-ws-zh].
  - `agent`/`agent_max` cost $0.573411 per 1000 in BJ/US/HK/JP/DE but **$10.00 in Singapore**; Responses/Anthropic `web_search` is billed as `agent` [q-ws].
- **Silent skip:** above 15 RPS per account the search is skipped with **no error** [q-ws-zh].

```json
// DashScope native request (shape per [q-ws], [q-ws-zh])
{"model":"qwen-plus","input":{"messages":[{"role":"user","content":"Alibaba stock price"}]},
 "parameters":{"result_format":"message","enable_search":true,
  "search_options":{"forced_search":true,"enable_source":true,"enable_citation":true,"citation_format":"[ref_<number>]"}}}
// response
{"output":{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"...[ref_1]..."}}],
  "search_info":{"search_results":[{"index":1,"title":"...","url":"https://...","site_name":"百家号","icon":"https://.../favicon.ico"}],"extra_tool_info":[]}},
 "usage":{"input_tokens":2004,"output_tokens":203,"plugins":{"search":{"count":1,"strategy":"agent"}},"prompt_tokens_details":{"cached_tokens":0}},
 "request_id":"45c231d2-..."}
```

**Responses-API built-in tools** [q-resp], [q-ws]
- Tools: `{"type":"web_search"}`, `{"type":"web_extractor"}` (needs `web_search`), `{"type":"code_interpreter"}`, `{"type":"web_search_image"}`, `{"type":"image_search"}`, `{"type":"file_search","vector_store_ids":[…]}` (one knowledge base), `{"type":"mcp","server_protocol":"sse","server_label","server_url","headers"}`, plus `function`.
- Output items: `reasoning`, `web_search_call` (`action.query`/`sources`), `web_extractor_call`, `code_interpreter_call`, `mcp_call`, `file_search_call`, `message` (`url_citation` annotations). qwen3.8-omni-flash supports only `web_search`.

**Other built-in tools**
- **Code interpreter on Chat/DashScope:** `enable_code_interpreter: true` (thinking + streaming in the examples) [q-occ], [q-ci](https://www.alibabacloud.com/help/en/model-studio/qwen-code-interpreter).
  - Chat "does not return code execution details". DashScope returns `output.tool_info:[{"type":"code_interpreter","code_interpreter":{"code":"…"}}]` + `usage.plugins.code_interpreter.count`.
  - **Exclusive with function calling** (error). Free for a limited time, like the web extractor [q-wx].
- **Anthropic-compatible search:** `web_search_20250305`/`web_fetch_*` work **only if `system` contains `x-anthropic-billing-header: cc_entrypoint=cli;`**, otherwise search "does not take effect". That conflicts with the no-hidden-prompt principle [q-ws], [q-wx].
- `enable_text_image_mixed: true` (qwen-max/plus-latest/flash) embeds HTML `<img>` tags in replies [q-ws-zh].

### 3.4 Multimodal input
[q-occ], [q-vis], [q-omni]

- **Chat parts:** `{"type":"text"}`, `{"type":"image_url","image_url":{"url"}}`, `{"type":"input_audio","input_audio":{"data":<URL or data-URI>,"format":"wav"}}`, `{"type":"video","video":[frame urls]}`, `{"type":"video_url","video_url":{"url"}}`.
  - Per-part options: `fps` (0.1–10, default 2), `min_pixels`, `max_pixels`, `total_pixels`, `cache_control`.
  - DashScope parts: `{"text"}`, `{"image"}`, `{"video": url|[frames], "fps"}`.
- **Images:**
  - Each side >10 px, aspect ratio ≤200:1, ≤8K (JPEG/PNG only above 4K; BMP/JPEG/PNG/TIFF/WEBP/HEIC below).
  - URL ≤20 MB on qwen3.5+/Qwen3-VL (10 MB on others); data URI ≤20 MB (32 MB on Anthropic).
  - ≤2048 images on qwen3.8-max/flash and qwen3.7-plus (else 256; 250 via Base64). Billed at 1 token per 32×32 px, plus 2.
  - `vl_high_resolution_images:true` pins the pixel limit to 16,777,216 px.
- **Video:** URL ≤2 GB, Base64 <10 MB; 2 s–2 h; frame lists of 4–8000 images; ≤64 videos. **Non-Omni models ignore the audio track.**
- **Omni:**
  - `qwen3.8-omni-flash` outputs text only; audio input ≤3 h (AMR/WAV/3GP/AAC/MP3).
  - `qwen3.5-omni-*` output speech with `"modalities":["text","audio"],"audio":{"voice":"Tina","format":"wav"}`, delivered as base64 in `delta.audio.data`. **Streaming only**, and no audio output in thinking mode.
- **Documents:**
  - VL models don't accept PDF/Word [q-vis]. Responses `input_file{file_url}` takes PDFs ≤100 MB for qwen3.8-max/flash [q-resp].
  - **Qwen-Long** (BJ only) [q-long]: upload with `POST /compatible-mode/v1/files` (`purpose=file-extract`), then put `"fileid://file-fe-xxx"` (comma-separated for several) in a system message.
    - Formats: TXT/DOCX/PDF/XLSX/EPUB/MOBI/MD/CSV/JSON + images. ≤150 MB/file, ≤100 files/request.
    - Parsing is async, and file tokens are billed on every call.

### 3.5 Other parameters, caching, JSON
| Param | Facts |
|---|---|
| `partial: true` on the last assistant message | Prefix continuation; prefix billed as input. Non-thinking Qwen3.7/3.6/3.5 Max/Plus/Flash, qwen-max/plus/flash/turbo, Coder, VL, Bailian-hosted Kimi. **qwen3.8 not listed** [q-partial] |
| Implicit cache | Always on, cannot be disabled. Prefix ≥1024 tokens; hit billed at 20% (qwen3.8 differs). `prompt_tokens_details.cached_tokens` (DashScope SG: `usage.cached_tokens`) [q-cache] |
| Explicit cache | `cache_control:{"type":"ephemeral"}` on content blocks (system/user/assistant/tool, not tools). ≤4 markers, 20-block lookback, ≥1024 tokens, 5-min TTL reset on hit. Create 125%, hit 10%. **Mutually exclusive with implicit.** Fields: `cache_creation_input_tokens`, `cache_creation.ephemeral_5m_input_tokens`, `cache_type` [q-cache], [q-occ] |
| Session cache (Responses) | header `x-dashscope-session-cache: enable` + `previous_response_id` [q-resp] |
| `response_format` | `json_object` requires the word "JSON" in the prompt (else 400). `json_schema` (+`strict`) only on the Qwen3.7/3.8 Max/Plus/Flash series. Responses: `text.format` [q-json](https://www.alibabacloud.com/help/en/model-studio/qwen-structured-output), [q-occ] |
| `top_k` | default 20 (QVQ 10, QwQ 40, Qwen3-Omni-Flash 50); `null` or >100 disables [q-occ] |
| `repetition_penalty` / `presence_penalty` | repetition >0 (1.0 = off). **Presence defaults to 1.5 server-side** for qwen3.8/3.7/3.6/3.5 non-thinking, qwen-max, Qwen3-VL; 0.5 for qwen-plus thinking; else 0 [q-occ] |
| `temperature` / `top_p` / `seed` | [0, 2) / (0, 1] / [0, 2³¹−1] [q-occ] |
| `n` / `stop` | 1–4, Qwen3 non-thinking only, forced to 1 with `tools` / string or array (no mixing strings and token ids) [q-occ] |
| `logprobs` / `top_logprobs` (0–5) | qwen-plus snapshots, qwen3-vl-plus, Qwen3 open-source only; never with reasoning [q-occ] |
| `tool_choice`, `parallel_tool_calls`, `tool_stream` | `required` unsupported. Parallel defaults to `true` (Chat ref) vs `false` (DashScope ref). `tool_stream` streams complex arguments (qwen3.8-max, 3.7/3.6/3.5) [q-occ], [q-ds] |
| `translation_options` | Qwen-MT `{"source_lang":"auto","target_lang":"English"}` [q-qmt](https://www.alibabacloud.com/help/en/model-studio/machine-translation) |
| Header `X-DashScope-DataInspection: {"input":"cip","output":"cip"}` | extra guardrail, BJ only, billed [q-guard] |
| `user` | not documented (**UNVERIFIED**); `request_id` appears in bodies [q-ds], [q-guard] |

### 3.6 Streaming and usage
- **OpenAI-compatible:**
  - `data: {chunk}` frames, ending with `data: [DONE]`.
  - Deltas arrive as `delta.role`, then `delta.reasoning_content`, then `delta.content`. Tool calls come as `delta.tool_calls[{index,id,function:{name (first only),arguments}}]`.
  - With `include_usage`, the last chunk has `choices: []`.
  - `finish_reason` ∈ `stop|length|tool_calls`; no `content_filter`/`sensitive` is documented [q-occ], [q-think].
- **DashScope:** needs `X-DashScope-SSE: enable` + `incremental_output:true`; otherwise every chunk repeats the full text [q-ds], [q-stream].
  - `finish_reason` is the **string** `"null"` until the end.
  - `search_info` arrives in the first chunk [q-ws-zh].

  ```
  id:1
  event:result
  :HTTP_STATUS/200
  data:{"output":{"choices":[{"message":{"content":"..","reasoning_content":"..","role":"assistant"},"finish_reason":"null"}]},"usage":{...cumulative...},"request_id":".."}
  ```
- **Responses** [q-resp]:
  - Events: `response.created|in_progress|completed|incomplete`, `response.output_item.*`, `response.content_part.*`, `response.output_text.delta/done`, `response.reasoning_text.delta/done`, `response.web_search_call.in_progress/searching/completed`, `response.code_interpreter_call.*`, `response.mcp_call*`.
  - Every event has `sequence_number`. Statuses: `queued|in_progress|completed|failed|cancelled|incomplete`.
- **Anthropic:** full usage arrives only in `message_delta` [q-anth].
- **Usage fields:**
  - Chat: `prompt_tokens_details{cached_tokens,text_tokens,image_tokens,video_tokens,audio_tokens,cache_creation…}`, `completion_tokens_details{reasoning_tokens,text_tokens,audio_tokens}`. **`text_tokens` includes `reasoning_tokens`**. Plus `x_tools` [q-occ].
  - DashScope: `input_tokens`/`output_tokens`/`output_tokens_details.reasoning_tokens`/`plugins.*`. Responses: `x_tools`, `x_details[]` [q-ds], [q-resp].
- **Silent truncation:** a non-streaming request that runs ≥300 s returns **partial content with no error** and the header `x-dashscope-partialresponse: true` [q-occ], [q-vis].
- **Errors inside an SSE stream** (e.g. output moderation): framing **UNVERIFIED**.

### 3.7 Errors
Body shapes [q-compat], [q-guard]:
- OpenAI-compatible: `{"error":{"message","type","param","code"},"id","request_id"}`, e.g. `code:"data_inspection_failed"`.
- DashScope: `{"code":"DataInspectionFailed","message":"Output data may contain inappropriate content.","request_id"}`.

| HTTP | Code (DashScope / OpenAI-compat) | Meaning | Retry? |
|---|---|---|---|
| 400 | `InvalidParameter` / `invalid_request_error` | bad params | no |
| 400 | `DataInspectionFailed` / `data_inspection_failed` | moderation flagged input **or output** | no |
| 400 | `InvalidParameter.DataInspection` | media download timeout during inspection | maybe (re-host media) |
| 400 | `Arrearage` | account overdue | no |
| 401 | `InvalidApiKey` / `invalid_api_key` | bad key / wrong region | no |
| 403 | `AccessDenied`, `Model.AccessDenied`, `Workspace.AccessDenied`, `AllocationQuota.FreeTierOnly` | permissions / activation | no |
| 404 | `ModelNotFound`, `model_not_supported`, `WorkSpaceNotFound` | | no |
| 429 | `Throttling`, `Throttling.RateQuota`, `.BurstRate`, `.Concurrency`, `.ServiceOverloaded`, `.ResourceExhausted` | rate limit / overload (dynamic limits on qwen3.8) | **yes, with backoff** |
| 429 | `Throttling.AllocationQuota` / `insufficient_quota` | TPM limit, or (same text) quota exhausted/arrears | check message |
| 429 | `BudgetLimitExceeded`, `PrepaidBillOverdue`, `CommodityNotPurchased` | billing | no |
| 500 | `InternalError` / `internal_error`, `InternalError.Algo`, `SystemError`, `ModelServiceFailed`, `RequestTimeOut` | server error. `InternalError.Algo` "Expecting ',' delimiter" = model emitted invalid tool-call JSON | yes, limited |
| 503 | `ServiceUnavailable`, `ModelUnavailable` | capacity | yes |

Sources for the table: [q-err], [q-rate](https://www.alibabacloud.com/help/en/model-studio/rate-limit).

### 3.8 Async, batch, resume
- **No async chat.** Responses "does not support… `background`… supports only synchronous calls" [q-resp]. `X-DashScope-Async` is for task APIs only (429 on unsupported models) [q-err].
- **Batch:** 50% price, SG/BJ only, `completion_window` 24h–336h.
  - ≤50,000 requests / 500 MB per file; `url` `/v1/chat/completions`.
  - `enable_thinking` must be top-level in each body. A free `batch-test-model` exists [q-batch].
- **No stream resume.**
  - A disconnect bills only "the portion generated before the server received the termination request", so generation stops [q-stream].
  - `GET /responses/{id}` returns only *completed* `store=true` responses [q-respget].
  - Recovery is `partial:true` continuation, on non-thinking models only [q-partial].

[q-occ]: https://www.alibabacloud.com/help/en/model-studio/qwen-api-via-openai-chat-completions
[q-ds]: https://www.alibabacloud.com/help/en/model-studio/qwen-api-via-dashscope
[q-ds-zh]: https://help.aliyun.com/zh/model-studio/qwen-api-via-dashscope
[q-resp]: https://www.alibabacloud.com/help/en/model-studio/qwen-api-via-openai-responses
[q-respget]: https://www.alibabacloud.com/help/en/model-studio/retrieve-a-response
[q-anth]: https://www.alibabacloud.com/help/en/model-studio/anthropic-api-messages
[q-compat]: https://www.alibabacloud.com/help/en/model-studio/compatibility-of-openai-with-dashscope
[q-regions]: https://www.alibabacloud.com/help/en/model-studio/regions
[q-think]: https://www.alibabacloud.com/help/en/model-studio/deep-thinking
[q-fc]: https://www.alibabacloud.com/help/en/model-studio/qwen-function-calling
[q-mt]: https://www.alibabacloud.com/help/en/model-studio/multi-round-conversation
[q-ws]: https://www.alibabacloud.com/help/en/model-studio/web-search
[q-ws-zh]: https://help.aliyun.com/zh/model-studio/web-search
[q-wx]: https://www.alibabacloud.com/help/en/model-studio/web-extractor
[q-vis]: https://www.alibabacloud.com/help/en/model-studio/vision
[q-omni]: https://www.alibabacloud.com/help/en/model-studio/qwen-omni
[q-long]: https://www.alibabacloud.com/help/en/model-studio/long-context-qwen-long
[q-partial]: https://www.alibabacloud.com/help/en/model-studio/partial-mode
[q-cache]: https://www.alibabacloud.com/help/en/model-studio/context-cache
[q-stream]: https://www.alibabacloud.com/help/en/model-studio/stream
[q-err]: https://www.alibabacloud.com/help/en/model-studio/error-code
[q-guard]: https://www.alibabacloud.com/help/en/model-studio/content-security
[q-batch]: https://www.alibabacloud.com/help/en/model-studio/batch-interfaces-compatible-with-openai
[q-price]: https://www.alibabacloud.com/help/en/model-studio/model-pricing

## 4. How OpenRouter (OR) exposes GLM / Kimi / Qwen
**Data snapshot.** Public `GET https://openrouter.ai/api/v1/models` (467 models) and `GET /api/v1/models/{id}/endpoints` for all 73 `z-ai/*`, `moonshotai/*`, `qwen/*` ids, fetched 2026-11-11. No `thudm/*` ids remain.

**Legend.**
- In = input modalities (T text, I image, V video, A audio, F file).
- Reasoning column: `mand` = `reasoning.mandatory:true`; `on` = `default_enabled:true`; `def` = `default_effort`.

### 4.1 Surfaces and hosts
- **Endpoints:** all 66 non-batch ids list `endpoints: ["/api/v1/chat/completions","/api/v1/completions","/api/v1/messages","/api/v1/responses"]`. The 7 `:batch` ids list only `["/api/v1/batches"]`.
  - OR translates each surface itself; none is the vendor's native Anthropic or Responses endpoint.
  - OR Responses is "**stateless** … Requests that set `store: true` or a non-null `previous_response_id` are rejected with a `400` error" ([responses overview](https://openrouter.ai/docs/api_reference/responses/overview.md)).
- **First-party hosts** (`GET https://openrouter.ai/api/v1/providers`):
  - `z-ai` "Z.AI" (HQ SG, datacenters `["SG"]`, policy at chat.z.ai);
  - `moonshotai` "Moonshot AI" (SG, `["SG"]`, platform.moonshot.ai);
  - `alibaba` "Alibaba" (SG, `["SG","CN"]`, Alibaba Cloud *international* terms; endpoint tags `alibaba`, `alibaba/us-east-1`).
  - So OR uses the **international** platforms; the exact upstream base URLs are **UNVERIFIED**.
- **Alibaba also hosts third-party models on OR:** `z-ai/glm-5.1/5.2/5.3` (incl. the `alibaba/prime` tier and the Alibaba-only `z-ai/glm-5.3-prime`), `moonshotai/kimi-k3`, `kimi-k3.1-pro`, `kimi-k2.7-code`. For those, Alibaba is *not* the official vendor.
- **Pinning:** endpoint slugs have the shape `provider[/variant][/region][/tier]` (e.g. `z-ai/fp8`, `alibaba/us-east-1`, `moonshotai/highspeed`). Pin with `provider.order`/`only` + `allow_fallbacks:false`; `quantizations` filters by quant ([provider-selection](https://openrouter.ai/docs/guides/routing/provider-selection.md)).
  - Default routing optimises price/throughput/latency/reliability, so unpinned GLM/Kimi requests usually do **not** hit the official host.

### 4.2 Catalog: GLM (`z-ai/*`)
Official $/M is the Z.AI endpoint price. "Other hosts" is a count, or the names when there are ≤3.

| model id | ctx | In | OR reasoning metadata | official endpoint [quant label] | other hosts | official $/M in/out |
|---|---|---|---|---|---|---|
| `z-ai/glm-5.3` | 1M | T | mand/on; max,high,low (def max) | `z-ai/fp8` | 35 (incl. Alibaba) | 1.4/4.4 |
| `z-ai/glm-5.3-flash` | 1M | TIV | mand/on; max,high,low (def max) | `z-ai/fp8` | 36 | 0.15/0.5 |
| `z-ai/glm-5.3-flashx`, `-flashx-legal` | 1M | TIV | mand/on; max,high,low | `z-ai/fp8` | 0 | 0.37/1.25 |
| `z-ai/glm-5.3-prime` | 1M | T | mand/on | none | Alibaba only (2.8/8.8) | – |
| `z-ai/glm-5.2` | 1M | T | opt/on; xhigh,high,none (def **high**) | `z-ai/fp8` | 23 (incl. Alibaba) | 1.4/4.4 |
| `z-ai/glm-5.1` | 204K | T | opt/on | `z-ai/fp8` | 11 (incl. Alibaba) | 1.4/4.4 |
| `z-ai/glm-5`, `glm-5-turbo` | 202–204K | T | opt/on | `z-ai/fp8`, `z-ai` | 7 / 0 | 1/3.2, 1.2/4 |
| `z-ai/glm-5v-turbo` | 202K | TIV | opt/on | `z-ai/fp8` | 0 | 1.2/4 |
| `z-ai/glm-4.7` / `glm-4.6` | 204K | T | opt/on / opt | `z-ai/fp4` | 4 / DeepInfra, Novita, Venice | 0.6/2.2 |
| `z-ai/glm-4.7-flash` | 200K | T | opt/on | **none** | Cloudflare, Novita | – |
| `z-ai/glm-4.6v` | 131K | TIV | opt | `z-ai/fp8` | Novita | 0.3/0.9 |
| `z-ai/glm-4.5` / `-4.5-air` / `-4.5v` | 131K/131K/65K | T/T/TI | opt | `z-ai/fp8` | 0 / Novita, SiliconFlow / Novita | 0.6/2.2, 0.2/1.1, 0.6/1.8 |
| `z-ai/glm-5.3:batch`, `glm-5.3-flash:batch`, `glm-5.2:batch` | 1M | – | as base | **none** | DeepInfra / Morph / Sail Research | – |

### 4.3 Catalog: Kimi (`moonshotai/*`)
| model id | ctx | In | OR reasoning metadata | official endpoint | other hosts | official $/M |
|---|---|---|---|---|---|---|
| `moonshotai/kimi-k3.1-pro` | 1M | TIV | opt/on; max,high,low,**none** (def max) | `moonshotai/mxfp4`, `moonshotai/highspeed` (priority tier) | 16 (incl. Alibaba) | 2/10 (highspeed 4/20) |
| `moonshotai/kimi-k3` | 1M | TIV | opt/on; max,high,low,**none** (def max) | `moonshotai` | 19 (incl. Alibaba, Bedrock) | 3/15 |
| `moonshotai/kimi-k2.7-code` | 262K | TI | mand/on | `moonshotai`, `moonshotai/highspeed` | 9 (incl. Alibaba) | 0.95/4 |
| `moonshotai/kimi-k2.6` | 262K | TI | opt/on | `moonshotai` | 15 | 0.95/4 |
| `moonshotai/kimi-k2.5` | 262K | TI | opt/on | **none** | AtlasCloud int4, SiliconFlow int4, Venice, Novita, Bedrock | – |
| `moonshotai/kimi-k2-thinking` / `kimi-k2-0905` / `kimi-k2` | 262K/262K/131K | T | mand/on / – / – | **none** | Novita only | – |
| `moonshotai/kimi-k3:batch` | 1M | – | as base | **none** | DeepInfra | – |

### 4.4 Catalog: Qwen (`qwen/*`)
Proprietary models are Alibaba-only; open-weight models are mostly third-party-only.

| model id | ctx | In | OR reasoning metadata | official endpoint | other hosts | official $/M |
|---|---|---|---|---|---|---|
| `qwen/qwen3.8-max-0902` / `-prime` | 1M | TIV | **mand**/on; xhigh,high,medium,low,minimal (def xhigh) | `alibaba` (+`us-east-1`) | 0 | 2/6; prime 4/12 |
| `qwen/qwen3.8-flash`, `qwen3.8-omni-flash` | 1M | TIV / TIVA (out: text) | opt/on, `supports_max_tokens` | `alibaba` (+`us-east-1`) | 0 | 0.15/0.47 |
| `qwen/qwen3.8-27b` | 1M | TIV | opt/on; xhigh,medium,low,none | `alibaba` | 24 | 0.425/2.55 |
| `qwen/qwen3.8-2.4t-a95b` ("open-weight variant of Qwen3.8 Max") | 1M | T | mand/on; xhigh,medium,low | `alibaba` | Novita, SiliconFlow, Venice | 2/6 |
| `qwen/qwen3.7-max` / `-plus` / `-flash` | 1M | T / TI / TIV | opt/on (flash: `supports_max_tokens`) | `alibaba` (+`us-east-1`) | 0 | 1.475/4.425; 0.32/1.28; 0.03/0.13 (tiered via `pricing.overrides[].min_prompt_tokens`) |
| `qwen/qwen3.6-plus`/`-flash`, `qwen3.5-plus-*`, `qwen3.5-flash-02-23` | 1M | TIV | opt | `alibaba` | 0 | tiered |
| `qwen/qwen3.6-27b`, `qwen3.5-27b/35b-a3b/122b-a10b/397b-a17b` | 262K | TIV | opt(/on) | `alibaba` | 3–7 each | – |
| `qwen/qwen3.6-35b-a3b`, `qwen3.5-9b`, all `qwen3-*` (VL, coder, next, 2507, 14b/32b/30b), `qwen2.5-*` | 32–262K | T/TI | varies | **none** | DeepInfra, Parasail, SiliconFlow, Novita, Venice, … | – |
| `qwen/qwen-plus`, `qwen/qwen3-coder-flash` | 1M | T | none | `alibaba` | 0 | 0.26/0.78; 0.195/0.975 |

**Fidelity (endpoints JSON).**
- **Quant labels:** official endpoints are labelled `fp8` (Z.AI), `fp4` (Z.AI glm-4.6/4.7), `mxfp4` (Moonshot K3.1-pro) or `unknown` (Alibaba, Moonshot K3/K2.x). Third-party hosts commonly label `fp4`/`nvfp4`/`int4`/`fp8`.
- **Per-host differences:** `max_completion_tokens` varies (16K–943K for kimi-k3), and so do `supported_parameters` and pricing.

### 4.5 Parameters on the *official* endpoints (`supported_parameters`)
| Host | Listed | Notably absent |
|---|---|---|
| Z.AI | `reasoning, include_reasoning, max_tokens, temperature, top_p, tools, tool_choice, top_k, response_format` (+`reasoning_effort` on 5.2/5.3; +`structured_outputs` on 5.3-flashx-legal) | `stop`, `seed`, penalties, `logprobs`, `parallel_tool_calls`. `tool_choice` is `auto`-only on most (glm-4.5v/4.6/5 also none/required/function) |
| Moonshot AI | `reasoning, include_reasoning, max_tokens, temperature, top_p, stop, frequency/presence_penalty, structured_outputs, response_format, tools, tool_choice (none/auto/required[/function]), logprobs, top_logprobs` (+`reasoning_effort` on K3/K3.1-pro; `highspeed` lacks logprobs) | `seed`, `top_k`, `n`, `parallel_tool_calls` |
| Alibaba | `reasoning, include_reasoning, max_tokens, temperature, top_p, seed, presence/frequency_penalty, response_format, structured_outputs, tools, tool_choice (none/auto; some +required/function), logprobs, top_logprobs, top_k, stop` (+`reasoning_effort` on 3.8-max/27b/2.4t) | `repetition_penalty`, `n`, `parallel_tool_calls`, every DashScope extension (`enable_search`, `search_options`, raw `enable_thinking`/`thinking_budget`, `vl_high_resolution_images`, `translation_options`, `modalities`/`audio`) |

How OR treats parameters ([parameters](https://openrouter.ai/docs/api_reference/parameters.md), [provider-selection](https://openrouter.ai/docs/guides/routing/provider-selection.md#requiring-providers-to-support-all-parameters), [API overview](https://openrouter.ai/docs/api_reference/overview.md)):
- **Absent:** OR omits absent params upstream ("rather than substituting a hardcoded value").
- **Unsupported:** they are ignored by providers that don't support them. `provider.require_parameters:true` excludes such providers instead. `tools`/`response_format`/`verbosity` act as soft routing preferences.
- **Provider-specific passthrough:** OR passes only "some provider-specific parameters, such as `safe_prompt` for Mistral or `raw_mode` for Hyperbolic".
  - The schema also has `provider.options.<slug>` (slugs `alibaba`, `moonshotai`, `z-ai`): "Only options for the matched provider are forwarded; the rest are ignored. Unrecognized keys are silently dropped" ([chat API schema](https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion.md)).
  - Whether vendor keys like `{"alibaba":{"enable_search":true}}` or `{"z-ai":{"thinking":{…,"clear_thinking":false}}}` get through is **UNVERIFIED**. OR documents a per-model `allowed_passthrough_parameters` list only for video models ([cookbook](https://openrouter.ai/docs/cookbook/video-generation/provider-specific-video-options.md)).
- **`n`:** not in OR's Chat Completions request schema at all.
- **Injected identity:** OR's `user` is folded "into the hashed identity sent upstream and never forwards it raw. If omitted, requests use an account-level identity, so provider policy blocks can affect the whole account" (same schema). Vendor `user_id`/`safety_identifier` are therefore OR-controlled.
- **Debugging:** `debug: {"echo_upstream_body": true}` (streaming only) "includes the transformed upstream request body in a debug chunk at the start of the stream" (same schema). This is the tool for checking every UNVERIFIED mapping here.

### 4.6 Reasoning mapping
[reasoning-tokens](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens.md)

- **Request:**
  - `reasoning: {"effort":"max|xhigh|high|medium|low|minimal|none"}` *or* `{"max_tokens":N}`, `{"enabled":bool}`, `{"exclude":bool}` (Chat only); also top-level `reasoning_effort` and the legacy `include_reasoning`.
  - Per-model `/models` metadata: `supported_efforts`, `default_effort`, `default_enabled`, `mandatory` ("do not send `effort: "none"` — the model rejects it"), `supports_max_tokens`.
- **Upstream translation:** documented only for Alibaba ("Some Alibaba Qwen thinking models (mapped to `thinking_budget`)"), i.e. ids with `supports_max_tokens`: `qwen3.8-flash`, `qwen3.8-omni-flash`, `qwen3.7-flash`.
  - How `effort` becomes Z.AI `thinking`/`reasoning_effort`, Moonshot `reasoning_effort`/`thinking`, or DashScope `enable_thinking`/`reasoning_effort` is **UNVERIFIED**.
  - GLM `clear_thinking`, Kimi `thinking.keep` and Qwen `preserve_thinking` have no OR field.
- **Response:**
  - `message.reasoning` (string) + `reasoning_details[]` (stream: `delta.reasoning` / `delta.reasoning_details`), with items like `{"type":"reasoning.text","text":…,"format":…,"index":…}`.
  - The `format` OR uses for these vendors is **UNVERIFIED**; no Z.AI/Moonshot/Alibaba-specific value is in the documented enum, so expect `"unknown"`.
- **Multi-turn:** send prior reasoning as `message.reasoning`, `reasoning_content` ("alias – functions identically") or unchanged `reasoning_details`.
  - Whether OR re-emits it upstream as `reasoning_content` on assistant/tool-call turns is **UNVERIFIED**. That is critical for K3 (required) and qwen3.8 (`preserve_thinking` default true).
- **Budget:** `max_tokens` covers reasoning + answer. If reasoning exhausts it: `finish_reason:"length"` with empty content, still billed.

### 4.7 Official built-in tools via OR: not reachable
- **No native tools.** `native_tools` is `{}` on **every** endpoint of all 73 GLM/Kimi/Qwen ids (compare Gemini `{"openrouter:web_search":{"type":"google_search"}}` and xAI `{"type":"web_search"}`). OR's native-search list is "OpenAI, Anthropic, Google, SpaceXAI, Perplexity" ([web-search server tool](https://openrouter.ai/docs/guides/features/server-tools/web-search.md)).
- **What `openrouter:web_search` does instead:** with `engine:"auto"` (default) it falls back to **Exa** ($0.007/request for instant/fast/auto). Alternatives: `exa|firecrawl|parallel|perplexity`.
  - Citations come back as OR `url_citation` annotations, not GLM `web_search[]` / Kimi `web_search_call` / Qwen `search_info`.
  - The `plugins:[{id:"web"}]` plugin and the `:online` suffix are deprecated.
- **Pitfall:** OR's `tools[]` accepts only:
  - `function` tools;
  - `openrouter:*` server tools;
  - a shorthand `{"type":"web_search"|"web_search_preview"|"web_search_preview_2025_03_11"|"web_search_2025_08_26"}`, "Automatically converted to openrouter:web_search" ([chat API schema](https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion.md)).

  So the old bot's GLM payload `{"type":"web_search","web_search":{"enable":false}}` would plausibly **turn on Exa search** through OR; the handling of the extra key is **UNVERIFIED**. Never send vendor-native tool objects to OR.
- **Not expressible at all:** Kimi `builtin_function`/`$web_search`, Formula tools, GLM `retrieval`/`mcp`, Qwen `enable_search`/`search_options`/`enable_code_interpreter`/Responses tools. 400 vs. silent drop is **UNVERIFIED**.
- **OR's other server tools** (`openrouter:web_fetch`, `datetime`, `image_generation`, …) are executed by OR, not by the vendor, so the project principles exclude them.

### 4.8 Caching via OR
[prompt-caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching.md)

- **Z.AI:** automatic; reads in `prompt_tokens_details.cached_tokens`; writes "limited-time free". OR "sends Z.AI a session affinity key with each request, derived from your account and, when provided, your `session_id`". That is an upstream field the client did not send.
- **Moonshot:** automatic; doc says writes free, reads "0.25x". The OR JSON ratio for kimi-k3 is 0.3/3 = 0.1x, and Moonshot itself bills K3 cache *writes* (§2.6), so OR's doc conflicts with both.
- **Alibaba:** "requires explicit cache breakpoints" `cache_control:{"type":"ephemeral"}` (5-min TTL); writes 1.25x, reads 0.1x.
  - Doc model list: `qwen3-max`, `qwen-plus`, `qwen3.6-plus`, `qwen3-coder-plus`, `qwen3-coder-flash` (stale vs catalog); snapshots `qwen3.5-plus-02-15`/`qwen3.5-flash-02-23` excluded.
  - Most `alibaba` endpoints report `supports_implicit_caching:false`.
- **Usage fields:** `prompt_tokens_details.{cached_tokens,cache_write_tokens}`, `completion_tokens_details.reasoning_tokens`, `cost`, `cost_details`, `server_tool_use_details` ([chat API schema](https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion.md)).

### 4.9 Multimodal via OR
[multimodal overview](https://openrouter.ai/docs/guides/overview/multimodal/overview.md), [pdfs](https://openrouter.ai/docs/guides/overview/multimodal/pdfs.md), [videos](https://openrouter.ai/docs/guides/overview/multimodal/videos.md), [audio](https://openrouter.ai/docs/guides/overview/multimodal/audio.md)

- **Images:** `image_url` (URL or `data:` base64).
- **Video:** a `video_url` part (Chat) / `input_video` (Responses) on ids listing `video`: GLM-5.3-flash/flashx, GLM-5v-turbo, GLM-4.6v, Kimi K3/K3.1-pro, most Qwen3.5+/3.8. Per-vendor URL handling is **UNVERIFIED**.
- **Audio:** input only on `qwen/qwen3.8-omni-flash`, as base64 `input_audio` (no URLs). **No GLM/Kimi/Qwen id has an `audio` output modality.**
- **PDF/files:** no GLM/Kimi/Qwen id lists `file` input, so OR's `file-parser` plugin parses PDFs itself. It will "default first to the model's native file processing … if that's not available … `mistral-ocr`" ($2/1000 pages); `cloudflare-ai` is free.
  - This is OR-side extraction, not Moonshot `file-extract` / GLM `file_url` / Qwen-Long `fileid://`.
  - OR's Files API (`or_file_…` ids) is OR storage, not vendor file ids ([files-api](https://openrouter.ai/docs/guides/features/files-api.md)).

### 4.10 Streaming, errors, recovery via OR
[streaming](https://openrouter.ai/docs/api_reference/streaming.md), [errors](https://openrouter.ai/docs/api_reference/errors-and-debugging.md)

- **SSE format:**
  - Keep-alive comments `: OPENROUTER PROCESSING`.
  - The final usage chunk is *always* sent (`stream_options.include_usage` "has no effect") and carries one choice repeating `finish_reason` + `native_finish_reason` (not OpenAI's empty `choices`).
  - `finish_reason` is normalised to `tool_calls|stop|length|content_filter|error`. Vendor values (GLM `sensitive`/`network_error`) can only survive in `native_finish_reason` (**UNVERIFIED** per vendor).
- **Errors:**
  - Pre-commit: HTTP status + `{"error":{"code","message","metadata"}}`.
  - Mid-stream: a chunk with top-level `error` + `finish_reason:"error"` on HTTP 200.
  - `error.metadata.error_type` (`rate_limit_exceeded` 429, `provider_overloaded` 503, `provider_unavailable` 502, `content_policy_violation` 403, `context_length_exceeded` 400, `timeout` 504, …) + `provider_code` (upstream code, omitted on 500s). `Retry-After` is sent on 429/503.
- **Not verbatim:** "When a request fails with a `500`, the `message` is replaced with a generic string".
  - Raw upstream bodies are only available later via `GET /api/v1/generation/content?id=gen-…` (`error.raw`, `previous_errors[].raw`, plus stored prompt/completion/reasoning).
  - That endpoint needs a **management key**, and content is returned only "when stored" ([generation content API](https://openrouter.ai/docs/api/api-reference/generations/get-stored-prompt-completion-and-error-content-for-a-generation.md)).
- **Cancellation:** **Alibaba is listed "Not Currently Supported"**: "the model will continue processing and you will be billed for the complete response". Z.AI/Moonshot are in neither list (**UNVERIFIED**).
- **Resume:** OR documents none. Recovery = record `X-Generation-Id` at stream start, then fetch `/generation/content` (if storage is enabled, **UNVERIFIED** prerequisite).
- **Async alternative:** `:batch` ids via `/api/v1/batches` (results within 24h) ([batch-api](https://openrouter.ai/docs/guides/features/batch-api.md)).

## 5. OpenRouter gaps for Zhipu GLM
- **Official web search is unreachable.** Every `z-ai/*` endpoint has `native_tools: {}` (endpoints JSON), and Z.AI is absent from OR's native-search provider list ([web-search](https://openrouter.ai/docs/guides/features/server-tools/web-search.md)).
  - `openrouter:web_search` therefore runs Exa or another OR engine. There are no `search_engine`/`search_result`/`search_prompt` controls and no `web_search[]` result array.
  - **Danger:** OR's schema converts any `{"type":"web_search",…}` tool into its own Exa-backed `openrouter:web_search` ([chat API schema](https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion.md), `ChatWebSearchShorthand`). The old bot's "disable search" object must never go to OR; how its extra `web_search.enable:false` key is treated is **UNVERIFIED**.
- **`retrieval`, `mcp` and Responses-API `web_search` tools are unreachable.** OR `tools[]` only takes `function` + `openrouter:*` tools (same schema).
- **Preserved thinking has no OR knob.**
  - No `thinking.clear_thinking` equivalent exists. With the vendor default (`true`), any reasoning OR forwards from earlier turns is discarded.
  - Whether `provider.options["z-ai"]` can carry `thinking`/`clear_thinking`/`tool_stream`/`do_sample` is **UNVERIFIED**; test with `debug.echo_upstream_body`.
- **Effort metadata partly disagrees with Z.ai.**
  - glm-5.3*: OR says `mandatory`, efforts `max,high,low`, default `max`. That matches Z.ai's "only enabled", `low|high|max`, default `max` ([glm-zchat]).
  - glm-5.2: OR lists `xhigh,high,none` with default **`high`**, but Z.ai's default is **`max`** and it accepts 7 values. Whether OR *sends* a default effort is **UNVERIFIED**; OR says absent params are omitted ([parameters](https://openrouter.ai/docs/api_reference/parameters.md)).
  - glm ≤5.1: OR offers on/off only. That is consistent with Z.ai (`reasoning_effort` only on 5.2+).
- **Parameters dropped or narrowed on the Z.AI endpoint.**
  - `supported_parameters` has no `stop`, even though Z.ai supports 4 stop words (1 on intl).
  - No `tool_stream`, `do_sample` or `request_id`.
  - `user_id` is replaced by OR's hashed identity (`user` description in the chat API schema).
  - `structured_outputs` appears only on `glm-5.3-flashx-legal`; Z.ai itself has no `json_schema` ([glm-zstruct]).
- **Streaming signals are flattened.**
  - `finish_reason` is normalised to `stop|length|tool_calls|content_filter|error` ([streaming](https://openrouter.ai/docs/api_reference/streaming.md)).
  - GLM's `sensitive`, `network_error` and `model_context_window_exceeded` can only survive as `native_finish_reason` (**UNVERIFIED**).
  - Whether the per-chunk `content_filter[{role,level}]` array is forwarded is **UNVERIFIED**.
- **Errors are not verbatim.**
  - Business codes (1301/1302/1305/1113) become OR `error_type` + `provider_code`, and 500-class messages are masked ([errors](https://openrouter.ai/docs/api_reference/errors-and-debugging.md)).
  - The raw body is only available via the management-key `/generation/content` API.
- **Files are not handled by Zhipu.**
  - No `z-ai/*` id lists `file` input, so PDFs go through OR's `file-parser` (default `mistral-ocr`) rather than Zhipu's `file`/`file_url` server-side parsing ([pdfs](https://openrouter.ai/docs/guides/overview/multimodal/pdfs.md)).
  - `glm-4-voice` audio input is not on OR.
- **Models missing or mis-hosted.**
  - Not on OR: `glm-4.5-airx/-flash`, `glm-4.6v-flash/flashx`, `glm-4.7-flashx`, `glm-4.1v-thinking-*`, `glm-4v-flash`, `glm-4-voice`, and the knowledge-base/retrieval models.
  - `z-ai/glm-4.7-flash` has no Z.AI endpoint (Cloudflare/Novita only).
  - `z-ai/glm-5.3-prime` is served **only by Alibaba**.
  - The `:batch` variants are served only by DeepInfra/Morph/Sail Research (endpoints JSON).
- **Default routing rarely reaches Z.AI.**
  - `z-ai/glm-5.3` has 35 non-Z.AI hosts, mostly `fp4`/`nvfp4`/`fp8`. One of them is Alibaba, whose endpoint params differ (`seed`, `stop`, penalties, logprobs listed).
  - Pin with `provider:{"only":["z-ai"],"allow_fallbacks":false}` for raw-model fidelity. The Z.AI endpoint itself is labelled `fp8` (`fp4` for glm-4.6/4.7).
- **CN-only features are unreachable.** OR's Z.AI provider is SG/intl (`GET /api/v1/providers`), so `search_intent`, the CN engine list, MCP connector, retrieval, async chat and Batch are out of reach.
- **Async and resume.**
  - Zhipu's async chat (`/async/chat/completions` + `/async-result/{id}`) is not exposed.
  - OR documents no resume; recovery only via `X-Generation-Id` + `/generation/content`.
  - Z.AI is not in OR's stream-cancellation lists (**UNVERIFIED** whether disconnects stop billing).
- **Caching is automatic but OR adds a field.** OR "sends Z.AI a session affinity key with each request" ([prompt-caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching.md)), which the client did not request. That is harmless, but worth knowing for the "transparent requests" principle.

## 6. OpenRouter gaps for Moonshot Kimi
- **No official search through OR.**
  - The real built-in search is the Responses-API `web_search` tool on `api.moonshot.*/v1/responses` (K3 only) [k-resp]. Every `moonshotai/*` endpoint has `native_tools: {}` (endpoints JSON).
  - OR's own Responses API converts a `{"type":"web_search"}` tool into `openrouter:web_search` (Exa) ([chat API schema](https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion.md) `ChatWebSearchShorthand`, [web-search](https://openrouter.ai/docs/guides/features/server-tools/web-search.md)). The same request *shape* yields a different search backend.
  - Legacy `builtin_function`/`$web_search` and the Formula tools (`/v1/formulas/...`) cannot be expressed in OR's `tools[]`. `builtin_function` is not in the schema; send behaviour is **UNVERIFIED**.
  - The REST `/v1/tools/search|search_pro|fetch` endpoints are separate Moonshot APIs, which a bot could only call directly.
- **Thinking controls are lossy.**
  - K3/K3.1-pro: OR metadata says `mandatory:false` and offers `none`, but Moonshot says K3 "always on" (low/high/max only) [k-params]. `effort:"none"` may therefore only work on third-party hosts, and on the Moonshot endpoint it is **UNVERIFIED** (error or ignored).
  - k2.6: `thinking.keep:"all"` (preserved thinking) has no OR field. `provider.options.moonshotai` passthrough is **UNVERIFIED**.
  - The K3 requirement to replay `reasoning_content` on every assistant/tool-call turn [k-thinking] relies on OR re-emitting `reasoning`/`reasoning_details` upstream as `reasoning_content` (**UNVERIFIED**). If OR drops it, multi-turn tool use on K3 degrades or errors.
- **Partial mode is not documented.** OR's generic "assistant prefill" (trailing `assistant` message) ([API overview](https://openrouter.ai/docs/api_reference/overview.md)) does not document adding Moonshot's required `partial:true`, nor carrying `reasoning_content` on the partial message (**UNVERIFIED**). Partial-based resume (§2.9) may not work through OR.
- **Parameters missing on the Moonshot endpoint** (endpoints JSON `supported_parameters`): `prompt_cache_options` (1h TTL), `safety_identifier` (OR substitutes its hashed `user`), `prediction`, the dynamic `role:"system"` tool messages, and the `X-Msh-*` headers.
  - Request-signature verification (`X-Msh-Request-Nonce` → `/v1/signatures/verify`) is impossible, so "raw official model" cannot be proven through OR [k-sig].
  - OR lists `temperature`/`top_p`/penalties as supported, but Moonshot fixes them on K3. Non-default values will 400 upstream. Third-party hosts may silently accept them, which is *non-official behaviour*.
- **Files and media.**
  - No `moonshotai/*` id lists `file` input, so PDFs go through OR `file-parser` (`mistral-ocr` by default) instead of Moonshot `file-extract` ([pdfs](https://openrouter.ai/docs/guides/overview/multimodal/pdfs.md)).
  - `ms://file_…` references and `purpose:"video"` uploads are unreachable.
  - Moonshot rejects public image URLs [k-vision]. Whether OR downloads and inlines http(s) images for the Moonshot endpoint is **UNVERIFIED** (OR has an `image_download_failed` error type, so it fetches in some cases).
  - OR marks `kimi-k2.6`/`k2.7-code` as image-only (TI), although Moonshot supports video on them [k-vision].
- **Caching and billing mismatch.**
  - OR's caching doc says Moonshot "Cache writes: no cost" ([prompt-caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching.md)). The OR JSON has no `input_cache_write` price for Moonshot endpoints.
  - Moonshot bills K3 cache writes (5m = input price, 1h = 2×) [k-price], [k-cache]. How OR bills these is **UNVERIFIED**.
  - Moonshot's `cache_write_tokens` maps onto OR `prompt_tokens_details.cache_write_tokens` (field exists in OR usage schema); actual population is **UNVERIFIED**.
- **Model availability and fidelity.**
  - `moonshotai/kimi-k2.5`, `kimi-k2-thinking`, `kimi-k2-0905` and `kimi-k2` are still listed on OR but have **no Moonshot endpoint**, which matches Moonshot retiring them [k-changelog]. They are served only by third parties (int4/fp8/bf16: AtlasCloud, SiliconFlow, Venice, Novita, Bedrock), so this is open-weight inference, not the official API.
  - `kimi-k3` has 19 non-Moonshot hosts and `kimi-k3.1-pro` 16, mostly fp4/mxfp4, plus Alibaba and Bedrock. Pin with `provider.only:["moonshotai"]`. `kimi-k3.1-pro-highspeed` = the `moonshotai/highspeed` (priority-tier) endpoint.
  - `kimi-k3:batch` is DeepInfra-only, so Moonshot's own Batch API (60% price) is unreachable.
- **Errors and streams.**
  - Moonshot `error.type` (`engine_overloaded_error`, `rate_limit_reached_error`, `exceeded_current_quota_error`, `content_filter` "high risk") is folded into OR `error_type`/`provider_code`, and 500s are masked ([errors](https://openrouter.ai/docs/api_reference/errors-and-debugging.md)).
  - Moonshot bills disconnected requests in full [k-trouble], so cancelling an OR stream presumably doesn't save cost. Moonshot is not in OR's cancellation lists (**UNVERIFIED**).
- **OR also offers Responses/Messages surfaces for all Kimi ids, but these are OR translations.** Moonshot's own `/v1/responses` and `/anthropic` (K3-only, with `web_search_call`, `summary_text` reasoning items and `thinking` signatures) are not proxied natively.

## 7. OpenRouter gaps for Alibaba Qwen
- **Official search is unreachable.**
  - `enable_search`, `search_options` (`forced_search`, `search_strategy`, `enable_source`, `enable_citation`, `citation_format`, `freshness`, `assigned_site_list`, …) and `search_info` are absent from every Alibaba endpoint's `supported_parameters`. `native_tools` is `{}` (endpoints JSON).
  - `openrouter:web_search` runs Exa or another OR engine ([web-search](https://openrouter.ai/docs/guides/features/server-tools/web-search.md)).
  - Even if `provider.options.alibaba.enable_search` were forwarded (**UNVERIFIED**), DashScope's OpenAI-compatible mode "does not support returning search sources" [q-ws]. Visible citations need DashScope native or Responses directly.
- **Other built-in tools are unreachable** for the same schema reason ([chat API schema](https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion.md)): `enable_code_interpreter` and the Responses tools `web_search`/`web_extractor`/`code_interpreter`/`web_search_image`/`image_search`/`file_search`/`mcp`.
- **Thinking controls are narrower.**
  - OR marks `qwen/qwen3.8-max-0902`/`-prime` and `qwen3.8-2.4t-a95b` as `mandatory:true` (no `none`). DashScope documents qwen3.8 as hybrid, where `reasoning_effort:"none"` → `enable_thinking=false` [q-occ]. **Thinking cannot be turned off for qwen3.8-max via OR.**
  - `reasoning.max_tokens` → `thinking_budget` only where OR sets `supports_max_tokens` (`qwen3.8-flash`, `qwen3.8-omni-flash`, `qwen3.7-flash`) ([reasoning-tokens](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens.md)). There is no budget control for qwen3.8-max.
  - `preserve_thinking` has no OR knob. It defaults to `true` on qwen3.8 [q-occ], so correctness depends on OR forwarding `reasoning`/`reasoning_details` as `reasoning_content` (**UNVERIFIED**).
- **Sampling and decoding parameters are missing.** Not exposed on Alibaba endpoints:
  - `repetition_penalty`, `n` (1–4 on Qwen), `parallel_tool_calls`, `tool_stream`;
  - `vl_high_resolution_images`, per-part `fps`/`min_pixels`/`max_pixels`/`total_pixels`, `translation_options`;
  - `enable_text_image_mixed`, `include_tool_usage`, and the `X-DashScope-DataInspection` header.
  
  `top_k`, `seed`, `presence/frequency_penalty`, `stop`, `logprobs` and `response_format`/`structured_outputs` are supported (endpoints JSON).
- **Caching semantics change.**
  - OR says Alibaba caching "requires explicit cache breakpoints" (`cache_control`) and lists only `qwen3-max`, `qwen-plus`, `qwen3.6-plus`, `qwen3-coder-plus`, `qwen3-coder-flash` ([prompt-caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching.md)). Most `alibaba` endpoints show `supports_implicit_caching:false`.
  - DashScope's implicit cache (always on, 20% hit price) is **mutually exclusive** with explicit cache [q-cache]. Whether OR bills implicit hits at the discount is **UNVERIFIED**.
- **Multimodal limits.**
  - Images and `video_url` work on models listing `image`/`video`.
  - DashScope's frame-list video part `{"type":"video","video":[…]}` has no OR equivalent (**UNVERIFIED**).
  - Audio input exists only on `qwen3.8-omni-flash`, and OR accepts audio only as base64 ([audio](https://openrouter.ai/docs/guides/overview/multimodal/audio.md)).
  - **No Qwen model on OR outputs audio**, so Omni speech output (`modalities:["text","audio"]`) is unreachable.
  - No `qwen-long`/`fileid://`. PDFs go through OR's `file-parser` (default `mistral-ocr`) instead of DashScope/Qwen-Long parsing.
- **Prefill/partial is not documented.** OR assistant prefill → DashScope `partial:true` mapping is undocumented (**UNVERIFIED**). DashScope doesn't support partial on qwen3.8 anyway [q-partial].
- **Stream cancellation differs from direct.** Direct DashScope bills only "the portion generated before the server received the termination request" [q-stream]. OR lists **Alibaba under "Not Currently Supported"** for stream cancellation, so "the model will continue processing and you will be billed for the complete response" ([streaming](https://openrouter.ai/docs/api_reference/streaming.md)).
- **Errors.**
  - `DataInspectionFailed`/`data_inspection_failed` (moderation of input *or output*), `Throttling.*`, `Arrearage` etc. become OR `error_type`s (+`provider_code`), and 500s are masked ([errors](https://openrouter.ai/docs/api_reference/errors-and-debugging.md)).
  - DashScope's silent non-stream partial response (`x-dashscope-partialresponse: true` header) has no documented OR surface (**UNVERIFIED**). Always stream.
- **Models and regions.**
  - Some ids are missing from OR's catalog: the moving alias `qwen3.8-max` (only `qwen3.8-max-0902` snapshot and `-prime`), `qwen-flash`, `qwen-max`, `qwen-long`, `qwen3.5-omni-*`, Qwen-MT, QwQ/QVQ and commercial `qwen3-vl-plus`.
  - Older/smaller open-weight ids (all `qwen3-*` except `qwen3-coder-flash`, `qwen3.5-9b`, `qwen3.6-35b-a3b`, `qwen2.5-*`) have **no Alibaba endpoint**, i.e. third-party fp8/bf16/int4 inference.
  - OR's Alibaba endpoints are `alibaba` (datacenters SG/CN) and `alibaba/us-east-1`. Which DashScope region/domain `alibaba` maps to is **UNVERIFIED**. Beijing-only features (Qwen-Long, guardrail header, BJ pricing) are presumably unreachable.
- **Stateful Responses features are unreachable.** OR's Responses API rejects `store:true`/`previous_response_id` ([responses overview](https://openrouter.ai/docs/api_reference/responses/overview.md)), so DashScope Responses' `previous_response_id`, `conversation`, session cache and `GET /responses/{id}` retrieval are out.
- **What does work via OR, officially hosted:**
  - Proprietary Qwen3.5+–3.8 models are served **only by Alibaba**, so there is no third-party fidelity risk for those ids.
  - The `:batch` variants of `qwen3.8-max-0902` and `qwen3.7-plus` are also Alibaba-served (endpoints JSON).
