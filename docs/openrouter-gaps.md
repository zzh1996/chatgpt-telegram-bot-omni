# OpenRouter 相对官方 API 的功能缺口

## 1. 说明

**缺口**：厂商官方 API 能做到、但经 OpenRouter（下称 OR）无论怎样配置请求和账户都做不到的事。以下都**不算**缺口，见 [openrouter-fidelity.md](openrouter-fidelity.md)：

- 格式、参数名、默认值不同，字段被 OR 归一化，以及路由行为；
- 用配置就能避开或做到的：固定 provider（`provider.only`）、`allow_fallbacks:false`、`require_parameters:true`、关闭 plugin、显式 `max_tokens`、改用 OR 的 Responses（RSP）或 Messages（MSG）端点、`provider.options` 透传、BYOK、开启 Input & Output Logging 后查 `/generation/content`、读 `native_finish_reason` 等；
- 只有 BYOK 才能用的功能；
- 官方本身也不支持或会忽略的参数，以及客户端能复现的用法（例如用多次请求代替 `n`）。

表中的缺口都有正面证据：需要服务端状态或 OR 没有的路由，或者 OR 的文档、schema、catalog 写明了替换、拒绝或缺失（例如同名工具被换成 OR 自己的，catalog 里没有该模型、模态或 endpoint）。

**疑似缺口（待实测）**：成立与否取决于 OR 没有文档、也还没测过的行为，主要有三类：

1. 厂商私有字段能否原样到达上游。顶层字段只能靠 `provider.options.<slug>`：这是 OR 文档化的透传口，但白名单未公开，未识别的 key 会被静默丢弃。content part 里的字段则要看 OR 是否转发。
2. OR 是否把自己的字段映射成上游需要的字段，例如 `reasoning_details` → `reasoning_content`、prefill → `partial:true`。
3. 厂商特有的响应字段是否保留。

每条都附了能定案的测试。测明可行就删掉该条，测明不行就移进表里。

**通用测法**：bot 默认给每个请求加 `debug.echo_upstream_body`（只在流式 CC/RSP 上有），回复 + `/info` 能看到 OR 实际发往上游的 body，下文简称 echo。bot 也总是发送 `X-OpenRouter-Metadata: enabled`，事后还可以查 `GET /api/v1/generation?id=`。

**对 bot 用户影响最大的几条**：断线后只能重新生成（Gemini 4 long decode 除外）；OpenAI、Claude 的请求要先过 OR 的审核；经 OR 的 DeepSeek、智谱、Kimi、Qwen、豆包都没有官方联网搜索（`+s` 会直接报错）；各家的 Files API、托管 agent 和大部分内置工具都不可达。

调研日期 2026-11-11。"实测 2026-11-11"指维护者当天用真实请求测得的结果，与文档冲突时以它为准。

| 缩写 | 文件 |
|---|---|
| OR | [research/openrouter.md](research/openrouter.md) |
| OA | [research/openai.md](research/openai.md) |
| AN | [research/anthropic.md](research/anthropic.md) |
| GE | [research/gemini.md](research/gemini.md) |
| XD | [research/xai_deepseek.md](research/xai_deepseek.md)（Part A/B 为官方，C/D 为 OR） |
| GKQ | [research/glm_kimi_qwen.md](research/glm_kimi_qwen.md)（§1–3 为官方，§4–7 为 OR） |
| DB | [research/doubao.md](research/doubao.md)（火山方舟、BytePlus 与 OR） |

`OR§6` 表示 research/openrouter.md 第 6 节，其它缩写同理；原始 URL 在各文件末尾。

## 2. 通用缺口

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| 服务端会话状态 | 由厂商保存对话，按 id 接续或取回：OpenAI 的 `store` + `previous_response_id`、Conversations、`GET /responses/{id}`、WebSocket 模式；Anthropic Message Threads；Gemini Interactions 的 `previous_interaction_id`；xAI stored responses（30 天）与 WebSocket 模式；智谱、Qwen、豆包 Responses 的 `store`/`previous_response_id`/GET（Qwen 另有 session cache，豆包另有依赖它的显式上下文缓存 `caching`） | OR 的三个端点都是无状态 `POST`：RSP 的 `store` 恒为 `false`，`store:true` 或非空 `previous_response_id` → 400；没有 GET、DELETE、`input_items` 路由，也没有 WebSocket | OR§1, OR§9, OA§2.3, AN§9, GE§2.6, XD§A1, GKQ§7, DB§9.1 |
| 后台执行、断线续流、异步取结果 | OpenAI 的 `background` + 轮询、`cancel`、webhooks，流式时用 `starting_after` 续流；Gemini Interactions 的 `background` + `last_event_id` 重放（完成后也能重放）与 `cancel`；xAI deferred chat completions（24 h 内取一次）；智谱国内站的异步 chat + 轮询 | 同上。RSP schema 里虽有 `background` 字段，但没有取回、取消或续流的路由，事件也不能重放。唯一例外是 Gemini 4 long decode：经 OR CC 能跨 hop 续写，断流后也能从 checkpoint 接着写（实测 2026-11-11）。OR 的 Batch API 是另一回事（无流式，≤24 h） | OR§9, OA§5, GE§2.4–2.5, XD§A10, GKQ§1.8, 实测 2026-11-11 |
| 服务端上下文压缩 | OpenAI 的 `context_management` 自动压缩、`POST /v1/responses/compact`；xAI 的 `POST /v1/responses/compact` | OR schema 没有 `context_management`，也没有 compact 路由；OR 的 `context-compression` plugin 是从中间删消息，不是同一功能（OpenAI 显式的 `compaction_trigger` 见 §3 疑似） | OA§2.4, OA§10.1, XD§A1, XD§D1 |
| OR 的输入审核 | 官方没有这一层，内容只受厂商自己的政策约束 | `top_provider.is_moderated:true` 的 endpoint（几乎所有 OpenAI、Anthropic 模型，以及 Amazon、Cohere、Meta）先过 OR 的审核，命中返回 403（`reasons`、`flagged_input`）；没有文档化的关闭方式（BYOK 是否例外也没写），所用分类器未公开 | OR§7, OR§11.2 #10 |
| 厂商 Files API 与大文件 | 上传到厂商存储后按 id/URI 引用，从而发送超过内联上限的媒体：Gemini Files API（2 GB/文件，保留 48 h；内联 ≤100 MB）、方舟 Files API（≤512 MB，视频经自有 TOS ≤2 GB；内联图片 ≤10 MB、视频/PDF ≤50 MB）、Kimi `ms://` 与 `purpose:"video"`、Qwen-Long `fileid://`、智谱 `file_id`、DeepSeek 图片 `file_id`（≤64 MiB，内联 ≤32 MiB） | OR 的 Files API 只能存到 OR 自己、`openai` 或 `anthropic`（后两者需 BYOK，所以 OpenAI、Anthropic 不算缺口）；`or_file_…` 是 OR 自己的 id。其它厂商只能收到内联或 URL 输入 | OR§4.2, GE§5, GKQ§1.4, GKQ§2.5, GKQ§3.4, XD§B6, DB§9.1 G8 |
| 厂商的非推理接口 | token 计数（Anthropic `count_tokens`、OpenAI `/responses/input_tokens`、方舟 `tokenization`、Moonshot `estimate-token-count`）；Moonshot `file-extract`（文档转 Markdown）；独立的搜索、抓取、阅读接口（DeepSeek `/v1/tools/search` 与 `/fetch`、Moonshot `/v1/tools/*` 与 Formula 工具、智谱 Web Search / Reader） | OR 只代理推理请求（外加它自己的 images、audio、videos、batches、files 接口），没有这些路由 | OA§10.0, AN§10.2, XD§B1, GKQ§1.1, GKQ§2.1, DB§2 |

**疑似缺口（待实测）**

- **推理内容回传给上游**：bot 对同一模型原样回传 `reasoning_details`，但 OR 是否把它转成各家要求的字段没有文档。各家的要求：
  - DeepSeek：思考 + 工具时，之前所有轮的 `reasoning_content` 都要回传，缺了就 400；
  - Kimi K3：多轮与工具循环都必须回传 `reasoning_content`；
  - Qwen3.8：默认 `preserve_thinking:true`，历史推理要放在 `reasoning_content` 字段，缺了不报错、只降质；
  - 智谱：回传 `reasoning_content`，并且要 `clear_thinking:false`（见 §8）；
  - 豆包 2.1 Turbo、2.0-lite-260428：回传 `encrypted_content`；OR `reasoning_details` 的 `format` 枚举里没有字节系的值；
  - Gemini：文本、图片 part 上的 `thoughtSignature`，以及带签名的多轮图片编辑（函数调用 part 的签名已有文档）。

  测：两轮对话（DeepSeek、豆包用一个工具回合），看第二轮 echo 里的 assistant 消息。（OR§3.3, XD§C2, GKQ§4.6, GE§3.1, DB§9.2 P1）
- **prefill 映射到厂商的续写模式**：OR 把末尾的 assistant 消息当作 prefill，但没写是否映射成以下几种：DeepSeek 的 `prefix:true`（官方要走 beta base URL，思考模式可带 `reasoning_content` 前缀）；Kimi、Qwen 的 `partial:true`（Kimi 可带 `reasoning_content`，Qwen 只限非思考模型）；Seed 的 prefill 续写（方舟支持）。测：末尾放 assistant 消息，看 echo 和输出是否接着写。（OR§2.1, XD§D2, GKQ§6, GKQ§7, DB§9.2 P9）
- **OR 用哪种上游接口**：实测 OR 把 OpenAI、xAI 的 CC 请求以 Responses 发往上游；DeepSeek、智谱、Kimi、Qwen、Seed 没测（research 认为 OR 自己翻译到 CC）。若 OR 的 RSP 请求以厂商 Responses 发出，并像对 OpenAI 那样原样透传托管工具，以下可能可行：
  - DeepSeek：`json_schema` 输出（官方 CC 只有 `json_object`，OR 的 DeepSeek endpoint 也只列它），以及对话中途插入工具调用；
  - Qwen Responses：`code_interpreter`、`file_search`、`mcp`、`input_file`（PDF）；
  - BytePlus Responses：MCP 工具。

  测：发 OR RSP 请求，看 echo 是 `input`（Responses）还是 `messages`（CC），再逐项试。（实测 2026-11-11, XD§B1, XD§D2, GKQ§3.3, GKQ§4.1, DB§5）
- **关闭 `file-parser` 后的文件**：对 OR 上不列 `file` 输入的模型，发 `{"id":"file-parser","enabled":false}` 时 OR 怎样处理 `file` part，没有文档。若原样交给上游，智谱的服务端文件解析就能用（`file` part 带 `file_id`/`file_url`/`file_data`，支持 pdf、word、xlsx、pptx）。bot 目前对这类模型直接拒绝发送文件。测：关闭 file-parser 后发 PDF，看 echo。（OR§4.3, GKQ§1.4, GKQ§4.9）

## 3. OpenAI

第一方 `openai`（另有 tier endpoint `openai/flex`、`openai/fast`、`openai/ultrafast`）。OR 把 CC 请求以 Responses 调用发往上游（实测 2026-11-11）。除 §2 外：

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| `web_search` 的部分参数 | `filters.blocked_domains`、`external_web_access`（只用缓存/离线）、`return_token_budget`、图片搜索（`search_content_types` + `image_settings`） | 任何 `web_search`/`web_search_preview` 工具都会被提升为 `openrouter:web_search`，只带 OR 自己的参数：后三者没有，`excluded_domains` 对 OpenAI 原生搜索被静默忽略 | OA§3.1, OA§10.2, OR§5.1–5.2 |

**疑似缺口（待实测）**

- **完整来源列表与图片结果**：官方用 `include` 的 `web_search_call.action.sources`、`web_search_call.results` 取回。OR RSP 的 `include` 枚举没有这两项；搜索又由 OR 的 server tool 发起，结果统一为 `url_citation`。测：RSP 带上这两个 include。（OA§10.1, OA§11, OR§5.4）
- **CC 独有参数与 logprobs**：官方 CC 有 `seed`、`stop`、`logit_bias`、penalty、`prediction`（Responses 都没有），effort 为 `none` 时还能取 logprobs。OR 把 CC 请求转成上游 Responses，而 OR RSP 的 `include` 也没有 `message.output_text.logprobs`。这些参数是被丢弃、被拒，还是会让 OR 改走上游 CC（音频输出只在 CC 上有，说明 OR 有时会走 CC），未知。测：CC 带 `seed`、`stop`、`logprobs`（如 `gpt-6.1-terra` + `e=none`），看 echo 和响应。（OA§1.2–1.3, OA§9, 实测 2026-11-11）
- **托管工具（经 RSP 原样透传）**：
  - `code_interpreter`、hosted `shell` 生成的文件，官方可经 `container_file_citation` 下载；OR 的 annotation schema 没有这个类型，OR 的 `/containers` 只服务 OR 自己的沙箱，BYOK 也没有经 OR 下载的途径；
  - `image_generation` 工具的 schema 缺 `action`、`quality` 的 `xhigh`/`max`、gif/svg、`complexity` 和多帧参数，GA 的 `computer` 工具类型也不在 schema 里；
  - `mcp` 由 OpenAI 还是 OR 执行，没写；
  - 顶层 `max_tool_calls` 已被 OR 当作 server-tool 的步数预算，是否还转发给 OpenAI，未知。

  测：RSP 固定 `openai` 逐项发送，看 echo 和输出项。（OA§10.2, OA§11）
- **显式 compaction**：`compaction_trigger` 输入项和 `compaction` 项在 OR schema 里，但端到端未测；若可用，可部分替代 §2 的自动压缩。测：RSP 的输入以 `compaction_trigger` 结尾。（OA§2.4, OA§10.1）
- **私有字段**：`moderation`（请求内的审核打分或拦截）、`image.context_compression`、`prompt_cache_options.prewarm`；原生搜索的 `search_context_size`（OR 文档一处说生效、一处说被忽略）；part 级的 PDF `input_file.detail`（页面图精度）。测：放在原位或 `provider.options.openai`，看 echo。（OA§1.2, OA§10.1, OR§5.1）
- **非 PDF 文件**：官方 RSP 的 `input_file` 能读 docx/xlsx/pptx、代码等约 100 种 MIME；经 OR 是原样交给 OpenAI，还是被 OR 解析或拒绝，未知。测：RSP 发一个 docx，看 echo 以及是否出现 `file` annotation。（OA§6.2, OA§10.1）

## 4. Anthropic

第一方 `anthropic`、`anthropic/fast`。原生 block 在 MSG 上保留（bot 目前走 CC，见 fidelity 文档）。除 §2 外：

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| 服务端代码执行、MCP connector、computer use | `code_execution_*` 工具（容器、`container` 复用、`container_upload`、下载生成的文件）；`mcp_servers`；computer use 客户端工具 | MSG schema 没有 `code_execution_*` 工具类型、`container` 和 `mcp_servers`；代码执行的结果 block 只出现在响应里，不能作为输入回放；computer use 在校验时被拒；`openrouter:bash`/`shell` 是 OR 自己的沙箱 | AN§3, AN§10.2, AN§11 |
| 服务端拒答回退 | `fallbacks`（beta）：拒答时在同一个响应里改由备选模型继续，留下 `fallback` block，并配合 `fallback_credit_token` | MSG 的同名字段是 OR 自己的多模型路由（≤3 个，只有 `model`），整个请求换模型重来；没有 `fallback_credit_token` | AN§1.8, AN§10.2, OR§2.6 |
| `web_fetch` 的参数 | `citations`、`use_cache`、`url_sources` | 只能经 `openrouter:web_fetch` + `engine:"native"`（映射为 `web_fetch_20260209`），这些参数都不暴露 | AN§3.2, AN§10.2 |

**疑似缺口（待实测）**

- **web 工具结果的多轮回放**：`web_fetch_tool_result` 和代码执行的结果 block 都不是 MSG 的合法输入。原生抓取的结果，以及 `_20260209` 版 web 工具自动做动态过滤（即代码执行）的结果，在下一轮是被剥离、被拒，还是在 CC 上经 `reasoning.server_tool_call` 带回，未知。测：`+s` 两轮对话，看第二轮 echo 或报错。（AN§3, AN§10.2, OR§3.2）
- **私有字段**：`inference_geo:"us"`（只在美国推理）、`diagnostics`（缓存未命中原因）、按需 `compaction` 参数都不在 schema 里；OR 的 `us.` 区域主机是否对应 `inference_geo`，也没写。测：CC 带 `provider.options.anthropic`，看 echo。（AN§1.3, AN§10.2）

## 5. Google Gemini

第一方 `google-ai-studio`（bot 默认）和 `google-vertex`。上游是 generateContent 形式：echo 里能看到 `safetySettings`、`includeThoughts`（实测 2026-11-11）。除 §2（Interactions API 的状态、background、重放）外：

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| 托管 agent | Interactions API 的 `agent`（Deep Research 等） | OR 没有 Interactions 或 agent 接口 | GE§2.2, GE§10 |
| 其它内置工具 | URL context、代码执行、Google Maps grounding、File Search、Computer Use、MCP，以及与函数工具组合（`includeServerSideToolInvocations`） | endpoint 的 `native_tools` 只有 `google_search`；`openrouter:web_fetch`、`shell` 是 OR 自己执行的 | GE§1.8, GE§10, GE§11 |
| Google Search 的时间范围与图片搜索 | `googleSearch.timeRangeFilter`、`searchTypes.imageSearch` | 只能经 `openrouter:web_search`（原生映射为 `google_search`），它的参数里没有这两项 | GE§1.8, GE§10, OR§5.1 |
| 显式缓存 `cachedContents` | 创建命名缓存、自定义 TTL（默认 1 h）、跨请求复用（Gemini 4 本身不支持显式缓存） | 只有 `cache_control` 断点：只用最后一个，第一条 system 不可变，TTL 固定 5 分钟；没有 `cachedContents` API | GE§1.2, GE§9, GE§10, OR§10 |

**疑似缺口（待实测）**

- **grounding 元数据**：`searchEntryPoint.renderedContent`（即 Search Suggestions，Google 要求展示）、`webSearchQueries`、`groundingSupports`（含 `confidenceScores` 和按 part 计的字节 offset）、`urlContextMetadata`。OR 把引用统一成 `url_citation`，其余字段没有文档说保留。测：`+s` 后看原始响应（CC、RSP、MSG 各一次）。（GE§4, GE§11, OR§5.4）
- **私有字段**：
  - 实测 OR 把所有类别的 `safetySettings` 设为 `OFF`（与 Google 默认相同），能否改用更严格的阈值，未知；
  - 全局 `mediaResolution`、`enableEnhancedCivicAnswers`；
  - 3.8/4 的 endpoint 不列 `top_k`、penalty、logprobs。Google 在 3.6+ 上本来就忽略 `topK`；logprobs 和 penalty 在新模型上是否生效，官方也没写。

  测：`provider.options["google-ai-studio"]`，看 echo。（GE§1.4, GE§1.7, GE§10, 实测 2026-11-11）
- **媒体 part 字段**：`videoMetadata`（`fps`、起止 offset）、逐 part 的 `mediaResolution`（含 `ULTRA_HIGH`）。OR 的 `video_url` 只有 `url` 和 `processing`；图片的 `detail` 如何映射到 `mediaResolution`，没写。测：发 `detail:"low"`、`"original"` 的图片，看 echo。（GE§1.3, GE§1.6, GE§10）
- **Office 文档**：Gemini 原生读 docx/pptx/xlsx；经 OR 的 `file` part 是否原样交给 Gemini，未知。测：发 docx，看 echo 以及是否出现 `file` annotation。（GE§5, GE§10）
- **图片输出的尺寸与宽高比**：官方有 `imageSize` 512/1K/2K/4K 和 14 种宽高比（含 1:4、4:1、1:8、8:1）。CC 的 `image_config` 是无类型 map，Gemini 的 key 能否透传未测（bot 的 `1k`/`2k`/`4k`、`ar=` 依赖它）；Images API 有 `resolution` 和 `aspect_ratio`。测：`+4k+ar=1:8`，看 echo。（GE§1.6, GE§10）
- **TTS 输出格式**：OR 的 `/api/v1/audio/speech` 只给 `pcm`（请求 mp3 → 400）。文档说 TTS 的 `provider.options["google-ai-studio"]` 会并入 generationConfig，但 `responseFormat`（MP3、OGG_OPUS、WAV）能否生效未知。测：带上它请求，看返回格式。（GE§6, GE§10）

## 6. xAI Grok

第一方 `xai`（基础 slug 也匹配 `xai/zdr`、`xai/zdr/us`；`xai/priority` 是 tier endpoint）。OR 把 CC 请求以 xAI Responses 调用发往上游（实测 2026-11-11）。除 §2（stored responses、deferred、WebSocket、compaction）外：

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| 搜索的部分功能 | `web_search` 的 `enable_image_understanding`、`enable_image_search`；单独使用 `x_search` | `openrouter:web_search` 不暴露前两者；X 搜索只能作为原生 web search 的 `x_search` 参数一起开启 | XD§A5, XD§C3, OR§5.1 |
| Imagine 图片与视频 | 图片编辑最多 5 张参考图，支持 21:9 与 5:2 比例，还有 `grok-imagine-image` 1.0；视频支持参考图/参考音频、关键帧、编辑、延长 | OR 的 `grok-imagine-image-2.0` 最多 3 张参考图、没有 21:9/5:2，1.0 不在目录；视频只有文生视频和首帧图生视频，`allowed_passthrough_parameters: []` | XD§A6, XD§C1, XD§D1 |

**疑似缺口（待实测）**

- **其它托管工具**：`code_interpreter`、`file_search`（collections）、远程 `mcp`、`image_generation` 工具。endpoint 的 `native_tools` 只有 web search，但 OR 对 OpenAI 会原样透传托管工具，而 xAI 的上游正是 Responses。测：RSP 固定 `xai`，发 `{"type":"code_interpreter"}`，看 echo 和输出项。（XD§A5, XD§D1, OA§10.2）
- **文件 / PDF**：官方的 `input_file` 会触发 `attachment_search`；经 OR 是原样交给 xAI，还是被 OR 解析，未知。测：发 PDF，看是否出现 `file` annotation（bot 会提示）。（XD§A6, XD§C3）
- **引用**：默认返回的 `citations[]`（全部来源），以及 Responses 默认的内联 `[[N]](url)` 与 `include:["no_inline_citations"]`，经 OR 是否保留、转发。测：`+s` 后看原始响应。（XD§A5, XD§D1）
- **缓存路由**：`prompt_cache_key`（xAI 强烈建议设置，也可用 `x-grok-conv-id` 头）是否转发；不转发会降低缓存命中。测：看 echo，并对比连续两轮的 `cached_tokens`。（XD§A3, XD§C3）
- **参数**：
  - xAI endpoint 不列 `top_k`、`min_p`、`max_turns`、`guidelines`（自定义审核策略）；
  - `seed` 虽然列出，但上游 Responses 没有 `seed`；
  - grok-4.3 的 `supported_efforts` 缺 `xhigh`，会被映射为 `high`；
  - `x-ai/grok-4.20` 默认非推理，`reasoning.enabled:true` 能否切到推理版，没写。

  测：逐项看 echo（grok-4.20 看上游的 `model`）。（XD§A3, XD§C1–C3）

## 7. DeepSeek

第一方 `deepseek`，只在带日期的 slug（`deepseek-v4-pro-0813`、`deepseek-v4.1-flash-1017`）上有。

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| 官方联网搜索 | Responses 与 Anthropic 接口上的服务端 `web_search`（仅 v4-pro） | endpoint 是 `native_tools:{}`，任何 `web_search` 工具都被换成 OR 的 Exa 搜索 | XD§B5, XD§C4, XD§D2 |

**疑似缺口（待实测）**（另见 §2 的推理回传、prefill、上游接口三条）

- **FIM 与 `strict` 工具**：FIM（`/beta/completions` 的 `suffix`、`echo`）和 beta 的 `strict` 函数工具都要走 beta base URL；OR 的 `/api/v1/completions` 没有关于 `suffix` 的文档。测：带 `suffix` 调 `/api/v1/completions`；CC 带 `strict:true` 的工具，看 echo。（XD§B3, XD§D2）
- **`user_id`**：官方用它做 KV cache 隔离和每用户并发；OR 只发自己的哈希身份，是否填进 `user_id`，未知。测：带 `user`，看 echo。（XD§B3, XD§D2）
- **推理相关字段**：`reasoning_effort:"max"` 是否原样上传（`max` 时官方默认 `max_tokens` 为 128K）；logprobs 里的 `logprobs.reasoning_content` 是否返回。测：看 echo 和响应。（XD§B3, XD§C2, XD§C4）

## 8. 智谱 GLM

第一方 `z-ai`，对接 Z.ai 国际站（OR 的 provider 元数据：HQ 和数据中心都在 SG）。

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| 官方联网搜索 | chat 工具 `web_search`（`search_engine`、`count`、`search_recency_filter`、`search_result`、`search_prompt` 等，结果在顶层 `web_search[]`） | 所有 `z-ai/*` endpoint 都是 `native_tools:{}`；任何 `{"type":"web_search",…}` 都会被换成 OR 的 Exa 搜索 | GKQ§1.3, GKQ§4.7, GKQ§5 |
| 国内站（bigmodel.cn）独有的功能 | `search_intent` 与国内搜索引擎、`retrieval`（知识库）与 `mcp` 工具、异步 chat、Batch（5 折）、Responses API | OR 只接国际站，`tools[]` 也只收 function 和 `openrouter:*`；OR 上 GLM 的 `:batch` 只由 DeepInfra、Morph、Sail Research 提供 | GKQ§1.1, GKQ§1.3, GKQ§1.8, GKQ§4.1–4.2 |
| 部分模型 | `glm-4.5-airx`/`-flash`、`glm-4.6v-flash`/`-flashx`、`glm-4.7-flashx`、`glm-4.1v-thinking-*`、`glm-4v-flash`、`glm-4-voice`（音频输入）；官方的 `glm-4.7-flash` | 前者不在 OR 目录；`z-ai/glm-4.7-flash` 没有 Z.AI endpoint，只有第三方 | GKQ§4.2, GKQ§5 |

**疑似缺口（待实测）**（另见 §2 的推理回传、关闭 file-parser 两条）

- **保留思考**：官方默认 `clear_thinking:true`，服务端会丢掉回传的 `reasoning_content`；要保留，需 `thinking.clear_thinking:false` 并逐字回传。OR 没有对应字段，所以除非能透传，bot 回传的推理在这里不起作用。测：`provider.options["z-ai"]`，看 echo。（GKQ§1.2, GKQ§5）
- **其它参数**：`stop`（官方支持，国际站限 1 个；Z.AI endpoint 不列）、`do_sample`、`tool_stream`、`request_id`。测：`provider.options["z-ai"]`，看 echo。（GKQ§1.5, GKQ§4.5）
- **审核信号**：`finish_reason:"sensitive"`（要求撤回已展示的内容）和 `network_error` 是否保留在 `native_finish_reason`；每个 chunk 的 `content_filter[{role,level}]` 是否转发。测：触发一次审核拦截，看原始流。（GKQ§1.6, GKQ§4.10）

## 9. Moonshot Kimi

第一方 `moonshotai`（K3.1-pro、K3、K2.7-code、K2.6；`moonshotai/highspeed` 是 priority tier）。

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| 官方联网搜索与 Formula 工具 | Responses 的 `web_search`（仅 K3）；Formula 工具（`moonshot/web-search`、`fetch`、`memory`、`excel` 等，经 `/v1/formulas/…/fibers` 执行） | `native_tools:{}`，`web_search` 被换成 OR 的 Exa 搜索；Formula 是单独的接口，`tools[]` 也表达不了 | GKQ§2.4, GKQ§4.7, GKQ§6 |
| 输出来源证明 | 请求带 `X-Msh-Request-Nonce`，响应返回 `Msh-Request-Signature`，用 `/v1/signatures/verify` 可以证明输出出自官方模型 | OR 只透传少数请求头（文档只列了 `x-anthropic-beta`），响应头是 OR 自己的，也不代理验证接口 | GKQ§2.2, GKQ§6, OR§2.2 |
| 官方 Batch | 由 Moonshot 执行，6 折 | `kimi-k3:batch` 只由 DeepInfra 提供 | GKQ§2.9, GKQ§4.3 |

**疑似缺口（待实测）**（另见 §2 的推理回传、prefill 两条）

- **参数**：k2.6 的 `thinking.keep:"all"`（保留思考）、`prompt_cache_options`（1 h TTL；OR 的同名字段只用于 OpenAI）、`prediction`、K3 的动态工具消息（`role:"system"` + `tools`）。测：`provider.options.moonshotai`，看 echo。（GKQ§2.2, GKQ§2.6, GKQ§6）
- **k2.6、k2.7-code 的视频输入**：官方支持，OR 只标了图片输入（两者 2026 年底退役）。测：发视频，看是否被拒。（GKQ§2.5, GKQ§6）

## 10. 阿里 Qwen

第一方 `alibaba`（另有 `alibaba/us-east-1`；适用阿里云国际站条款）。

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| Responses 的联网搜索类工具 | `web_search`（含 `action.sources`）、`web_extractor`、`web_search_image`、`image_search` | `web_search` 被换成 OR 的 Exa 搜索（`native_tools:{}`）；后三种工具类型 OR 的 schema 表达不了 | GKQ§3.3, GKQ§4.7, GKQ§7 |
| 语音输出 | `qwen3.5-omni-*` 的 `modalities:["text","audio"]` | OR 上没有任何 Qwen 模型有 audio 输出模态，`qwen3.5-omni-*` 也不在目录 | GKQ§3.4, GKQ§4.9 |
| 部分模型 | `qwen-flash`、`qwen-max`、`qwen-long`（及 `fileid://`）、`qwen3.5-omni-*`、Qwen-MT、QwQ/QVQ、`qwen3-vl-plus`、滚动别名 `qwen3.8-max`；阿里云托管的旧/小开源 Qwen（大部分 `qwen3-*`、`qwen2.5-*` 等） | 前者不在 OR 目录；后者在 OR 上没有 Alibaba endpoint，只有第三方部署 | GKQ§4.4, GKQ§7 |

**疑似缺口（待实测）**（另见 §2 的推理回传、prefill、上游接口三条）

- **关闭 qwen3.8-max 的思考**：OR 把 `qwen3.8-max-0902`/`-prime` 标为 `mandatory:true`（发 `none` 会得到 OR 自己的 400），官方却是混合模式（`reasoning_effort:"none"` → `enable_thinking=false`）。测：`provider.options.alibaba` 带 `enable_thinking:false`，看 echo。（GKQ§3.2, GKQ§7）
- **参数**：
  - CC 层面的搜索与代码解释器：`enable_search` + `search_options`、`enable_code_interpreter`。即使能透传，兼容模式也不返回搜索来源（来源 `search_info` 只有 DashScope 原生接口给）；
  - qwen3.8-max 的 `thinking_budget`；
  - `repetition_penalty`、`parallel_tool_calls`、`tool_stream`、`vl_high_resolution_images`、`translation_options`、`enable_text_image_mixed`、`include_tool_usage`。

  测：`provider.options.alibaba`，看 echo。（GKQ§3.3, GKQ§3.5, GKQ§4.5, GKQ§7）
- **媒体 part 字段**：视频帧列表（`{"type":"video","video":[…]}`）、逐 part 的 `fps`、`min_pixels`/`max_pixels`/`total_pixels`。测：看 echo。（GKQ§3.4, GKQ§7）

## 11. 豆包（火山方舟）

OR 上 Seed 模型唯一的 provider `seed` 是 BytePlus ModelArk（国际站），不是火山方舟；每个模型只有一个 endpoint（`seed/fp8`）。bot 的 `db$` 等 alias 直连方舟（`ark:`，见 [usage.md](usage.md)），不受本节影响；只有经 OR 用 `bytedance-seed/…` 时才相关。除 §2（Responses 状态与显式缓存、Files API）外：

| 功能 | 官方 API 能做什么 | 为什么 OpenRouter 做不到 | 来源 |
|---|---|---|---|
| 方舟独有的模型 | `doubao-seed-2-1-pro-*`、`doubao-seed-2-1-lite-260915`（1M 上下文、音频输入）、`doubao-seed-evolving`、`doubao-seed-character-*`、`doubao-seed-translation-250915`；BytePlus 的 `seed-2-0-pro-260328` | BytePlus 不卖前者；后者不在 OR 目录 | DB§1, DB§9.1 G1 |
| 指定版本 | 按版本号调用（如 LTS 的 `…-260428` 与 `…-260228`），或选原始 CoT 的 `…-260215` | OR 的 slug 不带版本号（只带 OR 的上架日期），实际对应哪个版本未公开，还有 OR 自己的 `expiration_date` | DB§1.2, DB§9.1 G2 |
| 内置工具 | 豆包搜索（Chat `web_search_options`、Responses `web_search`、Messages `web_search_20250305`）、联网内容插件（头条、抖音、墨迹）、`image_process`、`doubao_app`、`knowledge_search`、`apply_patch` | 每个 Seed endpoint 都是 `native_tools:{}`，`web_search` 被换成 Exa；BytePlus 本身也没有这些工具（它的 Responses 只支持 function 和 MCP） | DB§5, DB§9.1 G3 |
| 音频输入 | 2.1-lite、2.0-lite/mini-260428 收音频，包括视频里的音轨 | OR 上 Seed 的输入模态只有 text、image、video | DB§6, DB§9.1 G4 |
| 服务等级 | `service_tier` 的 `flex`（约 5 折）、`fast`（低延迟）、`auto`（TPM 包） | OR 的 tier 是独立 endpoint、各自计价，Seed 只有一个 endpoint，`service_tier:null` | DB§3, DB§9.1 G7 |
| Seedream 图像的扩展参数 | `layer_decomposition`、透明背景、`watermark:false`、1.5K、`sequential_image_generation`、流式、`web_search` 工具、`optimize_prompt_options` | OR 的 Seedream endpoint 只接受 `resolution`（1K/2K）、`aspect_ratio`、`n=1`、`input_references`、`seed`，`allowed_passthrough_parameters: []`，不能流式 | DB§5, DB§9.1 G10 |

**疑似缺口（待实测）**（另见 §2 的推理回传、prefill、上游接口三条）

- **2.1 Turbo 的推理强度与关闭思考**：OR 元数据没有 `supported_efforts`，也不列 `reasoning_effort`。测：`e=low`、`nothink`，看 echo。（DB§9.2 P2）
- **两种输出预算**：官方 `max_tokens` 只计回答（默认 4096），`max_completion_tokens` 计回答 + 思考；OR 只有一个 `max_tokens`，映射没写。测：看 echo。（DB§3, DB§9.2 P3）
- **强制工具调用**：`tool_choice:"required"` 或指定函数；OR 把 Seed 2.x 标为 `required:false, function:false`。测：看 echo。（DB§9.2 P4）
- **图片精度**：`detail:"xhigh"`（≤9 MP）与 `image_pixel_limit`；OR 的 `detail` 只有 auto/low/high/original，也没有像素上限字段。测：看 echo。（DB§6, DB§9.2 P5）
- **更长的输出**：2.1 Turbo 官方 256K；OR endpoint 标 262144 上下文、235929 最大输出（实测 2026-11-11）。OR 是否拒绝更大的 `max_tokens`，未知（Gemini 4 实测可以超过 endpoint 标的值）。测：短 prompt + `max=250000`。（DB§9.2 P7）
- **响应元数据**：`service_status.model_fallback`（方舟是否换了模型）、`moderation_hit_type`、实际服务的模型版本（`model` 字段）。测：看原始响应和 `/generation`。（DB§7, DB§9.2 P8）
- **其它**：`parallel_tool_calls:false`（Seed endpoint 不列）；Messages 的 `thinking.type:"adaptive"`（OR 把它改写成 `reasoning.enabled:true`；方舟各模型是否支持 adaptive，官方也没写）。测：`provider.options.seed`，看 echo。（DB§9.1 G9, G11）
