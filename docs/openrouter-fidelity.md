# 通过 OpenRouter 的保真度说明

日期：2026-11-11。本文是 [openrouter-gaps.md](openrouter-gaps.md) 的补充。那里只列"无论怎么配置都做不到"的缺口；这里收录经 OpenRouter（下称 OR）时，请求和响应与直连官方**不同、但不算缺口**的地方，以及 bot 的规避措施。这些地方包括：路由、注入的默认值、参数映射、归一化，以及只在别的端点或 BYOK 下才有的功能。

来源记号同缺口文档 §1。"实测 2026-11-11"指维护者当天用真实请求测得的结果。

bot 经 OR 时按厂商用最接近官方原生的接口：OpenAI、xAI 走 Responses（RSP，`/api/v1/responses`），Anthropic 走 Messages（MSG，`/api/v1/messages`），其余走 Chat Completions（CC，`/api/v1/chat/completions`）。豆包另有直连火山方舟的 provider（`ark:`，见 [usage.md](usage.md)），不在本文范围内。

## 1. bot 的规避措施

默认值来自 `[providers.openrouter]` 配置和 auto-max 预设（`crates/omni-llm/src/openrouter/options.rs`、`crates/omni-llm/src/budget.rs`）。每个请求用了哪些补丁，回复 + `/info` 都能看到。

| 措施 | 请求中的体现 | 如何关闭或覆盖 | 局限 |
|---|---|---|---|
| 固定第一方 provider | `provider.only:["<slug>"]`，按模型作者映射：`openai`→`openai`、`anthropic`→`anthropic`、`google`→`google-ai-studio`、`x-ai`→`xai`、`deepseek`→`deepseek`、`z-ai`→`z-ai`、`moonshotai`→`moonshotai`、`qwen`→`alibaba`、`bytedance-seed`→`seed`；配置 `[providers.openrouter.first_party]` 可改 | `+p=<slug>`（多个：`p=a\|b`）换成白名单 `allowed_providers` 里的其它官方 endpoint；不能交回 OR 自由路由（白名单检查会拒绝） | 基础 slug 匹配同一 provider 的所有区域和变体（`xai` 也匹配 `xai/zdr/us`），但不匹配 tier endpoint；没有第一方 endpoint 的模型直接失败，不重试（如 DeepSeek 不带日期的 slug） |
| 禁止 fallback | `provider.allow_fallbacks:false` | 总是发送 | — |
| 不静默丢参数 | `provider.require_parameters:true` | 配置 `require_parameters = false` | 只按 endpoint 的 `supported_parameters` 判断，不支持就失败；管不到 OR 自己的改写（§3）和厂商"接受但忽略"的参数（如 xAI ≥4.20 的 logprobs） |
| 服务等级 | `+tier=<x>` 发 `service_tier`，并把 `<slug>/<tier>` 加进 `provider.only` | 不写 `tier` | 基础 slug 仍在列表里，tier endpoint 不可用时 OR 可能回落到普通档（看响应的 `service_tier`） |
| 关闭上下文压缩 | `plugins:[{"id":"context-compression","enabled":false}]`（OR 默认对上下文 ≤8,192 的 endpoint 开启它，从中间删消息） | 总是发送 | 账户开启 "Prevent overrides" 时无效 |
| 显式 `max_tokens` | 取 min(第一方 endpoint 的 `max_completion_tokens`, 393216, 上下文 − 估算 prompt − 余量)。prompt 用上一轮同模型回答报告的 token 数，加上之后新消息的保守估算；余量为 max(上下文/50, 1024)。仍超上下文时，按报错里的数字缩小后立即重发。Gemini 4（long decode）的值是跨 hop 的总预算：min(393216, 上下文 − 估算 prompt − 余量)，用完后回复 `/continue` 续写 | 配置 `max_tokens_cap`（0 = 不封顶）、`long_decode_budget`；没有按请求覆盖的选项 | endpoint 标的值可能低于官方上限（如 Seed），也可能不被 OR 强制（Gemini 4）；OR 按 `max_tokens` 冻结在途额度，额度很小的 key 上并发大请求可能 402 |
| 其它 auto-max 预设 | 推理强度取 `supported_efforts` 的最高档（没有档位列表的推理模型：CC 发 `reasoning.enabled:true`，MSG 发 adaptive thinking）；OpenAI 发 `reasoning.summary:"detailed"`；Claude（有该选项的模型）发 `thinking.display:"summarized"`；图片输出模型发 `modalities`。配置里的预设（如 Gemini 图片 4K）另外生效 | 单项用选项覆盖（`e=`、`budget=` 等，见 usage.md）；没有关闭全部预设的开关，也没有任意 JSON | 所选档位不在列表里、或对不能关闭推理的模型写 `e=none`：发送前本地报错，不交给 OR 映射 |
| 原样回传推理 | 同一模型、同一接口的 assistant 回合原样回放：CC 带回 `reasoning_details`，RSP 带回全部输出项（加密推理、`phase`、托管工具调用），MSG 带回原始 content block（带签名的 thinking 等） | — | 换模型时只回传文本；CC 上 OR 是否把 `reasoning_details` 转成上游的 `reasoning_content`、`encrypted_content`，见缺口文档 §2 疑似 |
| 文件只走原生 | CC、RSP 上 PDF/文档只在 `input_modalities` 含 `file` 时发送，否则报错；MSG 上 PDF 作为 document block 发送，其它文档报错。CC 上附 `file-parser` 的 `pdf.engine:"native"`，响应出现 `file` annotation 时提示"OR 自行解析了文件"（只在 CC 上检测）。纯文本文件作为独立 text 段内联（不另加提示，D63）。图片、音频、视频以 base64/data URL 发送；图片 `detail`：OpenAI 为 `original` 或 `high`，xAI 为 `high`，CC 不发；音频、视频和 YouTube 等远程视频链接只在 CC 上发送（RSP、MSG 上报错） | — | endpoint 不支持原生文件时 `engine:"native"` 的行为没有文档（bot 已先按模态拦截） |
| 搜索只用原生 | `+s` → `openrouter:web_search` + `engine:"native"`；发送前检查所固定 endpoint 的 `native_tools`，没有原生搜索就报错（否则 OR 会静默改用 Exa） | —（不接 OR 的第三方搜索引擎） | endpoint 元数据不在缓存里时先取一次；所固定的 endpoint 都要有原生搜索，否则报错 |
| 审计 | 配置 `echo_upstream_body`（默认开）→ CC/RSP 请求带 `debug.echo_upstream_body:true`，回显记入日志和 `/info`。总是发送 `X-OpenRouter-Metadata: enabled`，可看 `openrouter_metadata` 里的 pipeline、attempts | 配置关闭 | 回显只在流式 CC/RSP 上有（MSG 没有）；脱敏是 best effort |
| 会话与用户标识 | `session_id = "tg-<对话根>"`，作为 OR 的 sticky 路由键；配置了 `user_id_salt` 时发 `user = sha256(salt + Telegram user id)` | 不配置 salt 就不发 `user` | OR 会把 `user` 再哈希一次才发往上游（§3） |
| 费用与断流 | 记录 `X-Generation-Id`；没拿到用量的调用由后台查 `GET /api/v1/generation?id=` 补账；重启打断时用 `/generation/content` 取回断开前已送达的文本（需开启 Input & Output Logging）；Gemini 4 long decode 的 checkpoint 落盘，断流或重启后在原消息上接着写 | — | 其它模型断线后只能重新生成（缺口文档 §2） |

## 2. 路由与账户设置

- **默认路由**：不固定 provider 时，OR 每次按价格、吞吐、延迟、错误率等加权随机选 endpoint，失败就自动换 provider。在首个 token 之前，甚至在 HTTP 200 之后，它也会静默重试。带 tools 的请求会被 Auto Exacto 重排；开源模型的 priority endpoint 也参与竞争。对话中途换 provider 会丢缓存，还可能让推理签名失效。（OR§2.5, OR§11.2 #1–3）
- **第三方与量化部署**：开源权重模型大多落到 fp4/fp8 的第三方。例如 `moonshotai/kimi-k3` 有 28 个 endpoint，第一方列第 24 位；`z-ai/glm-5.3` 有 47 个；DeepSeek 不带日期的 slug 没有第一方 endpoint（`deepseek/deepseek-v4-pro` 实为 0423 预览版权重）；Kimi K2.5 及更早的模型只有第三方。闭源模型也会落到 Azure、Bedrock、Vertex，它们的参数集、原生工具和可取消性都不同。例如 Bedrock 上的 OpenAI 模型没有原生工具，忽略 `reasoning.mode:"pro"`，async 工具同步执行，也没有 `seed`。（OR§0, OA§10.0, XD§C1, GKQ§4.2–4.4）
- **第一方 endpoint 之间的差异**：
  - Gemini 的 AI Studio 和 Vertex 都是 Google 官方，但能力不同：YouTube 链接只在 AI Studio 上可用；3.8 Flash 的 `stop` 只在 Vertex 上有，而 Vertex 又缺 `temperature`/`top_p`；Vertex 上 3 Pro Image 最高 2K；continuation token 绑定签发它的 provider。
  - xAI 的 `xai/zdr/us`（1.1× 价格）没有原生搜索。
  - Alibaba 在 OR 上也托管 GLM、Kimi，但它不是这两家的官方。

  （GE§10, XD§C5, GKQ§4.1）
- **服务等级是独立 endpoint**：`flex`、`priority`、`ultrafast` 都是单独的 endpoint（如 `openai/flex`、`google-ai-studio/flex`、`xai/priority`、`anthropic/fast`），基础 slug 不匹配它们。`ultrafast` 依次回落到 priority、default；`:nitro` 会放进 priority endpoint（按 priority 价计费），`:floor` 会放进 flex endpoint。（OR§2.4–2.5, OA§10.1）
- **sticky 路由与缓存**：缓存命中后，同一账户、模型、对话会留在同一个 endpoint。空闲 60 分钟失效，设置 `provider.order` 时关闭，Auto Exacto 也可能打断它。缓存按 provider 隔离。（OR§10）
- **账户和工作区的默认值**会叠加到每个请求上：默认的 provider 排序与允许/忽略列表、默认 service tier、ZDR（开启后 OpenAI 只剩 Azure）、数据政策、默认 plugin（开启 "Prevent overrides" 时请求无法关闭）、preset，以及 guardrails（预算、模型白名单、提示注入正则、敏感信息脱敏）。bot 用的 key 应放在设置中性的独立 workspace 里；`openrouter_metadata.pipeline[]` 能看到实际生效的阶段。（OR§2.3, OR§11.2 #7–9）

## 3. OR 对请求的改写

**实测注入（实测 2026-11-11，来自 `echo_upstream_body`）**

- 不传 `max_tokens` 时，OR 发 65536，DeepSeek、GLM 为 131072。官方的默认值各不相同：DeepSeek 思考模式 64K（`max` 时 128K），智谱 65536，豆包 4096 且静默截断。bot 总是显式发送（§1）。
- GLM 被注入了模型 `default_parameters` 里的 `temperature=1`、`top_p=0.95`，与智谱官方默认值相同。OR 文档说不传的采样参数不会被填充，这里正好相反。
- Gemini 被注入了全部类别的 `safetySettings: OFF`（与 Google 2.5+ 的默认相同），以及 `includeThoughts`。
- OpenAI、xAI 的 CC 请求以 Responses API 发往上游。
- `verbosity:"max"`：OpenAI 变成 `"high"`；Claude 变成 `output_config.effort`，并覆盖 `reasoning.effort`。

**文档写明的改写**

- **Claude**：
  - 默认注入 `thinking.display:"summarized"`（官方 5.x 默认是 `omitted`）；Sonnet 5.5 不设推理时注入 `{type:"adaptive",display:"summarized"}`；
  - 流式请求的每个工具都加 `eager_input_streaming:true`；不带 structured-outputs beta 头时，剥离工具的 `strict:true`；
  - Claude 4.7/5/5.5 的 `budget_tokens`/`reasoning.max_tokens` 被接受后忽略（直连官方会 400）；对 mandatory 模型关闭推理，得到的是 OR 自己的 400；`~…-latest` 别名把关闭推理强制改为最低档；`minimal`→`low`；
  - 只透传两个 `anthropic-beta` 值，其余 beta 靠 OR 自动识别功能后附加。

  （AN§10.2, AN§11, OR§11.2 #16）
- **OpenAI**：自动请求加密推理；fallback 到不支持的 provider 时剥离 `reasoning.mode`、`prompt_cache_breakpoint`、`prompt_cache_options`；`reasoning.mode:"pro"` 改为路由到 `*-pro` slug；顶层 `max_tool_calls` 变成 OR server-tool 的步数预算（默认值即上限 30）。（OA§10.1, OR§11.1）
- **智谱**：OR 给 Z.AI 附带由账户和 `session_id` 派生的 session affinity key。（OR§10）
- **提示与结构**：对非 OpenAI 模型，消息的 `name` 会被拼成 `{name}: {content}`（bot 不发 `name`）；provider 没有原生 tool calling 时，tools 被渲染成 YAML 提示模板（`require_parameters` 会排除这类 endpoint）；部分 provider 会合并 system 消息或相邻的 user 消息。（OR§2.1, OR§11.2 #12–14）
- **文件解析**：`file-parser` 优先用模型的原生文件能力，没有时用 `mistral-ocr`（$2/1000 页，美国区 $2.2，BYOK 也收费；最多转发 8 张抽取的图片），或免费的 `cloudflare-ai`。OR 没有"不能原生就报错"的开关。DeepSeek、GLM、Kimi、Qwen、Seed 的模型在 OR 上都不列 `file` 输入。（OR§4.3, GKQ§4.9, DB§1.2）
- **搜索**：`engine:"auto"` 只在 endpoint 的 `native_tools` 含搜索时用原生，否则用 Exa；`engine:"native"` 在没有原生搜索的模型上同样回退到 Exa，不报错（只有 workspace 的引擎列表只含 `native` 时才 403）。只有 OpenAI、Anthropic、Google、xAI、Perplexity 有原生搜索；Anthropic 的原生搜索只在 `anthropic`、`claude-on-aws` endpoint 上有。Google 原生搜索不支持域名过滤：带过滤时 `auto` 静默改用 Exa，`native` 报 400。（OR§5.1–5.2）
- **工具对象**：任何 `{"type":"web_search",…}` 工具都会变成 OR 的 `openrouter:web_search`。旧 bot 给智谱发的"关闭搜索"对象 `{"type":"web_search","web_search":{"enable":false}}`，经 OR 反而可能打开 Exa 搜索。所以绝不要把厂商的原生工具对象发给 OR。（GKQ§4.7）
- **多模态传输**：音频只能 base64，RSP 的音频只收 wav/mp3；视频只收 mp4/mpeg/mov/webm，YouTube 链接只在 Gemini + AI Studio 上可用，MSG 不收视频；图片 `detail:"original"` 在不支持的 endpoint 上降为 `high`。（OR§4.1）
- **Gemini 4 long decode**：早先轮次的 continuation item 会被移除，continuation hop 上跳过 `cache_control`。（OR§11.1）

**推理强度映射**：不支持的非 `none` effort 被映射到最近的档位（直连 OpenAI 等会 400）。只支持 budget 的模型把 effort 换算成输出上限的比例：max/xhigh≈0.95、high 0.8、medium 0.5、low 0.2、minimal 0.1。Gemini 的 `thinkingLevel` 1:1 映射，`xhigh`→`high`；Gemini 3 的 `thinkingBudget` 由 Google 自己映射到档位，Gemini 4 不支持 budget。

`/models` 里的推理元数据与官方有出入：

- Opus 5.5 的默认值标为 `high`（官方是 `medium`，而且不设置时 OR 什么都不发）；Sonnet 4.6 标为 `medium`（官方 `high`）；
- glm-5.2 标为 `xhigh,high,none`、默认 `high`（官方默认 `max`）；
- K3 提供 `none`（官方始终思考）；qwen3.8-max 标为 mandatory（官方是混合模式）；Nano Banana 2.1 缺 `medium`。

bot 默认发这里列出的最高档，`e=` 的可选值也以这里为准，所以要注意这些差异。（OR§3.1, AN§11, GE§10, GKQ§5–7）

**用户标识**：`user`/`safety_identifier` 被折进 OR 的哈希身份后才发往上游，从不原样转发。不传时 OR 用账户级身份，一个用户触发的策略封禁可能殃及整个账户。厂商按用户区分的功能（OpenAI 和 Kimi 的 `safety_identifier`、智谱的 `user_id`）因此只能看到 OR 的哈希；DeepSeek 的 `user_id` 见缺口文档疑似。（OR§2.1, OA§10.1, GKQ§4.5）

## 4. 只在 Responses / Messages 端点上有的功能

这些功能在对应端点上都能用，所以不是缺口。bot 对 OpenAI、xAI 走 RSP，对 Anthropic 走 MSG，已用到其中的原样回放（RSP 的输出项含 `phase`，MSG 的 thinking block 与签名）、`thinking.display` 和 OpenAI 托管的 `code_interpreter`（`+c`），其余没有接；其它厂商走 CC，用不到这些。

- **OpenAI（RSP）**：
  - assistant 消息上的 `phase`（GPT-5.3-codex 起）：CC 无法表示，OR 称这会导致 "significant performance degradation … early stopping"；bot 走 RSP 并原样回放，不受影响；
  - GPT-6 的 async 工具（CC 上被剥离）；
  - OpenAI 托管工具：`code_interpreter`、hosted `shell`、`image_generation` 工具、`mcp`、`computer_use_preview`。CC 的 `tools[]` 只收 function、`openrouter:*` 和 web_search 简写；
  - `include`、`truncation`、`instructions`、`openrouter:apply_patch`。

  反过来，CC 上有 `configuration_update`（OR 扩展，只能改 effort）和音频输出（只在 CC 上，且必须流式）。（OR§1, OA§10.1）
- **Anthropic（MSG）**：
  - 原生 thinking/redacted_thinking block 与签名：CC 把它们压平为 `reasoning_details`，流式时加密推理可能显示为 `[REDACTED]`；
  - `thinking.display`（`omitted`、`updates`），以及 Sonnet 5.5 的 `thinking.type:"between_tools"`；
  - `block_binding` 与 `input_transformations`；
  - 对话中途的 system 消息与 `tool_addition`/`tool_removal`（CC 只有改 effort 的 `configuration_update`）；
  - 文档 citations（`char_location` 等）；原生搜索的结果 block 与 `web_search_result_location`/`encrypted_content`（CC 压平为 `url_citation`）；
  - `context_management`（clear_tool_uses、clear_thinking、`compact_20260112`）、`speed`、`openrouter:bash`。

  MSG 没有 `debug` 回显，也不收视频。（AN§10.2, OR§1）
- **RSP 自身的局限**：没有音频输出；`input_audio` 只收 wav/mp3；忽略 `reasoning.exclude`；块级 `cache_control` 只能写成 `prompt_cache_breakpoint`，TTL 会丢失（变成默认 5m）。（OR§1, OR§10）

## 5. 响应与错误的归一化

- **`finish_reason`**：
  - 只剩 `stop`/`length`/`tool_calls`/`content_filter`/`error` 五种，原值放在 `native_finish_reason`（如 DeepSeek 的 `insufficient_system_resource`/`aborted`，Gemini 4 的 `CONTINUATION`/`MAX_TOKENS`）；
  - 上游的 token 上限类错误被改写成 `length`；拒答变成 `message.refusal` + `content_filter`；
  - 最后的 usage chunk 带非空 `choices`，重复一遍 finish_reason；
  - 实测 2026-11-11：一个 Gemini 响应以 `finish_reason:"error"`、`native_finish_reason:"RECITATION"` 结束，没有 error 对象。

  bot 同时记录两者。（OR§6, XD§C4）
- **错误**：
  - 上游一旦返回 header，HTTP 状态就锁定为 200，之后的失败都在流内：CC 是带 `error` 的 chunk + `finish_reason:"error"`，RSP 是 `response.failed`。非流式的 200 也可能只含 `error`；
  - 错误被映射为 OR 的 `error_type`，RSP 的 `error.code` 大多合并为 `server_error`。500 类错误的 `message` 被换成通用文本，并去掉 `provider_code`；只有非 500 错误带 `metadata.raw`；
  - 402 永远指 OR 余额，厂商自己的余额、账单错误不会出现；Anthropic 的 `req_…` 变成 `gen-…`；
  - Gemini 的安全拦截从 200 + `finishReason:SAFETY` 变成 403 `content_policy_violation`；OpenAI 的 bio/cyber 拦截变成 403 `refusal`，不重试，也不换 provider。

  原始错误体可以事后从 `/generation/content` 取（需 I/O Logging）。bot 保存完整的错误 JSON，路由死路不重试。（OR§6–7, GE§11）
- **用量**：统一为 OR 的 `usage`（`cost`、`cost_details`、`cached_tokens`、`cache_write_tokens`、`reasoning_tokens`、`server_tool_use_details`），替代 OpenAI 的 `tool_usage`、xAI 的 `cost_in_usd_ticks`/`server_side_tool_usage_details`、DeepSeek 的 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens` 等原生字段。`usage.cost` 就是 OR 实际扣的钱。`GET /api/v1/generation?id=` 用普通 key 就能查，其中 `usage` 是实际收费，另有 `cancelled` 标志（实测 2026-11-11）。（OR§8, XD§D1–D2）
- **引用**：原生搜索的结果统一为 `url_citation`（`url`、`title`、`content`、`start_index`、`end_index`；`content` 是 OR 加的摘录）。Claude 的原生引用 block 用 MSG 可以保留；Gemini、xAI 的厂商字段是否保留，见缺口文档疑似。（OR§5.4）
- **服务等级**：响应的 `service_tier` 是实际服务的档位。`fast` 报告为 `priority`；Claude fast mode 也报 `priority`，而 Anthropic 自己会写 `standard`。（OR§11.1, AN§11）

## 6. 断连、取消与计费（按厂商）

| 厂商 | bot 固定的 slug | 断连后 OR 的处理 | 官方直连：断连 / 续流 / 异步 |
|---|---|---|---|
| OpenAI | `openai` | 取消上游（实测 2026-11-11），文档称同时停止计费；Bedrock 上不取消 | 同步请求断连即取消；`background` + `starting_after` 续流、`cancel`、webhooks（缺口文档 §2） |
| Anthropic | `anthropic` | 取消上游（实测 2026-11-11）；Bedrock、Vertex 上不取消 | 无法重新接上流（断连后是否停止生成，官方没写）；Message Batches；Message Threads（beta） |
| Gemini | `google-ai-studio` | **不取消**，上游继续生成，但 OR 只记录、只收取断开前已送达的部分（实测 2026-11-11）；Gemini 4 long decode 可从 checkpoint 续写（实测 2026-11-11） | generateContent 断连即取消，Gemini 4 在第一个 checkpoint 前断流不收费；Interactions 的 `background` + `last_event_id` 重放 |
| xAI | `xai` | 取消上游（实测 2026-11-11）；Bedrock 上不取消 | 没有流续传；deferred CC；stored responses |
| DeepSeek | `deepseek` | 取消上游（实测 2026-11-11） | 没有任何异步或续流手段（断连后是否计费，官方没写） |
| 智谱 GLM | `z-ai` | 未测（不在 OR 文档的两张表里） | 国内站有异步 chat；没有流续传 |
| Kimi | `moonshotai` | 未测（同上） | 断连后仍完整计费；可用 Partial Mode 续写 |
| Qwen | `alibaba` | **不取消**，只收取已送达的部分（实测 2026-11-11） | 断连即停止，只按已生成部分计费；`partial` 续写（仅非思考模型） |
| 豆包 | `seed` | 未测（同上） | 方舟：断连即停止，按已生成部分计费；非流式请求超时也计费 |

- OR 文档自相矛盾：streaming 页说不可取消的 provider "按完整响应计费"，broadcast 页说只计断开前已送达的部分。实测支持后者。非流式请求从不取消（bot 总是流式）。
- 断流后能查到的：`GET /api/v1/generation?id=` 的费用与 `cancelled`。开启 Input & Output Logging 时，`/generation/content?id=` 约 9 秒后可取，但只有断开前已送达的文本（实测 2026-11-11）。不可取消的 provider 在断开后继续生成的那部分，取不回来。
- Gemini 4 的计费差异：直连时第一个 checkpoint 前断流不收费；经 OR 则按已送达的部分收费。

（OR§6, OR§9, OA§5, AN§9, GE§2.9, XD§A10, XD§B10, GKQ§1.8, GKQ§2.9, GKQ§3.8, DB§8）

## 7. 只有 BYOK 才能用的功能

用自己的厂商 key（BYOK；超出免费额度后另收官方价的 5%）时 OR 才能做到，因此不算缺口：

- **OpenAI**：`file_search`（OpenAI vector stores）；`code_interpreter` 的显式容器与 `file_ids`；OpenAI Files（`/api/v1/files?provider=openai`）的 file id；`access_programs.cyber` 的非 `standard` 值；依赖组织资格的功能（推理摘要所需的组织验证、Daybreak、Scale tier、数据驻留、项目级 service-tier 策略）。用 OR 自己的 key 能否拿到推理摘要、BYOK 后这些功能是否与直连完全一致，都未验证。（OA§10.1–10.2, OA§11）
- **Anthropic**：Files API（`/api/v1/files?provider=anthropic`）的 file id。（AN§10.2）
- 附带：Mistral OCR 在 BYOK 下照常收费；`/generation` 的 `upstream_inference_cost` 只对 BYOK 有值。（OR§4.3, OR§8.1）

## 8. 按厂商的其它差异

- **OpenAI**：加密推理按 provider 分格式（`openai-responses-v1`、`azure-…`、`bedrock-…`），互相能否解密未知，所以要固定单一 provider；`prompt_cache_key` 兼作 OR 的 sticky key，是否转发给 OpenAI 未知；`prompt_cache_options` 在格式转换时丢 TTL。（OA§10.1, OR§10）
- **Anthropic**：
  - 经 OR 时，Opus/Sonnet/Haiku 5.5 和 Fable 5.1 "不执行"前缀检查：同一段改过的历史，直连会 400，经 OR 却会成功；
  - Sonnet/Haiku 5.5 的账户绑定 thinking 被 OR 固定到能读取它的 provider 组，`only`/`ignore` 排除了它就 400；Fable 5.2 的前缀检查与账户绑定在 OR 上的行为没有文档（bot 固定 `anthropic`，账户绑定不会成问题）；
  - `speed:"fast"` 没固定 endpoint 时会回落到标准 endpoint，按标准价计费；
  - OR 向上游发哪个 `anthropic-version` 未知（Sonnet 4.6 接受 temperature，疑为 `2023-06-01`）；
  - 目录仍把 Sonnet 4.5 标为 1M 上下文（官方 beta 已退役，回到 200K）。

  （AN§10.2, AN§11）
- **Gemini**：
  - 上游格式是 generateContent（echo 里能看到 `safetySettings`、`includeThoughts`）；
  - long decode（实测 2026-11-11）：endpoint 标的最大输出是 32768，但 OR 接受更大的预算，`max_tokens` 是跨 hop 的累计预算。每个 hop 以 `finish_reason:"length"` 结束，`native_finish_reason` 为 `CONTINUATION`（预算还有剩余）或 `MAX_TOKENS`（预算用完），并带一个 `id:"gemini-continuation"` 的 `reasoning.encrypted` 项。把它作为最后一条 assistant 消息回传，就能从句子中间接着写（`MAX_TOKENS` 之后要调高 `max_tokens`）。流式的 checkpoint 让断流后也能续；
  - OR 缓存文档里的价格（0.25×）已过时。

  （GE§10）
- **xAI**：推理以加密形式回传（`xai-responses-v1`）；目录把 grok-4.20 标为 2M 上下文（官方 1M）；OR 接受 webp/gif 图片，而 xAI 文档只收 jpg/png。（XD§C1–C3）
- **DeepSeek**：只有两个带日期的 slug 是官方节点；官方没有 PDF 输入，OR 会用 `file-parser`（默认 Mistral OCR）代为解析，bot 则拒绝发送；峰谷价由 OR 按时段的价格 override 体现。（XD§C1, XD§C4）
- **GLM / Kimi / Qwen**：
  - OR 接的都是国际站：GLM 的 Z.ai 在 SG；Qwen 对应哪个 DashScope 地域未知；
  - K3 的采样参数是固定值，OR 列为支持，但非默认值会在上游 400（与直连相同）；
  - Moonshot 不收公网图片 URL（bot 总是发 base64）；DashScope 的非流式请求超过 300 s 会静默返回部分内容（bot 总是流式）；
  - 缓存计费可能与官方不一致：Moonshot 官方对 K3 的缓存写入收费，OR 文档却说免费；多数 `alibaba` endpoint 标 `supports_implicit_caching:false`，隐式命中是否打折未知。

  （GKQ§4.8, GKQ§6–7）
- **豆包（经 OR，即 BytePlus）**：endpoint 标 `fp8`，但 OR 的 Seed provider 就是 BytePlus 本身，官方精度没有公开，无从比较；Seed endpoint 标 `supports_implicit_caching:false` 且没有缓存读取价，而 BytePlus 官方对缓存输入打折，实际是否打折待测。（DB§1.2, DB§9.2 P6）

## 9. 不算缺口的常见情况

- **客户端能复现**：
  - `n>1` 用多次请求代替（xAI Imagine 的 `n` 同理）；
  - 豆包的 PDF：方舟本身就是把每页转成图片再走视觉，客户端可以自己转成页图发送；
  - 豆包视频的 `fps`：客户端抽帧，并在每帧前加 `"<t> second"`，与方舟的做法相同。

  （DB§9.3）
- **官方本身也不支持或会忽略**：Seed 2.x 的 penalty、`stop`、logprobs、`logit_bias`、`seed`、`n`；Seed 2.1 的原始 CoT（方舟也只给摘要）；Gemini 3.6+ 的 `temperature`/`topP`/`topK`；xAI ≥4.20 的 logprobs；DeepSeek 思考模式下的 `temperature`。（DB§9.3, GE§1.4, XD§A3, XD§B3）

## 10. 待实测（非缺口）

- OR 的 `: OPENROUTER PROCESSING` keep-alive 间隔（bot 每个请求结束时在日志里记 `longest_gap_ms`），以及 bot 的 `idle_timeout_secs`（3600 s）对长推理是否足够。（OR§9）
- CC 流式里 `url_citation` annotation 的 chunk 形状（是否为 `delta.annotations`），以及图片输出是否以 `delta.images` 流式返回。（OR§4.6, OR§5.4）
- CC 上 Claude thinking block 与 `reasoning_details` 的映射细节，以及 `[REDACTED]` 出现的条件。（AN§10.2）
- 用 OR 自己的 key 能否拿到 OpenAI 的推理摘要（组织验证）。（OA§10.1）
- `+tier=` 时是否真由 tier endpoint 服务（看响应的 `service_tier`）。（OR§2.5）
- Z.AI、Moonshot、Seed 断连后是否取消上游、怎样计费。（OR§6）
- xAI 收到 webp/gif 图片时的结果。（XD§C3）
- 缓存计费：Moonshot K3 的缓存写入、Qwen 的隐式命中、Seed 的缓存输入。（GKQ§6–7, DB§9.2 P6）

### 实测发现的行为差异（2026-11-11）

- **超过上限的 `max_tokens` 被悄悄接受**：Messages 接口给 Claude Haiku 5.5 发 `max_tokens: 500000`（官方上限 128K），OR 照常生成；官方 API 会返回 400。bot 自己从不发超过上限的值（fixture `msg_max_tokens_too_big`）。
- **上下文超长在 OR 侧就被拒**：CC 和 MSG 都由 OR 先按 endpoint 的上下文长度检查，返回 400 "This endpoint's maximum context length is …"，请求不会到达提供方，也不计费（fixtures `cc_context_too_long`、`msg_context_too_long`）。
- **余额检查按 max_tokens 预估**：可用余额付不起 `max_tokens` 的最大费用时直接 402 "requires more credits, or fewer max_tokens"，错误信息里带有 key 设置页的链接（fixtures `cc_cannot_afford`、`rsp_cannot_afford`）。
- 各种出错情况的原始响应见 `crates/omni-llm/tests/fixtures/errors/`，录制脚本 `tools/record-openrouter-errors.py`。

