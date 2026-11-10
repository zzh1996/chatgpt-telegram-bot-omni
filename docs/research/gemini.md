# Gemini API (Gemini Developer API / AI Studio) and how OpenRouter maps it

Research snapshot: **2026-11-11**. Every claim links to a page fetched that day (link list in [Sources](#sources)). Anything not backed by a fetched page is marked **UNVERIFIED**. Abbreviations: **GC** = `generateContent` / `streamGenerateContent`; **IA** = Interactions API; **OR** = OpenRouter.

> **Doc landscape.** `ai.google.dev/gemini-api/docs/*` now 301-redirects to `aistudio.google.com/docs/*`. Every page has a Markdown twin (append `.md`); pages come in two flavours: default = IA, `?generate-content=true` = GC. The full corpus is `https://aistudio.google.com/llms-full.txt`. Google labels GC "legacy … still supported" and tells integrators to use IA "as the default across all workflows" ([llms.txt][g-llms], [IA overview][g-ia]).

---

## 0. TL;DR for the Rust rewrite

1. **Model line-up moved on**: `gemini-4-carbon` (GA 2026-11-10), `gemini-4-argon` (GA 2026-10-13), `gemini-3.8-flash` (default Flash). `gemini-2.5-pro`/`-flash` auto-route to 3.8 Flash / 3.5 Flash-Lite on 2026-12-01; 3.7/3.5 Flash already auto-route ([changelog][g-changelog], [deprecations][g-deprec]).
2. **Sampling params are dead on new models**: `temperature`/`topP`/`topK` are "silently ignored on Gemini 3.6 Flash and newer" (deprecation warning headers; future 400); `candidateCount` unsupported on Gemini 3+; a request whose last non-empty turn is `model` (prefill) → **400** on 3.6+ ([GC request ref][g-ref-req], [3.6 guide][g-36], [troubleshooting][g-trouble]). Keep the toggles (older models honour them) but label them.
3. **Thinking** = `thinkingConfig.thinkingLevel` (`minimal|low|medium|high`, per-model subset) on Gemini 3/4; `thinkingBudget` only meaningful on 2.5 (on Gemini 4 it maps to a level with a warning; both together → 400) ([thinking][g-think]).
4. **Gemini 4 output caps**: 32,768 tokens per non-streaming request, 65,536 per stream; past that the response ends `finishReason: CONTINUATION` + `continuationToken` ("long decode", up to 1M). In streams, after 32K tokens a checkpoint token arrives every ~5K tokens → **a dropped GC stream can be resumed from the last checkpoint** ([long decode][g-ld]).
5. **IA is the only Google API with real disconnect recovery**: `background:true` (+`stream:true`), persist `interaction.id` + last `event_id`, reconnect with `GET /v1beta/interactions/{id}?stream=true&last_event_id=…`; the server replays missed events even after completion. Requires `store:true` (retained 55 days paid / 1 day free; `DELETE` when done) ([background][g-bg], [IA long decode][g-ld-ia]).
6. **Thought signatures**: mandatory to echo for function calls (400 otherwise); recommended for every other part; from 3.5 Flash the model uses reasoning context from *all* previous turns when signatures are present (costs input tokens). Store parts byte-exact, never merge/split ([signatures][g-sig], [3.5 guide][g-35]).
7. **Grounding offsets are UTF-8 byte offsets per part**; Search Suggestions (`searchEntryPoint.renderedContent`, HTML/CSS) carry ToS display obligations that a Telegram text bot cannot literally satisfy (**UNVERIFIED** how to comply) ([reference][g-ref], [terms][g-terms]).
8. **Safety filters default OFF** (2.5+). GC reports blocks in-band (`promptFeedback.blockReason`, `finishReason`); IA turns them into **HTTP 400** with `code: "safety"` etc. ([safety GC][g-safety], [safety IA][g-safety-ia]).
9. **Inputs**: inline payload up to **100 MB** (PDF 50 MB) since 2026-01-08; Files API 2 GB/file, 48 h; Office/OpenDocument files accepted natively (docx/pptx/xlsx…; PPT/ODP/ODS added 2026-11-05) ([file input][g-fin], [documents][g-docs], [changelog][g-changelog]).
10. **OpenRouter** exposes Gemini only through stateless Chat Completions / Responses / Messages (no IA, no background, no `store`); maps `reasoning.effort`→`thinkingLevel`; only native tool is Google Search (no URL context / code execution / Maps / File Search); no documented safety-settings, media-resolution, `videoMetadata`, `topK`, `logprobs`; stream cancellation is **not** supported for Google (you keep paying); grounding is flattened to `url_citation` annotations. Full gap list in §11. Use OR `debug.echo_upstream_body` (streaming only) to audit the exact upstream body ([OR errors & debugging][or-errors]).

### 0.1 Current models (Gemini Developer API)

| Model id | Role | `thinkingLevel` (\*=default) | Context / max output | Notes |
|---|---|---|---|---|
| `gemini-4-carbon` | strongest reasoning | low / medium\* / high (no `minimal`, cannot disable) | 1,048,576 **unified** in+out; 32K non-stream / 64K stream per request; long decode to 1M | explicit caching → error; sampling ignored; not on free tier; set ≥20 min HTTP timeouts ([carbon][g-carbon]) |
| `gemini-4-argon` | efficient Gemini 4 | same | same | same ([argon][g-argon], [model page][g-m-argon]) |
| `gemini-3.8-flash` | default Flash | low / medium\* / high (`minimal` → error) | 1M in / 64K out | ([3.8][g-38]) |
| `gemini-3.6-flash` | previous Flash | minimal / low / medium\* / high | 1M / 64K | ([thinking][g-think]) |
| `gemini-3.5-flash-lite` | cheapest | minimal\* / low / medium / high | 1M / 64K | ([3.6 guide][g-36]) |
| `gemini-3.1-pro-preview` | Pro preview | low / medium / high\* | 1M / 64K | >200K-token tiered price; `-customtools` variant ([3.5 guide][g-35], [pricing][g-pricing]) |
| `gemini-3.1-flash-lite`, `gemini-3-flash-preview` | | minimal\*…high / minimal…high\* | 1M / 64K | ([3.5 guide][g-35]) |
| `gemini-2.5-pro`, `gemini-2.5-flash` | deprecated | `thinkingBudget` only | 1M / 64K | access limited to past users since 2026-09-18 ([changelog][g-changelog]) |
| `gemini-nano-banana-2.1` | image gen default | minimal / medium\* / high | 131K / 32K | 1K/2K/4K (no 512) ([image gen][g-img]) |
| `gemini-3.1-flash-image` | Nano Banana 2 (deprecated, no date) | minimal\* / high | 131K / 32K | 512/1K/2K/4K |
| `gemini-3.1-flash-lite-image` | fastest image | minimal\* / high | 65K / 4K | 2K/4K unsupported ([model page][g-m-flimg]) |
| `gemini-3-pro-image` | Nano Banana Pro | thinking always on | 65K / 32K | up to 14 reference images |
| `gemini-3.8-flash-tts`, `gemini-3.8-flash-lite-tts` | TTS | — | 8,192 in / 16,384 out | returns WAV by default ([TTS][g-tts]) |

Aliases `gemini-flash-latest`, `gemini-flash-lite-latest`, `gemini-pro-latest` exist and are hot-swapped with 2 weeks' email notice ([models][g-models], [IA create ref][g-ref-iac]).

---

## 1. `generateContent` / `streamGenerateContent` (GC)

### 1.1 Transport

| Item | Value |
|---|---|
| Unary | `POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent` ([ref][g-ref]) |
| Stream | `POST …/v1beta/models/{model}:streamGenerateContent?alt=sse` → SSE; each `data:` event is one `GenerateContentResponse` chunk ([stream ref][g-ref-stream], [text gen][g-text]). Without `alt=sse` the body is a streamed JSON array — **UNVERIFIED** in current docs. |
| Auth | header `x-goog-api-key: $KEY` (or `?key=`) ([text gen][g-text]) |
| Versions | `v1` stable, `v1beta` preview features (per-part `mediaResolution`, context caching, audio output…) ([API versions][g-apiver], [media res][g-mr]) |
| JSON casing | proto3 JSON: parsers accept lowerCamel **and** snake_case names; enums as names ([protobuf JSON][pb-json]). Google's own REST samples mix `"thinkingLevel": "HIGH"` and `"medium"` ([3.5 guide][g-35], [argon][g-argon]). |
| Server timeouts (Gemini 4) | non-streaming closed after 10 min, streaming after 20 min; set client read timeout 20 min ([long decode][g-ld]) |

### 1.2 Top-level request body ([GC request ref][g-ref-req])

| Field | Type / values | Notes |
|---|---|---|
| `contents[]` | `Content{role: "user"\|"model", parts[]}` | full history (stateless). Roles should alternate. |
| `systemInstruction` | `Content` | "Currently, text only." |
| `tools[]` | `Tool` | §1.8 |
| `toolConfig` | `ToolConfig` | §1.9 |
| `safetySettings[]` | `SafetySetting` | §1.7 |
| `generationConfig` | `GenerationConfig` | §1.4 |
| `cachedContent` | `"cachedContents/{id}"` | explicit cache; **error on Gemini 4** ([caching][g-cache], [argon][g-argon]) |
| `serviceTier` | `standard`(default) / `flex` / `priority` | echoed in `usageMetadata.serviceTier`; priority may downgrade → check header `x-gemini-service-tier` ([flex][g-flex], [priority][g-prio]) |
| `store` | bool | request logging; overrides project setting. New **paid** projects default to `store=true` since 2026-10-26 ([changelog][g-changelog]) |
| `continuationToken` | string | resume long decode; `contents` must replicate the original request exactly ([GC request ref][g-ref-req]) |
| `labels` | map | Cloud-label rules; aggregators may set `safety_identifier` ([GC request ref][g-ref-req]) |

### 1.3 `Content.parts[]` — `Part` union + metadata ([reference: Content][g-ref])

| Part field | Shape | Notes |
|---|---|---|
| `text` | string | |
| `inlineData` | `{mimeType, data(base64), displayName?}` | `displayName` used as reference id under `verbalization_mode=REFERENCE_ONLY` |
| `fileData` | `{fileUri, mimeType?, displayName?}` | Files API URI, public/signed HTTPS URL (≤15 MB), GCS registration, or **YouTube URL** ([file input][g-fin], [video][g-video]) |
| `functionCall` | `{name, id?, args}` | `id` always returned on Gemini 3 ([FC][g-fc]) |
| `functionResponse` | `{name, id, response, parts?[], willContinue?, scheduling?}` | `parts[]` = multimodal results (`inlineData`/`fileData`, incl. audio/video/Office since 2026-11-05); `response` may reference blobs via `{"$ref": displayName}` |
| `executableCode` | `{language: "PYTHON", code, id?}` | built-in code execution |
| `codeExecutionResult` | `{outcome: OUTCOME_OK\|OUTCOME_FAILED\|OUTCOME_DEADLINE_EXCEEDED, output?, id?}` | text only; generated images arrive as separate `inlineData` parts ([code exec][g-code]) |
| `toolCall` / `toolResponse` | `{toolType: GOOGLE_SEARCH_WEB\|GOOGLE_SEARCH_IMAGE\|URL_CONTEXT\|GOOGLE_MAPS\|FILE_SEARCH\|MEDIA_PROCESSING, id?, toolName?, args?/response?}` | server-side tool trace; returned with `toolConfig.includeServerSideToolInvocations=true` and always for agentic video; must be echoed back |
| `thought` | bool | marks a thought-summary part (text or image) |
| `thoughtSignature` | string (opaque) | §3 |
| `videoMetadata` | `{startOffset: "40s", endOffset: "80s", fps: (0,24], default 1.0}` | sibling of `inlineData`/`fileData` |
| `mediaResolution` | `{level: MEDIA_RESOLUTION_LOW\|MEDIUM\|HIGH\|ULTRA_HIGH}` | per-part, Gemini 3/4, v1beta, "experimental" ([media res][g-mr]) |
| `mediaProcessing` | `STATIC` \| `AGENTIC` | video only; agentic on 4 Carbon/Argon, 3.8, 3.6, 3.5 Flash-Lite ([video][g-video]) |
| `partMetadata` | object | client bookkeeping |
| `audioTranscription`, `speechMetadata{speaker, style}` | | transcription output / TTS direction on text parts ([TTS][g-tts]) |

### 1.4 `generationConfig` ([GC request ref][g-ref-req])

| Field | Values / notes |
|---|---|
| `maxOutputTokens` | default = model `outputTokenLimit`; **includes thinking tokens**; Gemini 4: per-request cap 32K (non-stream REST) / 64K (stream); values above the cap = total budget for long decode. The long-decode page both says ">32,768 on non-streaming REST without a `continuation_token` returns 400" and shows a REST hop-1 with 262,144 — conflicting, test it (**UNVERIFIED**) ([thinking][g-think], [long decode][g-ld]) |
| `temperature`, `topP`, `topK` | ignored on 3.6+ & 4; defaults per model (`getModel`); range: reference says temperature [0.0, 2.0], troubleshooting table says 0.0–1.0 (conflicting) ([ref][g-ref-req], [troubleshooting][g-trouble]) |
| `candidateCount` | default 1; unsupported Gemini 3+ ([troubleshooting][g-trouble]) |
| `stopSequences` | ≤5 |
| `seed` | int; random if unset |
| `presencePenalty`, `frequencyPenalty` | number |
| `responseLogprobs` + `logprobs` (0–20) | → `candidate.avgLogprobs`, `logprobsResult{topCandidates, chosenCandidates, logProbabilitySum}`; per-model availability **UNVERIFIED** |
| `responseMimeType` | `text/plain` (default) \| `application/json` \| `text/x.enum` |
| `responseJsonSchema` (preferred) / `responseSchema` (OpenAPI subset) | structured output; also new flat `responseFormat.text{mimeType: APPLICATION_JSON, schema}` used in REST samples ([structured][g-struct]) |
| `responseFormat` | `{text{mimeType,schema}, audio{mimeType: AUDIO_MP3\|OGG_OPUS\|L16\|WAV\|ALAW\|MULAW, delivery: INLINE\|URI, sampleRate, bitRate}, image{mimeType: IMAGE_JPEG, delivery, aspectRatio, imageSize}}` |
| `responseModalities` | `["TEXT"]`, `["TEXT","IMAGE"]`, `["IMAGE"]`, `["AUDIO"]` — must match a supported combination exactly, else error |
| `thinkingConfig` | `{includeThoughts, thinkingBudget, thinkingLevel}` — §1.5 |
| `imageConfig` | `{aspectRatio, imageSize}` — §1.6 |
| `mediaResolution` | global: `MEDIA_RESOLUTION_LOW\|MEDIUM\|HIGH` (no ULTRA_HIGH globally) |
| `speechConfig` | `{voiceConfig{prebuiltVoiceConfig{voiceName}} \| voice, multiSpeakerVoiceConfig{speakerVoiceConfigs[{speaker, voiceConfig}]}, languageCode}` |
| `enableEnhancedCivicAnswers` | bool — "Enables enhanced civic answers. It may not be available for all models." (semantics otherwise undocumented) |
| `enableAffectiveDialog`, `translationConfig`, `audioTranscriptionConfig{languageCodes, customVocabulary, wordTimestamp, diarization, sourceText, mode: VERBATIM\|SMART}` | Live / transcription models |

### 1.5 Thinking ([thinking GC][g-think])

| Knob | Behaviour |
|---|---|
| `thinkingLevel` | `MINIMAL` ("little to no thinking", not guaranteed off), `LOW`, `MEDIUM`, `HIGH` ("Dynamic"). Required for Gemini 4, recommended Gemini 3, error on 2.5. Support per model in §0.1. |
| `thinkingBudget` | 2.5 Pro 128–32768 (cannot disable); 2.5 Flash 0–24576 (`0` off, `-1` dynamic default); 2.5 Flash-Lite 512–24576 (off by default). Gemini 4: deprecated, mapped to nearest level + warning; sending both → 400 ([3.5 guide][g-35]: both → 400 on Gemini 3 too). |
| `includeThoughts` | returns **summaries** as parts with `thought: true` (streaming = rolling incremental summaries). Thinking happens & is billed regardless; levels/budgets apply to raw thoughts, not summaries. |
| Cost | output price covers thoughts (`usageMetadata.thoughtsTokenCount`); echoed signatures add input tokens. |
| Disable | impossible on Gemini 4, 3 Flash/Flash-Lite (use `minimal`), 3 image models ([image gen][g-img]) |

### 1.6 Media resolution & image generation config

**Token budget per media item** ([media res][g-mr]):

| Level | Gemini 3/4: image / video frame / audio / PDF page | Gemini 2.5: image / video / audio |
|---|---|---|
| UNSPECIFIED (default) | 1120 / 70 / 25 per s / 560 | 256 + pan&scan / 256 / 32 per s |
| LOW | 280 / 70 / 25 / 280 + native text | 64 / 64 / 32 |
| MEDIUM | 560 / 70 / 25 / 560 + native text | 256 / 256 / 32 |
| HIGH | 1120 / 280 / 25 / 1120 + native text | 256 + pan&scan / 256 / 32 |
| ULTRA_HIGH (per-part only) | 2240 / – / – / – | – |

Precedence: per-part beats global. Recommended: images HIGH, PDFs MEDIUM, video LOW (HIGH for OCR). `mediaResolution` and `mediaProcessing` are independent.

**Image output** ([image gen GC][g-img], [GC request ref][g-ref-req]):

| Item | Detail |
|---|---|
| Request | `generationConfig.responseModalities: ["TEXT","IMAGE"]` (default for image models) or `["IMAGE"]`; `imageConfig.aspectRatio` ∈ `1:1 1:4 4:1 1:8 8:1 2:3 3:2 3:4 4:3 4:5 5:4 9:16 16:9 21:9` (1:4/4:1/1:8/8:1 only NB 2.1 & 3.1 Flash Image); `imageConfig.imageSize` ∈ `512` (3.1 Flash Image only) \| `1K` (default) \| `2K` \| `4K` — **uppercase K required, lowercase rejected** |
| Default aspect | matches input image, else 1:1 |
| Thinking | Gemini 3 image models always think; produce up to **two interim "thought images"** (`thought:true`, not charged); last thought image = final image |
| Inputs | up to 14 reference images (3 Pro Image: 5 high-fidelity); video input only NB 2.1 / 3.1 Flash / 3.1 Flash-Lite Image; no audio input |
| Grounding | `tools: [{"googleSearch": {"searchTypes": {"webSearch": {}, "imageSearch": {}}}}]` (image search only NB 2.1 & 3.1 Flash Image; no people) |
| Output tokens / image | NB 2.1 & 3.1 Flash: 1K 1120, 2K 1680, 4K 2520 (pricing page says NB 2.1 4K = 3780 — docs inconsistent); 512 = 747; 3 Pro Image: 1K/2K 1120, 4K 2000; 2.5 Flash Image 1290 |
| Watermark | all outputs carry SynthID |

### 1.7 Safety settings ([safety GC][g-safety], [GC request ref][g-ref-req])

| Item | Values |
|---|---|
| `category` | `HARM_CATEGORY_HARASSMENT`, `HARM_CATEGORY_HATE_SPEECH`, `HARM_CATEGORY_SEXUALLY_EXPLICIT`, `HARM_CATEGORY_DANGEROUS_CONTENT`, `HARM_CATEGORY_CIVIC_INTEGRITY` (listed as supported in request docs, absent from the `HarmCategory` enum page), `HARM_CATEGORY_JAILBREAK` (new) |
| `threshold` | `OFF`, `BLOCK_NONE`, `BLOCK_ONLY_HIGH`, `BLOCK_MEDIUM_AND_ABOVE`, `BLOCK_LOW_AND_ABOVE`, `HARM_BLOCK_THRESHOLD_UNSPECIFIED` |
| Default | **Off for Gemini 2.5 and later**; core harms (e.g. child safety) always blocked, not adjustable |
| One per category | unspecified categories use defaults |
| Feedback | `promptFeedback.{blockReason, safetyRatings}`; `candidate.{finishReason, safetyRatings[{category, probability: NEGLIGIBLE\|LOW\|MEDIUM\|HIGH, blocked}]}`; blocked content is not returned |

The old Python bot's "all OFF" request is now equivalent to the default; sending it explicitly is still harmless and transparent.

### 1.8 `tools[]` ([reference: Tool][g-ref])

| Tool JSON | Config fields | Notes |
|---|---|---|
| `{"googleSearch": {}}` | `timeRangeFilter{startTime,endTime}`, `searchTypes{webSearch{}, imageSearch{}}` | billed per executed query on Gemini 3+ ([search][g-search]) |
| `{"googleSearchRetrieval": {"dynamicRetrievalConfig": {...}}}` | `mode: MODE_DYNAMIC`, `dynamicThreshold` | legacy, old models only |
| `{"urlContext": {}}` | — | ≤20 URLs/request, ≤34 MB per URL, public only; types html/json/plain/xml/css/js/csv/rtf, png/jpeg/bmp/webp, pdf; not YouTube/Workspace/video/audio; fetched tokens show in `toolUsePromptTokenCount` ([URL ctx][g-url]) |
| `{"codeExecution": {}}` | — | Python only (IA also `bash` on Gemini 3+), 30 s runtime, ≤5 auto-retries, fixed library set (matplotlib for plots), file in via inline/fileData, files out always `inlineData` ([code exec][g-code], [IA ref][g-ref-ia]) |
| `{"googleMaps": {"enableWidget": true}}` | + `toolConfig.retrievalConfig.latLng/languageCode` | display/attribution rules ([maps][g-maps]) |
| `{"fileSearch": {"fileSearchStoreNames": [...], "topK", "metadataFilter"}}` | | RAG over hosted stores (100 MB/doc) ([file search][g-fsearch]) |
| `{"computerUse": {"environment": "ENVIRONMENT_BROWSER\|MOBILE\|DESKTOP", ...}}` | `excludedPredefinedFunctions`, `enablePromptInjectionDetection`, `disabledSafetyPolicies` | **client executes** actions (GA 2026-10-30) ([computer use][g-cu]) |
| `{"mcpServers": [{"name", "streamableHttpTransport": {"url", "headers"}}]}` | | remote MCP (IA notes "Gemini 3 does not support remote MCP" — **conflicting**) ([IA overview][g-ia]) |
| `{"functionDeclarations": [{name, description, parameters \| parametersJsonSchema, response?, behavior?}]}` | | client tools |

Built-in + custom tools in one request ("tool combination", preview, Gemini 3/4): set `toolConfig.includeServerSideToolInvocations: true`, then echo back the whole model `Content` (incl. `toolCall`/`toolResponse` parts, ids, signatures) ([tool combination][g-combo]).

### 1.9 `toolConfig` ([GC request ref][g-ref-req])

`functionCallingConfig{mode: AUTO|ANY|NONE|VALIDATED, allowedFunctionNames[]}`, `retrievalConfig{latLng{latitude, longitude}, languageCode}`, `includeServerSideToolInvocations: bool`.

### 1.10 Validation rules worth enforcing client-side

- No trailing `model` turn on 3.6+ → 400 ([3.6][g-36]). 3.8 guide also says "ensure the final user turn contains non-empty text" ([3.8][g-38]); whether a media-only user turn fails is **UNVERIFIED** (test; maybe append the caption or a placeholder only if the user asks — don't inject silently).
- Parallel FC responses must follow all calls: `FC1+sig, FC2, FR1, FR2` (interleaving → 400) ([signatures][g-sig]).
- `functionResponse` should carry the `id` (and `name`) of the call ([3.6][g-36], [FC][g-fc]).

### 1.11 Example (REST, camelCase)

```json
POST /v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse
{
  "systemInstruction": {"parts": [{"text": "…user-provided system prompt…"}]},
  "contents": [
    {"role": "user", "parts": [
      {"fileData": {"fileUri": "https://www.youtube.com/watch?v=9hE5-98ZeCg"},
       "videoMetadata": {"startOffset": "40s", "endOffset": "80s", "fps": 2},
       "mediaProcessing": "STATIC"},
      {"inlineData": {"mimeType": "image/jpeg", "data": "<b64>"},
       "mediaResolution": {"level": "MEDIA_RESOLUTION_HIGH"}},
      {"text": "Compare the slide in the image with the video segment."}]}
  ],
  "tools": [{"googleSearch": {}}, {"urlContext": {}}, {"codeExecution": {}}],
  "safetySettings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "OFF"}],
  "generationConfig": {
    "maxOutputTokens": 65536,
    "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}
  },
  "serviceTier": "standard"
}
```

---

## 2. Interactions API (IA)

### 2.1 Status & surface

GA since June 2026, "recommended interface for all new projects"; new models/agents/tools launch here first ([IA overview][g-ia]). Core resource `Interaction` = one turn as a chronological list of **steps** ([IA ref][g-ref-ia]). Breaking schema of May 2026 (`outputs` → `steps`, polymorphic `response_format`) selected via header `Api-Revision: 2026-05-20`; legacy removed 2026-06-08 ([breaking changes][g-ia-bc]). Interactions are GA in `v1` as well as `v1beta` ([API versions][g-apiver]).

| Method | HTTP |
|---|---|
| create | `POST /v1beta/interactions` (`?alt=sse` / `"stream": true`) |
| get | `GET /v1beta/interactions/{id}` — `?stream=true&last_event_id=…` resumes |
| cancel | `POST /v1beta/interactions/{id}/cancel` → status `cancelled` (slight delay) |
| delete | `DELETE /v1beta/interactions/{id}` → later GET 404 |

([IA ref][g-ref-ia], [get ref][g-ref-iag], [background][g-bg])

### 2.2 Request ([create ref][g-ref-iac])

| Field | Notes |
|---|---|
| `model` \| `agent` | model id, or managed agent (`deep-research-preview-04-2026`, `antigravity-preview-09-2026`, …) + `agent_config` |
| `input` | string \| `Content` \| `Content[]` \| **`Step[]`** (stateless replay) |
| `system_instruction` | string |
| `tools[]` | `{type: function\|google_search{search_types[web_search,image_search]}\|url_context\|code_execution\|google_maps\|file_search\|computer_use\|mcp_server}` (error message also lists `filesystem`, `bash`) ([IA errors][g-err-ia]) |
| `generation_config` | `seed`, `stop_sequences`, `tool_choice` (`auto\|any\|none\|validated` or `{mode, allowed_tools}`), `thinking_level`, `thinking_summaries` (`auto\|none`), `max_output_tokens`, `video_config`, `transcription_config`, `structured_speech_config`. **No** temperature/top_p/top_k/penalties/logprobs/candidate count/global media resolution/civic answers in the reference (overview prose still mentions temperature — **UNVERIFIED**) |
| `response_format` | one or array of `{type: text, mime_type, schema}`, `{type: image, aspect_ratio, image_size (512\|1K\|2K\|4K), mime_type: image/jpeg, delivery}`, `{type: audio, mime_type, sample_rate, bit_rate, delivery}`, `{type: video,…}` |
| `safety_settings[]` | `{type: harassment\|hate_speech\|sexually_explicit\|dangerous_content\|jailbreak, threshold: off\|block_only_high\|block_medium_and_above\|block_low_and_above, method?: severity\|probability}` |
| `previous_interaction_id` | server-side history (only inputs/outputs; `tools`, `system_instruction`, `generation_config` must be re-sent each turn) |
| `service_tier` | `standard\|flex\|priority` |
| `stream`, `store` (default true), `background` | input-only flags |
| `continuation_token` | long decode hop |
| `webhook_config{uris, user_metadata}`, `labels`, `environment` (agents) | |

Media parts: `{type:"image", data|uri, mime_type, resolution: low|medium|high|ultra_high}`; `{type:"video", uri:"https://www.youtube.com/watch?v=…", processing: "agentic"|"static"|{type:"static", start_offset:"10.5s", end_offset, fps}}`; `{type:"audio", data|uri, mime_type}`; `{type:"document", data|uri, mime_type: application/pdf|text/csv|docx|pptx|xlsx|…}` ([IA ref][g-ref-ia]).

### 2.3 Response

`Interaction{id, status, created, updated, model, steps[], usage, service_tier, …}`. Status: `in_progress`, `requires_action` (client FC pending), `completed`, `failed`, `cancelled`, `incomplete` (hit `max_output_tokens`), `requires_continuation` (long-decode hop) ([IA ref][g-ref-ia]). `POST` returns only model-generated steps; `GET` returns the full timeline incl. `user_input` ([IA overview][g-ia]).

Step types: `user_input`, `model_output{content[]}`, `thought{signature, summary[]}`, `function_call{id,name,arguments}`, `function_result{call_id,name,result[]}`, `google_search_call{arguments.queries, search_type, signature}`, `google_search_result{result[{title,url,snippet}], search_suggestions (HTML), signature}`, `url_context_call/result`, `code_execution_call{arguments{language: python|bash, code}}`, `code_execution_result{result: string|Content[], is_error}`, `google_maps_call/result`, `file_search_call/result`, `mcp_server_tool_call/result`, `processing_call/result` (agentic video). Text content carries `annotations[]`: `url_citation{url,title,start_index,end_index}` (byte offsets), `file_citation`, `place_citation` ([IA ref][g-ref-ia]).

`usage{total_input_tokens, initial_input_tokens, input_tokens_by_modality[], total_cached_tokens, cached_tokens_by_modality[], total_output_tokens, output_tokens_by_modality[], total_tool_use_tokens, total_thought_tokens, total_tokens, grounding_tool_count[{type: google_search|google_maps, count}]}` ([IA ref][g-ref-ia]).

### 2.4 Streaming & resumption ([IA streaming][g-stream-ia], [background][g-bg], [IA long decode][g-ld-ia])

Abridged; on the wire each event is an `event:` line plus a `data:` line.

```
event: interaction.created      data: {"interaction":{"id":"v1_…","status":"in_progress",…},"event_type":"interaction.created"}
event: step.start               data: {"index":0,"step":{"type":"thought"},"event_type":"step.start"}
event: step.delta               data: {"index":0,"delta":{"type":"thought_signature","signature":"…"},…}
event: step.start               data: {"index":1,"step":{"type":"model_output"},…}
event: step.delta               data: {"index":1,"delta":{"type":"text","text":"1, 2, 3, "},…}
event: step.stop                data: {"index":1,"event_type":"step.stop"}
event: interaction.completed    data: {"interaction":{…,"usage":{…}},…}
event: done                     data: [DONE]
```

- Delta types: `text`, `image{mime_type,data}`, `audio`, `video`, `document`, `thought_summary{content}`, `thought_signature` (last delta before `step.stop`), `arguments_delta` (accumulate JSON string), `text_annotation_delta`, per-tool `*_call`/`*_result` deltas. Unknown event types must be ignored.
- Every event can carry **`event_id`** (also on `interaction.status_update`); resume with `GET /v1beta/interactions/{id}?stream=true&last_event_id=<id>` → continues *after* that event; "read the stream to the end … the server replays missed events, even after the interaction completes".
- Errors mid-stream: `event: error`, `data: {"error":{"code":"gateway_timeout","message":"…"},"event_type":"error"}`.
- Long decode streams: after 32K tokens output arrives in ~5K-token chunks at checkpoints (pauses are normal); 15–20 min read timeouts.

### 2.5 Background execution, polling, cancellation, retention

| Topic | Rule |
|---|---|
| Enable | `"background": true` on create; returns id immediately; models (4 Carbon/Argon, 3.8 Flash, …) and agents ([background][g-bg]) |
| Stream + background | combined create (`background:true, stream:true`) is shown for agents ([Deep Research][g-dr]); for models the documented pattern is create(background) then `GET ?stream=true` — combined create for models **UNVERIFIED** |
| Store | `background:true` with `store:false` → 400 |
| Poll | `GET /interactions/{id}` until status ≠ `in_progress`/`queued`; max runtime 24 h (then `failed`) ([IA long decode][g-ld-ia]) |
| Chaining | `previous_interaction_id` pointing to an `in_progress` interaction → 400 |
| Cancel / delete | `POST …/cancel`, `DELETE …` |
| Retention | paid 55 days (configurable 7/14/28/55 in AI Studio logs), free 1 day ([IA overview][g-ia]) |
| Webhooks | push completion notifications for Interactions/Batch/video ([webhooks][g-wh]) |

### 2.6 Statefulness

- **Stateful**: `store:true` + `previous_interaction_id`; server keeps thoughts/signatures; better implicit-cache hits.
- **Stateless**: `store:false`, keep `steps[]` locally, send them back as `input` + new `user_input` step; "you **must** preserve and resend all model-generated steps (such as `thought` and `function_call`) exactly as received" ([IA text gen][g-ia-text]).
- Mixing models in one conversation: later model must accept the earlier model's output modalities as input ([IA overview][g-ia]).

### 2.7 Errors & safety in IA ([IA errors][g-err-ia], [safety IA][g-safety-ia])

Body `{"error": {"code": "<snake_case>", "message": "…"}}`. Request codes: `invalid_request` 400, `failed_precondition` 400, `parameter_unknown` 400, `authentication` 401, `payment_required` 402, `permission_denied` 403, `not_found`/`model_not_found` 404, `already_exists`/`aborted` 409, `out_of_range` 416, `rate_limit_exceeded`/`quota_exceeded`/`too_many_requests` 429, `cancelled` 499, `api_error` 500, `unimplemented` 501, `service_unavailable` 503, `deadline_exceeded` 504. Generation-blocked codes (safety blocks fail the interaction with HTTP 400 per the safety page; status for the others not stated): `safety`, `recitation`, `language`, `prohibited_content`, `spii`, `blocklist`, `image_safety`, `image_prohibited_content`, `image_recitation`, `image_other`, `content_blocked`, `jailbreak`. Generation errors: `malformed_function_call`, `malformed_tool_call`, `unexpected_tool_call`, `no_image`, `too_many_tool_calls`, `missing_thought_signature`. Background failures → `status: failed` with the same code in `errors`.

### 2.8 Not (yet) in IA

Batch API, Python automatic function calling, explicit caching (`cachedContents`) ([IA overview][g-ia]); sampling/logprob/penalty/candidate/civic params (see §2.2).

### 2.9 GC vs IA for this bot

| Need | GC | IA |
|---|---|---|
| Local state, stateless replay | native (`contents[]`) | `store:false` + `Step[]` input |
| Resume an in-flight generation after crash/disconnect | only Gemini 4 long-decode checkpoints (after 32K tokens); otherwise restart | `background:true` + `last_event_id` replay (any model with background support); needs `store:true` |
| Cancel upstream work | close the HTTP connection (billing semantics: dropped stream before first checkpoint costs $0 on Gemini 4 — [long decode][g-ld]) | explicit `POST …/cancel` |
| Every legacy knob (temperature, topK, penalties, logprobs, candidateCount, civic answers, explicit cache) | yes | no |
| Built-in tools, thinking, images, safety, service tiers | yes | yes (+ agents, Deep Research) |
| Safety block UX | in-band 200 (`blockReason`/`finishReason`) | HTTP 400 / `error` event |
| Through OpenRouter | OR's upstream wire format to Google is undocumented (probably GC — **UNVERIFIED**; check with `debug.echo_upstream_body`) | **not available** |

Suggested design (opinion): keep the provider abstraction message-centric; implement GC first (OR parity, full knob coverage) and add an IA backend for direct-Google long generations (`background`+`stream`, persist `interaction_id`/`last_event_id`, `DELETE` after completion if the user wants no server retention).

### 2.10 Other async options

- **Batch API** (GC only): 50 % price, target ≤24 h, jobs expire after 48 h (`JOB_STATE_EXPIRED`), inline requests (keep total <20 MB) or JSONL file ≤2 GB ([batch][g-batch]).
- **Flex** tier: synchronous, 50 % off, "minutes (1–15 min target)", sheddable → 503/429, no auto-upgrade; set ≥10 min timeouts ([flex][g-flex]).
- **Priority** tier (Tier 2/3 billing accounts): +75–100 %, overflow silently downgraded to standard (billed standard; header `x-gemini-service-tier`) ([priority][g-prio]).

---

## 3. Thought signatures

### 3.1 generateContent rules ([signatures][g-sig], [thinking][g-think], [image gen][g-img], [video][g-video], [3.5 guide][g-35])

| Situation | Where signatures appear | Must echo? | If omitted |
|---|---|---|---|
| Gemini 3/4, single function call | on the `functionCall` part | **Yes**, in the exact part | 400 "Function call `X` in the `N.` content block is missing a `thought_signature`" |
| Parallel FCs | only on the **first** `functionCall` part of that response | Yes | 400 |
| Sequential FCs (multi-step turn) | each step's first FC | Yes, all steps of the *current* turn (turn starts at the most recent `user` content with non-FR content); older turns not validated | 400 |
| Model response without FC | last part (`text`, `inlineData`, …), when the model thought; in streams possibly an **empty-text part** — read until `finishReason` | Recommended | no error; quality may degrade |
| Gemini 3 image models | every non-thought image part; first text part after thoughts; thought images have none | "Failure to circulate … may cause the response to fail" | possible failure |
| Server-side tool trace (`toolCall`/`toolResponse`, agentic video `MEDIA_PROCESSING`) | on those parts | echo full `Content` | context loss / errors (**UNVERIFIED** exact error) |
| Gemini 2.5 | only with function declarations, on the first part | optional | none |
| Gemini 3.5 Flash+ | signatures in history ⇒ model reuses reasoning of *all* previous turns | recommended | cheaper (fewer input tokens), less continuity |

Additional rules: never concatenate parts carrying signatures, never merge a signed part with an unsigned one; signatures add billed input tokens; `finishReason: MISSING_THOUGHT_SIGNATURE` exists ([reference][g-ref]). Injecting foreign/hand-made function calls (e.g. history from another provider): set `thoughtSignature` to `"context_engineering_is_the_way_to_go"` or `"skip_thought_signature_validator"` to bypass validation ([signatures FAQ][g-sig]).

### 3.2 IA rules ([IA thinking][g-think-ia], [IA signatures][g-sig-ia])

Signatures live only in `thought` steps and built-in tool call/result steps (never on `user_input`, `model_output`, or `function_call`). Stateful mode: nothing to do. Stateless: resend every `thought` step (and tool-step signatures) unmodified. "When switching models within a session, you should still resend the previous model's thought blocks. The backend manages compatibility."

### 3.3 Switching models (GC)

No explicit GC rule found. IA guidance above suggests keeping them; the documented hard failure is only for missing FC signatures. Pragmatic policy (opinion): store signatures per part with the producing model id; replay as-is; if Google answers 400 on a signature (cross-family history), retry once with signatures stripped on non-FC parts and dummy signatures on FC parts, and surface that retry to the user (**UNVERIFIED** behaviour). Long-decode `continuationToken` is separate from signatures and only valid with the identical request.

---

## 4. Grounding & citations

### 4.1 `candidate.groundingMetadata` ([reference: Candidate][g-ref], [search][g-search])

| Field | Meaning |
|---|---|
| `webSearchQueries[]` | executed queries (billing unit on Gemini 3+; empty queries not billed) |
| `imageSearchQueries[]` | image-search queries |
| `searchEntryPoint.renderedContent` | HTML+CSS "Google Search" chip for Search Suggestions; `sdkBlob` base64 JSON |
| `groundingChunks[]` | union: `web{uri, title}` (uri is a `vertexaisearch.cloud.google.com` redirect, title usually the domain); `image{sourceUri, imageUri, title, domain}`; `retrievedContext{uri,title,text,fileSearchStore,customMetadata,pageNumber,mediaId}`; `maps{uri,title,text,placeId,placeAnswerSources.reviewSnippets[{reviewId,googleMapsUri,title}]}` |
| `groundingSupports[]` | `{segment{partIndex, startIndex, endIndex, text}, groundingChunkIndices[], confidenceScores[] (0–1, same length), renderedParts[]}` |
| `retrievalMetadata.googleSearchDynamicRetrievalScore` | legacy dynamic retrieval |
| `googleMapsWidgetContextToken` | when `googleMaps.enableWidget` |

**Offsets**: `startIndex`/`endIndex` are **bytes** from the start of the Part at `partIndex` (start inclusive, end exclusive). Google's own Python sample slices a `str` with them (correct only for ASCII) ([search][g-search]) — in Rust slice the UTF-8 `String` by bytes, check `is_char_boundary`, and map per `partIndex` (thought parts and other parts count). **Streaming**: each chunk's `groundingChunks` contains only *new* chunks; `groundingChunkIndices` index the concatenation across all chunks — accumulate in order ([reference][g-ref]).

Other metadata: `candidate.urlContextMetadata.urlMetadata[{retrievedUrl, urlRetrievalStatus: …_SUCCESS|_ERROR|_PAYWALL|_UNSAFE}]` ([URL ctx][g-url]); `candidate.citationMetadata.citationSources[{startIndex, endIndex (bytes), uri, license}]` = recitation of training data (license required for code) ([reference][g-ref]).

### 4.2 Display / usage obligations

- **Google Search** ([terms][g-terms]): display Grounded Results "with the associated Search Suggestion(s) to the end user who submitted the prompt"; don't modify or intersperse other content with Grounded Results/Search Suggestions; no interstitials/redirects between links and destinations; no click/link tracking; may store grounded text ≤2 years for the user's chat history; no caching/indexing/scraping of Links; when re-submitting interim results, show up to 5 Search Suggestions with the final result. Google stores grounding prompts/outputs 30 days. Group-chat display to non-requesters and HTML-less rendering are not addressed — compliance approach **UNVERIFIED** (options: render `webSearchQueries` as `https://www.google.com/search?q=` links, or send `renderedContent` as an HTML attachment).
- **Image search**: link to the containing page (not the image); single-click path from any shown source image ([image gen][g-img]).
- **Google Maps**: sources must immediately follow the grounded content, viewable within one interaction, attributed "Google Maps" per text-attribution guidelines, link via `uri`/`googleMapsUri` ([maps][g-maps]).

### 4.3 Inline citation recipe

Sort supports by `endIndex` desc; insert `[n](uri)` markers at byte `endIndex` within the right part; number by first appearance; append a source list (title → uri) plus suggestions. Keep raw `groundingMetadata` in local storage for re-rendering ([search][g-search]).

---

## 5. Inputs & limits

| Limit | Value | Source |
|---|---|---|
| Inline payload | 100 MB per request (PDF 50 MB) since 2026-01-08; older per-modality pages still say 20 MB | [file input][g-fin], [changelog][g-changelog], [audio][g-audio] |
| Files API | 2 GB/file, 20 GB/project, kept 48 h, `state: PROCESSING→ACTIVE\|FAILED`; resumable upload `POST /upload/v1beta/files` with `X-Goog-Upload-Protocol: resumable`; user uploads not downloadable | [files][g-files], [Files ref][g-ref-files] |
| External URL in `fileUri` | public or pre-signed HTTPS (S3/Azure SAS), ≤15 MB, not Gemini 2.0 (note: Telegram file URLs embed the bot token — don't pass them) | [file input][g-fin] |
| GCS registration | 2 GB/file, access ≤30 days | [file input][g-fin] |
| Images | png, jpeg, webp, heic, heif (reference enum also gif, avif; IA also bmp, tiff); ≤3,600 per request | [image und.][g-imgu], [reference][g-ref], [IA ref][g-ref-ia] |
| PDF | ≤50 MB or 1,000 pages; "each page = 258 tokens" (Gemini 3/4 instead use media-resolution budgets, 560/page default); pages scaled into 768–3072 px; Gemini 3: embedded text extracted and **not charged**, page images count as IMAGE | [documents][g-docs], [media res][g-mr] |
| Office/OpenDocument | docx/dotx/odt/pptx → text (+ embedded images for docx/pptx); doc/ppt/odp → page images; xlsx/xls/ods → **first sheet only** (CSV / data+formulas) | [documents][g-docs] |
| Text-ish MIME | `text/plain, text/html, text/css, text/javascript, text/x-typescript, text/csv, text/markdown, text/x-python, text/xml, text/rtf, application/json, application/x-ipynb+json, application/x-python-code, application/x-javascript, application/x-typescript, application/rtf` (processed as plain text) | [reference][g-ref], [documents][g-docs] |
| Audio | wav, mp3 (`audio/mp3`), aiff, aac, ogg, flac, `audio/mpeg`, m4a, l16, opus, alaw, mulaw, webm; ≤9.5 h total per prompt; 32 tok/s (25 tok/s on Gemini 3+ per media-res table); downsampled to 16 kbps mono | [audio][g-audio], [media res][g-mr] |
| MP3 MIME quirk | Google lists **both** `audio/mp3` and `audio/mpeg`; samples use `audio/mp3` for uploads and `audio/mpeg` for counting — either works per docs; send Telegram's real type | [audio][g-audio] |
| Video | mp4, mpeg, quicktime/mov, avi, x-flv, mpg, webm, wmv, 3gpp; static mode 1 fps (+1 kbps audio, timestamps each second); ~100 tok/s default, ~300 tok/s HIGH; 1M-ctx models: ~3 h (low res) / 1 h (high); `videoMetadata.fps` (0, 24], `startOffset`/`endOffset` like `"1250s"` | [video][g-video], [reference][g-ref] |
| YouTube | `fileData.fileUri` = watch URL; **public only** (not private/unlisted); ≤10 videos/request (2.5+); free tier ≤8 h/day; "preview … at no charge", pricing may change; not fetchable via URL context | [video][g-video], [URL ctx][g-url] |
| Agentic video | `mediaProcessing: AGENTIC` → up to 88 % fewer tokens on long videos; navigation = thinking tokens + `toolUsePromptTokenCount`; returns `MEDIA_PROCESSING` toolCall/toolResponse parts to echo | [video][g-video] |

---

## 6. Outputs

| Output | Where | How to save |
|---|---|---|
| Images (image models) | `parts[].inlineData{mimeType, data}` (e.g. `image/png`; IA `image/jpeg`); thought images have `thought:true` | base64-decode, keep the part (with `thoughtSignature`) in history for multi-turn editing ([image gen][g-img]) |
| Code-execution plots/files | separate `inlineData` parts next to `executableCode`/`codeExecutionResult`; "can't return other artifacts like media files" beyond that; IA: images in `model_output`, `code_execution_result.result` may be `Content[]` | decode and send as photo/document ([code exec][g-code], [IA code exec][g-code-ia]) |
| TTS audio | `responseModalities:["AUDIO"]`, `speechConfig.voiceConfig.voice` (prebuilt name, stored `voice_…`, or replication key); 3.8 TTS returns full **WAV** (24 kHz mono 16-bit) on unary; streaming yields raw PCM; force `responseFormat.audio.mimeType: AUDIO_L16\|AUDIO_MULAW\|AUDIO_ALAW\|AUDIO_MP3\|AUDIO_OGG_OPUS` (REST) | Telegram `sendAudio` wants MP3/M4A and `sendVoice` OGG/Opus, MP3 or M4A, so request `AUDIO_MP3`/`AUDIO_OGG_OPUS` or send WAV as a document ([TTS][g-tts], [GC request ref][g-ref-req], [Telegram sendVoice][tg-voice], [Telegram sendAudio][tg-audio]) |
| URI delivery | `responseFormat.*.delivery: URI` → file; generated files downloadable via `files.download` | ([files][g-files]) |

---

## 7. Streaming chunk format (GC)

Each SSE `data:` = `GenerateContentResponse` ([reference][g-ref], [text gen][g-text]):

```json
{"candidates":[{"content":{"role":"model","parts":[
    {"text":"**Plan**…","thought":true},
    {"text":"The answer is","thoughtSignature":"Cs8B…"}]},
  "index":0}],
 "usageMetadata":{"promptTokenCount":812,"thoughtsTokenCount":214},
 "modelVersion":"gemini-3.8-flash","responseId":"mAitaLmkHPPlz7IPvtfUqQ4"}
… last chunk: "finishReason":"STOP", groundingMetadata, full usageMetadata
```

- `responseId` identical across chunks; `usageMetadata` is cumulative within a stream — keep the last value ([long decode][g-ld]).
- Parts arrive as deltas; concatenate consecutive text parts of the same kind for display, but **store** parts exactly (signatures).

**`finishReason`** (`candidate`): `STOP`, `MAX_TOKENS`, `SAFETY`, `RECITATION`, `LANGUAGE`, `OTHER`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII`, `MALFORMED_FUNCTION_CALL`, `IMAGE_SAFETY`, `IMAGE_PROHIBITED_CONTENT`, `IMAGE_OTHER`, `NO_IMAGE`, `IMAGE_RECITATION`, `UNEXPECTED_TOOL_CALL`, `TOO_MANY_TOOL_CALLS`, `MISSING_THOUGHT_SIGNATURE`, `MALFORMED_RESPONSE`, `ESCALATION`, `PUP_LIMITED_DISABLED`, `CONTINUATION` (+ `continuationToken`); empty = still generating. `finishMessage` explains, only when `finishReason` set ([reference][g-ref]).

**`promptFeedback.blockReason`**: `SAFETY`, `OTHER`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `IMAGE_SAFETY` — then no candidates ([reference][g-ref]).

**`usageMetadata`**: `promptTokenCount` (includes cached), `cachedContentTokenCount`, `candidatesTokenCount`, `toolUsePromptTokenCount`, `thoughtsTokenCount`, `totalTokenCount`, `promptTokensDetails[]`, `cacheTokensDetails[]`, `candidatesTokensDetails[]`, `toolUsePromptTokensDetails[]` (each `{modality: TEXT|IMAGE|VIDEO|AUDIO|DOCUMENT, tokenCount}`), `serviceTier`. Plus `candidate.tokenCount`, `avgLogprobs`, `logprobsResult`, and top-level `modelVersion`, `responseId`, `modelStatus{modelStage: EXPERIMENTAL|PREVIEW|STABLE|LEGACY|RETIRED, retirementTime, message}` ([reference][g-ref]). Long decode via SDK adds client-side `initialPromptTokenCount` (not on the wire) ([long decode][g-ld]).

---

## 8. Errors (GC)

Body ([API errors GC][g-err]):
```json
{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT",
  "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com","metadata":{"service":"generativelanguage.googleapis.com"}},
             {"@type":"type.googleapis.com/google.rpc.LocalizedMessage","locale":"en-US","message":"…"}]}}
```

| HTTP | status | Typical cause | Retry? |
|---|---|---|---|
| 400 | `INVALID_ARGUMENT` | malformed body, unsupported param/level, prefill, missing signature, both budget+level | no |
| 400 | `FAILED_PRECONDITION` | free tier unavailable in region / billing | no |
| 402 | `RESOURCE_EXHAUSTED` | prepay credits depleted | no |
| 403 | `PERMISSION_DENIED` | bad/leaked key, verification, restricted project | no |
| 404 | `NOT_FOUND` | file/model not found | no |
| 429 | `RESOURCE_EXHAUSTED` | RPM/TPM/RPD/spend limits (also Flex congestion) | yes, backoff + jitter |
| 499 | `CANCELLED` | client closed connection | n/a |
| 500 | `INTERNAL` | incl. "input context too long" | yes (limited) |
| 503 | `UNAVAILABLE` | overloaded / Flex capacity | yes |
| 504 | `DEADLINE_EXCEEDED` | too large to finish in time | yes with longer timeout |

Retry guidance: exponential backoff with jitter on 429/408/5xx only; SDKs retry 4× (1 s → 60 s) ([troubleshooting][g-trouble]). `RetryInfo`/`Retry-After` not documented (**UNVERIFIED**). Mid-stream GC error framing is undocumented (**UNVERIFIED**; expect an SSE `data:` carrying `{"error":…}` or a dropped connection). In-band failures (safety/recitation/etc.) arrive as HTTP 200 with `finishReason`/`blockReason` — surface verbatim.

---

## 9. Pricing structure (paid tier, USD per 1M tokens, Standard tier) ([pricing][g-pricing])

| Model | Input | Output (incl. thinking) | Cached input | Notes |
|---|---|---|---|---|
| `gemini-4-carbon`, `gemini-4-argon` | $2.00 (→ $4.00 from 2026-12-01) | $10 (→ $20) | $0.10 (→ $0.20), implicit only | no free tier |
| `gemini-3.8-flash`, `gemini-3.6-flash` | $0.75 (→ $1.50 from 2027-01-01) | $3.75 (→ $7.50) | $0.075 + $0.50/M tok·h storage (explicit) | |
| `gemini-3.5-flash-lite` | $0.30 (all modalities) | $2.50 | $0.03 + $1/M·h | |
| `gemini-3.1-pro-preview` | $2 (≤200K prompt) / $4 (>200K) | $12 / $18 | $0.20 / $0.40 + $4.50/M·h | tiered by prompt size |
| `gemini-3.1-flash-lite` / `gemini-3-flash-preview` | $0.25 / $0.50 (text, image, video); **audio $0.50 / $1.00** | $1.50 / $3.00 | | separate audio rate on older models |
| `gemini-2.5-pro` / `-flash` | $1.25/$2.50 (>200K) / $0.30 (+$1.00 audio) | $10/$15 / $2.50 | | deprecated |
| `gemini-nano-banana-2.1` | $1.50 | $7.50 text/thinking; **$30 image** (≈$0.0336 1K, $0.0504 2K, $0.113 4K) | | |
| `gemini-3.1-flash-image` | $0.50 | $3 text; $60 image ($0.045 512, $0.067 1K, $0.101 2K, $0.151 4K) | | |
| `gemini-3.1-flash-lite-image` | $0.25 | $1.50 text; $30 image ($0.0336 1K) | | |
| `gemini-3-pro-image` | $2.00 | $12 text; $120 image ($0.134 1K/2K, $0.24 4K) | | |
| `gemini-3.8-flash-tts` | $0.50 text (→ $1.00) | $9 audio (→ $18) | | |

Modifiers: **Batch & Flex −50 %**; **Priority ≈ ×1.8** (e.g. 3.8 Flash $1.35/$6.75; Gemini 4 $3.60/$18) ([pricing][g-pricing]). **Implicit caching** automatic ≥4,096-token prompts on 3.6+/4 (2,048 on 2.5); Gemini 4 caches the previous request only if the new one adds ≥2,050 tokens; Gemini 4 cache hits −95 %, expiry 15–20 min ([caching][g-cache], [long decode][g-ld]). **Explicit caching**: default TTL 1 h, storage billed per hour, not on Gemini 4 ([caching][g-cache]).

Tools: Google Search on Gemini 3+ = 5,000 free queries/month (shared across Gemini 3+), then **$14 / 1,000 executed queries**, default cap 1,500 RPD/project; Gemini 2.5 = $35 / 1,000 grounded prompts. Maps on Gemini 3+: same $14/1,000 queries with 5,000 free; 2.5: $25/1,000 prompts. URL context, code execution, computer use: token-priced (code/results billed as output when produced and input when re-read). Gemini 4 bills internal tool system instructions & declarations when grounding ([pricing][g-pricing], [search][g-search], [maps][g-maps], [code exec][g-code]).

Cost estimate (derived, not an official formula): `(promptTokenCount − cachedContentTokenCount + toolUsePromptTokenCount)·in + cachedContentTokenCount·cached + (candidatesTokenCount + thoughtsTokenCount)·out` with IMAGE/AUDIO tokens from `candidatesTokensDetails` priced at their modality rate, plus `#non-empty webSearchQueries · $0.014` (Gemini 3+). Long decode: sum every hop; DOCUMENT tokens bill at the image rate; Gemini 3 PDF native text free ([pricing notes][g-pricing], [documents][g-docs], [long decode][g-ld]).

---

## 10. How OpenRouter exposes Gemini

**Surfaces**: Chat Completions `/api/v1/chat/completions`, Responses `/api/v1/responses` (**stateless only**: `store:true` or `previous_response_id` → 400), Anthropic Messages `/api/v1/messages`, Image API `/api/v1/images`, TTS `/api/v1/audio/speech`, Batch, Files ([Responses overview][or-resp], [image gen][or-img], [TTS][or-tts], [batch][or-batch]). No Interactions-API equivalent (no background, no server state).

**Upstreams (live endpoint list, 2026-11-11)**: Gemini models are served by `google-ai-studio` and `google-vertex` (`/global`, `/us`, `/eu`) plus tier endpoints `…/flex`, `…/priority`; capabilities differ per endpoint (e.g. 3.8 Flash on Vertex lacks `temperature`/`top_p` but has `stop`; AI Studio lacks `stop`). Model-level `supported_parameters` for Gemini 3.8/4: `include_reasoning, max_tokens, reasoning, reasoning_effort, response_format, seed, stop, structured_outputs, temperature, tool_choice, tools, top_p` — no `top_k`, `frequency_penalty`, `presence_penalty`, `logprobs`, `top_logprobs`. `native_tools` = only `{"openrouter:web_search": {"type": "google_search"}}` ([endpoints 3.8 Flash][or-ep-38], [endpoints Argon][or-ep-argon], [models API][or-models]). Pin with `"provider": {"only": ["google-ai-studio"], "allow_fallbacks": false}` for reproducible tests ([provider routing via chat ref][or-chat]).

| Gemini feature | OR status | OR parameter / shape | Caveats & evidence |
|---|---|---|---|
| Thinking level | **Supported** | `reasoning.effort` (or `reasoning_effort`): `minimal→minimal, low→low, medium→medium, high→high, xhigh→high`; unsupported values snap to nearest | [reasoning][or-reason]; per-model `reasoning.supported_efforts`/`default_effort`/`mandatory` in `/api/v1/models` (e.g. 3.8 Flash `[high,medium,low]`, NB 2.1 `[high,minimal,none]` — `medium` missing vs Google's list) ([models API][or-models]) |
| Thinking budget | Partial | `reasoning.max_tokens` → `thinkingBudget`; Gemini 3 maps it to a level; **not supported on Gemini 4**; cannot combine with effort | [reasoning][or-reason], [Gemini 4 guide][or-g4] |
| `includeThoughts` | Partial | reasoning returned by default as `message.reasoning` / `reasoning_details`; `reasoning.exclude:true` (Chat only) hides it — upstream `includeThoughts` value **UNVERIFIED** | [reasoning][or-reason] |
| Thought signatures | Partial / **UNVERIFIED** | `reasoning_details[]` items `{type: reasoning.encrypted\|reasoning.text\|reasoning.summary, format: "google-gemini-v1", data/signature}`; echo the assistant message with `reasoning_details` intact; Responses: echo `reasoning` items (`encrypted_content`) | only documented explicitly for long-decode continuation items and agentic-video navigation; generic Gemini signature mapping undocumented ([reasoning][or-reason], [videos][or-vid], [Gemini 4 guide][or-g4]) |
| Long decode (Gemini 4) | **Supported** | continuation token = `reasoning_details` item `{type:"reasoning.encrypted", id:"gemini-continuation", format:"google-gemini-v1", data}`; `finish_reason:"length"` + `native_finish_reason:"CONTINUATION"\|"MAX_TOKENS"`; resend with stopped turn as last assistant message; OR default `max_tokens` = 65,536 when omitted | streaming checkpoints → resumable after drop; continuation sticks to the issuing provider (no fallback); token can be MBs ([Gemini 4 guide][or-g4]) |
| Sampling params | Partial | `temperature`, `top_p`, `seed`, `stop` (Vertex), no `top_k` | Google ignores sampling on 3.6+/4 anyway ([endpoints][or-ep-38]) |
| Logprobs, penalties, `candidateCount` | **Unsupported** for Gemini endpoints | — | absent from `supported_parameters`; unsupported params are silently dropped unless `provider.require_parameters: true` ([endpoints][or-ep-38], [chat ref][or-chat]) |
| Structured output | **Supported** | `response_format: {type: json_schema \| json_object}`, `structured_outputs` | [chat ref][or-chat] |
| Function calling | **Supported** | OpenAI `tools`/`tool_choice`, `parallel_tool_calls` | `VALIDATED` mode, `allowedFunctionNames` mapping **UNVERIFIED** |
| Google Search grounding | Partial | server tool `{"type":"openrouter:web_search","parameters":{"engine":"auto"\|"native"}}` (plugin `{"id":"web"}` / `:online` deprecated); `user_location` honoured by native; domain filters unsupported natively (auto → falls back to Exa, native → 400) | results normalized to `annotations[{type:"url_citation", url_citation{url,title,content,start_index,end_index}}]`; no documented `searchEntryPoint`, `webSearchQueries`, `confidenceScores`; usage `server_tool_use_details.web_search_requests`; native list in docs names only Gemini 3/3.1/3.5 Flash/Pro, while live endpoints report native search for 3.8/4 too ([web search][or-ws], [web plugin][or-wp], [endpoints][or-ep-38]) |
| Image search grounding | **UNVERIFIED** | — | not documented |
| URL context | **Unsupported natively** | `openrouter:web_fetch` (engines exa/openrouter/firecrawl/parallel/native) is a different, OR-run fetcher | Gemini endpoints list no native `web_fetch` ([web fetch][or-wf], [endpoints][or-ep-38]) |
| Code execution | **Unsupported** | OR `openrouter:shell` / `openrouter:bash` are OR sandboxes, not Gemini's tool | [server tools][or-st] |
| Google Maps, File Search, Computer Use, MCP (Google-side) | **Unsupported** | — | no mapping in docs / `native_tools` ([server tools][or-st], [endpoints][or-ep-38]) |
| Safety settings | **Unsupported / UNVERIFIED** | no documented field; `provider.options[slug]` forwards only allow-listed keys ("Unrecognized keys are silently dropped") and no allowlist is published for Gemini chat | Google default is OFF anyway. OR maps Gemini `SAFETY` blocks to `error_type: content_policy_violation` (403) with `metadata.raw` = provider payload ([chat ref][or-chat], [errors][or-errors]) |
| `mediaResolution` (global / per-part) | **Unsupported / UNVERIFIED** | image part `detail: auto\|low\|high\|original` exists, mapping to Gemini undocumented | [chat ref][or-chat], [images][or-imgin] |
| Images in | **Supported** | `image_url` (URL or data URL) / Responses `input_image`; png/jpeg/webp/gif | Vertex image URLs: HTTPS + known extension ([images][or-imgin], [batch quickstart][or-bq]) |
| PDF / documents | **Supported** | `{"type":"file","file":{"filename","file_data": data-URL\|URL}}`; plugin `file-parser` `pdf.engine: native\|cloudflare-ai\|mistral-ocr` (default native for Gemini) | Office docs through `file` parts **UNVERIFIED**; OR Files API accepts docx/xlsx/pptx (`file_id`) ([PDFs][or-pdf], [files][or-files]) |
| Audio in | **Supported** | `{"type":"input_audio","input_audio":{"data":<b64>,"format":"mp3"\|"wav"\|"ogg"\|…}}`; base64 only (no URLs); Responses only `wav`/`mp3` | [audio][or-audio] |
| Video in | **Supported** | `{"type":"video_url","video_url":{"url": data-URL or YouTube, "processing":"agentic"\|"static"}}` | YouTube **only on AI Studio** (Vertex: base64 only); formats mp4/mpeg/mov/webm; no `videoMetadata` (fps/clipping) ([videos][or-vid], [multimodal][or-mm]) |
| Image generation | **Supported** | Chat: `modalities:["image","text"]`, `image_config` (provider-specific map; Gemini keys e.g. `aspect_ratio`/`image_size` **UNVERIFIED** in current docs), images in `message.images[].image_url.url` (data URL). Image API: `resolution` (`1K/2K/4K`, `512` for 3.1 Flash Image), `aspect_ratio`, `n=1`, `input_references` ≤14, passthrough `cachedContent` only, no streaming | Vertex `gemini-3-pro-image` max 2K; image billing all-or-nothing; multi-turn editing with signatures **UNVERIFIED** ([image gen][or-img], [chat ref][or-chat], [image endpoints NB 2.1][or-iep-nb], [image endpoints 3 Pro][or-iep-pro]) |
| TTS | **Supported** | `/api/v1/audio/speech`, `voice`, `instructions` → `speech_metadata.style`, multi-speaker `input[]` (≤2 voices), `response_format: "pcm"` only (mp3 → 400); `provider.options["google-ai-studio"]` forwarded into generationConfig (custom `speechConfig` replaces OR's) | [TTS][or-tts] |
| Caching | **Supported** | implicit automatic; explicit via `cache_control` breakpoints (only the last one used; system prompt immutable when cached); cache write = input price + 5 min storage, TTL 5 min fixed | OR doc text "cached tokens 0.25×" is stale vs live prices (3.8 Flash read $0.075 = 0.1×) ([caching][or-cache], [endpoints][or-ep-38]); `cache_control` skipped on continuation hops ([Gemini 4 guide][or-g4]) |
| Service tiers | **Supported** | `service_tier: "flex"\|"priority"\|"default"` (or slugs `google-ai-studio/flex`, `google-vertex/global/priority`); `:floor` / `:nitro` variants | flex fallback bounded by price; response reports served tier ([tiers][or-tiers]) |
| Batch | Supported (OR's own) | `…:batch` model variants at ~50 % | YouTube batch only on AI Studio ([models API][or-models], [batch quickstart][or-bq]) |
| `cachedContent`, `store`, `labels`, `enableEnhancedCivicAnswers`, `continuationToken` (raw) | **Unsupported** (except continuation via reasoning items) | — | not in schema |
| Usage & cost | **Supported** | `usage{prompt_tokens, completion_tokens, prompt_tokens_details{cached_tokens, cache_write_tokens, audio_tokens, video_tokens}, completion_tokens_details{reasoning_tokens, audio_tokens}, cost, cost_details{upstream_inference_*}, server_tool_use_details}`; streams end with a usage chunk | per-modality image-output tokens not separated (**UNVERIFIED**); prices incl. `>200K` overrides via `pricing.overrides[{min_prompt_tokens}]` ([chat ref][or-chat], [streaming][or-stream], [endpoints 3.1 Pro][or-ep-pro]) |
| Finish reasons | Partial | normalized `stop\|length\|tool_calls\|content_filter\|error` + raw `native_finish_reason` | [overview][or-overview] |
| Errors | Partial | `{error:{code, message, metadata{error_type, provider_code, raw?}}}`; mid-stream: 200 + chunk with top-level `error` and `finish_reason:"error"`; 500s masked | upstream text not always verbatim ([errors][or-errors], [streaming][or-stream]) |
| Stream cancellation | **Unsupported for Google** | aborting the connection does not stop generation/billing on "Google, Google AI Studio" | [streaming][or-stream] |
| Disconnect recovery | Partial | only Gemini 4 long-decode checkpoints; otherwise none. Possible fallback: `GET /api/v1/generation/content?id=<X-Generation-Id>` returns stored prompt/completion if Input & Output Logging is enabled (≥3 months retention) — usefulness for in-flight recovery **UNVERIFIED** | [generation content][or-gen], [I/O logging][or-iolog], [streaming][or-stream] |
| Transparency | **Supported (debug)** | `"debug": {"echo_upstream_body": true}` returns the exact upstream body (streaming only, not for prod) | [errors & debugging][or-errors] |

---

## 11. OpenRouter gaps for Google Gemini

- **No Interactions API**: no `background`, no server-side state, no `last_event_id` stream resumption, no cancel endpoint; Responses API is stateless-only ([OR Responses][or-resp], [Google background][g-bg]).
- **Cancellation doesn't stop billing**: Google/AI Studio are listed under "Not Currently Supported" for stream cancellation ([OR streaming][or-stream]).
- **Only Google Search is native**: endpoint `native_tools` show just `google_search`; no URL context, code execution, Google Maps, File Search, Computer Use ([endpoints][or-ep-38], [server tools][or-st]).
- **Grounding metadata flattened**: only `url_citation` annotations; `searchEntryPoint.renderedContent` (required Search Suggestions), `webSearchQueries`, `groundingSupports.confidenceScores`, image-search chunks, `urlContextMetadata` not documented as passed through — makes Google's display requirements hard to meet ([OR web plugin][or-wp], [Google terms][g-terms]).
- **Domain filters break native search**: with Google, `allowed_domains`/`excluded_domains` force Exa (`auto`) or 400 (`native`) ([OR web search][or-ws]).
- **Missing generation knobs**: no documented `safetySettings`, global/per-part `mediaResolution`, `videoMetadata` (fps/offsets), `topK`, `presencePenalty`/`frequencyPenalty`, `responseLogprobs`/`logprobs`, `candidateCount`, `enableEnhancedCivicAnswers`, `toolConfig.retrievalConfig`, `includeServerSideToolInvocations`, `serviceTier` echo semantics, `store`, `labels`, `cachedContent` (chat) ([OR chat ref][or-chat], [endpoints][or-ep-38]).
- **Thought-signature handling undocumented** beyond continuation/agentic-video items; whether every Gemini signature (text parts, image parts, function calls) round-trips via `reasoning_details` is **UNVERIFIED** — test with `debug.echo_upstream_body` ([OR reasoning][or-reason], [OR errors & debugging][or-errors]).
- **`thinkingBudget` precision lost** on Gemini 3 (mapped to level) and unsupported on Gemini 4; `xhigh` silently maps to `high` (`max`/`none` handling for Gemini undocumented); effort lists differ from Google's (e.g. NB 2.1) ([OR reasoning][or-reason], [models API][or-models]).
- **Hidden defaults**: OR sends `max_tokens = 65,536` for Gemini 4 Argon when omitted; reasoning is requested/returned by default (exact upstream `includeThoughts` **UNVERIFIED**) ([OR Gemini 4][or-g4], [OR reasoning][or-reason]).
- **Provider inconsistency**: AI Studio vs Vertex differ (YouTube only AI Studio, `stop` only Vertex, Vertex 3 Pro Image ≤2K, Vertex image-URL restrictions); fallbacks can change behaviour mid-conversation, and continuation tokens are provider-bound ([OR videos][or-vid], [endpoints][or-ep-38], [image endpoints][or-iep-pro], [OR Gemini 4][or-g4]).
- **Media constraints**: audio base64 only; Responses API audio only wav/mp3; video formats limited to mp4/mpeg/mov/webm ([OR audio][or-audio], [OR videos][or-vid]).
- **Image generation config** in chat is an untyped provider-specific `image_config` map (current docs show no Gemini keys); Image API has no streaming and `n` max 1; multi-turn image editing with signatures undocumented ([OR chat ref][or-chat], [OR image gen][or-img], [image endpoints][or-iep-nb]).
- **TTS** limited to `pcm` output and ≤2 speakers; other Gemini TTS controls only through `provider.options` passthrough, and whether a passthrough `responseFormat` (WAV/MP3/Opus) is honoured is **UNVERIFIED** ([OR TTS][or-tts]).
- **Error fidelity**: 500-class upstream messages masked; Gemini `SAFETY` blocks become 403 `content_policy_violation` instead of Google's 200 + `finishReason` (raw payload in `metadata.raw`) ([OR errors][or-errors]).
- **Docs drift**: OR caching page still quotes 0.25× cached price and 2.5-era minimums; native-search model list omits 3.8/4 although live endpoints have it ([OR caching][or-cache], [OR web search][or-ws], [endpoints][or-ep-38]).

---

## Sources

The lines below are Markdown link-reference definitions: in rendered Markdown every inline `[label][id]` above is a clickable link to these URLs (view the raw file to read them as a list).

Google (fetched 2026-11-11; `.md` twins used):

[g-llms]: https://aistudio.google.com/llms.txt
[g-ia]: https://aistudio.google.com/docs/interactions-overview
[g-changelog]: https://aistudio.google.com/docs/changelog
[g-deprec]: https://aistudio.google.com/docs/deprecations
[g-models]: https://aistudio.google.com/docs/models
[g-ref]: https://aistudio.google.com/api-reference/generate-content
[g-ref-req]: https://aistudio.google.com/api-reference/generate-content/generate-content
[g-ref-stream]: https://aistudio.google.com/api-reference/generate-content/stream-generate-content
[g-text]: https://aistudio.google.com/docs/text-generation?generate-content=true
[g-think]: https://aistudio.google.com/docs/thinking?generate-content=true
[g-think-ia]: https://aistudio.google.com/docs/thinking
[g-sig]: https://aistudio.google.com/docs/thought-signatures?generate-content=true
[g-sig-ia]: https://aistudio.google.com/docs/thought-signatures
[g-mr]: https://aistudio.google.com/docs/media-resolution?generate-content=true
[g-img]: https://aistudio.google.com/docs/image-generation?generate-content=true
[g-safety]: https://aistudio.google.com/docs/safety-settings?generate-content=true
[g-safety-ia]: https://aistudio.google.com/docs/safety-settings
[g-search]: https://aistudio.google.com/docs/google-search?generate-content=true
[g-url]: https://aistudio.google.com/docs/url-context?generate-content=true
[g-code]: https://aistudio.google.com/docs/code-execution?generate-content=true
[g-code-ia]: https://aistudio.google.com/docs/code-execution
[g-maps]: https://aistudio.google.com/docs/maps-grounding?generate-content=true
[g-fsearch]: https://aistudio.google.com/docs/file-search?generate-content=true
[g-cu]: https://aistudio.google.com/docs/computer-use?generate-content=true
[g-combo]: https://aistudio.google.com/docs/tool-combination?generate-content=true
[g-fc]: https://aistudio.google.com/docs/function-calling?generate-content=true
[g-struct]: https://aistudio.google.com/docs/structured-output?generate-content=true
[g-cache]: https://aistudio.google.com/docs/caching?generate-content=true
[g-flex]: https://aistudio.google.com/docs/flex-inference?generate-content=true
[g-prio]: https://aistudio.google.com/docs/priority-inference?generate-content=true
[g-ld]: https://aistudio.google.com/docs/long-decode?generate-content=true
[g-ld-ia]: https://aistudio.google.com/docs/long-decode
[g-stream-ia]: https://aistudio.google.com/docs/streaming
[g-bg]: https://aistudio.google.com/docs/background-execution
[g-dr]: https://aistudio.google.com/docs/deep-research
[g-ia-bc]: https://aistudio.google.com/docs/interactions-breaking-changes-may-2026
[g-ia-text]: https://aistudio.google.com/docs/text-generation#stateless-conversations
[g-ref-ia]: https://aistudio.google.com/api-reference/interactions-api
[g-ref-iac]: https://aistudio.google.com/api-reference/interactions-api/create-interaction
[g-ref-iag]: https://aistudio.google.com/api-reference/interactions-api/get-interaction-by-id
[g-err]: https://aistudio.google.com/docs/api-errors?generate-content=true
[g-err-ia]: https://aistudio.google.com/docs/api-errors
[g-trouble]: https://aistudio.google.com/docs/troubleshooting
[g-batch]: https://aistudio.google.com/docs/batch-api
[g-wh]: https://aistudio.google.com/docs/webhooks
[g-fin]: https://aistudio.google.com/docs/file-input-methods?generate-content=true
[g-files]: https://aistudio.google.com/docs/files?generate-content=true
[g-ref-files]: https://aistudio.google.com/api-reference/files/create-file
[g-docs]: https://aistudio.google.com/docs/document-processing?generate-content=true
[g-imgu]: https://aistudio.google.com/docs/image-understanding?generate-content=true
[g-video]: https://aistudio.google.com/docs/video-understanding?generate-content=true
[g-audio]: https://aistudio.google.com/docs/audio?generate-content=true
[g-tts]: https://aistudio.google.com/docs/speech-generation?generate-content=true
[g-pricing]: https://aistudio.google.com/docs/pricing
[g-terms]: https://aistudio.google.com/docs/terms#grounding-with-google-search
[g-apiver]: https://aistudio.google.com/api-reference/api-versions
[g-carbon]: https://aistudio.google.com/docs/whats-new-gemini-4-carbon?generate-content=true
[g-argon]: https://aistudio.google.com/docs/whats-new-gemini-4-argon?generate-content=true
[g-m-argon]: https://aistudio.google.com/docs/models/gemini-4-argon
[g-38]: https://aistudio.google.com/docs/whats-new-gemini-3.8?generate-content=true
[g-36]: https://aistudio.google.com/docs/whats-new-gemini-3.6?generate-content=true
[g-35]: https://aistudio.google.com/docs/whats-new-gemini-3.5?generate-content=true
[g-m-flimg]: https://aistudio.google.com/docs/models/gemini-3.1-flash-lite-image
[pb-json]: https://protobuf.dev/programming-guides/json/
[tg-voice]: https://core.telegram.org/bots/api#sendvoice
[tg-audio]: https://core.telegram.org/bots/api#sendaudio

OpenRouter (docs `.md` twins and public JSON APIs, fetched 2026-11-11):

[or-reason]: https://openrouter.ai/docs/guides/best-practices/reasoning-tokens
[or-g4]: https://openrouter.ai/docs/cookbook/evaluate-and-optimize/model-migrations/gemini-4
[or-chat]: https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion
[or-resp]: https://openrouter.ai/docs/api_reference/responses/overview
[or-overview]: https://openrouter.ai/docs/api_reference/overview
[or-stream]: https://openrouter.ai/docs/api_reference/streaming
[or-errors]: https://openrouter.ai/docs/api_reference/errors-and-debugging
[or-ws]: https://openrouter.ai/docs/guides/features/server-tools/web-search
[or-wf]: https://openrouter.ai/docs/guides/features/server-tools/web-fetch
[or-st]: https://openrouter.ai/docs/guides/features/server-tools
[or-wp]: https://openrouter.ai/docs/guides/features/plugins/web-search
[or-img]: https://openrouter.ai/docs/guides/overview/multimodal/image-generation
[or-mm]: https://openrouter.ai/docs/guides/overview/multimodal/overview
[or-imgin]: https://openrouter.ai/docs/guides/overview/multimodal/image-understanding
[or-pdf]: https://openrouter.ai/docs/guides/overview/multimodal/pdfs
[or-audio]: https://openrouter.ai/docs/guides/overview/multimodal/audio
[or-vid]: https://openrouter.ai/docs/guides/overview/multimodal/videos
[or-tts]: https://openrouter.ai/docs/guides/overview/multimodal/tts
[or-cache]: https://openrouter.ai/docs/guides/best-practices/prompt-caching
[or-tiers]: https://openrouter.ai/docs/guides/features/service-tiers
[or-files]: https://openrouter.ai/docs/guides/features/files-api
[or-batch]: https://openrouter.ai/docs/guides/features/batch-api
[or-bq]: https://openrouter.ai/docs/batch-quickstart
[or-iolog]: https://openrouter.ai/docs/guides/features/input-output-logging
[or-gen]: https://openrouter.ai/docs/api/api-reference/generations/get-stored-prompt-completion-and-error-content-for-a-generation
[or-models]: https://openrouter.ai/api/v1/models
[or-ep-38]: https://openrouter.ai/api/v1/models/google/gemini-3.8-flash/endpoints
[or-ep-argon]: https://openrouter.ai/api/v1/models/google/gemini-4-argon/endpoints
[or-ep-pro]: https://openrouter.ai/api/v1/models/google/gemini-3.1-pro-preview/endpoints
[or-iep-nb]: https://openrouter.ai/api/v1/images/models/google/gemini-nano-banana-2.1/endpoints
[or-iep-pro]: https://openrouter.ai/api/v1/images/models/google/gemini-3-pro-image/endpoints
