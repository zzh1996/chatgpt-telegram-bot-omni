# OpenRouter 相对官方 API 的功能缺口

调研日期：2026-11-11。本文只汇总当天的 research 笔记，不含新的实测。

## 1. 说明

### 1.1 目的

bot 的所有 LLM 请求都经 OpenRouter（下称 OR）发出，以后可能改为直连官方 API。本文按厂商列出官方 API 提供、但 OR 支持得不好、做法不同或完全不支持的功能，让用户知道：在 bot 里看到的模型行为，哪些地方因为 OR 而不是"原汁原味"的官方体验。

### 1.2 信息来源与记号

| 缩写 | 文件 | 内容 |
|---|---|---|
| OR | [research/openrouter.md](research/openrouter.md) | OR 本身；§11.1 是按厂商的限制，§11.2 是破坏"干净请求"的行为表 |
| OA | [research/openai.md](research/openai.md) | OpenAI 官方 API 与 OR 映射 |
| AN | [research/anthropic.md](research/anthropic.md) | Anthropic 官方 API 与 OR 映射 |
| GE | [research/gemini.md](research/gemini.md) | Google Gemini 官方 API 与 OR 映射 |
| XD | [research/xai_deepseek.md](research/xai_deepseek.md) | xAI 与 DeepSeek（Part A/B 为官方，C/D 为 OR） |
| GKQ | [research/glm_kimi_qwen.md](research/glm_kimi_qwen.md) | 智谱 GLM、Moonshot Kimi、阿里 Qwen（§1–3 为官方，§4–7 为 OR） |

- **来源写法**：`OR§6` 表示 research/openrouter.md 第 6 节，其它缩写同理；原始 URL 都在各文件末尾的 Sources 中。
- **UNVERIFIED**：research 笔记没有找到文档明确支持该说法，属于推断、旧资料或第三方证据，必须用真实请求验证（见 §4）。标"推断"的条目是本文把两条已知事实合起来得出的结论，同样待测。
- 第 3 节"通过 OpenRouter"一列的状态词：
  - **缺失**：经 OR 无法使用。
  - **降级**：能用，但能力或保真度不如官方。
  - **改写**：OR 改变了字段、默认值或语义。
  - **待测**：官方有此能力，OR 文档没说明是否转发、如何转发（UNVERIFIED）。
  - **等同**：与官方基本一致，仅在需要对照时列出。

### 1.3 bot 的默认规避措施

默认值来自 `FidelityConfig::standard()` 和 auto-max 预设（`crates/omni-llm/src/openrouter/options.rs`）。每个请求实际用了哪些补丁，都会列在回复的请求信息里。

| 措施 | 请求中的体现 | 如何关闭 | 局限 |
|---|---|---|---|
| 固定第一方 provider | `provider.only:["<slug>"]`，按模型作者映射：`openai`→`openai`、`anthropic`→`anthropic`、`google`→`google-ai-studio`、`x-ai`→`xai`、`deepseek`→`deepseek`、`z-ai`→`z-ai`、`moonshotai`→`moonshotai`、`qwen`→`alibaba` | `+p=any` 交回 OR 默认路由；`+p=<slug>` 改为固定其它 endpoint | 基础 slug 匹配同一 provider 的所有区域/变体（如 `xai` 也匹配 `xai/zdr/us`），但不匹配 tier endpoint；没有第一方 endpoint 的模型直接失败 |
| 禁止 fallback | `provider.allow_fallbacks:false` | `+p=any` | — |
| 不静默丢参数 | `provider.require_parameters:true` | `+p=any` | 只按 endpoint 的 `supported_parameters` 判断；不支持的参数会让请求失败，而不是被忽略 |
| 关闭上下文压缩 | `plugins:[{"id":"context-compression","enabled":false}]` | 总是发送，`+p=any` 不影响 | 账户开启 "Prevent overrides" 时无效 |
| 显式 `max_tokens`（auto-max 预设） | `max_tokens` 取目录里的 `top_provider.max_completion_tokens`；同时发送最高 effort、OpenAI 的 `reasoning.summary:"detailed"`、图片输出模型的 `modalities` | `+pure`（之后 OR 可能注入自己的默认 `max_tokens`） | 目录值可能高于第一方 endpoint 的上限（§4） |
| 原样回传推理 | 同一模型的 assistant 消息带回原始 `reasoning_details` | — | 换模型时只回传文本；OR 是否把它转成上游的 `reasoning_content`（DeepSeek/GLM/Kimi/Qwen）UNVERIFIED |
| 文件只走原生 | 只有 `architecture.input_modalities` 含 `file` 时才发送 PDF/文档，并附 `file-parser` `pdf.engine:"native"`；响应里出现 `file` annotation 时提示用户 | — | `engine:"native"` 在 endpoint 不支持原生文件时的行为 UNVERIFIED |
| 搜索只请求原生 | `+s` → `openrouter:web_search` + `engine:"native"` | `+s=<engine>` 指定其它引擎 | OR 在无原生搜索的 endpoint 上会把 `native` 静默降级为 Exa（§2 G8） |
| 审计 | `+debug` → `debug.echo_upstream_body:true`；配置项 `echo_upstream_body`（总是请求回显）、`router_metadata`（发送 `X-OpenRouter-Metadata: enabled`） | — | 回显只在流式请求、只在 CC/RSP 上有，MSG 没有；脱敏是"best effort" |

bot 目前只走 Chat Completions（CC，`/api/v1/chat/completions`）。Responses（RSP）独有的 OpenAI `phase` 和 async 工具，以及 Messages（MSG）独有的 Claude 原生 block、`tool_addition`、`thinking.display`，在 bot 中暂时用不到（§3.1、§3.2）。

## 2. 通用缺口（所有厂商都受影响）

### 2.1 总表

| # | 缺口 | 现象 | 影响 | bot 的应对 | 来源 |
|---|---|---|---|---|---|
| G1 | 无法断线续流 | OR 只有无状态的 `POST`：没有 `background` 模式、没有 `GET /responses/{id}`、没有 `starting_after` 或事件重放；RSP 的 `store` 恒为 `false`。断流后只能用 `GET /api/v1/generation?id=` 查费用和 `cancelled`；文本要开启 Input & Output Logging 才能从 `/generation/content` 取回，而且只有中断前的部分。唯一可续写的机制是 Gemini 4 long-decode continuation；唯一的服务端异步途径是 Batch API（无流式，≤24 h） | OpenAI background、Gemini Interactions、xAI deferred 等官方续流/异步能力全部失效；长推理断线只能整段重来，再付一次费 | 无法规避，只能重新生成 | OR§9，[responses overview](https://openrouter.ai/docs/api_reference/responses/overview) |
| G2 | 断连后是否取消、是否计费因 provider 而异 | 断开连接只对"支持取消"的 provider 立即停止生成和计费（OpenAI、Azure、Anthropic、xAI、DeepSeek 等）。Google、Google AI Studio、AWS Bedrock、Alibaba、Groq、Mistral 等不取消，文档说"按完整响应计费"。Z.AI、Moonshot 不在任何一张表里。非流式请求从不取消。Broadcast 文档又说只计断开前已交付的部分，两处矛盾 | 在 bot 里"停止"生成，对 Gemini、Qwen 不省钱；bot 为 Gemini 固定的 `google-ai-studio` 恰好不可取消 | 无法规避；按厂商见 §2.2 | OR§6，[streaming](https://openrouter.ai/docs/api_reference/streaming) |
| G3 | 没有服务端状态 | `store:true` 或非空 `previous_response_id` → 400。没有 Conversations、服务端 compaction（`context_management`、`/responses/compact`）、Anthropic Message Threads、xAI stored responses 和 WebSocket 模式，也没有 Qwen/GLM Responses 的服务端历史 | 依赖服务端状态的官方能力都不可用，例如 OpenAI compaction 能在压缩后保留推理 | bot 本来就在本地保存并完整重放历史；损失的是这些高级功能 | OR§1, OR§9, OA§11, XD§D1, GKQ§7 |
| G4 | 默认路由到第三方/量化节点，并静默 fallback | 不固定 provider 时，OR 每次按价格、吞吐、延迟、错误率等加权随机选 endpoint，失败自动换 provider；在首个 token 之前，甚至在 HTTP 200 之后也会静默重试。带 tools 的请求还会被 Auto Exacto 重排；开源模型的 priority endpoint 也参与竞争。开源权重模型大多落到 fp4/fp8 第三方：`moonshotai/kimi-k3` 有 28 个 endpoint，第一方列第 24 位；`z-ai/glm-5.3` 有 47 个；DeepSeek 不带日期的 slug 根本没有第一方 endpoint。闭源模型也会落到 Azure、Bedrock、Vertex，这些 endpoint 的参数、原生工具和可取消性都不同 | 测到的可能不是官方部署、不是官方精度；对话中途换 provider 会丢缓存，还可能让推理签名失效 | 默认 `provider.only:[第一方]` + `allow_fallbacks:false`；没有第一方 endpoint 的模型直接失败（不重试），不会悄悄落到第三方；`+p=any` 恢复 OR 默认路由 | OR§0, OR§2.5, OR§11.2 #1–3，[provider selection](https://openrouter.ai/docs/guides/routing/provider-selection) |
| G5 | 静默丢弃不支持的参数 | 默认 `require_parameters:false`：endpoint 不支持的参数被直接忽略；`provider.options.<slug>` 里不认识的 key 也静默丢弃；`tools`、`response_format`、`verbosity` 只是软偏好 | 以为设置了 `stop`、`top_k`、logprobs 等，实际没生效 | 默认 `require_parameters:true`，endpoint 不支持就失败。它管不到 OR 自己的改写（如 Claude 5.5 忽略 `budget_tokens`），也管不到厂商"接受但忽略"（如 xAI ≥4.20 的 logprobs） | OR§2.1, OR§2.5, OR§11.2 #4, GKQ§4.5 |
| G6 | 推理强度被映射 | 不支持的非 `none` effort 被映射到最近的档位（如 `xhigh`→`high`）。只支持 budget 的模型把 effort 换算成输出上限的比例（max/xhigh≈0.95、high 0.8、medium 0.5、low 0.2、minimal 0.1）；只支持 effort 的模型把 `reasoning.max_tokens` 换算成 effort。`~…-latest` 别名把 `none`/`enabled:false` 静默升为最低档；对 mandatory 模型发 `none` 得到 OR 自己的 400。`/models` 的 effort 元数据与厂商文档有出入（见 §3） | 实际推理强度可能不是所选档位；跨厂商对比时尤其要注意 | 默认取 `supported_efforts` 的最高档；所选档位不在列表中、或对 mandatory 模型选 `none` 时给出警告（不拦截） | OR§3.1, OR§11.2 #5，[reasoning tokens](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens) |
| G7 | PDF/文件被 OR 自行解析 | `file-parser` 优先用模型的原生文件能力，没有就用 `mistral-ocr`（$2/1000 页，美国区 $2.2，BYOK 也收费；最多转发 8 张抽取的图片，非视觉模型去掉全部图片），或免费的 `cloudflare-ai`。OR 没有"不能原生就报错"的开关。DeepSeek 和所有 GLM/Kimi/Qwen 模型在 OR 上都不列 `file` 输入 | 模型读到的是 OR 的 OCR/转换结果，不是原文件；厂商自己的文件解析（Moonshot `file-extract`、智谱 `file_url`、Qwen-Long、xAI `attachment_search`）被绕过 | 只有 `input_modalities` 含 `file` 时才发送，否则报错拒绝；附 `pdf.engine:"native"`；出现 `annotations[].type=="file"` 时提示"OR 自行解析了文件"。纯文本文件由 bot 作为独立文本段内联，并在回复中注明 | OR§4.3, OR§11.2 #11, GKQ§4.9，[PDFs](https://openrouter.ai/docs/guides/overview/multimodal/pdfs) |
| G8 | 厂商内置工具被替换成 OR 服务端工具 | `{"type":"web_search"}`、`web_search_preview` 等形状都会被提升为 `openrouter:web_search`。`engine:"auto"` 只在 endpoint 的 `native_tools` 含搜索时用原生，否则用 Exa；`engine:"native"` 在无原生搜索的模型上同样回退到 Exa，不报错（只有 workspace 引擎列表仅含 `native` 时才 403）。只有 OpenAI、Anthropic、Google、xAI、Perplexity 有原生搜索；DeepSeek/GLM/Kimi/Qwen 所有 endpoint 的 `native_tools` 都为空。代码执行、URL context、file search、MCP 等厂商内置工具基本不可达；`openrouter:shell`、`web_fetch`、`image_generation` 是 OR 自己执行的替代品 | 请求形状一样，背后可能是 Exa 而不是厂商搜索；大部分"官方内置工具"的体验拿不到 | `+s` 只请求 `engine:"native"`；是否真是官方搜索，要看该 endpoint 的 `native_tools`，或事后查 `/api/v1/generation` 的 `web_search_engine`；绝不向 OR 发送厂商原生工具对象 | OR§5.1–5.2, OR§8.2, GKQ§4.7，[web search](https://openrouter.ai/docs/guides/features/server-tools/web-search) |
| G9 | 搜索引用/grounding 被压平 | 原生搜索结果统一为 `url_citation`（`url`、`title`、`content`、`start_index`、`end_index`；`content` 是 OR 加的摘录，如 Exa highlights）。Gemini `groundingMetadata` 与 Search Suggestions、Anthropic `web_search_result_location` 与 `encrypted_content`、OpenAI `web_search_call.action.sources`、xAI `citations[]` 都没有文档说会保留，按丢失处理（UNVERIFIED）；CC 流式里 annotation 的形状也没有文档 | 无法按厂商要求展示引用（如 Google Search Suggestions 的展示义务）；Claude 搜索结果的加密内容无法原样回放 | 只能展示 `url_citation` | OR§5.4, GE§11, AN§10.2 |
| G10 | 错误被归一化，500 消息被屏蔽 | 上游一旦返回 header，HTTP 状态就锁定为 200，之后的失败都在流内（CC：带 `error` 的 chunk + `finish_reason:"error"`；RSP：`response.failed`）；非流式的 200 也可能只含 `error`。错误被映射为 OR 的 `error_type`，RSP 的 `error.code` 大多合并为 `server_error`。500 类错误的 `message` 换成通用文本，`provider_code` 被去掉；只有非 500 错误带 `metadata.raw`。402 永远指 OR 余额，厂商自己的余额/账单错误不会出现；Anthropic 的 `req_…` 变成 `gen-…`。原始错误体只能事后从 `/generation/content` 取（需 I/O Logging） | 用户看到的不是厂商原文，难以判断上游出了什么问题 | 保存完整错误 JSON（含 `error_type`、`metadata.provider_code`、`metadata.raw`）；路由死路（无可用 endpoint）不重试 | OR§6, OR§7，[errors & debugging](https://openrouter.ai/docs/api_reference/errors-and-debugging) |
| G11 | `finish_reason` 被归一化 | 只剩 `stop`/`length`/`tool_calls`/`content_filter`/`error`，原值放在 `native_finish_reason`。上游的 token 上限类错误被改写成 `length`；拒答变成 `message.refusal` + `content_filter`；最后的 usage chunk 带非空 `choices`，重复一遍 finish_reason。厂商特有值只能从 `native_finish_reason` 看到（如 DeepSeek `insufficient_system_resource`/`aborted`、Gemini 4 `CONTINUATION`）；GLM `sensitive`/`network_error` 是否保留 UNVERIFIED | 例如 GLM 的 `sensitive` 要求撤回已展示的内容，这个信号可能只剩一个 `content_filter` | 同时记录 `finish_reason` 和 `native_finish_reason` | OR§6, XD§C4, GKQ§4.10 |
| G12 | 默认注入 `max_tokens` | 不传 `max_tokens` 时 OR 可能自己填：CC 的 debug 示例里，Claude 的上游值为 `65536`；Gemini 4 默认 65,536；MSG 是否注入 UNVERIFIED。路由只选能输出这么多 token 的 endpoint，在途信用额度也按这个值冻结 | 输出上限不是官方默认值（Claude 5.x 官方最多 128K），长推理更容易被截断成 `length` | auto-max 预设总是显式发送 `max_tokens`；`+pure` 会去掉它，此时回落到 OR 默认值 | OR§2.1, OR§11.2 #15, AN§11, GE§11 |
| G13 | 上下文压缩 | `context-compression`（middle-out，从中间删消息）对上下文 ≤8,192 的 endpoint 默认开启。账户级默认 plugin 会作用于每个请求，开启 "Prevent overrides" 后请求无法关闭。旧的 `transforms` 字段已从文档移除 | 小上下文模型可能在不告知的情况下丢掉中间的历史 | 每个请求都带 `{"id":"context-compression","enabled":false}`（`+p=any` 也不会打开）；bot 的 OR 账户不应设置默认 plugin | OR§2.3, OR§11.2 #6–7，[message transforms](https://openrouter.ai/docs/guides/features/message-transforms) |
| G14 | OR 侧额外审核与 guardrails | `top_provider.is_moderated:true` 的 endpoint（几乎所有 OpenAI、Anthropic 模型，以及 Amazon、Cohere、Meta）先经过 OR 的输入审核，命中返回 403（`reasons`、`flagged_input`）。没有文档化的关闭方式，用的是什么分类器也不清楚（UNVERIFIED）。账户/工作区的 guardrails（预算、模型白名单、提示注入正则、敏感信息脱敏）也会拦截或修改请求 | 直连官方能通过的内容，经 OR 可能被拦 | 原样展示 403 的错误 JSON；bot 用的 key 不应配置 guardrails | OR§7, OR§11.2 #9–10 |
| G15 | 用户标识被哈希，并附加额外字段 | `user`/`safety_identifier` 被折叠进 OR 的哈希身份再发往上游，从不原样转发。不传时使用账户级身份，一个用户触发的策略封禁可能殃及整个账户。OR 还会给 Z.AI 附带由账户和 `session_id` 派生的 session affinity key | 厂商按用户区分的功能失效：OpenAI 建议 GPT-5.6+ 使用的 `safety_identifier`、DeepSeek `user_id`（KV cache 隔离、每用户并发）、Kimi `safety_identifier`、智谱 `user_id` | 可选配置 `user_id_salt`：把稳定的加盐用户哈希作为 `user` 发送（到上游前仍会被 OR 再哈希一次） | OR§2.1, OR§10, OR§11.2 #18, OA§1.2, XD§D2 |
| G16 | 隐藏的 prompt/结构改写 | 对非 OpenAI 模型，消息的 `name` 会被拼成 `{name}: {content}`；provider 没有原生 tool calling 时，tools 被渲染成 YAML 提示模板；部分 provider 会合并 system 消息或相邻的 user 消息 | 模型看到的 prompt 与发送的不同 | 不发送 `name`；`require_parameters:true` 排除不支持 `tools` 的 endpoint；可用 `+debug` 查看上游 body | OR§2.1, OR§11.2 #12–14 |
| G17 | 多模态传输限制 | 音频只能 base64，不能用 URL；RSP 的音频只收 wav/mp3。视频只收 mp4/mpeg/mov/webm，YouTube 链接只在 Gemini + AI Studio 上可用；MSG 不支持视频。图片 `detail:"original"` 在不支持的 endpoint 上被降为 `high`。OR Files API 的 `or_file_…` 不是厂商文件 id，厂商 Files API 只能经 BYOK 代理 | 依赖 URL、厂商文件 id 或特殊格式的输入不可用 | 图片、音频、视频文件都以 base64/data URL 发送（YouTube 等远程视频链接原样发送），图片带 `detail:"original"` | OR§4.1–4.2, OR§11.2 #22 |
| G18 | 服务等级变成独立 endpoint | `flex`/`priority`/`ultrafast` 是单独的 endpoint（如 `openai/flex`、`google-ai-studio/flex`、`xai/priority`、`anthropic/fast`）。基础 slug 匹配所有区域/变体，但**不匹配** tier endpoint。响应报告实际服务的 tier，`fast` 报告为 `priority`；`:nitro` 会放进 priority endpoint（按 priority 价计费），`:floor` 会放进 flex endpoint | 推断：bot 默认固定 `only:["openai"]` 这类基础 slug 时，请求 `flex`/`priority` 可能没有可用 endpoint（§4），需要 `+p=openai/flex` 这种显式 slug | — | OR§2.4–2.5 |
| G19 | 用量/计费字段被替换 | 统一为 OR 的 `usage`（`cost`、`cost_details`、`cached_tokens`、`cache_write_tokens`、`reasoning_tokens`、`server_tool_use_details`）。OpenAI `tool_usage`、xAI `cost_in_usd_ticks`/`server_side_tool_usage_details`、DeepSeek `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens` 等原生字段没有文档说会透传（UNVERIFIED） | 无法核对厂商侧的精确计费和工具调用次数 | 费用以 `usage.cost` 为准 | OR§8.1, OA§11, XD§D1–D2 |
| G20 | 账户级默认设置 | 账户/工作区的默认 provider 排序、允许/忽略列表、默认 service tier、ZDR、数据政策、默认 plugin、preset 都会叠加到请求上；例如开启 ZDR 会移除 OpenAI 的第一方 endpoint，只剩 Azure | 同一请求在不同账户下行为不同 | 建议给 bot 用设置中性的独立 workspace/key；开启 `router_metadata` 后可在 `openrouter_metadata.pipeline[]` 里看到实际生效的阶段 | OR§11.2 #7–9, OA§11 |

### 2.2 断连、取消与续流（按厂商）

| 厂商 | bot 固定的 slug | OR 断连后是否取消上游 | 官方直连：断连 / 续流 / 异步 | OR 上的替代 |
|---|---|---|---|---|
| OpenAI | `openai` | 取消（OpenAI、Azure）；Bedrock 不取消 | 同步请求断连即取消；`background:true` + `stream:true` 可用 `starting_after` 续流，可 cancel，有 webhooks；Batch | 无续流；OR Batch |
| Anthropic | `anthropic` | 取消（Anthropic、Azure）；Bedrock、Vertex 不取消 | 无法重新接上流（断连后是否停止生成 UNVERIFIED）；Message Batches；Message Threads（beta） | 无；OR Batch 支持 `/v1/messages` |
| Gemini | `google-ai-studio` | **不取消**（Google、Google AI Studio 都在不支持列表里） | Interactions API `background` + `last_event_id` 重放（需 `store:true`）；generateContent 的 long decode checkpoint；Gemini 4 在第一个 checkpoint 前断流不计费；Batch | 只有 Gemini 4 long-decode continuation；OR Batch |
| xAI | `xai` | 取消；Bedrock 不取消 | 没有流续传；deferred CC（结果 24 h 内只能取一次）；stored responses 保留 30 天；Batch | 无；OR Batch（`:batch`） |
| DeepSeek | `deepseek` | 取消 | 没有任何异步/续流手段（断连后是否计费 UNVERIFIED） | 无；`:batch` 变体（由哪个 host 执行 UNVERIFIED） |
| 智谱 GLM | `z-ai` | 不在列表里（UNVERIFIED） | CN 异步 chat + 轮询；CN Batch；Responses `store:true` 后可 GET；没有流续传 | 无 |
| Kimi | `moonshotai` | 不在列表里（UNVERIFIED）；官方直连断连后仍完整计费 | 没有异步/续流；Partial Mode 续写；Batch（6 折） | 无；`kimi-k3:batch` 只由 DeepInfra 提供 |
| Qwen | `alibaba` | **不取消**（Alibaba 在不支持列表里，按完整响应计费） | 断连即停止，只按已生成部分计费；`partial` 续写（仅非思考模型）；Batch | 无；`:batch`（qwen3.8-max-0902、qwen3.7-plus）由 Alibaba 提供 |

来源：OR§6, OR§9, OA§5, AN§9, GE§2.9, XD§A10, XD§B10, GKQ§1.8, GKQ§2.9, GKQ§3.8。

## 3. 按厂商

### 3.1 OpenAI

第一方 `openai`，另有 tier endpoint `openai/flex`、`openai/fast`、`openai/ultrafast`；同一模型也由 Azure、Amazon Bedrock 提供。官方推荐 Responses API，bot 目前走 CC。

| 功能 | 官方 API | 通过 OpenRouter | 影响/备注 | 来源 |
|---|---|---|---|---|
| 第一方 provider | `api.openai.com` | **降级**：也会路由到 Azure/Bedrock。Bedrock 没有原生工具（`web_search` 变 Exa），忽略 `reasoning.mode:"pro"`，async 工具同步执行，没有 `seed`，断连不取消。开启 ZDR 会移除第一方 endpoint | bot 默认固定 `openai` | OA§10.0, OA§11 |
| `phase`（`commentary`/`final_answer`） | GPT-5.3-codex 起必须原样回传，否则提前停止 | **缺失（CC）**：CC 无法表示 `phase`，只有 RSP 支持 | OR 文档称会"significant performance degradation … early stopping"；bot 走 CC，因此受影响 | OA§1.4, OR§1, OR§11.1 |
| effort | `none`…`max`，取值随模型而定；不支持的值 → 400 | **改写**：不支持的值被映射到最近档位（OpenAI 的请求级行为 UNVERIFIED）；`configuration_update` 中的不支持值 → 400 | 官方会报错的请求，经 OR 可能"成功"但档位不同 | OA§1.1, OA§10.1, OR§3.1 |
| 推理摘要 `reasoning.summary` | 需显式开启，可能需要组织验证 | **待测**：schema 支持；OR 自家 key 是否通过组织验证 UNVERIFIED；RSP 忽略 `reasoning.exclude` | bot 对 OpenAI 默认发 `summary:"detailed"` | OA§1.2, OA§10.1 |
| 加密推理回传 | `store:false` + 原样回放 reasoning item（`encrypted_content`）；改动前缀会丢推理 | **改写**：OR 自动请求 encrypted content；CC 用 `reasoning_details`（`openai-responses-v1`）回传；OpenAI/Azure/Bedrock 之间能否互相解密 UNVERIFIED（格式不同） | 必须固定单一 provider | OA§2, OA§10.1 |
| `reasoning.mode:"pro"`、`reasoning.context`、`configuration_update` | GPT-5.6/GPT-6 | **改写**：pro 改为路由到 `*-pro` slug（只在 OpenAI/Azure）；fallback provider 不支持时这些字段被剥离；`configuration_update` 只在 GPT-6/6.1 标准 slug 上可用 | — | OA§10.1, OR§11.1 |
| async 工具（`async:true`） | GPT-6 | **降级**：只在 RSP 上有，CC 上被剥离；Bedrock 同步执行 | — | OA§10.1, OR§11.1 |
| 采样 / logprobs / `n` | effort 为 `none` 时才能用 `temperature`/`top_p`/logprobs；`n`、`stop`、`seed` 只在 CC 上有 | **缺失**：`openai/gpt-6.1-sol` 不列 `temperature`/`top_p`/`stop`/`logprobs`；RSP 的 `include` 没有 `message.output_text.logprobs`；CC schema 没有 `n`（UNVERIFIED） | 在 `require_parameters:true` 下，发送这些参数预计会因无可用 endpoint 而失败 | OA§1.3, OA§10.1, OR§8.4 |
| `max_tool_calls` | 内置工具调用次数上限 | **改写**：变成 OR server-tool 的步数预算（默认值即上限 30）；是否转发给 OpenAI UNVERIFIED | — | OA§10.1 |
| `web_search` | 原生工具：`allowed_domains`/`blocked_domains`、`user_location`、`external_web_access`、`return_token_budget`、图片搜索 | **降级**：被提升为 `openrouter:web_search`；只有 OpenAI/Azure endpoint 有原生搜索（Bedrock 用 Exa）；`excluded_domains` 被静默忽略；没有 `external_web_access`、`return_token_budget`、图片搜索；原生搜索时 `max_results`、`search_context_size` 被忽略 | — | OA§10.2, OA§11 |
| 搜索引用 | `url_citation` + 完整来源列表 `web_search_call.action.sources`；要求内联可点击的引用 | **降级**：只有 `url_citation`；`include` 不支持 `web_search_call.action.sources`、`web_search_call.results` | 无法展示完整来源 | OA§3.1, OA§11 |
| `code_interpreter` | 显式/自动容器、`file_ids`、生成的文件可下载（`container_file_citation`） | **降级**：会透传，但用 OR 额度只能 `container:"auto"`，不能带 `file_ids`（需 BYOK）；OR 的 annotation schema 没有 `container_file_citation`；生成的文件无法下载（UNVERIFIED） | 图表、文件产物拿不到 | OA§10.2, OA§11 |
| `image_generation` 工具 | `action`；`quality` 含 `xhigh`/`max`；任意 `WxH`；gif/svg；`complexity`；多帧；`revised_prompt` | **降级**：schema 缺 `action`、`xhigh`/`max`、gif/svg、`complexity` 和帧参数；输出项只列 `id`/`result`/`status`（`revised_prompt` 等 UNVERIFIED）；`openrouter:image_generation` 是另一个由 OR 执行的工具（默认 `openai/gpt-image-2.6-flare`）；单独的出图请求走 OR 归一化的 Images API | — | OA§3.3, OA§10.2, OR§4.6 |
| `file_search` / `shell` / `mcp` / computer use | 都是官方内置工具 | **降级**：`file_search` 只能 BYOK；`shell` 走 OpenAI hosted shell（文件下载问题同 code interpreter，UNVERIFIED）；`mcp` 实际由谁执行 UNVERIFIED；GA 的 `computer` 类型不在 schema 中 | — | OA§10.2 |
| 图片输入 | `detail` 含 `original`（Astra/5.6/5.5/5.4） | **等同**：支持 `original`，endpoint 不支持时降为 `high`；schema 要求 `detail` 必填 | bot 发送 `detail:"original"` | OA§6.1, OA§10.1, OR§4.1 |
| 文件输入 | RSP：PDF（文字 + 页面图片，`detail` 控制页面图）、docx/xlsx/pptx/代码等约 100 种 MIME | **降级**：`input_file` 没有 `detail`；非 PDF 类型发给 OpenAI 的结果 UNVERIFIED；OpenAI file id 需 BYOK | — | OA§6.2, OA§10.1 |
| 音频 | RSP 没有音频；CC 的 `gpt-audio-1.5` 支持输入和输出 | **改写**：音频输出只在 CC 上有，且必须流式；RSP 的 `input_audio`（wav/mp3）是 OR 扩展，如何路由 UNVERIFIED | — | OA§6.3, OA§10.1 |
| 缓存 | `prompt_cache_key`；`prompt_cache_options`（30m/12h、`prewarm`）；`prompt_cache_retention` | **降级**：`prompt_cache_options`/`prompt_cache_breakpoint` 支持，但 fallback 时会被剥离，格式转换时 TTL 丢失；`prompt_cache_retention` 不在 schema；`prompt_cache_key` 兼作 sticky key，是否转发 UNVERIFIED | — | OA§10.1, OR§10 |
| 服务等级 | `service_tier`：auto/default/flex/scale/priority/fast/ultrafast | **改写**：tier 是独立 endpoint；`ultrafast` 依次回落到 priority、default；`scale` 只出现在枚举里；`fast` 报告为 `priority` | 推断：在 bot 默认固定下，`tier=flex` 可能找不到 endpoint（G18） | OA§10.1, OR§2.5 |
| 状态 / 续流 / compaction | `background` + `starting_after` 续流、cancel、`previous_response_id`、Conversations、服务端 compaction、webhooks、WebSocket | **缺失**：`store` 恒为 false；`previous_response_id` → 400；没有 GET/cancel/compact；断连即取消（OpenAI/Azure） | 长推理断线只能重来 | OA§5, OA§11 |
| 错误 | `error.type`/`code`/`param`、`x-request-id`、`x-ratelimit-*`、账单类 429 code | **改写**：RSP 的 `code` 多数合并为 `server_error`；500 消息被屏蔽；`x-ratelimit-*` 是否转发 UNVERIFIED；402 指 OR 余额；bio/cyber 拦截变成 403 `refusal`（不重试，也不换 provider） | — | OA§10.3, OA§11 |
| 其它请求字段 | `moderation`、`image.context_compression`、`stream_options.include_obfuscation`、`prompt`、`access_programs.cyber` | **缺失**：都不在 OR schema 里；`access_programs` 非 `standard` 时需 BYOK。`response.queued`、file_search/compaction/shell 进度事件也不在 OR 的流 schema 里 | — | OA§10.1, OA§11 |
| 用量 | `tool_usage`（image_gen token、计费的搜索次数） | **改写**：OR 文档没有 `tool_usage`，改为 `server_tool_use_details` + `cost` | — | OA§10.1, OA§11 |

### 3.2 Anthropic

第一方 `anthropic`、`anthropic/fast`；另有 `claude-on-aws`、`google-vertex/*`、`amazon-bedrock/*`、`azure/*`（Opus 5.5 共 11 个 endpoint）。research 建议 Claude 走 OR 的 MSG（`/api/v1/messages`）以保留原生 block，bot 目前走 CC。

| 功能 | 官方 API | 通过 OpenRouter | 影响/备注 | 来源 |
|---|---|---|---|---|
| 请求格式的保真度 | 原生 Messages API | **降级（CC）**：thinking/redacted thinking 被压平为 `reasoning_details`，server-tool block 和引用被压平为 annotations；没有 `tool_addition`、`block_binding`；流式时加密推理可能显示为 `[REDACTED]`。MSG 保留原生 block，但没有 `debug` 回显 | bot 走 CC，因此受影响 | AN§0, AN§11, OR§1 |
| `thinking.display` | Claude 5.x、Opus 4.8/4.7 默认 `omitted`（thinking 为空，首字最快）；另有 `summarized`、`updates`（beta） | **改写**：OR 默认注入 `summarized`；Sonnet 5.5 未设置推理时注入 `{type:"adaptive",display:"summarized"}`；CC 只能用 `reasoning.exclude`，无法设置 `display` | 经 OR 看到的是摘要化的 thinking，延迟特性也不同 | AN§1.4, AN§10.2, AN§11 |
| `budget_tokens` / 关闭推理 | Claude 4.7+ 上 `enabled` + budget → 400；Opus 5.5、Sonnet 5.5 等不能 `disabled` | **改写**：Claude 4.7/5/5.5 的 `budget_tokens`/`reasoning.max_tokens` 被接受后静默忽略，按 adaptive 发送（直连官方会 400）；对 Opus/Sonnet 5.5 关闭推理得到 OR 自己的 400 文案；`~…-latest` 别名把关闭强制改为最低档；`minimal`→`low` | bot 的 `budget=` 对这些模型无效 | AN§10.2, AN§11, OR§3.1 |
| effort | `output_config.effort` low…max；Opus 5.5 默认 `medium` | **改写**：`reasoning.effort` 和 `verbosity` 都映射到 `output_config.effort`，`verbosity` 优先；不设置就不发送。OR `/models` 却把 Opus 5.5 的默认值标为 `high`，把 Sonnet 4.6 标为 `medium`（官方为 `high`） | bot 默认发最高档；用 `+pure` 时要注意默认值的差异 | AN§10.2, AN§11 |
| thinking 签名 / 前缀检查 | 必须原样回传；preserved-thinking 模型会检查历史前缀是否被改动（2026-08-31 后创建的账户直接 400；Fable 5.2 对所有账户都 400）；`block_binding`、`input_transformations`（beta） | **改写**：经 OR 时，Opus/Sonnet/Haiku 5.5 和 Fable 5.1 "不执行该检查"（Fable 5.2 UNVERIFIED）；`block_binding` 只在 MSG 上有 | 同一段改过的历史，直连会 400，经 OR 却成功；两种后端行为不一致 | AN§2.2, AN§10.2, AN§11 |
| 账户绑定的 thinking（Sonnet/Haiku 5.5、Fable 5.2） | 只有产生它的账户（或关联账户）能读 | **改写**：OR 把回放固定到能读取它的 provider 组；若 `only`/`ignore` 排除了它 → 400；Fable 5.2 UNVERIFIED | 中途换 provider 会丢推理 | AN§10.2, AN§11 |
| 对话中途改配置 | 对话中途的 system 消息、`tool_addition`/`tool_removal`、逐条 effort（beta） | **降级**：MSG 支持（自动附加 beta 头）；CC 只有 `configuration_update`（只能改 effort） | — | AN§10.2 |
| beta 头 / 工具字段 | 可发任意 `anthropic-beta`；工具 `strict`；`eager_input_streaming` 可选 | **改写**：文档只允许透传 `interleaved-thinking-2025-05-14` 和 `structured-outputs-2025-11-13`，其余靠 OR 自动识别；不带 structured-outputs beta 头时 `strict:true` 被剥离；流式请求的每个工具都被加上 `eager_input_streaming:true` | — | AN§11, OR§11.1 |
| 采样参数 / API 版本 | `2026-10-21` 版本下，任何 `temperature`/`top_p`/`top_k` 都 → 400 | **改写**：OR 接受这些参数；Claude 5.x 的 `supported_parameters` 不含它们；在 Opus 4.7、Sonnet 5 上被静默忽略；OR 向上游发送哪个 `anthropic-version` UNVERIFIED（Sonnet 4.6 接受 temperature，疑为 `2023-06-01`） | — | AN§1.6, AN§10.2, OR§11.1 |
| `web_search` | `web_search_20250305`/`_20260209`/`_20260318`；域名过滤、`user_location`、`max_uses`、dynamic filtering | **降级**：只有 `anthropic`、`claude-on-aws` endpoint 有原生搜索（Bedrock/Vertex/Azure 没有）；`auto`/`native` 不可用时回退 Exa；`_20260318` 不在 schema；`max_uses` 会转发 | — | AN§10.2, OR§5.2 |
| 搜索引用与回放 | `web_search_result_location` 引用（`cited_text`、`encrypted_index`）；结果的 `encrypted_content` 必须原样回放 | **降级**：CC 压平为 `url_citation`，加密字段如何回放 UNVERIFIED；MSG 的 schema 含原生 block，最有可能保留 | — | AN§2.4, OR§5.4 |
| `web_fetch` | 原生工具（引用、`use_cache`、`url_sources`） | **降级**：只能用 `openrouter:web_fetch` + `engine:"native"`（映射为 `web_fetch_20260209`），否则回退到 Exa 或 OR 自己的抓取器；`web_fetch_tool_result` 不能作为输入回放；`citations`、`use_cache`、`url_sources` 不暴露 | — | AN§10.2, AN§11 |
| 代码执行 / 容器 / MCP / computer use | 官方 server tool 与客户端工具 | **缺失**：没有 `code_execution_*` 工具类型、没有 `container` 字段、没有 `mcp_servers`，computer use 在校验时被拒；`openrouter:bash`/`shell` 是 OR 的沙箱；advisor、tool search 可用 | — | AN§10.2, AN§11 |
| 文档引用（citations） | `document` 块开启 `citations` → `char_location`、`page_location` 等 | **待测**：MSG 的 schema 支持（未实测）；CC 没有对应表示（UNVERIFIED） | — | AN§4, AN§10.2 |
| 图片 / PDF / Files API | 图片 base64/url/file；PDF `document` 块；Files API | **降级**：图片、PDF 可用（Claude 默认原生解析 PDF）；只收 base64 的 provider（Bedrock/Vertex）遇到 URL 图片时如何处理 UNVERIFIED；Files API 只能 BYOK 代理；MSG 不支持视频（Claude 本身也不支持音视频） | — | AN§5, AN§10.2 |
| 缓存 | 块级 `cache_control`（≤4 个断点）+ 顶层自动缓存，5m/1h | **等同**：CC/MSG 两种形式都支持；RSP 只支持顶层形式，块级须用 `prompt_cache_breakpoint`，会被转成默认 5m（TTL 丢失）；缓存按 provider 隔离，sticky 路由 60 分钟 | — | AN§10.2, OR§10 |
| fast mode / 服务等级 / 数据驻留 | `speed:"fast"`；`service_tier`（auto/standard_only）；`inference_geo` | **改写**：`speed:"fast"` 路由到 `anthropic/fast`，但没有固定时会回落到标准 endpoint（按标准价计费）；响应报告 `service_tier:"priority"`（官方为 `standard`）；`service_tier` 的含义被替换；没有 `inference_geo`（OR 用 `eu.`/`us.` 主机代替） | bot 默认固定 `anthropic` 时 fast 能否路由到 `anthropic/fast`，待测 | AN§10.2, AN§11 |
| 其它字段 | `container`、`mcp_servers`、`thread`、`diagnostics`、`compaction`、`fallback_credit_token`、`fallbacks`（服务端拒答回退）、`count_tokens` | **缺失/改写**：都不可用；`fallbacks` 变成 OR 的多模型路由（≤3 个）；`context_management` 只在 MSG 上部分支持 | — | AN§10.2, AN§11 |
| 续流 / 异步 | 无法重新接上流；Message Batches；Message Threads（beta） | **降级**：断连会取消 Anthropic/Azure，Bedrock/Vertex 则继续计费；没有 Threads；OR Batch 支持 `/v1/messages`；`/generation/content` 只返回压平的文本，没有签名和 block | — | AN§9, AN§11 |
| 错误 / 拒答 | `error.type`、`request_id: req_…`、`stop_reason:"refusal"` + `stop_details` | **改写**：归一化为 `error_type`；`request_id` 变成 `gen-…` 或 null；500 消息被屏蔽；使用政策拒答变成 403 `refusal`；OR 还对 `is_moderated` 的 Claude endpoint 做额外审核 | — | AN§11, OR§7 |
| 模型元数据 | Sonnet 4.5 的 1M beta 已退役（回到 200K） | **改写**：OR 仍把 Sonnet 4.5 标为 1M | — | AN§11 |

### 3.3 Google Gemini

第一方 `google-ai-studio`（bot 默认固定）和 `google-vertex`（`/global`、`/us`、`/eu`），另有 `…/flex`、`…/priority` tier endpoint。Google 推荐 Interactions API（IA）；OR 只有无状态的 CC/RSP/MSG，向上游用的具体格式（推测为 generateContent）UNVERIFIED。

| 功能 | 官方 API | 通过 OpenRouter | 影响/备注 | 来源 |
|---|---|---|---|---|
| 第一方 provider 之间的差异 | — | **降级**：AI Studio 与 Vertex 能力不同。YouTube 只在 AI Studio；3.8 Flash 的 `stop` 只在 Vertex，而 Vertex 缺 `temperature`/`top_p`；Vertex 上 3 Pro Image 最高 2K；Vertex 对图片 URL 有限制；continuation token 绑定签发它的 provider | fallback 会在对话中途改变行为 | GE§10, GE§11 |
| Interactions API | 推荐接口：`background`、服务端状态、`last_event_id` 续流、cancel、managed agents（Deep Research） | **缺失** | — | GE§2, GE§11 |
| `thinkingLevel` | `minimal`/`low`/`medium`/`high`（各模型支持其中一部分） | **改写**：`reasoning.effort` 1:1 映射，`xhigh`→`high`，不支持的取最近档；`max`/`none` 如何处理没有文档；OR 的 effort 列表与 Google 不一致（如 Nano Banana 2.1 为 `[high,minimal,none]`，缺 `medium`） | — | GE§10, GE§11 |
| `thinkingBudget` | 只在 2.5 上有实际意义；Gemini 4 映射到 level 并给出警告；与 level 同时发送 → 400 | **降级**：`reasoning.max_tokens`→`thinkingBudget`，在 Gemini 3 上又被映射成 level（不精确），在 Gemini 4 上不支持 | bot 的 `budget=` 在 Gemini 3/4 上不精确或无效 | GE§1.5, GE§10 |
| `includeThoughts` | 返回思考摘要 | **待测**：OR 默认返回 reasoning，`reasoning.exclude:true` 可隐藏；上游实际的 `includeThoughts` 值 UNVERIFIED | — | GE§10 |
| thought signature | functionCall part 上的签名必须回传（否则 400），其它 part 建议回传；3.5 Flash 起，有签名时会复用之前所有轮的推理；part 不可合并或拆分 | **待测**：签名放在 `reasoning_details`（`reasoning.encrypted`，格式 `google-gemini-v1`），必须放在带 `tool_calls` 的 assistant 消息上；把 `thought_signature` 放在 tool call 上 → 400；文本、图片 part 的签名能否完整往返 UNVERIFIED | bot 对同一模型原样回传 `reasoning_details` | GE§3, GE§10, OR§3.3 |
| Long decode（Gemini 4） | `continuationToken`；流中 32K token 之后约每 5K token 一个 checkpoint，可续写 | **等同（OR 上唯一可续流的机制）**：continuation item 放在 `reasoning_details`（`id:"gemini-continuation"`），返回 `finish_reason:"length"` + `native_finish_reason:"CONTINUATION"`；绑定签发它的 provider；早先轮次的 continuation item 会被移除，continuation hop 上跳过 `cache_control`；断流时不返回 usage | — | GE§10, OR§9 |
| 采样参数 | 3.6+/4 忽略 `temperature`/`topP`/`topK`；另有 `seed`、`presencePenalty`/`frequencyPenalty`、`responseLogprobs`/`logprobs`、`candidateCount` | **缺失**：3.8/4 的 `supported_parameters` 没有 `top_k`、penalty、logprobs；不支持 `candidateCount` | 在 `require_parameters:true` 下，发送这些参数会找不到可用 endpoint | GE§10 |
| `safetySettings` | 类别和阈值可调；2.5 起默认 OFF | **缺失（UNVERIFIED）**：没有文档化的字段；`provider.options` 没有公布 Gemini 的白名单 | Google 默认已是 OFF，影响有限 | GE§1.7, GE§10 |
| 安全拦截的表现 | generateContent：HTTP 200 + `finishReason:SAFETY` 或 `promptFeedback.blockReason` | **改写**：变成 403 `content_policy_violation`，原始内容在 `metadata.raw` | — | GE§11, OR§7 |
| `mediaResolution` / `videoMetadata` / `mediaProcessing` | 全局和逐 part 的分辨率（含 `ULTRA_HIGH`）；视频 `fps` 和起止 offset；视频的 agentic/static 处理 | **降级**：没有 `mediaResolution`（图片 `detail` 如何映射 UNVERIFIED）；没有 `videoMetadata`；`processing`（agentic/static）可用 | — | GE§1.6, GE§10 |
| Google Search grounding | `googleSearch`（`timeRangeFilter`、`searchTypes` web/image） | **降级**：`openrouter:web_search` 原生映射为 `google_search`；`user_location` 有效；原生搜索不支持域名过滤：`auto` + 过滤 → 静默改用 Exa，`native` + 过滤 → 400；图片搜索 grounding UNVERIFIED | — | GE§10, GE§11 |
| grounding 元数据 | `groundingMetadata`：`webSearchQueries`、`searchEntryPoint.renderedContent`（有展示义务）、`groundingChunks`、`groundingSupports`（`confidenceScores`、按 part 计的 UTF-8 字节 offset）；`urlContextMetadata` | **降级**：只剩 `url_citation`；以上字段都没有文档说会透传 | 很难满足 Google 对 Search Suggestions 的展示要求 | GE§4, GE§11 |
| URL context / 代码执行 / Maps / File Search / Computer Use / MCP | 官方内置工具；tool combination（`includeServerSideToolInvocations`） | **缺失**：endpoint 的 `native_tools` 只有 `google_search`；`openrouter:web_fetch`/`shell` 是 OR 自己的工具；`toolConfig.retrievalConfig`、`includeServerSideToolInvocations` 不暴露 | — | GE§10, GE§11 |
| 多模态输入 | 图片（含 heic/heif）；PDF（≤50 MB 或 1000 页）；原生支持 Office 文档；文本类 MIME；音频（多种格式，可用 URL 或 Files API）；视频（多种格式）；YouTube（公开视频，每请求 ≤10 个）；Files API（2 GB、保留 48 h）；内联 100 MB | **降级**：图片只收 png/jpeg/webp/gif；PDF 走 `file` part（Gemini 默认原生）；Office 文档经 `file` part 能否处理 UNVERIFIED；音频只能 base64，RSP 只收 wav/mp3；视频只收 mp4/mpeg/mov/webm；YouTube 只在 AI Studio 上可用（Vertex 只能 base64）；没有 Google Files API | bot 固定 AI Studio，YouTube 可用 | GE§5, GE§10, GE§11 |
| 图片生成参数 | `imageConfig.aspectRatio`（14 种，含 1:4/4:1/1:8/8:1）；`imageSize` 512/1K/2K/4K（K 必须大写）；thought images；最多 14 张参考图；带签名的多轮编辑 | **降级**：CC 用无类型的 `image_config`（Gemini 的 key 名如 `aspect_ratio`/`image_size` 在当前文档中 UNVERIFIED）；Images API 有 `resolution` 1K/2K/4K（3.1 Flash Image 另有 512）、`aspect_ratio`，`n` 只能为 1，不支持流式；Vertex 上 3 Pro Image 最高 2K；带签名的多轮编辑 UNVERIFIED | bot 的 `1k`/`2k`/`4k`、`ar=` 选项正是发送 `image_config.image_size`/`aspect_ratio`，需实测 | GE§1.6, GE§10, GE§11 |
| TTS / 音频输出 | `responseModalities:["AUDIO"]`、`speechConfig`、多说话人、`responseFormat.audio`（MP3/OGG_OPUS/WAV 等） | **降级**：只有 `/api/v1/audio/speech`；`response_format` 只能 `pcm`（mp3 → 400）；最多 2 个说话人；其它控制只能经 `provider.options` 透传（`responseFormat` 是否生效 UNVERIFIED） | — | GE§6, GE§10 |
| 缓存 | 隐式缓存（3.6+/4 需 ≥4,096 token；Gemini 4 命中时便宜 95%）；显式 `cachedContents`（Gemini 4 不支持） | **降级**：隐式缓存自动生效；显式缓存改为 `cache_control` 断点，只用最后一个，第一条 system 消息不可变，TTL 固定 5 分钟；没有 `cachedContents` API；OR 文档里的价格（0.25×）已过时 | — | GE§9, GE§10, OR§10 |
| 服务等级 | `serviceTier` standard/flex/priority；priority 可能静默降级（看 `x-gemini-service-tier` 头） | **改写**：用 `service_tier` 或 tier slug（`google-ai-studio/flex`、`google-vertex/global/priority`）；响应报告实际 tier | 固定基础 slug 时不含 tier endpoint（G18） | GE§10, OR§2.5 |
| 断连 / 续流 / 取消 | IA 的 `background` + `last_event_id` 重放；generateContent 关闭连接即取消；Gemini 4 在第一个 checkpoint 前断流不计费；Batch | **降级**：Google 和 AI Studio 都**不支持取消**，断开后继续生成并计费；除 long decode 外没有续流 | bot 默认固定的 `google-ai-studio` 在不可取消列表里 | GE§2.9, GE§11, OR§6 |
| 错误 | `error.status` + `details[]`（ErrorInfo 的 `reason`）；IA 用 `code:"safety"` 等 | **改写**：归一化为 `error_type`；500 消息被屏蔽 | — | GE§8, GE§11 |
| 未暴露的字段 | `cachedContent`、`store`、`labels`、`enableEnhancedCivicAnswers`、原始 `continuationToken`；函数调用的 `VALIDATED` 模式与 `allowedFunctionNames` | **缺失/待测**：前一组不在 schema 中；后一组的映射 UNVERIFIED | — | GE§10 |

### 3.4 xAI Grok

第一方 `xai`（基础 slug 同时匹配 `xai/zdr`、`xai/zdr/us`；`xai/priority`、`xai/zdr/priority` 是 tier endpoint）；grok-4.6/4.7 也由 Amazon Bedrock 提供。OR 上的 provider 名为 "SpaceXAI"。官方推荐 Responses API（CC 已弃用）；从 `xai-responses-v1` 格式推断 OR 向上游调用的是 xAI Responses（UNVERIFIED）。

| 功能 | 官方 API | 通过 OpenRouter | 影响/备注 | 来源 |
|---|---|---|---|---|
| 第一方 provider | `api.x.ai` | **降级**：基础 slug `xai` 也匹配 `xai/zdr/us`（1.1× 价格，没有原生搜索）；Bedrock 的参数集不同，没有原生搜索和隐式缓存，搜索 $0.01/次，断连不取消 | research 建议另加 `ignore:["xai/zdr/us"]`，bot 目前没有加 | XD§C1, XD§C5, XD§D1 |
| effort | 4.6/4.7：low/medium/high/xhigh，不能关闭；4.3：none…xhigh（默认 low）；4.5 把 xhigh 当作 high | **改写**：透传；grok-4.3 的 `supported_efforts` 缺 `xhigh`；`max` 被向下映射；mandatory 模型发 `none` → 400 | — | XD§A2, XD§C1, XD§C3 |
| grok-4.20 的默认模式 | 别名 `grok-4.20` 指向推理版 slug | **改写**：`x-ai/grok-4.20` 默认是非推理（`default_enabled:false`）；`reasoning.enabled` 是否会在两个 slug 间切换 UNVERIFIED | — | XD§C1, XD§D1 |
| 加密推理回传 | `store:false` + `include:["reasoning.encrypted_content"]`，原样回放全部 output item；漏掉会丢失之前的推理并降低缓存命中率 | **待测**：用 `reasoning_details` 回传（格式 `xai-responses-v1`，Bedrock 为 `bedrock-xai-responses-v1`）；上游的确切要求、摘要文本是否暴露 UNVERIFIED | — | XD§A4, XD§C2, OR§3.3 |
| 采样与其它参数 | `top_k`/`min_p`（Responses）、`seed`/`n`（CC）、`max_turns`、`guidelines`（自定义审核策略）、`safety_identifier`；推理模型上 `stop`/penalty 直接报错；≥4.20 忽略 logprobs | **缺失**：xAI endpoint 不列 `top_k`、`min_p`、`n`、`max_turns`、`guidelines`、`safety_identifier`、`stop`（模型级列表里的 `stop`/`top_k` 只来自 Bedrock）；`logprobs` 虽然列出但无效；列出了 `seed`，但 xAI Responses 没有 `seed`（UNVERIFIED）；`default_parameters`（temperature 0.7、top_p 0.95）是否被注入 UNVERIFIED | — | XD§A3, XD§C3, XD§C6 |
| `web_search` | 域名过滤（各 ≤5 个）、`enable_image_understanding`、`enable_image_search` | **降级**：`xai` endpoint 有原生搜索；域名过滤可用（允许/排除二选一）；`enable_image_*` 不暴露；原生搜索时 `max_results`、`search_context_size`、`max_uses` 被忽略 | — | XD§C3, XD§D1 |
| X 搜索 | `x_search` 工具（handle、日期、图片/视频理解） | **改写**：只能作为原生 web search 的可选参数 `parameters.x_search{…}`（2026-09-21 起按帖子/用户数计费） | bot 的 `+s` 不会搜索 X | XD§C3, OR§5.2 |
| 引用 | 默认返回 `citations[]`（全部来源）；Responses 默认内联 `[[N]](url)`；`url_citation` annotation | **降级**：归一化为 `url_citation`；`citations[]`、内联格式，以及 `no_inline_citations`、`web_search_call.action.sources` 等 include 是否转发 UNVERIFIED | — | XD§A5, XD§D1 |
| 其它内置工具 | `code_interpreter`、collections/`file_search`、`attachment_search`、远程 MCP、`image_generation` 工具 | **缺失**：`native_tools` 只有 web search；`openrouter:shell`/`image_generation` 是 OR 的替代品 | — | XD§C3, XD§D1 |
| 图片输入 | 只收 jpg/png，≤20 MiB | **待测**：OR 接受 png/jpeg/webp/gif，webp/gif 发给 xAI 的结果未知 | — | XD§A6, XD§C3 |
| 文件 / PDF | `input_file` 触发 agentic 的 `attachment_search` | **待测**：模型列出了 `file` 模态，但文件是原生交给 `attachment_search`，还是被 OR 解析，UNVERIFIED | bot 会带 `engine:"native"`，并检测 `file` annotation | XD§C3, XD§D1 |
| 图片 / 视频生成 | Imagine：`n` 1–10；宽高比含 21:9/5:2/auto；`quality` 含 auto；编辑最多 5 张图；`deferred`、`storage_options`；视频支持参考图生成、关键帧、编辑、延长 | **降级**：`grok-imagine-image-2.0` 只能 `n=1`、最多 3 张参考图，没有 21:9/5:2，没有 `quality:auto`，没有 deferred/storage；1.0 不在目录中；视频只有文生视频和首帧图生视频 | — | XD§A6, XD§D1 |
| 缓存 | 自动缓存；强烈建议用 `prompt_cache_key`/`x-grok-conv-id` | **待测**：隐式缓存 + sticky 路由；是否转发为 `x-grok-conv-id` UNVERIFIED，命中率可能下降 | — | XD§C3, XD§D1 |
| 状态 / 异步 | stored responses（30 天，`previous_response_id`）、deferred CC（24 h 内取一次）、compaction、WebSocket、Batch；没有流续传 | **缺失**：全部不可用（`store:true` → 400）；断连会取消生成；只有 OR Batch（`:batch`） | — | XD§A10, XD§D1 |
| 用量 | `cost_in_usd_ticks`、`server_side_tool_usage_details`（`x_posts_fetched` 等） | **改写**：换成 OR 的 `cost`/`web_search_requests`（原字段是否透传 UNVERIFIED） | — | XD§C3, XD§D1 |
| 错误 | 文本 endpoint 的错误体格式没有文档 | **改写**：500 消息被屏蔽；流中错误以 HTTP 200 + OR `error_type` 的形式出现 | — | XD§D1 |

### 3.5 DeepSeek

第一方 `deepseek` 只出现在两个带日期的 slug 上：`deepseek/deepseek-v4-pro-0813`（对应官方 `deepseek-v4-pro`）和 `deepseek/deepseek-v4.1-flash-1017`（对应官方 `deepseek-flash`）。

| 功能 | 官方 API | 通过 OpenRouter | 影响/备注 | 来源 |
|---|---|---|---|---|
| 第一方 provider | `deepseek-v4-pro`、`deepseek-flash` | **降级**：只有上述两个带日期的 slug 有 `deepseek` endpoint。`deepseek/deepseek-v4-pro`（实际是 0423 **预览版**权重）、`-v4-flash`、`-v3.2` 等只有第三方：25–51 个 host，多为 fp8/fp4/mxfp4，最大输出 131K–943K（官方为 384K） | 在 bot 默认固定 `deepseek` 下，不带日期的 slug 会直接失败；请用带日期的 slug | XD§C1, XD§D2, OR§11.1 |
| thinking 开关 / effort | `thinking.type` + `reasoning_effort` none/low/high/max（`minimal`→low，`medium`/`xhigh`→high）；默认开启；`max` 时默认 `max_tokens` 为 128K | **待测**：用 `reasoning.effort`/`enabled`；`supported_efforts` 为 `[max,high,low,none]`（0423 的 slug 为 `xhigh,high,none`）；如何映射到 `thinking` + `reasoning_effort` 没有文档 | — | XD§B3, XD§C1, XD§C2 |
| `reasoning_content` 回传 | 带 `tools` 时，之前**所有轮次**的 `reasoning_content` 都必须回传，否则 400 | **待测**：用 `reasoning`/`reasoning_details`（别名 `reasoning_content`）回传；OR 是否把它转成上游的 `reasoning_content` UNVERIFIED，但这是必需的 | — | XD§B4, XD§C2 |
| 采样 / `tool_choice` / logprobs | thinking 模式下 temperature 无效；penalty 无效；`required` 或指定函数的 `tool_choice` → 400；logprobs 含 `logprobs.reasoning_content` | **降级**：官方 endpoint 列出了这些参数；flash 的 `supports_tool_choice.required/function` 为 false；`logprobs.reasoning_content` 是否透传 UNVERIFIED；第三方 host 还接受 `seed`/`top_k`/`min_p`/`logit_bias`（非官方行为） | — | XD§B3, XD§C1, XD§C4 |
| JSON 输出 | CC 有 `json_object`；`json_schema` 只在 Responses 上有 | **降级**：官方 endpoint 只有 `json_object`；`structured_outputs` 只在第三方 host 上有 | — | XD§C4 |
| Beta 功能 | 前缀续写（`prefix:true`，思考模式下可带 `reasoning_content` 前缀）；FIM `/beta/completions`（`suffix`/`echo`）；`strict` 工具 | **缺失** | — | XD§D2 |
| `user_id` | KV cache 隔离、每用户并发、内容安全归属 | **缺失（UNVERIFIED）**：OR 只发送哈希后的身份 | — | XD§C4, XD§D2 |
| 联网搜索 | Responses/Anthropic 接口上的服务端 `web_search`（仅 v4-pro）；独立的 Search/Fetch REST API | **缺失**：`native_tools:{}`，`openrouter:web_search` 静默改用 Exa；Search/Fetch API 不可达 | bot 的 `+s` 实际用的是 Exa | XD§C4, XD§D2 |
| Responses / Anthropic 接口语义 | `apply_patch` custom 工具、回放 `web_search_call` 即可恢复搜索结果、`output_config.effort`、模型别名 | **缺失**：OR 用自己的 skin 做翻译 | — | XD§D2 |
| 图片输入 | 只有 `deepseek-flash` 支持；Files API（`file_id`，仅图片） | **待测**：模型级标为 text+image，各 host 是否支持 UNVERIFIED；`file_id` 不可用 | — | XD§B6, XD§C4 |
| PDF / 文档 | 官方**不支持** | **改写**：OR 用 `file-parser`（默认 `mistral-ocr`，$2/1000 页）代为解析 | bot 在模型没有 `file` 模态时拒绝发送 | XD§C4, XD§D2 |
| 缓存 / 用量 / 计价 | 磁盘 KV cache；`prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`、`system_fingerprint`；峰谷价 | **改写**：隐式缓存 + sticky 路由；官方 endpoint 的价格是 DeepSeek 自己的（flash 命中 $0.003、未命中 $0.15），而 OR 指南写的是通用的 0.1×；hit/miss 字段换成 `cached_tokens`；峰谷价有按时段的 override（中国节假日谷价 UNVERIFIED） | — | XD§C4, XD§D2 |
| finish_reason | `insufficient_system_resource`、`aborted` | **改写**：被归一化，原值只在 `native_finish_reason` 中 | — | XD§C4 |
| 错误 | 402 表示余额不足；400 文本（如缺少 `reasoning_content`） | **改写**：DeepSeek 的 402 永远不会出现（OR 的 402 指 OR 余额）；500 被屏蔽；400 文本被包在 OR 的错误信封里 | — | XD§D2 |
| 续流 / 取消 | 没有任何异步/续流手段 | **等同**：断连会取消（DeepSeek 在可取消列表里）；有 OR 的 `:batch` 变体（由哪个 host 执行 UNVERIFIED） | — | XD§B10, XD§D2 |

### 3.6 智谱 GLM

第一方 `z-ai`（OR 标为 `fp8`，glm-4.6/4.7 标为 `fp4`），对接的是 Z.AI 国际站（新加坡）。`z-ai/glm-5.3` 除 Z.AI 外还有 35 个 host（OR§0 统计共 47 个 endpoint）。

| 功能 | 官方 API | 通过 OpenRouter | 影响/备注 | 来源 |
|---|---|---|---|---|
| 第一方 provider / 模型 | 国内 `open.bigmodel.cn` 与国际 `api.z.ai` | **降级**：OR 的 Z.AI 是国际站（SG），上游确切的 base URL UNVERIFIED；`z-ai/glm-4.7-flash` 没有 Z.AI endpoint；`z-ai/glm-5.3-prime` 只由 Alibaba 提供；`:batch` 只由 DeepInfra/Morph/Sail Research 提供；glm-4.5-airx/-flash、glm-4.6v-flash/flashx、glm-4.7-flashx、glm-4.1v-thinking-*、glm-4v-flash、glm-4-voice 不在 OR 上 | — | GKQ§4.1, GKQ§4.2, GKQ§5 |
| thinking / effort | `thinking.type` 默认 enabled（5.3 只能 enabled）；`reasoning_effort`（5.2+，默认 `max`）：5.3 为 low/high/max，5.2 接受 7 个值 | **改写**：glm-5.3 的元数据与官方一致；glm-5.2 列为 `xhigh,high,none`，默认 **high**（官方默认 `max`）；≤5.1 只有开/关；effort 如何映射到 `thinking`/`reasoning_effort` UNVERIFIED | — | GKQ§1.2, GKQ§5 |
| 保留思考 `clear_thinking` | 默认 `true`，服务端丢弃回传的 `reasoning_content`；设为 `false` 并逐字回传才会保留；工具循环中要回传 | **缺失**：没有对应字段；`provider.options["z-ai"]` 能否透传 UNVERIFIED；OR 是否把 reasoning 转成上游的 `reasoning_content` 也 UNVERIFIED | — | GKQ§1.2, GKQ§4.6, GKQ§5 |
| 参数 | temperature [0,1]、top_p、`do_sample`、`stop`（≤4 个，国际站 1 个）、`tool_choice` 只能 auto、`tool_stream`、`request_id`/`user_id`；没有 `json_schema` | **缺失**：Z.AI endpoint 不列 `stop`、`seed`、penalty、logprobs、`parallel_tool_calls`；没有 `tool_stream`、`do_sample`、`request_id`；`user_id` 被哈希身份替代；OR 列出了智谱文档未记载的 `top_k`（行为 UNVERIFIED）；`structured_outputs` 只在 5.3-flashx-legal 上有 | — | GKQ§1.5, GKQ§4.5, GKQ§5 |
| 联网搜索 | chat 工具 `{"type":"web_search","web_search":{"enable":true,"search_engine":…}}`（默认关闭）；结果在顶层 `web_search[]`；`search_prompt` 默认不标引用；Responses 的 `web_search` | **缺失/改写**：`native_tools:{}`；任何 `{"type":"web_search",…}` 都会被转成 OR 的 Exa 搜索——旧 bot 用来"关闭搜索"的对象经 OR 可能反而**打开** Exa 搜索（多余 key 如何处理 UNVERIFIED）；没有 `search_engine`、`search_result`、`search_prompt`，也没有 `web_search[]` | — | GKQ§1.3, GKQ§4.7, GKQ§5 |
| 其它工具 | `retrieval`（国内知识库）、`mcp`（国内）、独立的 Web Search/Reader API | **缺失** | — | GKQ§1.3, GKQ§5 |
| 多模态 | 视觉模型：图片、视频（URL）、文件（`file_id`/`file_url`/`file_data`，由智谱服务端解析）；`glm-4-voice` 音频 | **降级**：在列出视频模态的模型上可用图片和 `video_url`（URL 如何处理 UNVERIFIED）；没有任何 `z-ai/*` 模型列出 `file` 输入，PDF 只能走 OR `file-parser`；`glm-4-voice` 不在 OR 上 | bot 拒绝向 GLM 发送 PDF | GKQ§1.4, GKQ§4.9, GKQ§5 |
| finish_reason / 审核信号 | `sensitive`（审核拦截，需撤回已展示内容）、`network_error`、`model_context_window_exceeded`；每个 chunk 带 `content_filter[{role,level}]` | **改写**：被归一化，原值只可能留在 `native_finish_reason`（UNVERIFIED）；`content_filter` 数组是否转发 UNVERIFIED | "撤回"信号可能丢失 | GKQ§1.6, GKQ§5 |
| 错误 | 业务码 1301/1302/1305/1113 等（字符串 code，中文消息） | **改写**：变成 `error_type` + `provider_code`；500 被屏蔽；原始 body 只能经 `/generation/content`（需 management key）取 | — | GKQ§1.7, GKQ§5 |
| 缓存 | 隐式缓存；`cached_tokens` | **改写**：自动缓存，但 OR 额外发送一个由账户和 `session_id` 派生的 session affinity key | 无害，但请求不再"干净" | GKQ§4.8, GKQ§5 |
| 异步 / 续流 | 国内异步 chat + 轮询、国内 Batch（5 折）、Responses `store:true` 后 GET；没有流续传 | **缺失**：都不可达；Z.AI 不在取消列表里（断连后是否停止计费 UNVERIFIED） | — | GKQ§1.8, GKQ§5 |
| 国内专属功能 | `search_intent`、国内搜索引擎、MCP、retrieval、异步 chat、Batch | **缺失**：OR 用的是国际站 | — | GKQ§5 |

### 3.7 Moonshot Kimi

第一方 `moonshotai`，覆盖 K3.1-pro、K3、K2.7-code、K2.6；K3.1-pro 另有 `moonshotai/mxfp4` 和 priority tier `moonshotai/highspeed`（即 `kimi-k3.1-pro-highspeed`）。K2.5 及更早的模型**没有**第一方 endpoint。

| 功能 | 官方 API | 通过 OpenRouter | 影响/备注 | 来源 |
|---|---|---|---|---|
| 第一方 provider / 模型 | K2.5 与 `moonshot-v1-*` 已于 2026-08 下线（官方返回 404） | **降级**：`kimi-k2.5`、`kimi-k2-thinking`、`kimi-k2-0905`、`kimi-k2` 仍在 OR 目录中，但只有第三方（int4/fp8/bf16）；kimi-k3 共 28 个 endpoint（19 个非 Moonshot host），多为 fp4/mxfp4，第一方列第 24 位；kimi-k3.1-pro 有 16 个非 Moonshot host（含 Alibaba、Bedrock） | 不固定 provider 时，几乎一定测到第三方量化部署 | GKQ§4.3, GKQ§6, OR§0 |
| 来源证明 | `X-Msh-Request-Nonce` → `Msh-Request-Signature`，可用 `/v1/signatures/verify` 证明输出来自官方模型 | **缺失** | 无法证明经 OR 拿到的是原汁原味的官方输出 | GKQ§2.2, GKQ§6 |
| thinking / effort | K3 始终思考，`reasoning_effort` low/high/max（默认 max）；k2.6 用 `thinking.type` + `keep:"all"`；k2.7-code 只能 enabled + keep all | **改写**：K3/K3.1-pro 的元数据为 `mandatory:false` 并提供 `none`，与官方"始终开启"矛盾——在 Moonshot endpoint 上发 `none` 是报错还是被忽略 UNVERIFIED；`thinking.keep` 没有 OR 字段（`provider.options.moonshotai` UNVERIFIED） | — | GKQ§2.2, GKQ§6 |
| `reasoning_content` 回传 | K3 的多轮对话和工具循环中**必须**原样回传 | **待测**：依赖 OR 把 `reasoning`/`reasoning_details` 重新作为 `reasoning_content` 发给上游（UNVERIFIED）；若被丢弃，K3 的工具调用会变差或报错 | — | GKQ§2.3, GKQ§6 |
| 采样参数 | K3 的 temperature 1.0、top_p 0.95、n 1、penalty 0 都是固定值，其它值报错 | **改写**：OR 把 temperature/top_p/penalty 列为支持，发送非默认值会在上游 400；第三方 host 可能静默接受（非官方行为） | bot 的 `t=` 用于 K3 预计会报错 | GKQ§2.2, GKQ§6 |
| 其它参数 | `prompt_cache_options`（1h TTL）、`safety_identifier`、`prediction`、动态的 `role:"system"` 工具消息、`X-Msh-*` 头 | **缺失**：Moonshot endpoint 也不列 `seed`、`top_k`、`n`、`parallel_tool_calls`；`highspeed` 没有 logprobs | — | GKQ§4.5, GKQ§6 |
| Partial Mode | 最后一条 assistant 消息带 `partial:true`（可带 `reasoning_content`），官方推荐用于续写中断的输出 | **待测**：OR 的 assistant prefill 是否会加上 `partial:true` 没有文档 | 基于 partial 的续写可能不可用 | GKQ§2.6, GKQ§6 |
| 联网搜索 | Responses 的 `web_search`（仅 K3，官方称近期不建议用于生产）、Formula `moonshot/web-search`、REST search/fetch；`$web_search` 已不在文档中 | **缺失**：`native_tools:{}`；`{"type":"web_search"}` 变成 Exa；`builtin_function`/Formula 工具无法表达 | — | GKQ§2.4, GKQ§6 |
| 多模态 / 文件 | 图片只收 base64 或 `ms://`（拒绝公网 URL）；视频（k2.6/k2.7-code 也支持）；`file-extract` 输出 Markdown | **降级**：OR 是否会下载并内联 http(s) 图片 UNVERIFIED；OR 把 k2.6/k2.7-code 只标为图片输入（没有视频）；没有 `file` 输入，PDF 走 OR `file-parser`；`ms://` 引用不可达 | bot 总是发送 base64 data URL，不受公网 URL 限制影响 | GKQ§2.5, GKQ§6 |
| 缓存计费 | K3 写缓存收费（5m 按输入价，1h 按 2×）；`cache_write_tokens` | **待测**：OR 文档说写入免费、读取 0.25×，OR 的 JSON 却是 0.1× 且没有写入价；实际如何计费、`cache_write_tokens` 是否有值 UNVERIFIED | — | GKQ§2.6, GKQ§4.8, GKQ§6 |
| 错误 | `content_filter`（输入或**输出**触发，400）、`engine_overloaded_error` 等 | **改写**：折叠为 `error_type`/`provider_code`；500 被屏蔽 | — | GKQ§2.8, GKQ§6 |
| 续流 / 取消 / Batch | 没有异步/续流；断连后仍完整计费；Batch（6 折） | **缺失**：Moonshot 不在取消列表里（UNVERIFIED），断连大概率仍计费；`kimi-k3:batch` 只由 DeepInfra 提供，官方 Batch 不可达 | — | GKQ§2.9, GKQ§6 |
| Responses / Messages | 官方的 `/v1/responses`、`/anthropic`（仅 K3：`web_search_call`、`summary_text` 推理项、thinking 签名） | **改写**：OR 的同名 skin 是 OR 自己的翻译，不是 Moonshot 的原生接口 | — | GKQ§6 |

### 3.8 阿里通义千问

第一方 `alibaba`（另有 `alibaba/us-east-1`；数据中心在 SG/CN，适用阿里云**国际**条款）。专有的 Qwen3.5+ 至 3.8 只由 Alibaba 提供，没有第三方风险；较旧、较小的开源模型没有 Alibaba endpoint。

| 功能 | 官方 API | 通过 OpenRouter | 影响/备注 | 来源 |
|---|---|---|---|---|
| 第一方 provider / 模型 / 地域 | 多地域（北京、新加坡、香港、美国等），工作区域名 | **降级**：`alibaba` 对应哪个 DashScope 地域 UNVERIFIED，北京专属功能（Qwen-Long、guardrail 头）大概率不可达；没有滚动别名 `qwen3.8-max`（只有 `-0902` 和 `-prime`），也没有 `qwen-flash`、`qwen-max`、`qwen-long`、`qwen3.5-omni-*`、Qwen-MT、QwQ/QVQ、`qwen3-vl-plus`；旧的开源模型只有第三方 | Alibaba 在 OR 上也托管 GLM/Kimi，但对这些模型不是官方；bot 按作者固定 provider，不受影响 | GKQ§4.1, GKQ§4.4, GKQ§7 |
| thinking 开关 | qwen3.8 是混合模式，`reasoning_effort:"none"` → `enable_thinking=false` | **缺失**：`qwen3.8-max-0902`/`-prime`、`qwen3.8-2.4t-a95b` 被标为 `mandatory:true`，经 OR 无法关闭 qwen3.8-max 的思考 | — | GKQ§3.2, GKQ§7 |
| effort / 预算 | `reasoning_effort` low/medium/xhigh（`minimal`→low，`high`/`max`→xhigh），或 `thinking_budget` | **降级**：`reasoning.max_tokens`→`thinking_budget` 只在标了 `supports_max_tokens` 的模型（`qwen3.8-flash`、`qwen3.8-omni-flash`、`qwen3.7-flash`）上生效；qwen3.8-max 没有预算控制；qwen3.7-max 没有 effort 列表；effort 如何映射到 DashScope UNVERIFIED | — | GKQ§3.2, GKQ§4.6, GKQ§7 |
| `preserve_thinking` | qwen3.8 默认 `true`：必须在 `reasoning_content` 字段中回传；缺失时不报错，但质量下降 | **待测**：没有对应字段；依赖 OR 把 reasoning 转成上游的 `reasoning_content`（UNVERIFIED） | — | GKQ§3.2, GKQ§7 |
| 联网搜索 | `enable_search` + `search_options`（`forced_search`、`search_strategy`、`enable_source`、`enable_citation`、`freshness` 等）；来源 `search_info`（仅 DashScope 原生接口）；Responses 的 `web_search` | **缺失**：不在 `supported_parameters` 中，`native_tools:{}`，`openrouter:web_search` 用 Exa；即使经 `provider.options.alibaba` 透传成功（UNVERIFIED），兼容模式也不返回来源 | — | GKQ§3.3, GKQ§7 |
| 其它工具 | `enable_code_interpreter`；Responses 的 `web_extractor`、`code_interpreter`、`web_search_image`、`image_search`、`file_search`、`mcp` | **缺失** | — | GKQ§3.3, GKQ§7 |
| 采样 / 解码参数 | `top_k`、`repetition_penalty`、`n`（1–4）、`parallel_tool_calls`、`tool_stream`、`vl_high_resolution_images`、逐 part 的 `fps` 和像素限制、`translation_options`、`enable_text_image_mixed`、`include_tool_usage`、`X-DashScope-DataInspection` 头 | **降级**：支持 `top_k`、`seed`、presence/frequency penalty、`stop`、`logprobs`、`response_format`/`structured_outputs`；其余都缺失 | — | GKQ§3.5, GKQ§7 |
| 多模态 | 图片；视频 URL（≤2 GB）或帧列表；Omni 音频输入（≤3 h）；qwen3.5-omni 语音输出；Qwen-Long `fileid://`；Responses `input_file`（PDF） | **降级**：图片和 `video_url` 可用；帧列表视频没有对应形式（UNVERIFIED）；音频输入只在 `qwen3.8-omni-flash` 上有，且只能 base64；**OR 上没有任何 Qwen 模型能输出音频**；没有 `qwen-long`；PDF 走 OR `file-parser` | — | GKQ§3.4, GKQ§4.9, GKQ§7 |
| 缓存 | 隐式缓存（始终开启，命中按 20% 计费）或显式 `cache_control`（写入 125%、读取 10%），二者互斥 | **改写**：OR 称 Alibaba "需要显式断点"，其模型列表已过时；多数 `alibaba` endpoint 标为 `supports_implicit_caching:false`；隐式命中是否打折 UNVERIFIED | — | GKQ§3.5, GKQ§4.8, GKQ§7 |
| Partial 续写 | `partial:true`（仅非思考模型；qwen3.8 不支持） | **待测**：OR 的 prefill 是否映射为 `partial:true` 没有文档 | — | GKQ§3.5, GKQ§7 |
| 断连 / 续流 / 状态 | 断连即停止生成，只按已生成部分计费；Responses 的 `previous_response_id`、`conversation`、session cache、GET（仅限已完成的响应）；Batch（5 折） | **改写/缺失**：Alibaba 在 OR 的**不可取消**列表里，断开后继续生成并按完整响应计费；有状态的 Responses 功能全部不可达；`:batch`（qwen3.8-max-0902、qwen3.7-plus）由 Alibaba 提供 | 与直连相比，经 OR"停止"反而更贵 | GKQ§3.8, GKQ§7 |
| 错误 / 静默截断 | `DataInspectionFailed`（输入或输出审核）、`Throttling.*`、`Arrearage`；非流式请求超过 300 s 会静默返回部分内容，并带 `x-dashscope-partialresponse: true` 头 | **改写**：折叠为 `error_type`；500 被屏蔽；这个部分响应头在 OR 上没有对应（UNVERIFIED），因此应始终用流式 | bot 总是流式请求 | GKQ§3.6, GKQ§3.7, GKQ§7 |

## 4. 测试待确认清单

以下各点在 research 笔记中标为 UNVERIFIED，或是本文的推断（已标注）。验证方法：

- 用真实 OR key，保持 bot 默认固定的第一方 provider，在请求上加 `+debug`（即 `debug.echo_upstream_body:true`；仅流式，仅 CC/RSP），查看 OR 实际发给上游的 body。
- 同时开启 `router_metadata`（`X-OpenRouter-Metadata: enabled`），查看 `pipeline[]` 和 `attempts[]`。
- 事后用 `GET /api/v1/generation?id=` 查看 `provider_responses`、`web_search_engine`、`native_finish_reason`、`cancelled`。

**通用（OR）**

- [ ] `file-parser` 设为 `engine:"native"` 而 endpoint 不支持原生文件时，结果是报错、回退到 OCR，还是静默丢弃？设为 `{"id":"file-parser","enabled":false}` 时文件如何处理？（OR§4.3）
- [ ] `engine:"native"` 搜索在没有原生搜索的 endpoint 上是否确实回退到 Exa？用 `/generation` 的 `web_search_engine` 确认。（OR§5.2）
- [ ] CC 流式中 `url_citation` annotation 的 chunk 形状（是否为 `delta.annotations`），以及图片输出是否以 `delta.images` 流式返回。（OR§4.6, OR§5.4）
- [ ] 各厂商原生的 grounding/引用细节在 CC 上是否全部丢失。（OR§5.4）
- [ ] Z.AI、Moonshot 断连后是否取消上游；不可取消的 provider 是按完整响应计费，还是只按已交付部分计费（两份文档矛盾）。（OR§6）
- [ ] `: OPENROUTER PROCESSING` keep-alive 的间隔，以及 bot 默认 `idle_timeout_secs` 900 s 在长推理时是否足够。（OR§9）
- [ ] `/models` 中的 `default_parameters`（grok-4.7 的 temperature 0.7/top_p 0.95；deepseek-v4-pro-0813 的 1/1）在请求未传时是否被注入。（OR§8.4, XD§C6）
- [ ] 请求的 `metadata` 是否转发给上游；`n` 是被拒绝还是被忽略。（OR§2.1）
- [ ] 各模型的 `image_config` 接受哪些 key。（OR§2.1）
- [ ] OR 侧审核（`is_moderated`）用的分类器，以及 `moderation` plugin 的语义。（OR§2.3, OR§7）
- [ ] `/generation/content` 是否需要 management key；`/generation` 的记录多久之后可查。（OR§8.2）
- [ ] CC assistant 消息上的 `model` 字段是否会让 OR 丢弃格式不符的 `reasoning_details`。（OR§3.3）
- [ ] 推断：auto-max 的 `max_tokens` 取自 `top_provider.max_completion_tokens`，可能高于第一方 endpoint 的上限（如 DeepSeek 目录值 943,718，官方 393,216）。固定 provider 后，OR 是拒绝路由还是截断？（OR§2.1, XD§C1）
- [ ] 推断：固定基础 slug（`only:["openai"]`、`["anthropic"]`、`["google-ai-studio"]`）时，`service_tier:"flex"`/`"priority"` 和 Claude 的 `speed:"fast"` 能否命中 tier endpoint，还是 503？（OR§2.5）

**OpenAI**

- [ ] 不支持的 effort（如 Astra + `none`）是 400 还是被映射到最近档位。（OA§10.1）
- [ ] OR 自家 key 能否拿到 `reasoning.summary`（组织验证）。（OA§10.1）
- [ ] OpenAI、Azure、Bedrock 之间的加密推理能否互相解密。（OA§10.1）
- [ ] `max_tool_calls`、`prompt_cache_key` 是否转发；`background` 字段的实际语义。（OA§10.1）
- [ ] 显式 compaction（`compaction_trigger` 输入项）能否端到端工作。（OA§10.1）
- [ ] 非 PDF 的 `input_file`（docx/xlsx/pptx）发给 OpenAI 的结果；RSP 的 `input_audio` 如何路由到 `openai/gpt-audio`。（OA§10.1）
- [ ] 原生搜索时 OpenAI 的 `search_context_size` 是否生效。（OA§10.2）
- [ ] `code_interpreter`/`shell` 生成的文件能否下载；`image_generation` 输出的 `revised_prompt`、`size` 等是否透传；`mcp` 由谁执行。（OA§10.2）
- [ ] `x-ratelimit-*` 头是否转发；OpenAI 的 `tool_usage` 是否出现在响应中。（OA§10.1, OA§11）
- [ ] `chat-latest` 是否为非推理模型、是否接受 `temperature`。（OA§1.1, OA§1.3）

**Anthropic**

- [ ] OR 向上游发送的 `anthropic-version`。（AN§10.2）
- [ ] `metadata.user_id` 是否转发。（AN§10.2）
- [ ] CC 上 thinking block 与 `reasoning_details` 的映射细节，以及 `[REDACTED]` 出现的条件。（AN§10.2）
- [ ] Fable 5.2 的前缀检查与账户绑定在 OR 上的行为。（AN§10.2, AN§11）
- [ ] MSG 不传 `max_tokens` 时是否注入 65536。（AN§11）
- [ ] `native` 搜索在 Bedrock/Vertex/Azure endpoint 上如何判断回退。（AN§10.2）
- [ ] dynamic filtering（代码执行）的结果块在多轮回放时被剥离还是被拒绝。（AN§10.2）
- [ ] 文档 citations 在 CC 上是否有表示。（AN§10.2）
- [ ] 只收 base64 的 Bedrock/Vertex 遇到 URL 图片时，OR 是否代为下载。（AN§10.2）
- [ ] 任意 beta 头能否透传；`/v1/messages` 能否用 `x-api-key` 认证和 Anthropic 原生模型 id。（AN§10.2, AN§10.3）

**Gemini**

- [ ] OR 向上游用的是 generateContent 还是 Interactions。（GE§2.9）
- [ ] 上游实际的 `includeThoughts` 值。（GE§10）
- [ ] 文本、图片、函数调用 part 上的 thought signature 能否完整往返。（GE§11）
- [ ] `image_config` 的 Gemini key（`aspect_ratio`、`image_size`）是否生效——bot 的 `1k`/`2k`/`4k`、`ar=` 依赖它们。（GE§10）
- [ ] 带签名的多轮图片编辑能否工作。（GE§10）
- [ ] 图片 `detail` 如何映射到 `mediaResolution`；`provider.options` 能否透传 `safetySettings`。（GE§10）
- [ ] Office 文档经 `file` part 是否被原生处理。（GE§10）
- [ ] 图片搜索 grounding；函数调用 `VALIDATED` 模式与 `allowedFunctionNames` 的映射。（GE§10）
- [ ] TTS 的 `responseFormat` 透传；usage 是否单独列出图片输出 token。（GE§10, GE§11）
- [ ] 断流后能否用 `/generation/content` 恢复内容。（GE§10）

**xAI**

- [ ] OR 向上游是否调用 xAI Responses API；推理摘要文本是否暴露。（XD§C2, XD§C3）
- [ ] `reasoning.enabled` 是否会在 grok-4.20 的推理/非推理 slug 之间切换。（XD§C2）
- [ ] `seed` 的实际效果。（XD§C3）
- [ ] `citations[]`、内联 `[[N]]`，以及 include 标志是否转发。（XD§D1）
- [ ] PDF 是否被原生交给 `attachment_search`。（XD§D1）
- [ ] `prompt_cache_key` 是否转成 `x-grok-conv-id`；`cost_in_usd_ticks`、`server_side_tool_usage_details` 是否透传。（XD§D1）
- [ ] webp/gif 图片发给 xAI 的结果。（XD§C3）

**DeepSeek**

- [ ] effort 如何映射到 `thinking` + `reasoning_effort`；`max` 时的默认 `max_tokens`。（XD§C2）
- [ ] 工具循环中 `reasoning_details` 是否作为 `reasoning_content` 回传（否则会 400）。（XD§C2）
- [ ] `logprobs.reasoning_content`、`prompt_cache_hit_tokens`/`prompt_cache_miss_tokens` 是否透传。（XD§C4, XD§D2）
- [ ] 各第三方 host 是否支持图片输入；`:batch` 由哪个 host 执行；中国节假日是否按谷价。（XD§C4）

**GLM / Kimi / Qwen**

- [ ] OR 访问三家时的实际上游 base URL（国际站？Qwen 在哪个地域？）。（GKQ§4.1, GKQ§7）
- [ ] `provider.options.<slug>` 能否透传 `thinking.clear_thinking`、`tool_stream`、`do_sample`、`thinking.keep`、`enable_search`。（GKQ§4.5, GKQ§5–7）
- [ ] 三家的 effort 映射（`thinking`/`reasoning_effort`/`enable_thinking`），以及 `reasoning_details` 的 `format` 是否为 `unknown`。（GKQ§4.6）
- [ ] reasoning 是否以 `reasoning_content` 回传给上游——K3 必需，qwen3.8 默认开启 `preserve_thinking`。（GKQ§4.6）
- [ ] GLM 的 `sensitive`/`network_error` 能否保留在 `native_finish_reason` 中；`content_filter` 数组是否转发。（GKQ§5）
- [ ] 旧 bot 的 GLM 对象 `{"type":"web_search","web_search":{"enable":false}}` 发给 OR 是否真的会打开 Exa 搜索；`builtin_function` 是 400 还是被静默丢弃。bot 不应发送这些，测试只为确认风险。（GKQ§4.7）
- [ ] OR 为 Z.AI 列出的 `top_k` 是否真的生效（智谱文档没有记载 `top_k`）。（GKQ§1.5, GKQ§4.5）
- [ ] K3 在 Moonshot endpoint 上发 `effort:"none"` 的结果；Moonshot 写缓存如何计费、`cache_write_tokens` 是否有值。（GKQ§6）
- [ ] OR 的 prefill 能否映射为 Kimi/DashScope 的 `partial:true`。（GKQ§6, GKQ§7）
- [ ] OR 是否为 Moonshot 下载并内联 http(s) 图片。（GKQ§6）
- [ ] 各家对 `video_url` 中 URL 的处理；DashScope 的帧列表视频。（GKQ§4.9, GKQ§7）
- [ ] Qwen 隐式缓存命中是否打折；DashScope 的部分响应头在 OR 上如何体现。（GKQ§7）
