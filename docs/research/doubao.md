# Doubao / ByteDance Seed: Volcengine Ark (CN), BytePlus ModelArk (intl), and OpenRouter

Research date: **2026-11-11**. Sources were fetched live that day:
- the Ark docs corpus. `https://www.volcengine.com/docs/82379` now 301-redirects to `docs.volcengine.com/docs/ark`, and every page has a Markdown twin listed in [A-llms];
- the BytePlus ModelArk Markdown corpus [B-llms];
- OpenRouter docs (their `.md` twins) and the public OR JSON (`/api/v1/models`, `…/endpoints`, `/api/v1/providers`, `/api/v1/images/models/…`).

No API request was sent. Keys like `[A-chat]` resolve to URLs in §S. **UNVERIFIED** means no fetched doc settles the point: it is an inference, the docs conflict, or it needs a live call. For OR, check it with `debug.echo_upstream_body` [OR-debug].

Naming: Ark (China) ids look like `doubao-seed-…`, BytePlus ids like `dola-seed-…` or `seed-…` [B-models], and OR slugs like `bytedance-seed/…` [OR-models]. "Seed 2.x" below means the Doubao-Seed 2.0/2.1 family.

## 0. TL;DR

| Topic | Ark (CN) direct | Via OpenRouter |
|---|---|---|
| Who serves OR's Seed | n/a | The OR provider `seed` is **BytePlus**: its privacy policy and ToS point to docs.byteplus.com, HQ `SG` [OR-prov]. OR prices equal BytePlus list prices for all 2.x models [B-price][OR-models] |
| Flagships | `doubao-seed-2-1-pro-260915`, `doubao-seed-2-1-lite-260915` and the rolling `doubao-seed-evolving` (1M context) [A-models] | **Not on OR.** BytePlus does not sell them either [B-models] |
| Best surface | **Responses API** for built-in tools, stored state and explicit cache. **Chat API** for stateless replay (it now also has built-in Doubao Search) [A-chat][A-resp] | CC or Responses, stateless only [OR-rsp] |
| Thinking | On by default. `thinking.type` is `enabled`/`disabled`. `reasoning_effort` accepts 7 values; on Seed they collapse to off (`minimal`)/low/medium/high [A-think] | `reasoning.effort` none…high on 2.0. 2.1 Turbo exposes no effort selector [OR-models] |
| Multi-turn | 2.1, Evolving and 2.0-lite-260428 return a **summary** in `reasoning_content` plus **`encrypted_content`**. Echo `encrypted_content` in tool loops; omitting it gives no error but worse results [A-agent] | OR's `reasoning_details` has no ByteDance format, so the round trip is **UNVERIFIED** |
| Built-in tools | Doubao Search (Chat `web_search_options`, Responses `web_search`, Messages `web_search_20250305`), web content plugin, `image_process`, `doubao_app`, `knowledge_search`, remote MCP, `apply_patch` [A-tools] | None. Every Seed endpoint has `native_tools: {}` [OR-ep] |
| Resume after disconnect | None. A client disconnect stops generation, and the tokens generated so far are billed [A-faq] | None |

## 1. Model line-up

### 1.1 Ark (China) LLM/VLM models as listed on 2026-11-11

- **Limits** are context / max input / max output / max CoT [A-models]. Max output is the answer only, and the API default is 4k.
- **RPM/TPM** apply per account per model and are shared across versions and endpoints [A-text].
- **Price** is CNY per 1M tokens for input / cache hit / output, at the lowest input tier [A-price]. The bot prices every input-length tier, the fast (低延迟) and flex (低优) tables and audio input (`ark/mod.rs` `MODELS`, from the price page as fetched 2026-11-13). Measured 2026-11-13: `prompt_tokens` includes `prompt_tokens_details.audio_tokens` (57 text tokens alone, 90 with a 5 s clip of 32 audio tokens).

| Model id | Limits (tokens) | Input modalities | Thinking (default effort) | Reasoning returned | RPM / TPM | Price | Status | On OR? |
|---|---|---|---|---|---|---|---|---|
| `doubao-seed-evolving` | 1024k/1024k/256k/256k | text, image, video, PDF; **no audio** [A-evolving] | on (`high`) | summary + encrypted | 500 / 1M | 6 / 1.2 / 30 | rolling id; last update 2026-11-05 [A-evolving] | no |
| `doubao-seed-2-1-pro-260915` | 1024k/1024k/256k/256k | text, image, video, PDF | on (`high`) | summary + encrypted | 500 / 1M | 6 / 1.2 / 30 | current | no |
| `doubao-seed-2-1-lite-260915` | 1024k/1024k/256k/256k | + **audio** [A-models] | on (`high`) | summary + encrypted | 30k / 5M | 0.8 / 0.16 / 2.7 (audio in 12) | current | no |
| `doubao-seed-2-1-turbo-260628` | 256k/256k/256k/256k | text, image, video, PDF | on (`high`) | summary + encrypted | 500 / 1M | 3 / 0.6 / 15 | current | as `seed-2-1-turbo` via BytePlus `dola-seed-2-1-turbo-260628` (inferred from name and price) |
| `doubao-seed-2-1-pro-260628` | 256k/256k/256k/256k | text, image, video, PDF | on (`high`) | summary + encrypted | 500 / 1M | 6 / 1.2 / 30 | older | no |
| `doubao-seed-2-0-lite-260428` (LTS) | 256k/224k/128k/128k | + audio | on (`medium`) | summary + encrypted [A-agent] | 30k / 5M | 0.6 / 0.12 / 3.6 (3 tiers up to 256k) | LTS, ≥1-year lifecycle [A-dep] | ≈ `seed-2.0-lite` (version **UNVERIFIED**) |
| `doubao-seed-2-0-mini-260428` (LTS) | 256k/224k/128k/128k | + audio | on (`medium`) | raw CoT [A-think] | 30k / 5M | 0.2 / 0.04 / 2.0 | LTS | ≈ `seed-2.0-mini` |
| `doubao-seed-2-0-pro-260215` | 256k/224k/128k/128k | text, image, video, PDF | on (`medium`); temperature/top_p fixed | raw CoT | 30k / 5M | 3.2 / 0.64 / 16 | EOS **2026-11-24** [A-dep] | no (BytePlus sells `seed-2-0-pro-260328`) |
| `doubao-seed-2-0-lite-260215`, `…-mini-260215` | 256k/224k/128k/128k | n/a | on (`medium`) | raw CoT | n/a | n/a | EOS 2026-10-22 per [A-dep], yet still listed in [A-models] | no |
| `doubao-seed-2-0-code-preview-260215` | 256k/224k/128k/128k | text, image, video | on (`medium`) | raw CoT | 30k / 5M | 3.2 / 0.64 / 16 | EOS 2026-11-24, successor `doubao-seed-evolving` [A-dep] | ≈ `seed-2.0-code` (BytePlus `seed-2-0-code-preview-260328`, inferred from name and price) |
| `doubao-seed-character-260628` | 128k/96k/32k/128k | text, image, video | on (`medium`) | n/a | 30k / 5M | 0.8 / 0.16 / 2.0 | current | no |
| `doubao-seed-character-251128` | 128k/96k/32k | text | **none** | n/a | 30k / 5M | same | current | no |
| `doubao-seed-translation-250915` | 4k/1k/3k | text | none | n/a | 5k / 500k | 1.2 / n/a / 3.6 | current | no |

- **Retired**: `doubao-seed-1-8-251228`, `doubao-seed-1-6-250615/251015`, `doubao-seed-1-6-flash-250615/250828` and `doubao-seed-1-6-vision-250815`. EOS was **2026-09-21**. Leftover endpoints were switched automatically to `doubao-seed-2-0-lite-260428` or `-mini-260428` [A-dep].
  - So the old bot's 1.6/1.8 ids are dead. Calling them by Model ID now most likely returns `InvalidEndpointOrModel.NotFound` (**UNVERIFIED**).
- **Thinking default**: models tagged "深度思考" think unless told otherwise. Turn it off with Chat `thinking.type=disabled` or Responses `reasoning.effort=minimal` [A-models].
- **Tool support**: every Seed 2.x model above except `character-251128` supports function calling, knowledge search, MCP, the web plugin, image_process and doubao_app [A-models].
- **Non-LLM models**: Seedream 5.0 pro/flash (`doubao-seedream-5-0-pro-260628`, `-flash-260915`), Seedance 2.5/2.0 video, Seed3D and embedding-vision [A-models].

### 1.2 BytePlus ModelArk (international): what OR actually reaches

Base URLs are `https://ark.ap-southeast.bytepluses.com/api/v3` (Johor, all models) and `https://ark.eu-west.bytepluses.com/api/v3` (Dublin: `seed-2-0-lite` only, Chat and Image APIs only). Inference may spill over between regions [B-region].

| BytePlus id | ctx / in / out incl. CoT / CoT | Audio | Reasoning returned | USD/1M in / cached / out (≤128k; >128k doubles) | OR slug and price | OR `max_completion_tokens` |
|---|---|---|---|---|---|---|
| `dola-seed-2-1-turbo-260628` | 256K/256K/256K/256K, 0.5K RPM | no | summary + encrypted [B-think] | 0.5 / 0.1 / 2.5 (flat to 256k) | `seed-2-1-turbo` 0.5 / 2.5 | 235,929 |
| `seed-2-0-lite-260428` (LTS) | 256K/224K/128K/128K | yes | summary + encrypted | 0.25 / 0.05 / 2.00 | `seed-2.0-lite` 0.25 / 2 (0.5 / 4) | 131,072 |
| `seed-2-0-mini-260428` (LTS) | same | yes | raw | 0.10 / 0.02 / 0.40 | `seed-2.0-mini` 0.1 / 0.4 (0.2 / 0.8) | 131,072 |
| `seed-2-0-code-preview-260328` | same | no | raw (**UNVERIFIED**) | 0.50 / 0.10 / 3.00 | `seed-2.0-code` 0.5 / 3. OR `expiration_date` is **2026-11-11** | 131,072 |
| `seed-2-0-pro-260328` | same | no | raw | 0.50 / 0.10 / 3.00 | not on OR | n/a |
| Retiring **2026-11-11**: `seed-2-0-lite-260228`, `seed-2-0-mini-260215`, `seed-1-8-251228`, `seed-1-6-250915/250615`, `seed-1-6-flash-250715/250615` | 1.x: 256K/224K/64K/32K | n/a | n/a | n/a | `seed-1.6`, `seed-1.6-flash`. OR `expiration_date` is 2026-11-11 | 32,768 |

**What every OR Seed text endpoint looks like** [OR-ep]:

| Property | Value |
|---|---|
| Endpoint | one per model, provider "Seed", tag `seed/fp8`, `quantization: fp8` |
| Caching | `supports_implicit_caching: false`, and no cache-read price |
| Tools | `native_tools: {}` |
| Service tier | `service_tier: null` |
| `supported_parameters` | `reasoning, include_reasoning, frequency_penalty, max_tokens, temperature, top_p, stop, tools, tool_choice, response_format, structured_outputs`, plus `reasoning_effort` on 2.0-lite, 2.0-mini and 2.0-code |
| `supports_tool_choice` | on 2.x only `none`/`auto` (`required:false, function:false`); on 1.6 all four are true |
| Reasoning metadata | 2.0-lite/mini: `supported_efforts [high, medium, low, minimal, none]`, default `medium`. 2.0-code: `[high, medium, low, none]`. 2.1 Turbo: only `{mandatory:false}`, which OR defines as "does not expose effort selection" [OR-reason] |
| Input modalities | text, image, video. No audio, no file [OR-models] |

- **Upstream version**: OR canonical slugs carry OR listing dates (`…seed-2-1-turbo-20260810`, `…seed-2.0-lite-20260309`), not Ark version suffixes [OR-models]. Which BytePlus version each slug hits is **UNVERIFIED**.
- **Precision**: neither Ark nor BytePlus documents serving precision. Since OR's Seed provider *is* BytePlus, the `fp8` label probably describes the official deployment itself (**UNVERIFIED**).
- **Image/video generation on OR**: `bytedance-seed/seedream-5-0-pro|flash|lite` and `seedream-4.5` (served from `/api/v1/images`; hidden from the default `/models` and listed under `?output_modalities=image`); `bytedance/seedance-2.5|2.0|2.0-fast|2.0-mini|1-5-pro` [OR-img][OR-vid].

## 2. Ark API surfaces, auth and ids

All paths below are on `https://ark.cn-beijing.volces.com`.

| Surface | Endpoint | Notes |
|---|---|---|
| Chat API (OpenAI CC-compatible) | `POST /api/v3/chat/completions` | Stateless. API key or AK/SK; AK/SK requires an Endpoint ID [A-chat][A-auth] |
| Responses API | `POST /api/v3/responses`; `GET`/`DELETE /api/v3/responses/{id}`; `GET …/{id}/input_items` | API key only. `store` defaults to true (kept 3 days; up to 7 via `expire_at`). Built-in tools and explicit cache live here. Not available with TPM packages, fine-tuned models, model routing or version switching [A-resp][A-mig]. QPS: create unlimited; get/list/delete 20 [A-rthink] |
| Messages API (Anthropic-compatible) | `POST /api/compatible/v1/messages`, `…/messages/count_tokens` | Stateless. `thinking.type` `disabled`/`enabled`/`adaptive`; `output_config.effort`; server tool `web_search_20250305` [A-msg] |
| Files API | `POST`/`GET`/`DELETE /api/v3/files` | ≤512 MB per file (video ≤2 GB via your own TOS bucket), 20 GB quota, TTL 7 days by default (1–30) [A-files] |
| Models / Tokenization | `GET /api/v3/models`, `POST /api/v3/tokenization` | Lists visible model versions with features, limits and supported APIs [A-modelsapi] |
| Images | `POST /api/v3/images/generations` | Seedream [A-imggen] |
| Batch | Batch Job: TOS JSONL, created via control-plane `CreateBatchInferenceJob`. Batch Chat: `ep-bi-*` endpoints | Batch Chat is **marked for deprecation and not supported by Seed 2.1+**; use `service_tier:"flex"` instead [A-batch][A-flex] |
| Bot API (legacy) | `POST /api/v3/bots/chat/completions` | Works only for already-published zero-code apps; superseded by Managed Agents [A-bot]. Models tagged "支持 Bot API" reached EOS 2026-09-21 [A-dep] |
| Context API (legacy cache) | `context.create` / `context.completions` (SDK) | Superseded by Responses `caching` [A-mig] |
| Managed Agents | `POST /api/v3/agents`, `/api/v3/sessions`, `/api/v3/sessions/{id}/events[/stream]` … | Server-side agent harness (sandbox, memory, skills) [A-agentapi][A-sess] |
| Control plane | `https://ark.cn-beijing.volcengineapi.com/?Action=…&Version=2024-01-01` | AK/SK HMAC-SHA256 signing, service `ark`, region `cn-beijing`. Includes `GetApiKey` (temporary keys) [A-auth][A-tmpkey] |

- **Auth**: `Authorization: Bearer $ARK_API_KEY`. Coding Plan and Agent Plan keys use a different base URL [A-auth].
- **Model ID vs Endpoint ID — you can call by model name now.** A Model ID call goes to an auto-created *preset endpoint*, which supports the standard, low-latency (`fast`) and flex tiers.
  - Custom endpoints `ep-<timestamp>-<suffix>` are required for TPM packages, model units, intelligent routing, fine-tuned models, smooth version switching and batch (`ep-bi-*`) [A-deploy][A-std].
  - Some accounts get 404 `InvalidEndpointOrModel.ModelIDAccessDisabled` and must use endpoint ids [A-err].
  - The response `model` field is "the model name and version actually used" [A-chat]. Log it for `doubao-seed-evolving`.

## 3. Request parameters

Chat API field names are shown first; Responses and Messages equivalents are in brackets.

| Field | Values / default | Seed-specific notes |
|---|---|---|
| `thinking.type` | `enabled` (default on all Seed 2.x) or `disabled`. **No `auto`** is documented for Chat/Responses (Seed 1.6's mode is gone with 1.6) | Messages also accepts `adaptive`; per-model support is undocumented [A-think][A-msg] |
| `reasoning_effort` (Resp `reasoning.effort`, Msg `output_config.effort`) | `none, minimal, low, medium, high, xhigh, max`. All are accepted and then mapped | **2.1 and Evolving**: default `high`; `minimal` = off; `none`→`minimal`; `xhigh`/`max`→`high`. **2.0 and character-260628**: default `medium`, same mapping [A-think] |
| ↳ combined with `thinking.type=disabled` | only `minimal` is valid | any other value → error [A-ctx]. Effort governs the raw CoT, not the summary [A-think] |
| `max_tokens` | default **4096**; counts the **answer only**, not CoT | Truncates silently at 4k. Always send a limit; agent guide says ≥128000 [A-ctx][A-agent] |
| `max_completion_tokens` | answer + CoT; overrides the 4096 default; documented range `[1, 65536]` | That range conflicts with the 128k/256k "max output" in [A-models] (**UNVERIFIED** true cap). Setting both fields → error [A-chat][A-ctx] |
| Resp `max_output_tokens` | answer + CoT | Examples echo 32768 when unset (**UNVERIFIED** default). Responses has no `max_tokens` [A-ctx][A-think] |
| `temperature` | `[0,2]`, default 1.0 | Fixed at 1.0 on `2-0-pro/lite-260215` (and on `code-preview-260215` via Responses) [A-chat][A-resp] |
| `top_p` | `[0,1]`, default 0.7 | Fixed at 0.95 on the same models. Agent advice: `temperature=1, top_p=0.95` [A-agent] |
| `frequency_penalty`, `presence_penalty` | `[-2,2]` | **Not supported on Seed 2.0+** [A-chat][B-chat] |
| `stop` | ≤4 strings | Not supported by "deep-thinking" models, which is every Seed 2.x [A-chat] |
| `logprobs`, `top_logprobs` (0–20), `logit_bias` | n/a | Not supported by deep-thinking models [A-chat] |
| `n`, `seed`, `user` | n/a | Absent from the Chat reference [A-chat] |
| `response_format` (Resp `text.format`) | `text` / `json_object` / `json_schema{name, schema, description, strict}` (beta) | Not available with TPM packages [A-so] |
| `tools`, `tool_choice`, `parallel_tool_calls` | `tool_choice`: `none`, `auto`, `required` or `{type:"function", function:{name}}`. `parallel_tool_calls` default true. `tools[].function.strict` | Responses `tool_choice.type` can also be `web_search`, `image_process`, `mcp`, `knowledge_search` or `doubao_app` [A-chat][A-resp] |
| `stream`, `stream_options` | `include_usage`; `chunk_include_usage` (cumulative usage on every chunk) | See §7 |
| `service_tier` | `default`; `fast` (low-latency quota, else default); `auto` (TPM package, else default); `flex` (≈50 % price, best effort; Responses then needs `store:false`) | Flex can be capped with header `X-Ark-Max-Wait-Timeout-Ms` (≤20 min). The response echoes `scale`/`default`/`fast`/`flex` [A-chat][A-flex][A-upd] |
| `prompt_cache_key` | routing hint for implicit cache | Keep each key under 15 req/min. Chat and Responses only [A-chat] |
| `web_search_options` | `{}` turns on Doubao Search (Chat) | Ignored when `tools` is present. Its sub-fields have no effect [A-chat] |
| Responses-only fields | `previous_response_id`, `store`, `expire_at`, `instructions`, `caching{type, prefix}`, `context_management.edits[]` (`clear_tool_uses`, `clear_thinking`), `max_tool_calls` (1–10), `include`, `metadata` (16 kv), header `X-Fornax-Trace` | `reasoning.summary` and `text.verbosity` are accepted but **ignored** (since 2026-09-09) [A-resp][A-compat] |
| `role: "developer"` | accepted since 2026-09-09 | [A-compat] |
| Prefill | a final `role:"assistant"` message continues that text | Supported on Seed 2.x [A-prefill] |

Example Chat request for a Seed 2.1 model:
```json
{"model":"doubao-seed-2-1-pro-260915","stream":true,"stream_options":{"include_usage":true},
 "thinking":{"type":"enabled"},"reasoning_effort":"high","max_completion_tokens":65536,
 "messages":[{"role":"user","content":"…"}]}
```

## 4. Multi-turn rules

**What Ark feeds back to the model:**
- **Plain multi-turn**: earlier CoT is **not** concatenated into the context [A-think].
- **Tool loops** (since Seed 1.8): CoT from the current tool loop is fed back. Ark decides itself when to drop older CoT, and CoT it does not feed is not billed. Send the full history and let the server prune [A-think][A-ctx].

**Two reasoning classes** [A-agent][A-think][A-chat]:

| Class | Models | What comes back | What to echo |
|---|---|---|---|
| Summary + encrypted original | Ark: `evolving`, `2-1-pro-260915/260628`, `2-1-lite-260915`, `2-1-turbo-260628`, `2-0-lite-260428`. BytePlus: `dola-seed-2-1-turbo-260628`, `seed-2-0-lite-260428` | `reasoning_content` = **summary**; `encrypted_content` = encrypted raw CoT; `reasoning_tokens` = raw-CoT tokens (that is what you are billed for) | **Chat**: the whole assistant message, including the flat `encrypted_content` next to `tool_calls`. **Responses**: either `previous_response_id` with `store:true`, or the `reasoning` item replayed with its **original `id`** and non-empty `encrypted_content`; replay all items after the last user message, in order. **Messages**: the `thinking` block (`thinking` + `signature`) unchanged and before `tool_use` |
| Raw CoT | `2-0-*-260215`, `2-0-code-preview-260215`, `2-0-mini-260428` [A-think] | `reasoning_content` = raw CoT | `reasoning_content` on the assistant message (Responses: `summary[].text`, or `previous_response_id`) |

Docs conflict on `2-0-lite-260428`: [A-think] also lists it under raw pass-back, while [A-agent] and [A-chat] say encrypted from 260428 on.

**Echo rules** [A-agent]:
- `encrypted_content` takes priority; when it is present, `reasoning_content` is ignored.
- Echoing only the summary, or nothing, gives **no error but degraded quality**.
- Upgrading 260215→260428 silently switches the class, so code must handle both.

**Errors and normalisation:**
- Tampered or truncated `encrypted_content` → `Invalid signature` [A-chat][A-agent]. The 2026-09-09 compat notice says unparseable `encrypted_content`/`signature` **no longer error** [A-compat]. Which rule applies is **UNVERIFIED**.
- Setting both `max_tokens` and `max_completion_tokens` → error.
- `thinking.type=disabled` with any effort other than `minimal` → error.
- Responses `caching` together with `instructions` → 400 `…caching is not supported for instructions`.
- `caching` together with built-in tools or MCP → 400 [A-ctx][A-err][A-ws][A-mcp].
- Since 2026-09-09 these are normalised instead of rejected: null `content`, null `tool_call_id`, and malformed historical `arguments` [A-compat].

**Session stability:**
- Changing `thinking`, sampling parameters, the system prompt or tool definitions mid-session resets the prefix cache [A-agent][A-cache].
- Responses chains hold at most 1000 items. Leave about 100 ms between chained calls [A-rtg][A-resp].

Chat replay, turn 2 of a tool loop:
```json
[{"role":"assistant","content":"","reasoning_content":"<summary>","encrypted_content":"djF+2EIC…",
  "tool_calls":[{"id":"call_wiez…","type":"function","function":{"name":"get_weather","arguments":"{\"location\":\"北京\"}"}}]},
 {"role":"tool","tool_call_id":"call_wiez…","content":"5度"}]
```

## 5. Built-in tools (Ark only; BytePlus has none of these)

BytePlus Responses supports only function and MCP tools. Its Chat API has no `web_search_options`, and its Messages API has no server tools [B-rtools][B-chat][B-msg].

| Tool | API and config | Billing |
|---|---|---|
| **Doubao Search Custom** (recommended) | **Chat**: `web_search_options:{}`.<br>**Responses**: `{"type":"web_search","sources":["doubao"],"filters":{"allowed_domains":[≤20],"blocked_domains":[≤5],"auth_info_level":0\|1},"time_range":"OneDay\|OneWeek\|OneMonth\|OneYear\|YYYY-MM-DD..YYYY-MM-DD"}`. `limit`, `max_keyword` and `user_location` are ignored; `doubao` cannot be combined with other sources (400).<br>**Messages**: `{"type":"web_search_20250305","name":"web_search","max_uses":…,"allowed_domains":…,"blocked_domains":…}` | ¥0.020 per search, or monthly packs (¥5.9 per 1000 calls …). Must be activated in the console, else 403 `AccessDenied`. Exhausted free quota → 403 `QuotaExceeded.DoubaoSearchFreeQuotaExceeded` [A-ws][A-dsprice][A-err] |
| Web content plugin (联网内容插件) | Responses `web_search` with `sources` ⊂ `search_engine` (default), `toutiao`, `douyin`, `moji`. `limit` 1–50 (default 10), `max_keyword` 1–50, `user_location`, and `max_tool_calls` (default 3) | Web: 20k free per month, then ¥4 per 1000. Toutiao, Douyin and Moji: ¥6 per 1000, counted per source per call [A-wsprice] |
| `image_process` | Responses. Toggles `point`, `grounding`, `zoom` (0.5–2×) and `rotate` (0–359°). Header `ark-beta-image-process: true` | Free during beta [A-imgproc] |
| `doubao_app` (豆包助手) | Responses. `feature.{chat, deep_chat, ai_search, reasoning_search}`, one at a time. Header `ark-beta-doubao-app: true`. **Injects a default persona `role_description` ("你的名字是豆包,有很强的专业性。") that overrides system/instructions** | ¥0.1 / 0.2 / 0.2 / 0.5 per call; the underlying model is not billed separately [A-dbapp][A-dbappprice][A-resp] |
| `knowledge_search` | Responses, against a Viking knowledge base. Header `ark-beta-knowledge-search: true` | Knowledge-base billing [A-tools] |
| `mcp` (remote, Streamable HTTP) | Responses. `server_label`, `server_url`, `headers`, `allowed_tools`, `require_approval`. Header `ark-beta-mcp: true`. 1000 RPM | Model tokens only [A-mcp] |
| `apply_patch` | Responses | [A-patch] |
| Image generation | **Not a chat tool.** Separate `POST /api/v3/images/generations`. Seedream 5.0 pro/flash: `size` 1K/1.5K/2K or WxH, `layer_decomposition`, `background:"transparent"`, `output_format`, `watermark` (default true), `optimize_prompt_options`. 5.0 lite and 4.x: `sequential_image_generation`, `stream`; 5.0 lite only: `tools:[{"type":"web_search"}]` | 5.0 pro ¥0.30 per image (≤2.61 MP); flash ¥0.12 [A-imggen][A-price] |

**Citation formats:**
- **Chat**: `choices[].message.annotations[] = {"type":"url_citation","url_citation":{"title","url"}}`, deduplicated, with **no `start_index`/`end_index`**. Search count in `usage.tool_usage.web_search` [A-chat][A-ws].
- **Responses**: `output` items run `reasoning` → `web_search_call` → `message`.
  - `web_search_call.status` is `in_progress|searching|completed|incomplete|failed`; `action = {"type":"search","query","queries":[]}`.
  - The message's `output_text.annotations[]` items are `{"type":"url_citation","title","url","site_name","publish_time","summary","cover_image","logo_url","mobile_url","freshness_info"}`, with no offsets.
  - Usage: `usage.tool_usage.web_search` and `usage.tool_usage_details.web_search.doubao` [A-resp][A-ws].
- **Messages**: `server_tool_use` + `web_search_tool_result` blocks carrying `web_search_result{title, url, page_age, encrypted_content}`. Usage: `usage.server_tool_use.web_search_requests` [A-ws][A-msg].

**Stateless replay of tool turns:** Responses `input` accepts `message` (including `output_text` with annotations), `reasoning`, `function_call`, `function_call_output`, `mcp_*` and `item_reference`. **There is no `web_search_call` input type** [A-resp], so drop those items when replaying. Whether Ark would reject them is **UNVERIFIED**.

## 6. Multimodal input (Ark, Seed 2.0 and later)

| Modality | Chat part (Responses part) | Size / format limits | Knobs |
|---|---|---|---|
| Image | `{"type":"image_url","image_url":{"url"\|"file_id","detail","image_pixel_limit":{"min_pixels","max_pixels"}}}` (`input_image`) | URL or base64 ≤10 MB each; request body ≤64 MB; Files API ≤512 MB. w,h > 14 px; area 196–36 M px; aspect 1/150–150. Formats: jpeg png gif webp bmp tiff ico dib icns sgi jpeg2000 heic heif | `detail`: `low` = 1–1280 tokens; `high` (default) = 1280 tokens, ≤2,257,920 px; `xhigh` = 1280–5120 tokens, ≤9,031,680 px. Tokens ≈ w·h/1764. `image_pixel_limit` overrides `detail` (max 9,031,680, min 1764) [A-img][A-chat][A-evolving] |
| Video | `{"type":"video_url","video_url":{"url"\|"file_id","fps"}}` (`input_video`) | URL or base64 ≤50 MB; Files API ≤512 MB (≤2 GB via your own TOS bucket). mp4, avi, mov | `fps` 0.2–5 (default 1). ≤80k tokens per video (`max_video_tokens` 81920). Frames are compressed to 64–384 tokens each; over 1280 frames → 1280 frames × 64 tokens. Each frame is prefixed with `"<t> second"` [A-vid] |
| PDF | `{"type":"file","file":{"file_data"(+`filename`)\|"file_id"\|"file_url"}}` (`input_file`) | ≤50 MB via URL or base64; Files API ≤512 MB | Each page is turned into an image and run through vision [A-chat][A-doc] |
| Audio | `{"type":"input_audio","input_audio":{"data"+"format"\|"file_id"\|"url"}}` (`input_audio` with `audio_url`, `chunking_strategy` server_vad) | ≤25 MB and ≤120 min per request via base64/URL; Files API ≤512 MB. mp3 wav aac m4a (plus ac3/alac/pcm for audio inside video) | ≈6.25 tokens/s. Supported only by 2.1-lite and 2.0-lite/mini-260428, not Evolving. Audio inside a video needs a Files API upload with `preprocess_configs.video.model` [A-aud][A-models][A-chat] |

- Media parts may also appear in `system` messages [A-chat][A-img].
- Vision requests reject `frequency_penalty`, `presence_penalty` and `n` [A-img].
- Ark deletes media after processing [A-img].

## 7. Streaming, usage, finish reasons, errors

**Chat SSE** [A-chat][A-stream][A-agent]:
- Shape: a series of `data: {chat.completion.chunk}` lines ending with `data: [DONE]`.
- Deltas carry `content`, `reasoning_content` (a summary on 260428+) and `tool_calls[].index`.
- One **dedicated chunk** carries the whole `encrypted_content`, after the reasoning and before the content. Store it as a whole value; do not concatenate it.

**Chat usage** [A-chat]:
- A final `choices:[]` chunk appears only with `stream_options.include_usage:true`. Exceptions such as 2.1-lite send it by default.
- `chunk_include_usage` puts a cumulative snapshot on every chunk.

**Timeouts:** summary-mode models can pause for a long time between chunks. Ark recommends client timeouts of at least 1800 s [A-think].

**Responses SSE events** [A-life][A-resp]. Every event carries a `sequence_number`. `encrypted_content` arrives in `output_item.added`/`done` and in `response.completed`.

| Group | Events |
|---|---|
| Lifecycle | `response.created`, `in_progress`, `completed`, `incomplete`, `failed` |
| Items and parts | `response.output_item.added`/`done`, `response.content_part.added`/`done` |
| Text | `response.output_text.delta`/`done` |
| Reasoning | `response.reasoning_summary_part.added`/`done`, `response.reasoning_summary_text.delta`/`done`, `response.reasoning_text.delta`/`done` |
| Function calls | `response.function_call_arguments.delta`/`done` |
| Built-in tools | `response.web_search_call.in_progress`/`searching`/`completed`, `response.image_process_call.*`, `response.mcp_*`, `response.knowledge_search_call.*`, `response.doubao_app_call*`, `response.transcription_*` |
| Errors | `error{code, message, param}` |

**Usage fields** [A-chat][A-resp][A-msg]:

| API | Fields |
|---|---|
| Chat | `usage.{prompt_tokens, completion_tokens, total_tokens, prompt_tokens_details.{cached_tokens, audio_tokens, audio_cached_tokens}, completion_tokens_details.reasoning_tokens, tool_usage.web_search}` |
| Responses | `usage.{input_tokens, input_tokens_details.cached_tokens, output_tokens, output_tokens_details.reasoning_tokens, tool_usage.{web_search, mcp, knowledge_search, doubao_app}, tool_usage_details}` |
| Messages | `usage.{input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens (always 0), server_tool_use.{web_search_requests, web_fetch_requests}}` |

Extra response metadata [A-chat]:
- `service_tier`: the tier actually used.
- `service_status.model_fallback.{fallback_triggered, original_model}`: the docs never say when fallback happens.
- `choices[].moderation_hit_type`: vision models only, and only with the endpoint guardrail set to Basic.

**Termination** [A-chat][A-stream][A-respget]:
- **Chat** `finish_reason`: `stop`, `length` (any of the `max_tokens`, `max_completion_tokens` or context-window limits), `content_filter`, `tool_calls`.
- **Responses** `status`: `completed`, `incomplete`, `failed`, `in_progress`; details in `incomplete_details.{reason, content_filter}`.
- **Messages** `stop_reason`: `end_turn`, `stop_sequence`, `max_tokens`, `tool_use`, followed by `message_stop`.
- A bare `[DONE]` does not prove success.

**Errors** come as `{"error":{"code","message","param","type"}}` [A-err]:

| HTTP | Codes | Retry? |
|---|---|---|
| 400 | `MissingParameter`, `InvalidParameter`, `InvalidParameter.UnsupportedParameter` (e.g. flex on an unsupported model), `InvalidArgumentError.*` (image detail/pixels; that error text still says only "auto/high/low" are valid), `InvalidImageURL.*`, `OutofContextError` | No; fix the request |
| 400 | `SensitiveContentDetected[.SevereViolation\|.Violence]`, `{Input,Output}{Text,Image,Video,Audio}SensitiveContentDetected[.PolicyViolation…]`, `*RiskDetection` | No. Do not auto-retry around a content block [A-stream] |
| 400 | `InvalidEndpoint.ClosedEndpoint` | Later, or ask the endpoint admin |
| 401 / 403 | `AuthenticationError`, `InvalidAccountStatus`, `AccountOverdueError`, `AccessDenied` (also web search not activated), `OperationDenied.*`, `QuotaExceeded.DoubaoSearchFreeQuotaExceeded` | No |
| 404 | `InvalidEndpointOrModel.NotFound`, `ModelNotOpen` (model not activated), `InvalidEndpointOrModel.ModelIDAccessDisabled` | No |
| 408 | `RequestTimeout` | Yes, a bounded number of times |
| 429 | `RateLimitExceeded.Endpoint{RPM,TPM,FlexTPM}Exceeded`, `ModelAccount{Rpm,Tpm,FlexTpm,Ipm,Tpd}RateLimitExceeded`, `ModelAccountConcurrentRequestsLimitExceeded`, `AccountRateLimitExceeded`, `ServerOverloaded` (Seed ≤1.8), `RequestBurstTooFast` (Seed 2.0+; ramp traffic up gradually) | Yes, with backoff |
| 429 | `QuotaExceeded` (free trial or plan), `SetLimitExceeded` (your usage cap), `AccountQuotaExceeded` | No |
| 499 | `RequestCanceled` | Client-side abort |
| 500 / 503 | `InternalServiceError`, `ModelLoading`, `ServiceConnectionRefused`, `NoAvailableService` | Yes, after a delay |

TPM is **pre-deducted** from input plus the requested max output, so a huge `max_completion_tokens` can trigger 429 sooner. Requests rejected by RPM/TPM limits never start generating and are not billed [A-faq].

## 8. Async / resume after disconnect

- **No stream resume on Chat, Responses or Messages.** The streaming guide says it "does not define resumption" (本文不定义断点续传) [A-stream].
  - When the client disconnects, the server **stops generation**. Tokens generated up to that point are billed.
  - Non-streaming requests that time out return nothing, yet are still billed [A-faq].
- **Stored Responses results:**
  - `GET /api/v3/responses/{id}` returns the object only once it has finished. While it is still generating, it **returns an error** [A-respget].
  - Only `completed` and `incomplete` responses are stored. A response cut off inside the CoT is stored with empty `output`. Storage lasts 3 days by default (≤7) and never includes the CoT [A-rtg].
  - The reference has no `background` parameter [A-resp].
  - So an aborted call leaves nothing to fetch (inferred from [A-faq]; **UNVERIFIED** for `store:true`).
- **Asynchronous alternatives:**
  - Batch Job: TOS JSONL, 50 % price, a tokens-per-day quota, at most 20 running jobs [A-batch].
  - `service_tier:"flex"`: still a synchronous call, but queued server-side for up to 20 min [A-flex][A-upd].
  - Managed Agents sessions keep their events server-side. After a drop, reopen `GET /api/v3/sessions/{id}/events/stream` and back-fill with the paged `GET /api/v3/sessions/{id}/events`; there is no SSE resume [A-sess]. This is a harness, not a raw model call.
- **Via OR:** stateless (`store:true` or `previous_response_id` → 400) [OR-rsp]. "Seed" appears in neither OR list of providers whose streams can or cannot be cancelled [OR-stream], so what is billed after a disconnect is **UNVERIFIED**.

## 9. True OpenRouter gaps for Doubao/Seed

### 9.1 Confirmed: unreachable through OR however the request is configured

| # | What only the official API can do | Evidence |
|---|---|---|
| G1 | Run the China-only models: `doubao-seed-2-1-pro-*`, `doubao-seed-2-1-lite-260915` (1M context, audio), `doubao-seed-evolving`, `doubao-seed-character-*`, `doubao-seed-translation-250915`. Also BytePlus `seed-2-0-pro-260328` | Absent from [OR-models]. OR's only Seed provider is BytePlus [OR-prov], which does not list them [B-models] |
| G2 | Pin an exact version or LTS build (`…-260428` vs `…-260228`), or pick the raw-CoT `doubao-seed-2-0-*-260215` builds | OR slugs are unversioned (canonical slugs carry OR listing dates) and carry OR `expiration_date`s (`seed-1.6`, `seed-1.6-flash` and `seed-2.0-code` expire 2026-11-11) [OR-models] |
| G3 | Use official built-in tools: Doubao Search (Chat, Responses, Messages), the web content plugin (Toutiao/Douyin/Moji), `image_process`, `doubao_app`, `knowledge_search`, server-side remote MCP, `apply_patch` | `native_tools: {}` on every Seed endpoint [OR-ep]. Seed is not among OR's native-search providers, so `openrouter:web_search` runs Exa instead [OR-ws] |
| G4 | Send audio input (2.0-lite/mini-260428, 2.1-lite), including the audio track inside a video | OR input modalities are text, image and video only [OR-models]. Ark and BytePlus document audio [A-models][B-models] |
| G5 | Keep state on the server: `store`, `previous_response_id`, `GET`/`DELETE /responses/{id}`, `input_items`, `expire_at` | OR Responses rejects `store:true` and `previous_response_id` with 400 [OR-rsp] |
| G6 | Use the explicit context cache (`caching`: prefix cache and session cache) | It requires `previous_response_id` and `store` [A-cache], which G5 rules out |
| G7 | Choose a service tier: `flex` (≈50 % price), `fast` (low latency) or `auto` (TPM package) | OR offers tiers as separate endpoints; Seed has a single endpoint with `service_tier:null` [OR-tiers][OR-ep] |
| G8 | Reference an Ark `file_id`, or send media above the inline/URL caps (image >10 MB, video or PDF >50 MB, up to 512 MB or 2 GB via the Files API) | OR's Files API can store only on OR, `openai` or `anthropic` (`FileProvider` enum) [OR-oas] |
| G9 | Set `parallel_tool_calls:false` | Not in the Seed endpoints' `supported_parameters`. OR ignores unsupported parameters, or fails the request under `require_parameters` [OR-route] |
| G10 | Use Seedream extras: `layer_decomposition`, transparent `background`, `watermark:false`, the 1.5K tier, `sequential_image_generation`, `stream`, the `web_search` tool, `optimize_prompt_options` | OR's Seedream endpoints accept only `resolution` (1K/2K), `aspect_ratio`, `n=1`, `input_references` and `seed`, with `allowed_passthrough_parameters: []` and no streaming [OR-img] |
| G11 | Use Messages `thinking.type:"adaptive"` on Seed | OR rewrites `adaptive` to `reasoning.enabled:true` (the default effort) for non-Claude models [OR-reason]. Per-model support on Ark is undocumented |

### 9.2 Probable gaps (UNVERIFIED; test with `debug.echo_upstream_body` and by reading the response)

| # | Suspected gap | Why | How to test |
|---|---|---|---|
| P1 | `encrypted_content` round trip on 2.1 Turbo and 2.0-lite-260428 | OR's `reasoning_details` `format` enum has no ByteDance value [OR-reason]. Without the round trip, Ark warns that tool-loop quality drops [A-agent] | Run a tool turn. Look for `reasoning.encrypted` in `reasoning_details`, then check that the next upstream body carries `encrypted_content` |
| P2 | Choosing effort on 2.1 Turbo | No `supported_efforts` and no `reasoning_effort` parameter in OR's metadata [OR-models] | Send `reasoning.effort:"low"` and inspect the upstream `reasoning_effort` |
| P3 | Choosing `max_tokens` (answer only, 4096 default) vs `max_completion_tokens` (answer + CoT) | OR has a single budget [OR-reason] and its mapping is undocumented. Omitting it may fall back to Ark's 4096 default | Compare upstream bodies with and without `max_tokens` |
| P4 | Forced tool use (`tool_choice:"required"` or a named function) | OR marks Seed 2.x `required:false, function:false` [OR-ep]; Ark and BytePlus document both [A-chat][B-chat] | Send `required` and inspect the upstream body |
| P5 | Image `detail:"xhigh"` and `image_pixel_limit` | OR's `detail` enum is `auto`, `low`, `high`, `original` (`original` is downgraded to `high`), and there is no pixel-limit field [OR-oas] | Send `xhigh` and inspect the upstream body |
| P6 | The implicit-cache discount | Seed endpoints show `supports_implicit_caching:false` and no cache-read price [OR-ep], while BytePlus charges $0.1 / $0.05 / $0.02 for cached input [B-price] | Repeat a request with a 2k+ token prefix and compare `cached_tokens` and cost |
| P7 | Output above 235,929 tokens on 2.1 Turbo (official 256K), or above 32,768 on 1.6 (official 64K) | [OR-ep] vs [B-models] | Request a large `max_tokens` |
| P8 | Response metadata: `service_status.model_fallback`, `moderation_hit_type`, the exact `model` version | OR normalises responses | Diff OR responses against direct ones |
| P9 | Assistant prefill (continuation) | Ark supports it [A-prefill]; OR does not document it for Seed | Send a final assistant message |

### 9.3 Considered and excluded

- **Request formats that can be configured away:** `instructions`, `context_management` (prune thinking or tool uses client-side), `prompt_cache_key`, CC vs Responses vs Messages field names.
- **Inputs a client can replicate:**
  - PDF: rasterise the pages client-side. Ark itself feeds PDFs as page images [A-doc].
  - Video `fps`: send extracted frames, each preceded by `"<t> second"` text, the way Ark does [A-vid].
- **Not available upstream either:**
  - `frequency_penalty`, `presence_penalty`, `stop`, `logprobs`, `logit_bias`, `seed`, `n`: Seed 2.x does not support them on Ark or BytePlus [A-chat][B-chat]. OR lists `frequency_penalty` and `stop` as supported, but per those docs they have no upstream effect.
  - Raw CoT on 2.1 and Evolving: Ark returns only summaries too [A-think].
  - `xhigh`/`max` effort: mapped to `high` upstream anyway [A-think].
- **fp8:** OR's Seed provider is BytePlus itself [OR-prov], and the official precision is undocumented, so there is nothing demonstrable.

## S. Sources

The lines below are Markdown link-reference definitions. In rendered Markdown, every inline `[key]` above links to these URLs; view the raw file to read them as a list.

[A-llms]: https://docs.volcengine.com/docs/zh/ark/llms.txt
[A-models]: https://docs.volcengine.com/docs/zh/ark/model-list.md
[A-evolving]: https://docs.volcengine.com/docs/zh/ark/latest-model.md
[A-price]: https://docs.volcengine.com/docs/zh/ark/model-pricing.md
[A-text]: https://docs.volcengine.com/docs/zh/ark/text-generation.md
[A-auth]: https://docs.volcengine.com/docs/zh/ark/base-url-and-authentication.md
[A-chat]: https://docs.volcengine.com/docs/zh/ark/chat-api.md
[A-resp]: https://docs.volcengine.com/docs/zh/ark/create-model-responses-api.md
[A-respget]: https://docs.volcengine.com/docs/zh/ark/list-model-responses-api.md
[A-life]: https://docs.volcengine.com/docs/zh/ark/response-lifecycle.md
[A-msg]: https://docs.volcengine.com/docs/zh/ark/messages-api.md
[A-think]: https://docs.volcengine.com/docs/zh/ark/deep-thinking.md
[A-rthink]: https://docs.volcengine.com/docs/zh/ark/responses-api-deep-thinking.md
[A-ctx]: https://docs.volcengine.com/docs/zh/ark/context-management.md
[A-stream]: https://docs.volcengine.com/docs/zh/ark/streaming-output.md
[A-prefill]: https://docs.volcengine.com/docs/zh/ark/continuation-mode.md
[A-cache]: https://docs.volcengine.com/docs/zh/ark/context-cache.md
[A-files]: https://docs.volcengine.com/docs/zh/ark/file-api.md
[A-so]: https://docs.volcengine.com/docs/zh/ark/structured-output-beta.md
[A-rtg]: https://docs.volcengine.com/docs/zh/ark/responses-api-text-generation.md
[A-mig]: https://docs.volcengine.com/docs/zh/ark/responses-api-migration.md
[A-tools]: https://docs.volcengine.com/docs/zh/ark/tools-overview.md
[A-ws]: https://docs.volcengine.com/docs/zh/ark/web-search.md
[A-wsprice]: https://docs.volcengine.com/docs/zh/ark/online-content-plugin-pricing.md
[A-dsprice]: https://docs.volcengine.com/docs/zh/networked-search/online-search-product-billing.md
[A-imgproc]: https://docs.volcengine.com/docs/zh/ark/image-process.md
[A-dbapp]: https://docs.volcengine.com/docs/zh/ark/doubao-assistant.md
[A-dbappprice]: https://docs.volcengine.com/docs/zh/ark/doubao-assistant-pricing.md
[A-mcp]: https://docs.volcengine.com/docs/zh/ark/mcp-remote-mcp.md
[A-patch]: https://docs.volcengine.com/docs/zh/ark/apply-patch.md
[A-img]: https://docs.volcengine.com/docs/zh/ark/image-understanding.md
[A-vid]: https://docs.volcengine.com/docs/zh/ark/video-understanding.md
[A-doc]: https://docs.volcengine.com/docs/zh/ark/document-understanding.md
[A-aud]: https://docs.volcengine.com/docs/zh/ark/audio-understanding.md
[A-agent]: https://docs.volcengine.com/docs/zh/ark/agent-model-invocation-practice.md
[A-err]: https://docs.volcengine.com/docs/zh/ark/error-codes.md
[A-batch]: https://docs.volcengine.com/docs/zh/ark/batch-inference.md
[A-flex]: https://docs.volcengine.com/docs/zh/ark/online-inference-flex.md
[A-deploy]: https://docs.volcengine.com/docs/zh/ark/deployment-overview.md
[A-std]: https://docs.volcengine.com/docs/zh/ark/online-inference-standard.md
[A-dep]: https://docs.volcengine.com/docs/zh/ark/model-deprecation-notice.md
[A-upd]: https://docs.volcengine.com/docs/zh/ark/product-update-announcements.md
[A-compat]: https://docs.volcengine.com/docs/zh/ark/api-schema-upgrade-notice.md
[A-faq]: https://docs.volcengine.com/docs/zh/ark/modelark-faq.md
[A-bot]: https://docs.volcengine.com/docs/zh/ark/migrate-from-bot-api.md
[A-sess]: https://docs.volcengine.com/docs/zh/ark/session-event-stream.md
[A-agentapi]: https://docs.volcengine.com/docs/zh/ark/create-agent-api.md
[A-tmpkey]: https://docs.volcengine.com/docs/zh/ark/get-api-key-api.md
[A-imggen]: https://docs.volcengine.com/docs/zh/ark/image-generation-api.md
[A-modelsapi]: https://docs.volcengine.com/docs/zh/ark/models-api.md
[B-llms]: https://docs.byteplus.com/docs/en/modelark/llms.txt
[B-models]: https://docs.byteplus.com/docs/en/modelark/model-list.md
[B-price]: https://docs.byteplus.com/docs/en/modelark/model-pricing.md
[B-region]: https://docs.byteplus.com/docs/en/modelark/region-availability.md
[B-chat]: https://docs.byteplus.com/docs/en/modelark/chat-api.md
[B-think]: https://docs.byteplus.com/docs/en/modelark/deep-thinking.md
[B-rtools]: https://docs.byteplus.com/docs/en/modelark/responses-api-tool-calling.md
[B-msg]: https://docs.byteplus.com/docs/en/modelark/messages-api.md
[OR-models]: https://openrouter.ai/api/v1/models
[OR-ep]: https://openrouter.ai/api/v1/models/bytedance-seed/seed-2-1-turbo/endpoints
[OR-prov]: https://openrouter.ai/api/v1/providers
[OR-img]: https://openrouter.ai/api/v1/images/models/bytedance-seed/seedream-5-0-pro/endpoints
[OR-vid]: https://openrouter.ai/api/v1/models?output_modalities=video
[OR-reason]: https://openrouter.ai/docs/guides/best-practices/reasoning-tokens.md
[OR-stream]: https://openrouter.ai/docs/api_reference/streaming.md
[OR-ws]: https://openrouter.ai/docs/guides/features/server-tools/web-search.md
[OR-rsp]: https://openrouter.ai/docs/api_reference/responses/overview.md
[OR-route]: https://openrouter.ai/docs/guides/routing/provider-selection.md
[OR-tiers]: https://openrouter.ai/docs/guides/features/service-tiers.md
[OR-oas]: https://openrouter.ai/openapi.json
[OR-debug]: https://openrouter.ai/docs/api_reference/errors-and-debugging.md

`[OR-ep]` cites the 2.1 Turbo endpoint list. The sibling lists `…/seed-2.0-lite|seed-2.0-mini|seed-2.0-code|seed-1.6|seed-1.6-flash/endpoints` show the same fields. `[OR-img]` stands for `/api/v1/models?output_modalities=image` and the matching `/api/v1/images/models/bytedance-seed/seedream-5-0-flash/endpoints` as well.
