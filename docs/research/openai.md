# OpenAI official API (Responses-first) and how OpenRouter maps it

Researched 2026-11-11. I used only live sources fetched that day:

* the OpenAI docs export `https://developers.openai.com/api/llms-full.txt`;
* the per-endpoint reference pages (`…/api/reference/resources/responses/methods/create.md`, `…/streaming-events.md`, etc.);
* the model pages `…/api/docs/models/<id>.md`;
* OpenRouter's `https://openrouter.ai/docs/llms-full.txt` and its OpenAPI spec `https://openrouter.ai/docs/openapi/openapi.yaml`;
* the public OR catalog: `/api/v1/models` and `/api/v1/models/openai/gpt-6-astra/endpoints`.

Conventions:

* **OA** = OpenAI direct (`https://api.openai.com/v1`).
* **OR** = OpenRouter (`https://openrouter.ai/api/v1`).
* **RSP** = Responses API. **CC** = Chat Completions.
* `[tag]` = a source; the URLs are listed at the end.
* **UNVERIFIED** = no doc confirms the point. It is inference and should be tested. On OR, test it with `debug.echo_upstream_body` (see §10.0).

For generic OR behaviour (routing, plugins, other providers) see the sibling file `docs/research/openrouter.md`. This file only covers OpenAI models.

---

## 0. TL;DR for the bot

* **Use RSP for OpenAI.** CC no longer supports tool calling with GPT-6 Astra / GPT-6.1 Sol. From GPT-5.4 on, CC supports tool calling only with `reasoning_effort:"none"` [oa-reasoning][oa-migrate].
  * RSP-only features: built-in tools, reasoning summaries, encrypted reasoning replay, `phase`, `configuration_update`, background mode/resume, compaction.
* **Stateless replay works on OA.**
  * Send `store:false`, then replay *every* output item verbatim: reasoning items (`encrypted_content`), assistant messages with `phase`, and tool-call items.
  * In stateless mode OA now returns `encrypted_content` **by default**. `include:["reasoning.encrypted_content"]` is still accepted as a legacy value [oa-reasoning].
  * Never edit the prefix. Changing earlier items, tools, instructions or `text.format` makes the API **drop later reasoning** [oa-prefix].
* **In-flight recovery is only possible on OA, with background mode.**
  * Send `background:true, stream:true`. Persist `response.id` and the last `sequence_number`. Resume with `GET /v1/responses/{id}?stream=true&starting_after=N`.
  * Background data is held for only ~10 min unless `store:true` [oa-bg][oa-yourdata].
  * Cost: higher TTFT than a synchronous request [oa-bg].
* **OR's RSP is a stateless proxy.**
  * `store` must be `false`. `previous_response_id` is rejected with 400 [or-rsp].
  * There is no `GET/cancel/compact` on `/responses` [or-oas].
  * So there is **no background resume**. A client disconnect cancels the upstream OpenAI generation [or-stream].
* **On OR, pin the provider for OpenAI models:** `provider:{only:["openai"], allow_fallbacks:false, require_parameters:true}`.
  * `openai/gpt-6-astra` is also served by Azure and Amazon Bedrock [or-ep].
  * The Bedrock endpoints have **no native tools**, so `web_search` falls back to Exa. They also ignore `reasoning.mode:"pro"` and run async tools synchronously [or-ep][or-reason][or-gpt6].
  * Without `require_parameters`, unsupported params are silently ignored [or-route].
* **Official built-in tools via OR**
  * `web_search` → OpenAI native search on OpenAI/Azure endpoints.
  * `code_interpreter` → passed through, but on OR credits only with `container:"auto"` and no `file_ids`.
  * `file_search` → BYOK only.
  * `image_generation` (OpenAI shape) → passthrough, with a reduced schema.
  * `shell` → OpenAI hosted shell.
  * **Do not use** `openrouter:image_generation`, `openrouter:shell` or Exa search for an "official tool" claim. OR runs those itself [or-st][or-st-img][or-st-shell].

---

## 1. Responses API request parameters (`POST /v1/responses`)

### 1.1 Current OpenAI models (model pages, 2026-11-11; GPT-6.2 2026-11-18)

| Model id | `reasoning.effort` values (default) | Context / max out | Price in / cached / out per 1M | Notes |
|---|---|---|---|---|
| `gpt-6.2-astra` (2026-11-17) | low, medium, high, xhigh, max (medium; live: the 400 errors list them, a response without effort reports `medium`); `none`/`minimal` → 400 | 1.05M (max input 922K) / 128K | $8 / $0.4 / $40; cache write $10 (30m), $20 (12h) | "Most capable". No Flex (400: "Use background=true with background_options.completion_window instead"). Ultrafast 3× until 2027-02-28 (Pacific), 6× from March 1. `original` detail works (live) though the vision guide's table omits it. No dated snapshot. Knowledge cutoff 2026-05-18 [oa-gpt62] |
| `gpt-6.2-sol` (2026-11-17) | as Astra | 1.05M (922K) / 128K | $2 / $0.1 / $10; cache write $2.5, $5 (12h) | "Go-to model for everyday work". Same tiers as Astra. Knowledge cutoff 2026-06-01 [oa-gpt62] |
| `gpt-6-astra` | low, medium, high, xhigh, max (medium); `none` → HTTP 400 | 1.05M (max input 922K) / 128K | $10 / $1 / $50; cache write $12.5 | Flagship. CC supported, but no tool calling in CC [oa-gpt6]. Tools: web_search, file_search, image_generation, code_interpreter, hosted_shell, apply_patch, skills, computer_use, mcp, tool_search |
| `gpt-6.1-sol` | low…max (medium); no `none`/`minimal` | 1.05M / 128K | $2 / $0.1 / $10 | Ultrafast tier available [oa-ultrafast] |
| `gpt-6.1-terra` | none, low…max (medium) | 1.05M / 128K | $1 / $0.05 / $5 | Vision/writing |
| `gpt-6-sol`, `gpt-6-luna` | none, low…max (medium) | 1.05M / 128K | Sol $2/$0.2/$10; Luna $0.1/$0.01/$0.5 | CC function calling only with `reasoning_effort:"none"` |
| `gpt-5.6-sol`/`-terra`/`-luna` (`gpt-5.6` alias → sol) | none, low…max (medium) | 1.05M / 128K | Sol $4/$0.4/$20; Terra $2/$0.2/$12; Luna $0.2/$0.02/$1.2 | Default `reasoning.context` is `all_turns`; explicit cache breakpoints [oa-gpt56] |
| `gpt-5.5` | none, low…xhigh (medium) | 1.05M / 128K | $5 / $0.5 / $30 | |
| `gpt-5.5-pro`, `gpt-5.4-pro`, `gpt-5.2-pro` | 5.5-pro: medium, high (default), xhigh; 5.4-pro: medium (default), high, xhigh; 5.2-pro: medium, high, xhigh | 1.05M / 128K | 5.5-pro $30/–/$180 | RSP only; use background mode ("may take several minutes") |
| `gpt-5.4`, `gpt-5.4-mini` | none (default), low…xhigh | 1.05M (mini 400K) / 128K | $2.5/$0.25/$15; mini $0.75/$0.075/$4.5 | |
| `gpt-5.2` | none (default), low…xhigh | 400K / 128K | $1.75/$0.175/$14 | |
| `gpt-5.1` (deprecated), `gpt-5` (deprecated) | 5.1: none(default)…high; 5: minimal, low, medium, high | 400K / 128K | $1.25/$0.125/$10 | `minimal` exists only on the gpt-5 generation |
| `chat-latest` | not documented. "Latest Instant model used in ChatGPT"; the snapshot changes. The page lists no reasoning-token support, so it is likely non-reasoning (**UNVERIFIED**) | 400K (max input 272K) / 128K | $5 / $0.5 / $30 | Tools: function, web_search, file_search, image_generation, code_interpreter, mcp. No Batch. `gpt-5.x-chat-latest` are shut down [oa-deprec] |

Sources: [oa-models] and the per-model pages. "Not all reasoning models support every value. See the model page" [oa-create]. Models released after September 1, 2026 deprecate `connector_id` in favour of `server_url` [oa-create].

### 1.2 Parameter reference

| Param | Values / limits | Notes |
|---|---|---|
| `model` | string | |
| `input` | string, or an array of items (§2.2) | Content types: `input_text`, `input_image`, `input_file`. There is **no `input_audio` in RSP** [oa-create][oa-audio] |
| `instructions` | string | System/developer message. **Not carried over** with `previous_response_id` |
| `reasoning.effort` | `none\|minimal\|low\|medium\|high\|xhigh\|max` | Model-dependent (§1.1). An unsupported value → 400 (e.g. Astra + `none`) [oa-reasoning] |
| `reasoning.summary` | `auto\|concise\|detailed` | Opt-in. Without it, `reasoning.summary[]` is empty. `concise` works on computer-use and all reasoning models after gpt-5. `auto` ≈ `detailed` today. *May require organization verification* [oa-reasoning]. `generate_summary` is the deprecated alias |
| `reasoning.context` | `auto\|current_turn\|all_turns` | GPT-5.6 family, GPT-6.x and GPT-6.2 support `all_turns` (5.6 default; 6.2 accepts it, live). Earlier models default to `current_turn`. The response reports the effective value |
| `reasoning.mode` | `standard\|pro` | GPT-5.6/GPT-6 in RSP. Independent of effort. Billed at standard token rates, but uses more tokens |
| `text.verbosity` | `low\|medium\|high` (default medium) | |
| `text.format` | `{type:"text"}` \| `{type:"json_schema", name, schema, strict, description}` \| legacy `{type:"json_object"}` | Structured Outputs. `name` is ≤64 chars `[A-Za-z0-9_-]` |
| `max_output_tokens` | int ≥16 | Includes reasoning tokens. Hitting it → `status:"incomplete"`, `incomplete_details.reason:"max_output_tokens"`, possibly with no visible text. Reserve ≥25K for reasoning [oa-reasoning] |
| `max_tool_calls` | int | Cap across **all built-in** tool calls in the response |
| `parallel_tool_calls` | bool (default true) | |
| `tool_choice` | `none\|auto\|required` \| `{type:"allowed_tools", mode:"auto"\|"required", tools:[…]}` \| `{type:"function",name}` \| `{type:"mcp",server_label,name?}` \| `{type:"custom",name}` \| `{type:"file_search"\|"web_search_preview"\|"computer"\|"computer_use_preview"\|"computer_use"\|"code_interpreter"\|"image_generation"}` \| `{type:"apply_patch"}` \| `{type:"shell"}` \| `{type:"programmatic_tool_calling"}` | To force `web_search` (non-preview), use `required` or `allowed_tools` [oa-websearch] |
| `tools` | function, custom, namespace, file_search, web_search(+_2025_08_26), web_search_preview(+_2025_03_11), code_interpreter, image_generation, shell, local_shell, apply_patch, computer, computer_use_preview, mcp, tool_search, programmatic_tool_calling | §3 |
| `service_tier` | `auto\|default\|flex\|scale\|priority\|fast\|ultrafast` (default auto) | `fast` = `priority` (renamed 2026-07-30). The response echoes `priority` for ≤GPT-5.6. Flex costs 50% but can return 429 "Resource Unavailable", which is not charged [oa-flex]. Fast mode can silently downgrade to `default` when you ramp too fast [oa-fast]. `ultrafast`: the reference says it is available for `gpt-5.6-sol`; the guide says Astra / 6.1 Sol and recommends WebSockets [oa-ultrafast]. The response's `service_tier` is the tier actually used |
| `safety_identifier` | string ≤128 | Stable hashed per-end-user id. Recommended because GPT-5.6+ safeguards block or pause output [oa-gpt56]. From GPT-6.2 on, reasoning made with another identifier, or with one where the new request has none (or the reverse), is withheld (`end_user_mismatch`) [oa-reasoning]: in a group thread, other people's turns lose their reasoning (§1.5) |
| `user` | string | Deprecated. Replaced by `safety_identifier` + `prompt_cache_key` |
| `prompt_cache_key` | string | Cache routing/bucketing. For GPT-5.6+ it is "optional, for separate cache accounting" [oa-cache] |
| `prompt_cache_options` | `{mode:"implicit"\|"explicit", ttl:"30m"\|"12h", prewarm:bool, comparison_response_id}` | GPT-5.6+. Explicit breakpoints go on content blocks: `prompt_cache_breakpoint:{mode:"explicit", ttl?}`, up to 4 written per request. Cache writes cost 1.25× (30m) or 2.5× (12h) on GPT-5.6+ |
| `prompt_cache_retention` | `in_memory\|24h` | Deprecated. On gpt-5.5/5.5-pro only `24h` is accepted (`in_memory` errors) [oa-yourdata] |
| `truncation` | `auto\|disabled` (default disabled) | `disabled`: overflow → 400. `auto` drops the oldest items. Cannot be combined with `configuration_update` |
| `store` | bool (default **true**) | true = stored ≥30 days. ZDR orgs: always treated as false [oa-yourdata] |
| `include` | `reasoning.encrypted_content`, `web_search_call.action.sources`, `web_search_call.results`, `code_interpreter_call.outputs`, `file_search_call.results`, `message.output_text.logprobs`, `message.input_image.image_url`, `computer_call_output.output.image_url` | |
| `top_logprobs` | 0–20 | Together with `include:["message.output_text.logprobs"]` |
| `temperature` (0–2) / `top_p` (0–1) | | See §1.3 |
| `background` | bool | §5 |
| `stream`, `stream_options.include_obfuscation` | bool | Obfuscation pads delta events with a random `obfuscation` string (on by default). Set it to false to save bandwidth |
| `previous_response_id` / `conversation` | string / `conv_…` or `{id}` | Mutually exclusive. §2.3 |
| `context_management` | `[{type:"compaction", compact_threshold:≥1000}]` | Server-side compaction (§2.4) |
| `metadata` | ≤16 pairs; key ≤64, value ≤512 chars | |
| `moderation` | `{model:"omni-moderation-latest", policy:{input:{mode:"score"\|"block"}, output:{…}}}` | Moderation on input/output. Scores arrive after the full output, not with deltas [oa-stream] |
| `image.context_compression` | `auto\|enabled\|disabled` | Request-level image context optimisation |
| `access_programs.cyber` | `standard\|daybreak_blue\|daybreak_red` | Cyber programs. Unauthorized → 403 |
| `prompt` | `{id, version, variables}` | Stored prompt templates. Deprecated 2026-06-03 [oa-deprec] |

**Not in RSP** (CC only): `n`, `stop`, `seed`, `logit_bias`, `frequency_penalty`, `presence_penalty`, `prediction`, `modalities`, `audio` [oa-create][oa-chat].

### 1.3 Sampling restrictions on reasoning models

* GPT-5.2/5.4: `temperature`, `top_p` and `logprobs` are supported **only with `reasoning.effort:"none"`**. Otherwise the request errors.
* `gpt-5`, `gpt-5-mini`, `gpt-5-nano`: they always error [oa-gpt54].
* GPT-6: "When reasoning effort is not `none`, remove `temperature`, `top_p`, and `top_logprobs`… For Responses, remove `message.output_text.logprobs` from `include`" [oa-gpt6].
  * Astra and 6.1 Sol do not support `none`, so for them the restriction always applies.
* `chat-latest`: **UNVERIFIED**. OR lists no `temperature` in its `supported_parameters` [or-models].
* Effective rule for the bot: send sampling params and logprobs only when the effort is `none` on a model that accepts `none`. Show OA's 400 verbatim otherwise.

### 1.4 Mid-conversation changes without breaking the prefix

* **`configuration_update`** input item. Supported on GPT-6+ models in standard single-agent mode; **not** on pro or tournament models; not together with `truncation:"auto"` or auto-compaction; never two adjacent [oa-reasoning][or-gpt6]. Shape:

  ```json
  {"type":"configuration_update","reasoning":{"effort":"high"},"text":{"format":{"type":"json_schema","name":"x","schema":{}}}}
  ```

  It applies from that point on. The response's `reasoning.effort` keeps reporting the request-level value.
* **`additional_tools`** item (role `developer`, a `tools` array) adds tools append-only. Use `allowed_tools` to restrict tools per turn. To change instructions, append a new `developer` message [oa-prefix].
* **`phase`** on assistant messages: `commentary` or `final_answer` (GPT-5.3-codex+/5.4/5.5). Preserve it on replay; dropping it causes early stopping [oa-reasoning].

---

### 1.5 GPT-6.2: what changed (2026-11-17)

* **Withheld reasoning.** From GPT-6.2 on the API drops replayed reasoning items it would have kept before; `input_transformations` names the reason [oa-gpt62][oa-reasoning]. Measured on gpt-6.2-sol (2026-11-18): one turn, then a stateless follow-up replaying it with one change each:

  | Change in the follow-up | Earlier reasoning |
  |---|---|
  | nothing; `reasoning.effort`; `text.verbosity`; `max_output_tokens`; a `configuration_update` item | kept |
  | another `safety_identifier`, or none (the first request had one) | withheld: `end_user_mismatch` |
  | request-level `tools` (`[]` → `[web_search]`) | withheld: `prefix_mismatch` (and +4.4K input tokens: the search tool's definition) |
  | the same tool as an `additional_tools` developer item | kept, but only +4 input tokens: whether the hosted tool really works that way is **UNVERIFIED** |

  **With `store: true` (what the bot sends) the drop is silent**: the same probe with `store: true` reports `input_transformations: []` for another or a missing identifier, yet the input is 54 tokens instead of 66 (the reasoning item is gone); the bot's own group test (another member replying) showed the same. So the bot keeps what each answer was made with (fingerprints of its `safety_identifier` and `tools`, `made_with` in the native payload) and says the withholding ahead.
  The guides recommend append-only changes (`configuration_update` for effort and `text.format`, `additional_tools` / `allowed_tools` for tools) [oa-gpt62]. The bot keeps the per-user `safety_identifier` (the owner's choice: abuse is then blocked per person) and says in plain words why reasoning was withheld.
* **No Flex** on GPT-6.2: `service_tier: "flex"` → 400 pointing to **background completion windows** (public beta, 2026-11-17: GPT-6.2 Astra/Sol, GPT-6.1 Terra, GPT-6 Luna): `background: true`, `background_options.completion_window: "2h" | "8h"` (30% / 40% below Standard token rates), `service_tier: "default"`, no streaming; polled or a webhook; `continuation_id` for follow-ups. Not used (the owner: not now).
* **Ultrafast** (HTTP works; this account has it, live): 3× Standard through 2027-02-28, 6× from 2027-03-01 (Pacific) on GPT-6.2; GPT-6 Astra and 6.1 Sol stay 6×. US data residency only.
* **Caching** as on GPT-5.6+: cache reads 0.05×, writes 1.25× (`30m`, default) or 2.5× (`12h`).
* Also listed for 6.2 but not new: Multi-agent beta (6.1 Sol and GPT-5.6 too), `reasoning.mode: pro`.

## 2. Stateless multi-turn on OA (`store:false`)

### 2.1 Rules

1. Send `store:false`.
   * Reasoning items in `output[]` carry `encrypted_content` by default.
   * When streaming, take the final item from `response.output_item.done`. The copy in `.added` may be incomplete [oa-retrieve].
2. Next turn: `input` = the original input items + **all** `output` items of each response, in their original order. Then append new user items or tool outputs [oa-reasoning][oa-state].
3. Keep reasoning items with `id`, `summary[]` and `encrypted_content` unchanged.
   * Reasoning is reused **only within one model family**: the 5.6 sol/terra/luna family shares it, but nothing crosses between 5.6 and 5.5.
   * Incompatible reasoning is silently omitted when you switch models [oa-reasoning].
4. `reasoning.context:"all_turns"` (the 5.6 default) renders earlier turns' reasoning. It needs the replayed items. On the first turn it behaves like `current_turn`.
5. **Do not mutate history.** Shortening tool output, rewriting a developer message, or replacing turns with a summary all count as prefix changes. After a prefix change the API **drops subsequent reasoning items** (anti-distillation). Use OA compaction instead; it preserves reasoning [oa-prefix].
   * Bot impact: "edit an earlier message" or "switch model mid-chat" are legal, but they degrade the run. Surface this to users.
6. Even with replay, all input tokens are billed again (caching discounts apply) [oa-state].

### 2.2 What to pass back (input item types accepted by RSP [oa-create])

| Item `type` | Replay? | Notes |
|---|---|---|
| `message` (role user/assistant/system/developer; assistant may carry `phase`) | yes | Assistant `content[]` holds `output_text` (with `annotations`, `logprobs`) or `refusal` |
| `reasoning` (`id`, `summary[]`, `content[]?`, `encrypted_content`, `status`) | **yes, mandatory with tools** | "pass back the reasoning items returned with the function calls along with the tool outputs" |
| `function_call` / `function_call_output` (`call_id`; GPT-6 may carry `async:true`) | yes | Async tools: the output can arrive in a later request with the same `call_id` [or-gpt6] |
| `custom_tool_call` / `custom_tool_call_output` | yes | |
| `web_search_call` (`id`, `status`, `action`) | yes | |
| `code_interpreter_call` (`id`, `code`, `container_id`, `outputs`, `status`) | yes | Auto container mode "reuses an active container used by a previous `code_interpreter_call` item in the model's context". Containers expire after 20 min idle [oa-ci] |
| `image_generation_call` (`id`, `result` = base64, `status`, …) | yes | Edits can reference a prior image by `{"type":"image_generation_call","id":"ig_…"}`. Resolving id-only references needs stored responses (**UNVERIFIED** for `store:false`; replay the full item with `result` instead) [oa-imgtool] |
| `file_search_call`, `mcp_list_tools`, `mcp_call`, `mcp_approval_request`/`_response`, `shell_call`/`_output`, `local_shell_call`/`_output`, `apply_patch_call`/`_output`, `computer_call`/`_output`, `tool_search_call`/`_output`, `program`/`program_output` | yes | |
| `compaction` (`encrypted_content`) / `compaction_trigger` (must be last) / `configuration_update` / `additional_tools` / `item_reference` (`{type,id}`; needs storage) | yes | |

Minimal replay example:

```json
{"model":"gpt-6.1-sol","store":false,"reasoning":{"effort":"medium","summary":"auto","context":"all_turns"},
 "input":[
  {"role":"user","content":"Find the bug"},
  {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"…"}],"encrypted_content":"gAAAA…"},
  {"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"…"}]},
  {"role":"user","content":"Now patch it"}]}
```

### 2.3 Stored IDs vs local replay

| | `store:false` + full replay | `previous_response_id` | `conversation` (Conversations API) |
|---|---|---|---|
| State lives | locally (bot DB) | at OA, 30-day TTL | at OA, **no TTL** (until deleted) |
| ZDR | yes | no (store is forced false) | no |
| `instructions` | resent each time | not carried over | — |
| Branching / edits | free, but prefix rules apply | fork from any stored id | linear |
| Cross-provider portability | items are portable; reasoning is not | OA only | OA only |

Sources: [oa-state][oa-yourdata]. Recommendation: keep local replay as the canonical path (it matches the project principles). `previous_response_id` is only a performance option, and over OR it is impossible.

### 2.4 Compaction (OA)

* **Server-side compaction**: `context_management:[{"type":"compaction","compact_threshold":200000}]`.
  * When the rendered tokens cross the threshold, the server compacts mid-request.
  * It emits a `compaction` output item, plus `response.compaction.compacting` progress events (at most every 30 s).
  * It is ZDR-friendly with `store:false`.
  * Next turn: append the output as usual. Items before the latest `compaction` item may then be dropped [oa-compaction][oa-events].
* **Explicit compaction**: put a `compaction_trigger` item last in `/responses`.
* **Standalone `POST /v1/responses/compact`** returns a new window.
  * Pass the window on **unmodified**: no pruning, no `instructions`.
  * It rejects histories that contain `configuration_update` [oa-compaction][oa-reasoning].
* OA compaction keeps the reasoning that follows it, even though the prefix changes. Home-grown summaries should be appended at the **end** of the context [oa-prefix].

---

## 3. Built-in tools (OA)

### 3.1 `web_search` (and legacy `web_search_preview`)

| Param | Values | Notes |
|---|---|---|
| `type` | `web_search` \| `web_search_2025_08_26`; legacy `web_search_preview` \| `web_search_preview_2025_03_11` | Preview lacks `filters`, `external_web_access` and `return_token_budget` |
| `search_context_size` | `low\|medium\|high` (medium) | Not an exact token count |
| `user_location` | `{type:"approximate", city, region, country (ISO-2), timezone (IANA)}` | **Omitted → defaults to US**. Pass `{"type":"approximate"}` with no fields to avoid the fallback |
| `filters.allowed_domains` / `filters.blocked_domains` | up to 100 each, no scheme | Subdomains included. Allowed list in the reference; blocked list in the guide |
| `external_web_access` | bool (default true) | false = cache/offline only |
| `return_token_budget` | `default\|unlimited` | GPT-5+ reasoning search only |
| `search_content_types` + `image_settings:{max_results, caption}` | `["image","text"]` | Image results; read them with `include:["web_search_call.results"]` |

* **Output**: `web_search_call {id, status, action}`.
  * `action` is `{type:"search", query, queries[], sources[]}`, `{type:"open_page", url}` or `{type:"find_in_page", pattern, url}`.
  * `sources` (all consulted URLs, `{type:"url", url}`, including `oai-sports|weather|finance` feeds) appear only with `include:["web_search_call.action.sources"]`.
  * Image results are `{type:"image_result", image_url, source_website_url, thumbnail_url, caption}`.
  * The message `output_text.annotations[]` carries `{"type":"url_citation","start_index","end_index","url","title"}`.
  * **Display requirement:** "you must show inline citations as prominent, clickable links".
* **Limits**: the search context window is capped at 128K. `tool_choice:"auto"` makes search optional. For gpt-5, effort must be above `minimal`.
* **Pricing**: $10/1k calls plus search-content tokens at model rates. Preview on non-reasoning models: $25/1k (content tokens free). Only `search` actions are billed. `response.tool_usage.web_search.num_requests` counts billable searches [oa-websearch][oa-pricing][oa-retrieve].
* **CC**: only the `gpt-5-search-api` model with `web_search_options:{search_context_size, user_location:{type:"approximate", approximate:{…}}}`. It always searches, with no filters or sources. `gpt-4o(-mini)-search-preview` shut down 2026-07-23 [oa-websearch][oa-chat].

### 3.2 `code_interpreter` (model calls it "the python tool")

```json
{"type":"code_interpreter","container":{"type":"auto","memory_limit":"4g","file_ids":["file-1"],"network_policy":{"type":"disabled"}}}
```

* **Containers**
  * Auto mode creates a container or reuses one.
  * Explicit mode: `POST /v1/containers {name, memory_limit:"1g|4g|16g|64g", file_ids, expires_after:{anchor:"last_active_at", minutes}, network_policy, skills}`, then pass its id as `container:"cntr_…"`.
  * `file_ids` ≤50.
  * Containers **expire after 20 min idle**: their data is unrecoverable and calls fail. Any container operation refreshes `last_active_at` [oa-ci][oa-containers].
* **Outputs**
  * `code_interpreter_call.outputs[]` holds `{type:"logs", logs}` or `{type:"image", url}`. It is populated only with `include:["code_interpreter_call.outputs"]`.
  * Generated files appear as message annotations: `{"type":"container_file_citation","container_id","file_id":"cfile_…","filename","start_index","end_index"}`.
* **Download**: `GET /v1/containers/{container_id}/files/{file_id}/content` returns raw bytes. List with `GET /v1/containers/{id}/files`. Upload with `POST /v1/containers/{id}/files` (multipart or `{file_id}`) [oa-cfile][oa-ci].
  * Input files in the prompt are auto-copied into the container.
  * **Bot**: download artifacts immediately after `response.completed`, before the 20-min expiry, and upload them to Telegram.
* **Supported files**: c, cs, cpp, csv, doc, docx, html, java, json, md, pdf, php, pptx, py, rb, tex, txt, css, js, sh, ts, jpeg/jpg, gif, png, pkl, tar, xlsx, xml, zip.
* **Pricing**: per 20-min container session: $0.03 (1g), $0.12 (4g), $0.48 (16g), $1.92 (64g). Billed per minute with a 5-min minimum. Rate limit 100 RPM per org [oa-ci][oa-pricing].

### 3.3 `image_generation`

| Param | Values |
|---|---|
| `model` | `gpt-image-2` (the tool default, although the catalog marks gpt-image-2 / 1.5 / 1-mini / chatgpt-image-latest "Deprecated"), `gpt-image-2.5-sunburst`/`-flare` (+`-2026-09-08`), `gpt-image-2.6-sunburst`/`-flare`, `gpt-image-1.5`, `gpt-image-1-mini`, `chatgpt-image-latest` (`gpt-image-1` shut down 2026-10-23) [oa-create][oa-models]. The top-level `model` must be a mainline model (list in [oa-imgtool]; includes `chat-latest` and all GPT-5.2+/6 models) |
| `action` | `auto` (default) \| `generate` \| `edit` |
| `size` | `auto`, `1024x1024`, `1024x1536`, `1536x1024`, or any `WxH` for gpt-image-2/2.5 (both sides divisible by 16, ratio 1:3–3:1, max 3840x2160, >2560x1440 experimental) |
| `quality` | `auto`, `low`, `medium`, `high` (+ `xhigh`, `max` on 2.5) |
| `background` | `auto\|opaque\|transparent` (transparent needs png/webp; preview on gpt-image-2) |
| `output_format` / `output_compression` | `png` (default) \| `webp` \| `jpeg` \| `gif` \| `svg` / 0–100 |
| `partial_images` | 0–3 (streaming; not with gif) |
| `moderation` | `auto\|low` |
| `input_fidelity` | `high\|low` (gpt-image-1.5; omit for gpt-image-2) |
| `input_image_mask` | `{file_id \| image_url}` |
| `complexity`, `frame_count` (2–16), `frame_duration_ms`, `svg:{model, reasoning}` | planning effort, GIF and SVG options |

* **Output**: `image_generation_call {id, result:<base64>, status, revised_prompt, size, quality, background, output_format, action, error?{code, message, moderation_details}}`.
* **Streaming**: `response.image_generation_call.partial_image {partial_image_b64, partial_image_index, size, quality, background, output_format}`.
* **Forcing**: `tool_choice:{"type":"image_generation"}`.
* **Usage**: in `tool_usage.image_gen {input_tokens, input_tokens_details{image_tokens, text_tokens}, output_tokens, …}`.
* Send `safety_identifier` on every request, including edits [oa-imgtool][oa-create][oa-retrieve].

### 3.4 `file_search`

* Shape: `{"type":"file_search","vector_store_ids":[…],"max_num_results":1-50,"filters":{…},"ranking_options":{"ranker":"auto","score_threshold":0.5,"hybrid_search":{…}}}`.
* It needs pre-built vector stores (Files API plus vector stores).
* Results appear only with `include:["file_search_call.results"]`.
* Citations are `file_citation {file_id, filename, index}`.
* Pricing: $2.50/1k calls plus $0.10/GB-day storage (1 GB free) [oa-filesearch][oa-pricing].
* Bot value: low. A per-chat RAG needs vector-store lifecycle management; sending `input_file` directly is simpler.

### 3.5 Other tools: usefulness for a chat bot

| Tool | Executes where | Bot verdict |
|---|---|---|
| `shell` with `environment:{type:"container_auto"\|"container_reference"\|"local"}` | Hosted: OA container (Debian 12, `/mnt/data`, Python 3.11/Node 22/Java 17/Go 1.23…, no network unless org allowlist + `network_policy`). Same container/files API and pricing as code_interpreter [oa-shell] | Useful (a superset of the python tool). RSP only |
| `apply_patch` | **client** applies the diffs | Not useful (needs a client file system) |
| `computer` / `computer_use_preview` | **client** executes actions and returns screenshots | Not useful in Telegram |
| `local_shell` | client; legacy, tied to `codex-mini-latest` (removed) [oa-deprec] | No |
| `mcp` (`server_url` \| `tunnel_id` \| deprecated `connector_id` such as `connector_gmail`…; `require_approval` defaults to `"always"`; `allowed_tools`, `headers`, `authorization`) | OA calls a remote MCP server | Optional power feature. Approval flow = `mcp_approval_request` → `mcp_approval_response` |
| `tool_search` / `namespace` / `defer_loading` | OA | Only for large tool catalogs (gpt-5.4+) |
| `programmatic_tool_calling` (+`allowed_callers`) | OA hosted JS runtime; ZDR-compatible; no container cost | Niche |
| Skills | mounted into the hosted shell | Niche |

---

## 4. Streaming (SSE) events — complete list from [oa-events]

Every event carries `type` and `sequence_number`. Item-scoped events also carry `output_index` and `item_id`. Content-scoped events also carry `content_index`.

| Family | Events (key payload fields) |
|---|---|
| Lifecycle | `response.queued`, `response.created`, `response.in_progress`, `response.completed`, `response.failed`, `response.incomplete` (all carry the full `response`) |
| Items | `response.output_item.added`, `response.output_item.done` (`item`; use `.done` as the final item for replay) |
| Content parts | `response.content_part.added`, `response.content_part.done` (`part`: `output_text`\|`refusal`\|`reasoning_text`) |
| Text | `response.output_text.delta` (`delta`, `logprobs[]`), `response.output_text.done` (`text`), `response.output_text.annotation.added` (`annotation_index`, `annotation`: `url_citation`\|`file_citation`\|`container_file_citation`\|`file_path`) |
| Refusal | `response.refusal.delta`, `response.refusal.done` (`refusal`) |
| Reasoning summary | `response.reasoning_summary_part.added`, `response.reasoning_summary_part.done` (`summary_index`, `part{type:"summary_text",text}`, `status?:"incomplete"`), `response.reasoning_summary_text.delta`, `response.reasoning_summary_text.done` (`summary_index`, `delta`/`text`) |
| Raw reasoning | `response.reasoning_text.delta`, `response.reasoning_text.done`. OA says reasoning items "remain opaque, and the API does not return their reasoning text" for its models, so expect these only from open-weight/other models [oa-reasoning] |
| Function / custom tools | `response.function_call_arguments.delta/.done` (`arguments`), `response.custom_tool_call_input.delta/.done` (`input`) |
| Web search | `response.web_search_call.in_progress` → `.searching` → `.completed` |
| File search | `response.file_search_call.in_progress` → `.searching` → `.completed` |
| Code interpreter | `response.code_interpreter_call.in_progress` → `.interpreting` → `.completed`; `response.code_interpreter_call_code.delta/.done` (`code`) |
| Image gen | `response.image_generation_call.in_progress` → `.generating` → `.partial_image` (`partial_image_b64`, `partial_image_index`) → `.completed` |
| MCP | `response.mcp_list_tools.in_progress/.completed/.failed`; `response.mcp_call.in_progress/.completed/.failed`; `response.mcp_call_arguments.delta/.done` |
| Shell | `response.shell_call_command.added/.delta/.done` (`command_index`, `command`), `response.shell_call_output_content.delta/.done` |
| Compaction | `response.compaction.compacting` (progress only, ≤1 per 30 s) |
| Audio (legacy) | `response.audio.delta`, `.done`, `.audio.transcript.delta`, `.done` |
| Error | `error` (`code`, `message`, `param`, `sequence_number`) |
| Transport | `keepalive`: appears in background streams. Its `sequence_number` is a **connection counter, not a resume position**. Never store it as a cursor [oa-bg] |

**`sequence_number`**
* It is the per-response, monotonically increasing event index and the resume cursor for `starting_after`.
* Reference examples show `response.created` with 1 (**UNVERIFIED** whether numbering starts at 0 or 1; treat it as opaque and monotonic).
* Delta events may also carry an `obfuscation` padding string; ignore it.
* There is no `rate_limits.updated` in RSP SSE; that event exists only in the Realtime API.
* WebSocket mode (`wss`, `response.create` events, 60-min connection cap, `stream_id` lanes, ≤16 in-flight) adds `response.steer.*` events for mid-turn steering [oa-ws].

A typical order (shape per [oa-events]; ids abbreviated):

```text
event: response.created              data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_…","status":"in_progress",…}}
event: response.output_item.added    data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_…","summary":[]}}
event: response.reasoning_summary_text.delta  data: {"item_id":"rs_…","output_index":0,"summary_index":0,"delta":"**Planning**…"}
event: response.output_item.done     data: {…"item":{"type":"reasoning","id":"rs_…","summary":[…],"encrypted_content":"gAAAA…"}}
event: response.web_search_call.searching     data: {"item_id":"ws_…","output_index":1}
event: response.output_text.delta    data: {"item_id":"msg_…","output_index":2,"content_index":0,"delta":"Paris","logprobs":[]}
event: response.output_text.annotation.added  data: {…"annotation_index":0,"annotation":{"type":"url_citation","url":"https://…","title":"…","start_index":0,"end_index":5}}
event: response.completed            data: {"type":"response.completed","response":{…,"usage":{…}}}
```

---

## 5. Background mode and stream resumption (OA)

| Aspect | Behaviour [oa-bg][oa-cancel][oa-retrieve][oa-yourdata] |
|---|---|
| Start | `POST /v1/responses {…, "background":true}` → returns immediately with `status:"queued"`/`"in_progress"` |
| Poll | `GET /v1/responses/{id}` while `queued`/`in_progress`. Terminal states: `completed`, `failed`, `incomplete`, `cancelled` |
| Stream + resume | Create with `background:true, stream:true`, keep the cursor = last non-keepalive `sequence_number`. Resume: `GET /v1/responses/{id}?stream=true&starting_after=N` (optional `include_obfuscation`, `include[]`). **Only possible if the response was created with `stream:true`** |
| Cancel | `POST /v1/responses/{id}/cancel` (background only; idempotent; returns the final Response). A synchronous request is cancelled by closing the connection |
| `store` | Allowed with `store:false`, but the data is "temporarily stored to disk for roughly 10 minutes". ZDR projects always run with `store=false`. For Modified Abuse Monitoring projects, background responses outlive the polling period only with explicit `store:true` |
| Retention with `store:true` | ≥30 days; can be removed early with `DELETE /v1/responses/{id}` |
| Latency | "time to first token… from a background response is higher than… a synchronous one" |
| Webhooks | `response.completed\|failed\|cancelled\|incomplete`. Retried up to 72 h with backoff; dedupe on the `webhook-id` header [oa-webhooks][oa-webhooks-ref] |

**Recovery recipe for the bot**
1. Create the request with `background:true, stream:true`. Use `store:true` if you need recovery after a restart longer than ~10 min. To minimise retention, `DELETE` the response once the final state is in local storage.
2. On `response.created`/`response.queued`, persist `{response_id, telegram_msg_ids}` **before** rendering. Every few events, persist `last_seq` (skip `keepalive`) together with the accumulated render state.
3. Network drop → reconnect with `GET …?stream=true&starting_after=last_seq`.
   * Process restart → the same call using the persisted `last_seq`.
   * Alternatively request from the beginning (omit `starting_after`) and rebuild the message from scratch. **UNVERIFIED**: that omitting `starting_after` replays from the first event.
4. If the resume stream fails, fall back to `GET /v1/responses/{id}` and poll with backoff until terminal. A 404 means it expired or was deleted: mark the generation lost and offer a regenerate.
5. User "stop" → `POST …/cancel`.
6. Replay for the next turn uses the final `output` items from `response.completed` (or `GET`), never the delta-assembled text.

**UNVERIFIED**: how long the event log stays resumable after completion (presumably as long as the response is stored).
**UNVERIFIED**: whether a WebSocket-mode response survives a dropped socket. WebSocket mode does not use `background`, and the connection cache disappears on close [oa-ws].

---

## 6. Inputs

### 6.1 Images (`input_image`)

* Shape: `{"type":"input_image","image_url":"https://…" | "data:image/png;base64,…","file_id":"file-…","detail":"auto|low|high|original"}`. Default `detail` is `auto`. `file_id` comes from the Files API (`purpose:"user_data"` recommended for model inputs).
* **Formats**: PNG, JPEG, WEBP, non-animated GIF. Up to 1,500 images and 512 MB total payload per request. Each image ≤30,000 patches after resizing, or the request is rejected [oa-vision].
* **`original` detail**: gpt-6-astra, gpt-5.6-*, gpt-5.5, gpt-5.4 family. **Not** gpt-5.2/5.1/4.1/4o.
  * Not in the guide's table but measured (2026-11-18, one 2560×1440 PNG): gpt-6.2-astra, gpt-6.2-sol, gpt-6.1-sol, gpt-6.1-terra, gpt-6-sol and gpt-6-luna take 4,332 input tokens at `original` and 2,942 at `high`, so `original` keeps the full resolution there too.
  * Astra / 5.6: `auto` = `original`. `high` = ≤2,500 patches. `low` fits 512×512.
  * Image tokens = 32 px patches × 1.2 multiplier on GPT-5.2+. Example: a 1024² image at `high` = 1,229 tokens [oa-vision].

### 6.2 Files (`input_file`)

* Shape: `{"type":"input_file","filename":"a.pdf","file_data":"data:application/pdf;base64,…"| "file_id":"file-…" | "file_url":"https://…","detail":"auto|low|high"}`.
* **RSP accepts much more than PDF** [oa-files]:

  | File kind | How RSP processes it |
  |---|---|
  | PDF | Text **plus page images** (vision models). `detail` affects page images only: `auto` = `high` on GPT-5.6+, `low` on earlier models |
  | Text/code (.txt .md .json .html .xml .py .js .sql .srt .vtt … about 100 MIME types) | Text extraction |
  | Rich docs (.doc .docx .rtf .odt, Pages, Google Docs) | Text only. Embedded images/charts are **not** extracted; convert to PDF to keep them |
  | Presentations (.ppt .pptx, Keynote, Slides) | Text only |
  | Spreadsheets (.xls .xlsx .csv .tsv .iif, Sheets) | First 1,000 rows per sheet plus model-generated summary/header metadata |

* **Limits**: each file <50 MB, and 50 MB combined per request.
  * Page limit: not stated in the current docs (**UNVERIFIED**).
  * CC accepts **PDF only** (`file_data`/`file_id`; no `file_url`, no `detail`).

### 6.3 Audio and video

* **Audio input**: not in RSP ("Audio: Coming soon").
  * Use CC with `gpt-audio-1.5`: content part `{"type":"input_audio","input_audio":{"data":"<b64>","format":"wav|mp3"}}`.
  * Audio output: `modalities:["text","audio"]` with `audio:{voice, format:"wav|mp3|flac|opus|pcm16|aac"}`.
  * Or transcribe first with `/v1/audio/transcriptions` (`gpt-transcribe`).
  * GPT-Live / Realtime are separate session APIs [oa-audio][oa-chat].
* **Video input**: none for chat models. Only Sora/Videos generation exists [oa-models].

---

## 7. Usage, pricing and accounting

```json
"usage":{"input_tokens":75,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,
  "cache_write_tokens_details":{"ttl_30m_tokens":0,"ttl_12h_tokens":0}},
 "output_tokens":1186,"output_tokens_details":{"reasoning_tokens":1024},"total_tokens":1261},
"tool_usage":{"web_search":{"num_requests":2},
 "image_gen":{"input_tokens":…,"input_tokens_details":{"image_tokens":…,"text_tokens":…},"output_tokens":…,"output_tokens_details":{"image_tokens":…,"text_tokens":…},"total_tokens":…}}
```

Sources: [oa-retrieve][oa-reasoning].

* `tool_usage` is separate from base `usage`. It may be omitted, and its image counts may skip incomplete records.
* RSP `usage` has **no** image-input-token breakdown; image tokens are counted in `input_tokens`. CC usage adds `prompt_tokens_details.audio_tokens`, `completion_tokens_details.{accepted,rejected}_prediction_tokens` and `audio_tokens`.
* **Pricing levers**
  * Prompts over 272K input tokens cost 2× input/cache and 1.5× output for the whole request (Astra, 5.4…).
  * Cache writes cost 1.25× uncached input on GPT-5.6+ (2.5× for the 12h TTL). Cache reads cost 0.1× (0.05× on 6.1 Sol/Terra).
  * Batch/Flex cost 50%. Fast mode costs 2× (Astra).
  * Tool fees are listed in §3 [oa-pricing][oa-cache][oa-models].
* **Org-level accounting** needs an **Admin API key**; admin keys cannot call inference [oa-admin].
  * `GET /v1/organization/usage/completions?start_time&bucket_width=1m|1h|1d&group_by=project_id,user_id,api_key_id,model,batch,service_tier,api_source&…`. Per-bucket fields: `input_tokens`, `input_cached_tokens`, `input_uncached_tokens`, `input_text_tokens`, `input_image_tokens`, `input_audio_tokens`, `output_*`, `num_model_requests`.
  * Sibling endpoints (`…/usage/embeddings`, `images`, `audio_speeches`, `code_interpreter_sessions`, `vector_stores`, `moderations`).
  * `GET /v1/organization/costs?group_by=project_id,user_id,line_item,api_key_id,api_source` (daily buckets) [oa-usage-api][oa-costs-api].
  * There is **no end-user dimension**: `safety_identifier` is not a `group_by`. Per-Telegram-user cost must be computed from each response's `usage` and `tool_usage` plus the price table, or isolated per project/API key.

---

## 8. Errors and retries (OA)

* **HTTP error body**: `{"error":{"message":"…","type":"rate_limit_error|invalid_request_error|…","param":"service_tier"|null,"code":"slow_down"|…}}`.
* Headers: `x-request-id` (quote it to support), `openai-processing-ms`, `Retry-After`, `x-ratelimit-{limit,remaining,reset}-{requests,tokens}`, `x-ratelimit-*-project-tokens` [oa-errors][oa-ratelimits][oa-apiov].

| Status / code | Meaning | Retry? |
|---|---|---|
| 400 `invalid_request_error` (e.g. bad `service_tier`, unsupported effort, sampling param on a reasoning model, context overflow with `truncation:"disabled"`) | client | no; show verbatim |
| 401 / 403 (country, Daybreak access) | auth / policy | no |
| 408 | timeout (SDKs retry twice; Flex) | yes |
| 429 `rate_limit_error` + (no code \| `rate_limit_exceeded` \| `slow_down` ramp-rate) | rate | yes; honour `Retry-After`, else exponential backoff with jitter |
| 429 billing: `credit_balance_exhausted`, `organization_spend_limit_exceeded`, `project_spend_limit_exceeded`, `organization_usage_limit_exceeded` (type may still be `insufficient_quota`) | quota | **no** |
| 429 Flex "Resource Unavailable" | capacity (not charged) | yes, or retry with `service_tier:"auto"` |
| 500 | server | yes, briefly |
| 503 `service_unavailable_error` / `server_is_overloaded` | overload | yes; `Retry-After` |
| 502 / 504 | gateway | yes (generic HTTP practice; not listed by OA, **UNVERIFIED**) |

* **Mid-stream failures**: an `error` event (`code`, `message`, `param`), then the stream ends; or a `response.failed` whose `response.error.code` is one of:
  * `server_error`, `rate_limit_exceeded` (retryable);
  * `invalid_prompt`, `bio_policy`, `misalignment_policy_violation` (with a `misalignment{detailed_explanation, error_type, review_target, steer}` object), `data_residency_mismatch` (not retryable);
  * `vector_store_timeout` (retry maybe);
  * image errors (`invalid_image`, `invalid_image_format`, `invalid_base64_image`, `invalid_image_url`, `image_too_large`, `image_too_small`, `image_parse_error`, `image_content_policy_violation`, `invalid_image_mode`, `image_file_too_large`, `unsupported_image_media_type`, `empty_image_file`, `failed_to_download_image`, `image_file_not_found`) (fix the input).
  * Sources: [oa-retrieve][oa-events].
* **`response.incomplete`** carries `incomplete_details.reason`: `max_output_tokens`, `max_messages`, `content_filter`, or `steered` (WebSocket steering). It is not an error: render the partial output and say why.
* GPT-5.6+ safeguards can pause a stream for several seconds while classifiers review output [oa-gpt56]. Do not time out early.
* WebSocket-only codes: `previous_response_not_found`, `websocket_connection_limit_reached` [oa-errors].

---

## 9. Chat Completions vs Responses (what matters for us)

| Feature | CC (`/v1/chat/completions`) | RSP |
|---|---|---|
| Reasoning knob | `reasoning_effort` (same values) | `reasoning.{effort,summary,context,mode}` |
| Reasoning summaries / encrypted CoT replay | **no** | yes |
| Tool calling with reasoning | GPT-5.4+: only with `reasoning_effort:"none"`. GPT-6 Astra/6.1 Sol: no function calling at all | yes |
| Built-in tools (web_search filters/sources, code_interpreter, image_generation, file_search, shell, MCP, computer) | only search models (`gpt-5-search-api` + `web_search_options`) | yes |
| `n` (1–128), `stop`, `seed`, `logit_bias`, `frequency/presence_penalty` | yes | no |
| `prediction` (Predicted Outputs) | yes | no |
| Audio in/out (`modalities`, `audio`, `input_audio`) | yes (`gpt-audio-1.5`) | no |
| File inputs | PDF only | PDF plus docs/sheets/code |
| `logprobs` | `logprobs:true` + `top_logprobs` | `include` + `top_logprobs` |
| `verbosity`, `service_tier`, `safety_identifier`, `prompt_cache_*`, `moderation`, `store` (default **false** in CC), `metadata` | yes | yes |
| Streaming | `chat.completion.chunk` deltas; `stream_options.include_usage` gives a final usage chunk (lost if the stream breaks) | typed semantic events, `sequence_number`, background resume |
| `phase`, `configuration_update`, compaction, `truncation`, background, `previous_response_id`, conversations | no | yes |

Sources: [oa-chat][oa-chat-stream][oa-migrate][oa-reasoning][oa-gpt6][oa-files].

---

## 10. How OpenRouter exposes OpenAI models

### 10.0 Basics

* **Endpoint**: `POST https://openrouter.ai/api/v1/responses` ("OpenResponses" format). This is the only `/responses` method; there is no GET, cancel, input_items, compact or input_tokens [or-oas][or-rsp].
  * Generation id: the `X-Generation-Id` header. Response ids look like `gen-…`/`resp_…` [or-stream].
  * Assistant messages in replayed history **must** carry `id` and `status` [or-rsp-basic].
* **Model slugs**: `openai/gpt-6.2-astra`, `openai/gpt-6.2-sol` (and `-pro`; OpenAI the only provider, with `openai/fast` and `openai/ultrafast` endpoints, 2026-11-18), `openai/gpt-6-astra`, `openai/gpt-6.1-sol`, `openai/gpt-6.1-terra`, `openai/gpt-6-sol`, `openai/gpt-6-luna`, `openai/gpt-5.6-{sol,terra,luna}`, `openai/gpt-5.5`, `openai/gpt-5.4`, …
  * Pro variants: `openai/<model>-pro`, also selectable via `reasoning.mode:"pro"`.
  * Floating aliases: `~openai/gpt-{astra,sol,terra,luna,mini}-latest`.
  * `chat-latest` is `openai/gpt-chat-latest`.
  * The catalog's `reasoning.supported_efforts` matches OA (e.g. Astra `[max,xhigh,high,medium,low]`) [or-models].
* **Providers behind `openai/gpt-6-astra`** (live endpoints API): OpenAI (default, `openai/flex`, `openai/fast` = priority, `openai/ultrafast`), Azure (global, us), Amazon Bedrock (global, us-west-2).
  * `native_tools` is `{openrouter:web_search→web_search, openrouter:apply_patch→apply_patch}` on OpenAI and Azure, and **empty on Bedrock**.
  * Bedrock also lacks `seed` [or-ep].
* **Transparency controls**
  * `provider.require_parameters:true` (default false → unsupported params "ignored").
  * `provider.only`, `allow_fallbacks:false`.
  * `debug.echo_upstream_body:true` (streaming only) returns the exact upstream body. Use it to verify every UNVERIFIED item below [or-route][or-err].

### 10.1 Feature-by-feature mapping (OR Responses API unless noted)

| Feature (OA name) | OR status | OR parameter / shape | Caveats |
|---|---|---|---|
| `reasoning.effort` | **Supported** | `reasoning.effort` (`max…none`); CC: `reasoning.effort` or `reasoning_effort` | Per-model `supported_efforts` in `/models`. An unsupported effort in `configuration_update` → 400. Request-level mapping behaviour for OpenAI: **UNVERIFIED** (OR maps to "nearest" for some vendors) [or-reason] |
| `reasoning.summary` | **Supported (schema)** | `reasoning.summary: auto\|concise\|detailed` | Summary events are in the OR stream schema. OA org-verification requirement on OR's own key: **UNVERIFIED**. RSP ignores `reasoning.exclude`; reasoning items are always returned [or-oas][or-reason] |
| Encrypted reasoning replay | **Supported** | Echo `reasoning` items (`encrypted_content`, `summary`, `content`, `signature`) | OR **auto-injects** the encrypted-content request (a hidden param, though benign). Cross-provider (OpenAI↔Azure↔Bedrock) decryptability is **UNVERIFIED**; the formats differ (`openai-responses-v1` vs `azure-openai-responses-v1` vs `bedrock-openai-responses-v1`), so pin one provider [or-reason] |
| `reasoning.context` / `reasoning.mode` | **Supported** | same | `mode:"pro"` reroutes to the `*-pro` slug; OpenAI/Azure only (Bedrock silently ignores it). Cannot be combined with `configuration_update` [or-reason][or-gpt56] |
| `configuration_update` | **Supported** | the same item; CC extension: `{"role":"system","content":"","configuration_update":{…}}` | GPT-6/6.1 standard slugs only. Routed only to endpoints that accept it, else 400 [or-gpt6] |
| `phase` | **Supported (RSP only)** | same | Not representable in CC [or-gpt54] |
| Async tools (`async:true`) | **Supported (RSP)** | same | OpenAI/Azure honour it. Bedrock runs the call synchronously; on non-supporting routes the flag is stripped [or-gpt6] |
| `text.verbosity` | **Supported** | `text.verbosity` (CC: `verbosity`) | Soft routing preference [or-route] |
| `text.format` (structured outputs) | **Supported** | same / CC `response_format` | |
| `max_output_tokens` | **Supported** | same | Ultrafast eligibility depends on the credit check against max output [or-tiers] |
| `max_tool_calls` | **Changed semantics** | top-level `max_tool_calls` = OR **server-tool** step budget (default 30 = max); `stop_server_tools_when` | Whether OA's built-in-tool cap is forwarded: **UNVERIFIED** [or-st] |
| `service_tier` | **Supported** | `auto\|default\|flex\|priority\|fast\|scale\|ultrafast` | Tiers are separate endpoints. The reported tier is the one that served. `ultrafast` falls back to priority, then default. `scale`: enum only, undocumented [or-tiers][or-oas] |
| `safety_identifier` / `user` | **Partial** | both accepted | OR **hashes** them into its own upstream identity and never forwards the raw value. Only `user` attributes requests in OR analytics [or-user][or-oas] |
| `prompt_cache_key` | **Supported** | same | Also the sticky-routing key (after `session_id`/`x-session-id`). Forwarding to OA: **UNVERIFIED** [or-cache] |
| `prompt_cache_options` / `prompt_cache_breakpoint` | **Supported** | same (`mode:"explicit"` is required in the schema, `ttl`) | GPT-5.6+. Stripped for providers that don't support it [or-gpt56] |
| `prompt_cache_retention` | **Unsupported** (not in schema) | — | Deprecated on OA anyway |
| `truncation` | **Supported** | `auto\|disabled` | |
| `store` | **Forced false** | `store:false` (const) | `true` → 400 [or-rsp] |
| `include` | **Partial** | enum: `reasoning.encrypted_content`, `code_interpreter_call.outputs`, `file_search_call.results`, `message.input_image.image_url`, `computer_call_output.output.image_url`, `output.encrypted_content` | **Missing:** `web_search_call.action.sources`, `web_search_call.results`, `message.output_text.logprobs` [or-oas] |
| logprobs | **Unclear** | `top_logprobs` accepted | No include for `message.output_text.logprobs`. Not in the GPT-6 `supported_parameters` (expected for reasoning models) [or-oas][or-models] |
| `parallel_tool_calls`, `tool_choice`, `metadata` (16×64/512), `instructions`, `temperature`/`top_p` | **Supported** | same | OA restrictions still apply (sampling params only with effort `none`) |
| `background` | **Unsupported in practice** | the field exists in the schema | No retrieve/cancel/`starting_after`, and store is forced false. Semantics **UNVERIFIED** |
| `previous_response_id` / `conversation` | **Unsupported** | — | `previous_response_id` → 400. `conversation` is not in the schema |
| Server-side compaction (`context_management`), `/responses/compact` | **Unsupported** | — | `compaction` items and `compaction_trigger` input items **are** in the schema (explicit compaction, **UNVERIFIED** end-to-end). OR's `context-compression` plugin is not OA compaction |
| `stream_options.include_obfuscation`, `moderation`, `image.context_compression`, `prompt` | not in schema | — | OR has its own `moderation` plugin/guardrails |
| `access_programs.cyber` | **BYOK only** | same | A non-`standard` value on OR's own OpenAI key → 400 [or-oas] |
| Streaming events | **Mostly** | OA event names plus `response.debug`, `response.fusion_call.*`, `response.apply_patch_call_operation_diff.*` | Not in the OR spec: `response.queued`, `response.file_search_call.*`, `response.compaction.compacting`, `response.shell_call_*`, `response.audio.*`. SSE comment lines `: OPENROUTER PROCESSING` must be skipped [or-oas][or-stream] |
| Mid-stream errors | **Changed** | `response.failed` with `response.error{code,message}` plus a top-level `error_type` | Many causes collapse to `code:"server_error"`; use `error_type`. **500 messages are masked** ("generic string"). Content blocks keep the verbatim provider payload in `metadata.raw`/`provider_code` [or-err] |
| Usage | **Supported + extras** | `usage.{input_tokens, input_tokens_details{cached_tokens, cache_write_tokens}, output_tokens_details.reasoning_tokens, cost, is_byok, cost_details{upstream_inference_cost, …}}`; `server_tool_use_details.web_search_requests` | OA's `tool_usage` object is not in the OR docs (**UNVERIFIED**). Post-hoc: `GET /api/v1/generation?id=` [or-usage] |
| Images in | **Supported** | `input_image` with `image_url` (URL/data URL) or `file_id`, `detail:auto\|high\|low\|original` | The OR schema marks `detail` as **required**. PNG/JPEG/WEBP/GIF [or-oas][or-img] |
| PDF/files in | **Supported** | `input_file` (`file_data`, `file_url`, `file_id`, `filename`) | No `detail` field (OA's PDF page-image detail is lost). With a native-capable model the PDF goes through unparsed; `file-parser` plugin engines `native\|cloudflare-ai\|mistral-ocr` (OCR billed even with BYOK). Non-PDF types (docx/xlsx…) to OpenAI: **UNVERIFIED**. `or_file_…` ids are for OR sandboxes; OA file ids with `?provider=openai` need BYOK [or-pdf][or-files] |
| Audio in | **OR extension** | RSP `input_audio{data, format:mp3\|wav}`; CC any format | OA RSP has no audio. How OR routes this to `openai/gpt-audio` is **UNVERIFIED**. Audio output only via CC (`modalities`, `audio`) [or-audio] |
| Video in | **Not for OpenAI** | `input_video` (Gemini etc.) | OpenAI chat models have no video input |

### 10.2 Built-in tools via OR

| OA tool | OR handling | Verdict |
|---|---|---|
| `web_search` (any variant) | Alias of `openrouter:web_search`.<br>• `engine:auto` (default) → native OpenAI `web_search` on OpenAI/Azure endpoints; **Exa fallback** where there is no native tool (Bedrock!), or with `engine:"exa"` etc.<br>• Native OpenAI: `allowed_domains` honoured, `excluded_domains` ignored, `user_location` honoured. `max_results`, `search_context_size` and `max_characters` are documented as "ignored with native provider search" (for OA's own `search_context_size` field on the native tool: **UNVERIFIED**).<br>• No `external_web_access`, `return_token_budget`, `search_content_types`/`image_settings`, or sources `include`.<br>• Annotations: `url_citation` (`url`, `title?`, indices; Exa adds `content`).<br>• Price $0.01/search on `openai/*`; usage in `server_tool_use_details` [or-st][or-st-ws][or-models] | **Partial.** Pin `provider.only:["openai"]` and `engine:"native"` (a workspace-admin "native-only" list → 403 instead of a silent Exa fallback) |
| `code_interpreter` | Passed through to OA unchanged ("hosted tools the provider executes itself"). On OR credits: **`container:"auto"` only, no `file_ids`**; explicit container ids or file ids need BYOK. Output items and `code_interpreter_call_code.*` events are in the schema. The OR annotation schema lists only `file_citation`/`url_citation`/`file_path`, **not `container_file_citation`**. OR's `/containers/*` endpoints serve only OR sandboxes, so OpenAI container files cannot be downloaded on OR credits (**UNVERIFIED**, undocumented) [or-oas][or-st-toolsearch][or-containers] | **Partial.** Fine for text/plot answers; generated-file download is a gap unless you use BYOK and call OA directly |
| `image_generation` (OA shape) | In the schema with `background, input_fidelity, input_image_mask, model, moderation, output_compression, output_format (png/webp/jpeg), partial_images, quality (low/medium/high/auto), size`. **Missing:** `action`, `xhigh`/`max`, `gif`/`svg`, `complexity`, frame options. Partial-image events are in the stream schema. Output item lists only `id/result/status` (`revised_prompt` etc.: **UNVERIFIED**) [or-oas] | **Partial / UNVERIFIED end-to-end.** `openrouter:image_generation` is a *different* OR-run tool (inner call to `openai/gpt-image-2.6-flare` by default; returns an `openrouter:image_generation` item with a data URL) [or-st-img] |
| `file_search` | `vector_store_ids` are OpenAI vector stores, so **BYOK only** (on OR credits → 400) [or-oas] | BYOK only |
| `shell` | On OpenAI models `{type:"shell"}` uses **OpenAI's hosted shell** (`engine:auto`); `openrouter:shell` / `engine:"openrouter"` = OR sandbox (`/workspace/home`, 5-min sleep, files kept 30 days, downloadable via OR `/containers/{id}/files/{file_id}/content`). RSP + Messages only; global endpoint only [or-st-shell][or-containers] | Native: supported (downloading OpenAI-side files is the same gap as code_interpreter, **UNVERIFIED**) |
| `apply_patch` | native on OpenAI/Azure (`native_tools`) | Not useful for the bot |
| `mcp` | `McpServerTool` (OA shape) in the schema; OR also has its own `McpServerTool_OpenRouter` | **UNVERIFIED** which one runs |
| `computer_use_preview` | in the schema (the GA `computer` type is not) | Not useful |
| `tool_search` | OR's own `openrouter:tool_search` (state carried in encrypted reasoning) | Not needed |

### 10.3 Streaming, cancellation, recovery on OR

* **Errors before commit** are normal HTTP errors `{"error":{"code":<int>,"message":…,"metadata":{…}}}`:
  * 400; 401; 402 (no credits; sometimes `Retry-After` for the in-flight budget); 403 (guardrail, budget, moderation); 408; 429; 502 (provider down); 503 (no provider meets the routing constraints). `Retry-After` comes on 429/503 [or-err].
* **After commit (200 sent)** every failure is in-band: `response.failed` (or `error`).
  * The error can be the first and only event, so a 200 with no output is a failure.
  * Upstream length errors become `response.incomplete` with `max_output_tokens`.
  * A prompt-overflow error becomes `response.failed` with `invalid_prompt` [or-stream][or-err].
* **OpenAI GPT-6 bio/cyber blocks**: a 403 with `error_type:"refusal"`. In-stream: `response.failed` with `error.code:"bio_policy"|"cyber_policy"`. Not retried, and not failed over to another provider [or-gpt6].
* **Cancellation**: aborting the stream stops OpenAI/Azure processing and billing [or-stream].
  * There is **no resume**. After a crash, only cost/metadata are recoverable via `GET /api/v1/generation?id=gen-…` [or-usage].
  * Stored prompt/completion text (`GET /api/v1/generation/content?id=`) exists only if Input & Output Logging was enabled before the request. Retention is ≥3 months, so it is a debugging aid rather than a recovery path [or-oas][or-logs].

---

## 11. OpenRouter gaps for OpenAI

* **No background mode, no stream resumption, no polling, no server-side cancel.** OR has only `POST /responses`, and `store` is forced `false`. An in-flight generation dies with the client connection; OA's `GET /v1/responses/{id}?stream=true&starting_after=N` has no OR equivalent [or-rsp][or-oas][or-stream] vs [oa-bg].
* **No stored state:** `previous_response_id` → 400. No `conversation` and no Conversations API. No `GET /responses/{id}`, `input_items` or `DELETE` [or-rsp][or-oas].
* **No server-side compaction** (`context_management`) and no `POST /responses/compact`. Only explicit `compaction_trigger` items are in the schema (untested) [or-oas] vs [oa-compaction].
* **`include` is truncated.** No `web_search_call.action.sources` (full source lists), no `web_search_call.results` (image search results), no `message.output_text.logprobs` [or-oas] vs [oa-create].
* **Web search is degraded or ambiguous.**
  * `web_search` is aliased to OR's server tool. It silently becomes **Exa** on endpoints without native search (the Bedrock GPT-6 endpoints have none).
  * `excluded_domains`/`blocked_domains` are ignored on native OpenAI search.
  * No `external_web_access`, `return_token_budget`, or image search (`search_content_types`/`image_settings`) [or-st-ws][or-ep][or-oas] vs [oa-websearch].
* **Code interpreter is crippled on OR credits:**
  * no explicit containers;
  * no `file_ids` (BYOK required);
  * no documented way to download generated container files (`container_file_citation` is absent from OR's annotation schema; OR's containers API covers only OR sandboxes) [or-oas][or-containers] vs [oa-ci][oa-cfile].
* **`file_search` requires BYOK** (OpenAI vector stores) [or-oas].
* **The image_generation tool schema lags OA.**
  * No `action`, no `xhigh`/`max` quality, no `gif`/`svg`, no `complexity`/frame options.
  * The output item schema omits `revised_prompt`/`size`/… (**UNVERIFIED** whether they are passed through).
  * `openrouter:image_generation` is not OpenAI's hosted tool [or-oas][or-st-img] vs [oa-create][oa-imgtool].
* **PDF `detail` (`auto|low|high` page-image fidelity) is not in OR's `input_file`.**
  * Non-PDF `input_file` types (docx/xlsx/pptx/code) are undocumented for OpenAI routing.
  * Without a native route, OR may OCR or parse with Mistral/Cloudflare (a hidden transformation, billed) [or-oas][or-pdf] vs [oa-files].
* **Identity is rewritten.** `safety_identifier`/`user` are folded into OR's hashed identity and never forwarded raw. OpenAI cannot use your own per-user id. `user_id` attribution exists only in OR analytics [or-user].
* **Errors are less verbatim.**
  * RSP error codes collapse to `server_error` (the canonical cause is in `error_type`).
  * **500-class messages are masked.**
  * OpenAI `x-ratelimit-*` headers are not documented as forwarded (**UNVERIFIED**).
  * A 402 for credits replaces OA's billing codes [or-err] vs [oa-errors][oa-ratelimits].
* **Provider mixing risk.**
  * GPT-6 Astra is also served by Azure and Bedrock.
  * Bedrock ignores `reasoning.mode:"pro"`, runs async tools synchronously, lacks native tools and `seed`, and is on OR's list of providers where aborting a stream does **not** stop generation or billing [or-stream].
  * Encrypted reasoning portability across providers is unproven.
  * Default routing silently ignores unsupported params unless `require_parameters:true`.
  * ZDR enforcement on OR **removes first-party OpenAI endpoints** (Azure remains) [or-ep][or-reason][or-gpt6][or-route][or-zdr].
* **`max_tool_calls` is repurposed** as OR's server-tool step budget (default and max 30) [or-st] vs [oa-create].
* **Stream fidelity**
  * Not in OR's stream schema: `response.queued`, file_search progress, compaction progress, shell command/output deltas.
  * OR adds SSE comments and OR-specific events.
  * The OA `obfuscation` toggle is absent [or-oas][or-stream] vs [oa-events].
* **Missing request params:** `prompt_cache_retention`, `moderation` (OA in-request moderation), `image.context_compression`, `stream_options`, `conversation`, `context_management`. Daybreak `access_programs` needs BYOK [or-oas].
* **Accounting**
  * OR returns `cost` (a plus).
  * OA `tool_usage` (image_gen tokens, billable web search count) is not documented on OR; OR has `server_tool_use_details` instead.
  * OA Usage/Costs Admin APIs don't see OR traffic (it runs on OR's OpenAI org unless you use BYOK) [or-usage] vs [oa-usage-api].
* **Org-gated OA features** (summaries need org verification, Daybreak, Scale tier, data residency, custom service-tier project policy) depend on OR's OpenAI account unless you use BYOK. BYOK costs 5% of list price after the free allowance [or-byok]; whether gated features then match OA direct is **UNVERIFIED**.

---

## Sources

OpenAI (all fetched 2026-11-11; the `.md` twins are at the same URL + `.md`):

* [oa-create] https://developers.openai.com/api/reference/resources/responses/methods/create
* [oa-retrieve] https://developers.openai.com/api/reference/resources/responses/methods/retrieve
* [oa-cancel] https://developers.openai.com/api/reference/resources/responses/methods/cancel
* [oa-events] https://developers.openai.com/api/reference/resources/responses/streaming-events
* [oa-reasoning] https://developers.openai.com/api/docs/guides/reasoning
* [oa-prefix] https://developers.openai.com/api/docs/guides/preserved-reasoning
* [oa-state] https://developers.openai.com/api/docs/guides/conversation-state
* [oa-compaction] https://developers.openai.com/api/docs/guides/compaction (+ https://developers.openai.com/api/reference/resources/responses/methods/compact)
* [oa-bg] https://developers.openai.com/api/docs/guides/background
* [oa-stream] https://developers.openai.com/api/docs/guides/streaming-responses
* [oa-ws] https://developers.openai.com/api/docs/guides/websocket-mode
* [oa-webhooks] https://developers.openai.com/api/docs/guides/webhooks
* [oa-webhooks-ref] https://developers.openai.com/api/reference/resources/webhooks
* [oa-websearch] https://developers.openai.com/api/docs/guides/tools-web-search
* [oa-ci] https://developers.openai.com/api/docs/guides/tools-code-interpreter
* [oa-containers] https://developers.openai.com/api/reference/resources/containers/methods/create
* [oa-cfile] https://developers.openai.com/api/reference/resources/containers/subresources/files/subresources/content/methods/retrieve
* [oa-imgtool] https://developers.openai.com/api/docs/guides/tools-image-generation
* [oa-filesearch] https://developers.openai.com/api/docs/guides/tools-file-search
* [oa-shell] https://developers.openai.com/api/docs/guides/tools-shell
* [oa-vision] https://developers.openai.com/api/docs/guides/images-vision
* [oa-files] https://developers.openai.com/api/docs/guides/file-inputs
* [oa-audio] https://developers.openai.com/api/docs/guides/audio-chat-completions
* [oa-pricing] https://developers.openai.com/api/docs/pricing
* [oa-cache] https://developers.openai.com/api/docs/guides/prompt-caching
* [oa-errors] https://developers.openai.com/api/docs/guides/error-codes
* [oa-ratelimits] https://developers.openai.com/api/docs/guides/rate-limits
* [oa-apiov] https://developers.openai.com/api/reference/overview
* [oa-admin] https://developers.openai.com/api/docs/guides/admin-apis
* [oa-usage-api] https://developers.openai.com/api/reference/resources/organization/subresources/audit_logs/subresources/usage/methods/completions
* [oa-costs-api] https://developers.openai.com/api/reference/resources/organization/subresources/audit_logs/subresources/usage/methods/costs
* [oa-chat] https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create
* [oa-chat-stream] https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events
* [oa-migrate] https://developers.openai.com/api/docs/guides/migrate-to-responses
* [oa-gpt6] https://developers.openai.com/api/docs/guides/latest-model/gpt-6-astra
* [oa-gpt62] https://developers.openai.com/api/docs/models/gpt-6.2-astra, …/gpt-6.2-sol, https://developers.openai.com/api/docs/guides/latest-model ("Using GPT-6.2"), https://developers.openai.com/api/docs/guides/keep-prefix-unchanged, https://developers.openai.com/api/docs/guides/background (completion windows), changelog 2026-11-17 (all fetched 2026-11-18 as `.md`)
* [oa-gpt56] https://developers.openai.com/api/docs/guides/latest-model/gpt-5.6
* [oa-gpt54] https://developers.openai.com/api/docs/guides/latest-model/gpt-5.4 (also gpt-5.2 "parameter compatibility")
* [oa-models] https://developers.openai.com/api/docs/models, https://developers.openai.com/api/docs/models/all, and per-model pages `https://developers.openai.com/api/docs/models/{gpt-6-astra,gpt-6.1-sol,gpt-6.1-terra,gpt-6-sol,gpt-6-luna,gpt-5.6-sol,gpt-5.5,gpt-5.5-pro,gpt-5.4,gpt-5.4-mini,gpt-5.2,chat-latest}`
* [oa-yourdata] https://developers.openai.com/api/docs/guides/your-data
* [oa-flex] https://developers.openai.com/api/docs/guides/flex-processing
* [oa-fast] https://developers.openai.com/api/docs/guides/fast-mode
* [oa-ultrafast] https://developers.openai.com/api/docs/guides/ultrafast-mode
* [oa-deprec] https://developers.openai.com/api/docs/deprecations

OpenRouter (all fetched 2026-11-11):

* [or-rsp] https://openrouter.ai/docs/api_reference/responses/overview
* [or-rsp-basic] https://openrouter.ai/docs/api_reference/responses/basic-usage
* [or-reason] https://openrouter.ai/docs/guides/best-practices/reasoning-tokens
* [or-oas] https://openrouter.ai/docs/openapi/openapi.yaml (`ResponsesRequest`, `ResponseIncludesEnum`, `*ServerTool`, `InputImage`, `InputFile`, `OpenAIResponsesAnnotation`, `/responses`)
* [or-stream] https://openrouter.ai/docs/api_reference/streaming
* [or-err] https://openrouter.ai/docs/api_reference/errors-and-debugging
* [or-st] https://openrouter.ai/docs/guides/features/server-tools
* [or-st-ws] https://openrouter.ai/docs/guides/features/server-tools/web-search
* [or-st-img] https://openrouter.ai/docs/guides/features/server-tools/image-generation
* [or-st-shell] https://openrouter.ai/docs/guides/features/server-tools/shell
* [or-st-toolsearch] https://openrouter.ai/docs/guides/features/server-tools/tool-search
* [or-containers] https://openrouter.ai/docs/guides/features/containers
* [or-files] https://openrouter.ai/docs/guides/features/files-api
* [or-pdf] https://openrouter.ai/docs/guides/overview/multimodal/pdfs
* [or-img] https://openrouter.ai/docs/guides/overview/multimodal/image-understanding
* [or-audio] https://openrouter.ai/docs/guides/overview/multimodal/audio
* [or-tiers] https://openrouter.ai/docs/guides/features/service-tiers
* [or-cache] https://openrouter.ai/docs/guides/best-practices/prompt-caching
* [or-usage] https://openrouter.ai/docs/cookbook/administration/usage-accounting
* [or-user] https://openrouter.ai/docs/cookbook/administration/user-tracking
* [or-route] https://openrouter.ai/docs/guides/routing/provider-selection
* [or-zdr] https://openrouter.ai/docs/guides/features/zdr
* [or-byok] https://openrouter.ai/docs/guides/overview/auth/byok
* [or-gpt56] https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/gpt-5-6
* [or-gpt6] https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/gpt-6
* [or-gpt54] https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/gpt-5-4
* [or-models] https://openrouter.ai/api/v1/models (live catalog: `reasoning.supported_efforts`, `supported_parameters`, pricing)
* [or-ep] https://openrouter.ai/api/v1/models/openai/gpt-6-astra/endpoints (live: providers, tiers, `native_tools`, per-endpoint params)
* [or-logs] https://openrouter.ai/docs/guides/features/input-output-logging
