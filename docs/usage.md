# 用户手册

## 1. 开始对话

消息第一行里第一个 `$` 之前是**前缀**，之后是发给模型的正文，**原样发送**：通常写成 `c$你好`；`c$ 你好` 发出的正文开头就带一个空格。

`$` 是默认的**前缀标记**。部署时可以换成别的（`general.trigger_marker`，示例配置从环境变量 `OMNI_TRIGGER_MARKER` 读，例如 `%`、`¥`、`>>`，比如和正式 bot 在同一个群里的测试 bot），换了之后本文里所有的 `$` 都读作新标记，`/help`、`/models` 和命令说明也会跟着显示新标记。标记不能含字母、数字、空白和 ``,+{}"=/-.:_\@` `` 这些前缀本身要用的字符。

```
<spec>[,<spec>...]$<正文>
spec   := <模型>[+<选项>...]        （空 spec = 名字为空的前缀）
模型   := 前缀（alias） | OpenRouter 模型 ID | provider:模型 ID（直接写模型 ID 的条件见下）
选项   := 字母（可连写，如 4W） | key=value
```

- `$正文`：名字为空的前缀，和其它前缀一样定义（配置里 `[[aliases]] name = ""`，或管理员 `/alias $ <spec>`）。没有定义它的 bot 不认这个前缀。
- `alias$`：前缀，即配置文件 `[[aliases]]`（所有 bot 共用）或管理员 `/alias`（只对执行它的那个 bot）定义的快捷名；`/models` 列出当前 bot 的全部前缀（只列它有的模型）。前缀指向哪个平台完全由配置决定：示例配置里的前缀都直连官方 API（`c$` = `anthropic:claude-opus-5-5`），前面加 `o` 的同名前缀是同一个模型经 OpenRouter（`oc$` = `openrouter:anthropic/claude-opus-5.5`，`o$` 对应空前缀）；这只是示例配置的命名，bot 对前缀的名字没有任何约定。
- `author/model$`：直接写 OpenRouter 模型 ID（另有前缀的模型也可以），新模型无需改配置。需要 `[providers.openrouter].allow_unlisted_models = true`（代码默认 false，示例配置为 true），而且模型在该 bot 的 `[[bots]].models` 里；否则只有前缀能用到 OpenRouter。群里只接受目录里存在的 ID（避免误触发）；本地目录里没有时会先立即刷新一次目录再判断，所以刚上线的模型也能马上用。私聊会直接把错误告诉你。
- `<provider>:<模型 ID>$`（如 `openai:gpt-6.1-sol$`、`ark:doubao-seed-2-1-pro-260915$`）：直连该平台的官方 API，需要它配了 key，而且它的 `allow_unlisted_models = true`（默认 false：只能用前缀），见下文"直连各平台的官方 API"。
- 多个 spec 用逗号分隔，会并行生成多条回答（上限见 `max_parallel_specs`）。逗号之间留空表示空前缀：`c,,g$` = c、`$`、g 各答一次，`,c$` = `$` 和 c。多 bot 部署时，每个 bot 回答它有的那些 spec（几个 bot 都有的，各答一次）。
- 第一行没有 `$`：不触发（私聊里回复一条用法提示）。群里：bot 不认识的 spec 直接跳过，一个都不认识就不响应（可能是别的 bot 的前缀，也可能只是普通文字）；空 spec 和其它 spec 混用时，其它 spec 必须像模型名（只含字母、数字和 `._:/-`）才算前缀，所以 `, costs 5$` 这样的文字不会触发。私聊里第一行只要有 `$` 就当作前缀，不认识的直接报错（`it costs 5$` 这样的文字也会）。

### 选项

选项写在前缀里 `+` 后面，可以连写（`gi+4W$` = 4K + 16:9）。用法按各家**官方 API** 理解：你面对的是官方模型，OpenRouter 只是通道（直连官方 API 时通道就是官方，见"直连各平台的官方 API"）。每个选项在发出前都会本地校验：

- 官方 API 没有的功能/取值：直接报错（如 `GPT 没有 URL 抓取`）；
- 官方有、但经 OpenRouter 做不到：报错"OpenRouter 不支持 …（官方 API 有）"；
- 不认识的选项：报错。绝不把请求交给 OpenRouter 去"纠正"或静默丢弃。

| 选项 | 作用 |
|---|---|
| `s` | 厂商自己的网页搜索（OpenAI、Claude、Gemini、Grok；DeepSeek、智谱、Kimi、千问的官方搜索只有直连能用，经 OpenRouter 不可用） |
| `c` | 厂商自己的代码执行（直连：OpenAI、Claude、Gemini、Grok、千问；经 OpenRouter 只有 OpenAI 可用，且执行输出与生成的文件不返回） |
| `u` | 让模型读取消息里的网址（Claude 的 web_fetch；Gemini 的 URL context、千问的 web_extractor 只有直连能用） |
| `m` | Google Maps（只有直连的 `google:` 模型；对话里最新的位置作为用户所在位置，见"图片、文件、相册、位置、富消息"） |
| `i` | 在聊天里生成图片：厂商自己的出图工具（直连的 OpenAI、Grok 聊天模型，`image_generation`）。直接写出图模型的 ID 不需要它，见"生成图片和视频" |
| `e=<档位>` | 推理强度，取值为该模型官方支持的档位（报错时会列出）；`e=none` 关闭推理（仅限官方允许关闭的模型） |
| `budget=<n>` | 思考 token 预算（仅限官方支持预算的模型；与 `e=` 二选一） |
| `v=<low\|medium\|high>` | 回答详细程度（OpenAI） |
| `1` `2` `4` | 图片尺寸 1K / 2K / 4K（出图模型；视频模型 `1` = 720p、`2` = 1080p；各模型的具体像素见"生成图片和视频"） |
| `S` `W` `H` `w` `h`，`ar=<w:h>` | 图片比例 1:1 / 16:9 / 9:16 / 4:3 / 3:4；`ar=` 可写模型官方支持的任意比例（如 `ar=21:9`） |
| `q=<档位>` | 出图质量，取值为该模型官方支持的档位（默认最高） |
| `n=<张数>` | 一次生成几张（官方支持的出图模型；默认 1） |
| `bg=<auto\|opaque\|transparent>` | 图片背景（官方支持的出图模型） |
| `sec=<秒>` | 视频长度（视频模型） |
| `p=<slug>` | 换成该模型的另一个官方提供方，如 Gemini `p=google-vertex`、GPT `p=azure`、Claude `p=amazon-bedrock`；`p=a\|b` 给出多个（白名单见配置 `allowed_providers`） |
| `tier=<等级>` | 服务等级：`flex`（更便宜）谁都能用；`priority`/`fast` 等更贵的等级只有 `[access].priority_users` 里的人能指定；`tier=default`/`auto` 也接受，但不改变任何东西 |

### 默认参数（能力拉满）

不带选项时，请求就是官方 API 允许的"最大"配置：

- 推理强度 = 该模型支持的最高档（只有开关的推理模型则开启推理）；OpenAI 推理摘要 = detailed；Claude 思考显示 = summarized（官方默认 omitted 会看不到思考）；Gemini 带上思考摘要；
- 输出上限 = 该模型的最大输出，但不超过上下文窗口剩余空间（经 OpenRouter 另有上限 393216，因为它按 `max_tokens` 冻结额度）；Gemini 4 long decode 为跨多次续写的总预算（回复 `/continue` 继续）；
- 图片输入用官方最高精度（OpenAI `detail` = original 或 high，xAI = high，方舟 = xhigh，直连 Gemini = 最高的 media resolution；经 OpenRouter 走 Chat Completions 的其它厂商不发 `detail`）；
- 出图模型同时输出图片和文字；配置预设（如 Gemini 出图默认 4K）。

经 OpenRouter 时还有始终生效的**保真措施**：固定厂商自己的 endpoint（`p=` 只能换成白名单里的其它官方提供方，不能交给 OpenRouter 自由路由）、禁止 fallback、`require_parameters`、关闭 context-compression；PDF 强制原生解析。每个请求实际发送了什么，用 `/info` 查看。

各模型走 OpenRouter 的哪种接口：OpenAI、xAI 用 Responses API，Anthropic 用 Messages API（与官方原生接口一致），其余用 Chat Completions（这些厂商官方提供的 OpenAI 兼容格式）。

### 直连各平台的官方 API

配置了 key 的平台（`[providers.<id>]`，见示例配置）才有它的模型，写成 `<id>:<模型 ID>`（如 `openai:gpt-6.1-sol`、`deepseek:deepseek-v4-pro`），回复第一行也这样显示，经 OpenRouter 的则是 `author/model`，一眼能看出走的哪条路。**没有 key 的平台（OpenRouter 也一样）不提供它的模型**：`/models` 不列出它的前缀；直接写它的模型 ID 或用指向它的前缀，回复"没有配置"的错误（群里直接写 `author/model$` 这种 OpenRouter ID 时不回复，因为可能只是普通文字）；之前用它的对话继续时也报这个错。bot 照常运行，配上 key 重启就有了。直接写 `<id>:<模型 ID>$` 需要该 provider 的 `allow_unlisted_models = true`（默认 false：只能用前缀），而且只接受 bot 模型表里有的 ID。国内厂商接国内官网（和旧版一样）。

- 选项和上面一样按官方 API 校验，官方没有的报错；`p=` 只用于 OpenRouter，在这里报错。默认参数同样是官方允许的"最大"（各平台的具体值见下）。
- 费用：各平台的响应基本都不带费用（xAI 例外），统计和限额用 bot 内置的价目表（按日期标注来源）估算：未缓存输入、缓存读/写、输出（推理按输出计），以及搜索等工具按次的费用；人民币价目按 `cny_per_usd` 折算。**只支持 bot 模型表里的模型**：表里没有的 ID 价格未知，直接不支持（防止被滥用）；即将下线和已经下线的模型也不在表里。价目表里没有的额外收费（如 Claude 代码执行的容器时间）在页脚费用后标注（`≈$0.008 + code execution`）；没有公布价格的功能不支持。
- 更贵的服务等级（`tier=priority` 之类、Anthropic fast、Moonshot 的 `-highspeed` 模型）只有 `[access].priority_users` 能用。
- `+s` 要求了搜索、但 API 报告这次没有搜索时，回答里注明（搜不搜由模型决定）。
- 之前回合里**别的模型生成的图片、文件**：接口允许放进 assistant/model 消息的就放进去（Gemini、xAI），不允许的直接报错（不会偷偷丢掉，也不会改放进 user 消息）；要让这个模型看这张图，就把图作为附件发给它。

| id | 平台 | 接口 | key（环境变量） |
|---|---|---|---|
| `openai` | OpenAI | Responses API（`store: true`，和旧版一样；每轮仍原样回传全部输出项和加密推理，不依赖 OpenAI 存的副本） | `OPENAI_API_KEY` |
| `xai` | xAI（Grok） | Responses API（同上，带加密推理） | `XAI_API_KEY` |
| `anthropic` | Anthropic（Claude） | Messages API（`anthropic-version: 2026-10-21`） | `ANTHROPIC_API_KEY` |
| `google` | Gemini API（AI Studio） | generateContent（流式） | `GEMINI_API_KEY` |
| `deepseek` | DeepSeek（`api.deepseek.com`） | Chat Completions；`+s` 走它的 Anthropic 兼容接口 | `DEEPSEEK_API_KEY` |
| `zhipu` | 智谱 BigModel（`open.bigmodel.cn`） | Chat Completions | `ZHIPUAI_API_KEY` |
| `moonshot` | Moonshot / Kimi（`api.moonshot.cn`） | Chat Completions；`+s` 走它的 Responses API | `MOONSHOT_API_KEY` |
| `alibaba` | 阿里云百炼（DashScope，北京） | OpenAI 兼容 Chat Completions；`+s` `+u` `+c` 走它的 Responses API | `DASHSCOPE_API_KEY` |
| `ark` | 火山方舟（豆包） | Chat API | `ARK_API_KEY` |

**OpenAI**：`chat-latest`（`$`）、`gpt-6.1-sol`、`gpt-6-astra`、`gpt-5.6-sol` 等（官方 ID，`openai/gpt-chat-latest` 在官方叫 `chat-latest`）。默认最高推理强度、推理摘要 detailed（`chat-latest` 只发摘要，和旧版一样）、`max_output_tokens` 为模型上限（受上下文限制）、图片 `detail` 用 original（官方支持的模型）或 high。`+s` 官方网页搜索（搜索词、打开的网页和来源列在工具调用里，也请求搜索结果），`+c` 代码解释器（自动容器，执行输出也取回；生成的文件立即下载：图片显示在回答里——和执行输出里相同的图只显示一次——其它文件用真实文件名作附件），`v=` 回答详细程度，`tier=flex|priority|fast|ultrafast`（后三个只给 `priority_users`）；`u`（OpenAI 没有 URL 抓取）等报错。文件：PDF、Office 文档、表格和约 100 种文本/代码类型都按文件原生发送，其它文本文件内联。用户哈希作为 `safety_identifier`、对话作为 `prompt_cache_key`（缓存）发送。费用按官方价目表，含 272K 以上的长上下文倍率、缓存写入、搜索每次 $0.01、代码解释器每个容器 $0.03。流断了时（网络断了而连接没被关闭，OpenAI 会继续生成），先等 OpenAI 生成完，从它保存的结果补全这条回答并注明，等不到才重试；bot 重启会关闭连接，OpenAI 随即取消，只能重新生成。OpenAI 拿掉了之前的推理（`input_transformations`，`turn_boundary` 以外的原因）、或缓存因为对话或工具变了而没命中（GPT-5.6 起，`prompt_cache_options`）时各有提示。

**xAI**：`grok-4.7`、`grok-4.6`、`grok-4.5`、`grok-4.3` 等。默认最高推理强度（有档位的模型）；`+s` 网页搜索、`+c` 代码执行；`tier=priority|fast` 只给 `priority_users`。PDF 和文本文件按文件发送（xAI 实际用 `attachment_search` 读取时回答里注明）。xAI 在用量里报告费用，统计直接用它；下一轮的上下文大小用 xAI 报告的精确值（`context_details`）。

**Anthropic**：`claude-opus-5-5`、`claude-fable-5-2`、`claude-sonnet-5-5`、`claude-haiku-5-5` 等（官方 ID 用连字符：OpenRouter 的 `anthropic/claude-opus-5.5` 在官方叫 `claude-opus-5-5`）。默认自适应思考 + `effort=max`，思考显示 summarized（官方默认 omitted 会看不到思考）；4.5 代是思考预算（最大）。`e=` 按模型的档位；`e=none` 在能关的模型上关闭思考，Sonnet 5.5 上是官方说的最低档 `between_tools`，Opus 5.5、Fable 不能关。`budget=` 只用于有思考预算的模型。`+s` 网页搜索、`+u` 网页抓取、`+c` 代码执行（都是 Anthropic 自己的服务端工具）；`tier=fast`（`speed: fast`，只给 `priority_users`）。PDF 和文本文件都按文档原生发送（文本文件不再内联）。回放：同模型、同工具的回答原样回放全部内容块（思考及签名、工具调用和结果）；换了工具（如后来加了 `+s`）就只发文字，避免官方的"保留思考"校验报 400。自动提示缓存（5 分钟），不改变输出，只降低后续轮的输入费用。回答被 `pause_turn` 暂停时自动接着请求（最多 5 次），费用逐次累加。引用：`web_fetch` 抓取的网页和用户发的 PDF、文本文件都开启 citations（从对话第一轮起一直开，否则会改动之前的消息），📚 Sources 里显示被引用的原文、文档名和页码。代码执行生成的文件用 Files API 立即下载（图片显示在回答里，其它作附件）。拒答的错误标题写拒答类别（如 `refusal (bio)`）。每次请求带 `diagnostics`（上一条同模型回答的 ID），缓存因为对话或工具变了而没命中时提示。

**Gemini API**：`gemini-4-hydrogen`（2026-11-12 发布，多一档推理强度 `xhigh`）、`gemini-4-carbon`、`gemini-4-argon`、`gemini-3.8-flash`、`gemini-3.6-flash`、`gemini-3.1-pro-preview`、`gemini-nano-banana-2.1` 等（官方 ID 就是 OpenRouter ID 去掉 `google/`；实际由别的模型服务时，页脚的 `served …` 会写出来）。默认最高 `thinkingLevel` 并带思考摘要，输出上限为模型最大（Gemini 4 是 long decode：跨多次续写的总预算 `long_decode_budget`，到了回复 `/continue`；流断了从最近的 checkpoint 接着写，重启也一样）。图片用最高的 media resolution（视频、PDF 为 high）。安全过滤：Google 的可调过滤器在 2.5 以后默认就是关闭的，bot 不另发参数。`+s` Google 搜索（来源和 Google 要求展示的 Search Suggestions 都显示；Nano Banana 2.1 和 3.1 Flash Image 同时用图片搜索，页脚写 `web and image search`）、`+c` 代码执行、`+u` URL context、`+m` Google Maps；Gemini 3/4 的文字模型带这些工具时请求 `includeServerSideToolInvocations`，搜索、URL、地图的调用和结果按实际顺序出现（其它模型只在流的最后报告，显示在它支撑的回答之前）；`e=` 按模型的档位；出图模型 `1` `2` `4` 和比例按模型校验（预设默认最大尺寸）；`tier=flex`，`tier=priority` 只给 `priority_users`。图片、PDF、音频、视频、Office 文档、文本文件都按原生输入内联发送，YouTube 链接作为 `fileData`。用户哈希写在 `labels.safety_identifier`。费用含 >200K 的长上下文价、搜索/地图每次 $0.014（网页和图片搜索分开计次；每月 5000 次的免费额度 bot 看不到，所以是上限估计）。安全评级为 MEDIUM/HIGH 或被拦截时提示。其它模型生成的图片和文件作为 model 消息的 `inlineData` 发送。

**DeepSeek**：`deepseek-v4-pro`、`deepseek-flash`。默认开思考、`reasoning_effort=max`；`e=max|high|low`，`e=none` 关闭思考；`max_tokens` 显式发送（最多 393216）。`+s` 只有 `deepseek-v4-pro` 有（官方的服务端搜索只在它的 Anthropic 兼容接口和 Responses 接口上），bot 把这样的请求发到 Anthropic 兼容接口；有搜索失败或没有结果时注明次数。图片只有 flash 能读。不发 `user_id`：DeepSeek 用它按用户隔离 KV 缓存，群里几个人接着同一段对话聊就会互相用不上缓存（flash 缓存命中价是未命中的 1/50），旧版也没发。峰谷价：北京时间工作日 9–12 点、14–18 点为高峰（法定节假日按官方放假安排算平时），按请求发出的时刻计价。

**智谱**：`glm-5.3`（纯文本）、`glm-5.3-flash`（图片、视频、文件）等。思考默认开、`reasoning_effort=max`；5.3 系列 `e=max|high|low`、不能关思考；5.2 `e=max|high`，可 `e=none`。保留思考（`clear_thinking: false`，否则智谱会丢掉回传的上一轮推理）。`+s` 是智谱自己的 `search_pro` 搜索（每次 0.03 元），取官方允许的最多 50 条结果、`content_size: high`（每条内容更全，输入 token 会多很多），结果（标题、网站、日期、摘录）先于回答显示，智谱判断不需要搜索时注明；视觉模型（如 `glm-5.3-flash`）没有搜索。

**Moonshot**：`kimi-k3`、`kimi-k3.1-pro` 等（K2 系列年底停用，不在表里）。始终思考，`e=max|high|low`（默认 max）。输出上限用 `max_completion_tokens`。`+s` 走 Moonshot 的 Responses API 的 `web_search`，来源列在 📚 Sources，没有搜索时注明。PDF 和文档要先上传到 Moonshot（有状态），bot 不支持；图片、视频按 base64 发送。`-highspeed` 模型价格翻倍，只给 `priority_users`。账户余额见 `/credits`。

**阿里云百炼**：`qwen3.8-max`、`qwen3.8-flash`、`qwen3.7-max`、`qwen3.7-plus` 等。思考默认开；qwen3.8 默认 `reasoning_effort=xhigh`（`e=xhigh|medium|low`），`e=none` 关闭思考，`budget=<n>` 设思考预算（`thinking_budget`）。`+s`（联网搜索）、`+u`（网页抓取 `web_extractor`）、`+c`（代码解释器）都走百炼的 Responses API：搜索词和来源都显示（📚 Sources），每次搜索 0.004 元，没有搜索时注明；这个接口没有思考预算（带 `budget=` 报错），图片按默认分辨率发送（会注明），视频只能用公网 URL（带视频报错），对话超过上下文的 80% 报错。默认地址是旧版用的 `dashscope.aliyuncs.com`：2026-09-30 之后不再加新功能，请求 600 秒超时、请求体上限 16 MiB（几张 base64 大图或视频就可能超过）；建议把 `base_url` 设成工作空间域名 `https://<WorkspaceId>.cn-beijing.maas.aliyuncs.com/compatible-mode/v1`。

**火山方舟**（豆包，`ark:`）：示例配置里的 `db$`、`dbe$` 直连方舟 Chat API。选项与上面相同，方舟支持 `s`（豆包搜索，需先在方舟控制台开通；方舟报告的搜索次数显示为工具调用，每次按 0.02 元计入估算费用，没有搜索时注明）、`e=<high|medium|low>`、`e=none` 或 `e=minimal`（都是关闭思考）、`tier=flex`/`tier=fast`（`fast` 只给 `priority_users`）；`p=` 和其它选项报错。

方舟默认 `max_tokens` 只有 4096 且会静默截断，所以 bot 总是显式发送：模型最大回答长度（受上下文限制），上限 65536（`[providers.ark].max_tokens_cap`；方舟按"输入 + max_tokens"预扣 TPM，设得过大会更快触发 429）。方舟响应不带费用，统计里的费用按价目表（人民币，最低档位）估算，汇率见 `cny_per_usd`。

### 自定义 system prompt 与前文历史

新对话（`前缀$…`）的正文中单独成行的 `::system`、`::user`、`::assistant`（也可 `::sys` `::u` `::a` `::model`）把正文切成多轮（回复 bot 消息时不解析这些行）：

```
c$::system
你是一个只说英文的助手。
::user
你好
::assistant
Hello!
::user
今天天气怎么样？
```

- 第一个标记前若有文字，视为一条 user 消息。
- 以 `::assistant` 结尾即"prefill"（是否支持取决于模型；不支持会原样报错）。
- 消息附带的图片/文件挂在最后一条 user 消息上。
- 想发送一行字面量 `::user`，写成 `\::user`（去掉一个反斜杠；`\\::user` 发出 `\::user`）。

### 本聊天的前缀（自定义 chatbot）

被允许的聊天里，没被封禁的人都可以把"模型 + 选项 + system prompt + 一段历史"存成这个聊天自己的前缀，相当于一个简写：

```
/prefix pirate c+s$::system
你是一个海盗。
::user
你好
::assistant
Arr!
```

之后这个聊天里任何人发 `pirate$你是谁？`，就等于用 `c+s$` 发出"system prompt + 那段历史 + 你是谁？"。

- 也可以把已有的对话直接变成前缀：回复 bot 的某个回答发 `/prefix 名字`，这段对话（从开头到那个回答）连同它的模型和选项就成了前缀。
- 名字：3–16 个英文字母或数字，不区分大小写；不能和已有的前缀重名（包括配置里的和这个聊天已有的）。
- 只在这个聊天里有效，别的聊天不受影响（可以各自有同名的前缀）。
- 历史必须以 `::assistant` 结尾（新消息接在后面）；只有 system prompt 也可以。用前缀时正文里不能再写 `::system`；可以再写 `::user` / `::assistant` 接在它的历史后面，也可以加选项（`pirate+e=low$…`）。
- 创建时模型和全部选项就固定下来（引用的是底层模型，不随配置里同名前缀的变化而变）；回答、计费、统计、显示都和直接用这个模型完全一样。
- `/prefix` 列出本聊天的前缀，`/prefix 名字` 查看一个，`/unprefix 名字` 删除（创建者或管理员）。删除后，之前回答的消息照样可以回复继续，因为对话里已经包含那段历史和模型。
- 前缀开始的是新对话：不能用在 `/m` 里。

### 图片、文件、相册、位置、富消息

- 发送带前缀说明（caption）的图片/文件/相册；或用带前缀的消息**回复**一条图片/文件/相册/位置消息，就会附上它们（被回复消息的文字不附上，和旧版一样）。回复 bot 的回答时发图片、相册、文件或位置，就是带着它们继续对话。
- **相册**在 Telegram 里是每张一条消息，bot 把整个相册当作一条消息处理一次：所有图片按顺序附上，其它几张各自的说明文字接在正文后面。新对话的前缀写在任意一张的说明里都行；回复 bot 的回答发相册，只继续一次（不会每张一次）。
- **位置**（定位、地点、实时位置——取处理时的当前位置）作为一个位置附件，供 Gemini 的 Google Maps 工具使用（`google:` 模型加 `+m`，对话里最新的位置就是"用户所在的位置"，见"选项"）。其它模型和经 OpenRouter 都没有位置输入，带着位置会直接报错，不会把坐标改写成文字。
- **富消息**（用户发的或经 inline bot 发的 rich message）按 Markdown 交给模型：标题、列表、表格、代码块、引用、公式等结构都保留；其中的图片、文件作为附件（在正文前面），地图块作为位置。前缀写在第一段开头。
- **Telegram 的各种消息都能用**：照片和相册、不压缩发送的文件（图片当图片，其它当文件）、语音消息（OGG 音频）、视频和圆形视频消息、GIF（MP4）、贴纸（静态贴纸是 WebP 图片，视频贴纸是 WebM 视频；动画贴纸是 Lottie 动画，没有模型能读，直接报错）、投票（问题、选项和已知的票数）、联系人、骰子（表情和点数）、待办清单——后几种作为文字交给模型。模型的 API 不接受某种输入时会直接报错，不会偷偷转换。
- 下载按 Telegram 文件 ID 去重缓存，同一张图只下载一次。
- PDF、音频、视频：仅当模型在 OpenRouter 上原生支持该输入模态时才发送，否则报错（绝不本地转换冒充）。
- 纯文本类文件（.txt/.py/.md…）：Chat Completions、Responses、Messages 都没有原生文本文件输入，因此作为**独立的 text 段**原样内联。
- 单个文件不能超过 `general.max_file_mb`（默认 50 MB），超过则报错。
- `@https://www.youtube.com/watch?v=…` / `@https://youtu.be/…`：把 YouTube 视频作为 URL 附件交给模型（仅 Gemini 经 AI Studio 支持），正文不变。

### 生成图片和视频

两种方式，按各平台官方推荐的用法：

- **聊天里出图**：出图的聊天模型（Gemini 的 Nano Banana 系列）直接写就行；OpenAI、Grok 的聊天模型加 `+i`，用它们自己的 `image_generation` 工具（`openai:gpt-6.1-sol+i$画一只猫`）。图片和文字按产生的顺序出现在回答里；回复它就是带着上下文接着改。
- **直接用出图/视频模型**：写出图模型的 ID（`openai:gpt-image-2.6-sunburst$…`、`ark:doubao-seedream-5-5-261028$…`），bot 直接调该平台的图片/视频接口，不经过任何聊天模型。消息带图片、或者回复一张生成的图（视频）时是**编辑**：被回复的那张生成图放在最前面，接着是消息自己带的图。这些接口一次只收一个提示，不能带上对话：回复一条不能编辑的回答（比如只有文字，或者模型不收图片），直接报错，不会忽略上下文生成一张无关的图。

共同点：

- 默认参数拉满：最高质量、最大尺寸（OpenAI 的尺寸同时决定比例，没写比例时让 OpenAI 自己选尺寸和比例，写了比例就用该比例下的最大尺寸，只写尺寸字母是 1:1）、不加水印、平台允许的最低审核档。
- `n=`、`q=`、`bg=`、`sec=`、尺寸和比例按每个模型官方支持的取值校验，不支持的直接报错并列出可选值。
- 支持流式出图的接口，生成过程中先显示预览（"⏳ preview"），最后换成正式的图；每张图下小字标注尺寸、大小和格式，`/raw` 下载原图（原始文件，不压缩）。
- 费用按各平台的价目表估算（按张、按秒或按 token），xAI 用它报告的实际费用。
- 视频生成是异步任务：任务 ID 一建立就记下，bot 重启后接着查同一个任务，不会重复生成（也不会重复收费）；进度显示在工具调用里（"pending 40%"），完成后视频可直接播放。

各平台：

- **OpenAI**：`+i`（聊天模型，工具用 `gpt-image-2.6-sunburst`、`quality: max`、`moderation: low`，流式预览 3 张）；直接写 `gpt-image-2.6-sunburst`、`gpt-image-2.6-flare`、`gpt-image-2.5-sunburst`、`gpt-image-2.5-flare`（及带日期的快照）调 Images API：生成或编辑（最多 16 张输入图，PNG/JPEG/WebP），`n=1`–`10`，一张时流式预览；`q=` 默认最高（`max`），`bg=`，2.6 上 `e=` 对应 `complexity`（默认 high）。OpenAI 的视频接口（Sora）已关停。
- **xAI**：`+i`（工具没有参数，不能选尺寸、质量）；`grok-imagine-image-2.0`（推荐）、`grok-imagine-image` 生成或编辑（1–5 张输入图），`n=1`–`10`，`1` `2` = 1k / 2k（默认 2k），比例按 xAI 的列表，`q=` 只有 2.0，`bg=`。视频：`grok-imagine-video-1.5`、`-1.5-lite`、`grok-imagine-video`：只有文字时文生视频，一张图是首帧，两张以上是参考图；回复一个生成的视频（或附 MP4）是编辑（仅 `grok-imagine-video`）；`sec=1`–`15`（参考视频最长 10），不写时用 xAI 的默认 8 秒；`1` `2` = 720p / 1080p，默认最大。
- **Gemini**：出图的是聊天模型 Nano Banana（`gemini-nano-banana-2.1`、`gemini-3.1-flash-image` 等），没有 `n=` `q=` `bg=`；Imagen 已被 Google 关停。视频是 `gemini-omni-1.2-flash`（Interactions API 的后台任务）：只有文字时文生视频，带图片、视频（最多 6 张图、3 段视频）时按它们生成，带声音；`sec=3`–`15`（不写时 Google 默认 10 秒），`1` `2` `4` = 720p / 1080p / 4K（默认 4K，约 $0.21 每秒，默认的 10 秒约 $2.1），比例 `W` `H`（或 `ar=16:9`/`9:16`）。回复同一模型生成的视频时用 Google 保存的那次交互（`previous_interaction_id`，官方的编辑方式）接着改。Veo 已全部关停。
- **火山方舟 Seedream**（`ark:doubao-seedream-…`）：5.5（`doubao-seedream-5-5-261028`）、5.0 pro、5.0 flash，一次一张。默认最大分辨率档（2K，比例交给模型）、不加水印、PNG；`q=standard|fast` 是提示词优化模式（`fast` 只有 5.0 pro）。需要先在方舟控制台开通模型。
- **火山方舟 Seedance**（视频，`ark:doubao-seedance-…`）：2.5（`doubao-seedance-2-5-260628`，4–30 秒，最高 1080p）、2.0（4–15 秒，最高 4K）、2.0 fast、2.0 mini（720p），都带声音。只有文字时文生视频，一张图是首帧，两张是首尾帧，三张以上（或提示词里点名"图片1"）是参考图；带音频、或回复一个 Seedance 生成的视频（24 小时内）是全模态参考，由 Seedance 按提示词决定编辑、延长还是新生成。默认最高分辨率、带声音、不加水印；`1` `2` `4` = 720p / 1080p / 4K，比例 16:9、4:3、1:1、3:4、9:16、21:9，`sec=` 按模型；2.5 的 `q=draft` 先出 480p 草稿，回复草稿发 `q=final` 生成 1080p 成片；`+s` 联网搜索没有公布价格，不支持。按 token 计价（人民币，分辨率和是否有输入视频不同价）。需要先在方舟控制台开通（Seedance 2.x 还要求余额或资源包）。
- **智谱**：`glm-image`（异步任务，重启后接着查）、`cogview-4-250304`、`cogview-4`、`cogview-3-flash`。默认最大尺寸（GLM-Image 2048×2048，CogView 1440×1440）、`quality: hd`、不加水印；`q=hd|standard` 只有 CogView；一次一张；没有图片输入（带图报错，回复生成的图不能编辑，会注明）。视频：`cogvideox-3`（文生、首帧、首尾帧，按附图张数；5 或 10 秒；最大 3840×2160；AI 音效；1 元）、`cogvideox-2`（0.5 元）、`cogvideox-flash`（免费），Vidu 每种模式是一个模型（`viduq1-text`、`viduq1-image`、`viduq1-start-end` 2.5 元；`vidu2-image`、`vidu2-start-end` 1.25 元，`vidu2-reference` 2.5 元，1–3 张参考图）。默认最高质量、60 fps、带声音、不加水印，文生视频 3840×2160（带图时保持图的比例）；`q=quality|speed`（CogVideoX），`sec=` 取模型的可选时长，`1` `2` `4` = 短边 720 / 1080 / 2160；它们不收视频，回复生成的视频直接报错。
- **阿里云百炼**：`qwen-image-3.0-pro`、`qwen-image-3.0`、`qwen-image-2.1-pro`/`-turbo`、`qwen-image-2.0(-pro)`、`wan2.7-image(-pro)`、`wan2.6-image`（只能编辑）、`wan2.6-t2i`、`z-image-turbo`，走百炼推荐的同步接口。默认最大尺寸（千问编辑保持输入图的比例）、`n` 默认 1（Wan 2.6 官方默认 4 张，bot 显式发 1）、不加水印、有思考的模型开思考（z-image 的 `prompt_extend` 会让价格翻倍到 0.2 元）；`n=` 千问 1–6、Wan 1–4，`e=none` 关闭思考。千问图像和 Wan 2.6 默认会改写提示词，审核始终开启（官方没有开关）。
- **阿里云百炼视频**（北京，异步任务）：HappyHorse 1.1/1.0（`happyhorse-1.1-t2v`、`-i2v`、`-r2v` 等）、Wan 3.0（`wan3.0-video`、`-prime`，2–30 秒）、Wan 2.7（`wan2.7-t2v`、`-i2v`、`-r2v`、`-videoedit`）、Wan 2.6、2.5、2.2、2.1（`wanx2.1-…`）的文生、图生、首尾帧、参考图和视频编辑模型。图片按 base64 发送；百炼只收视频的 URL，所以只能用回复的那条百炼生成的视频（24 小时内）去编辑或续写，附带的视频报错。默认最高分辨率、不加水印、有开关的模型带声音，提示词改写保持百炼默认（开）；`1` `2` = 720P / 1080P，比例按模型，`sec=` 按模型（只有 5 秒的模型写 `sec=` 报错）；wan3.0 带 `+u` 时把消息里的网址作为参考网页。按秒计价。
- DeepSeek、Moonshot、Anthropic 没有出图或视频生成。

## 2. 继续对话

- **回复** bot 的任意一条消息（包括长回答的任意分段、生成的图片）即继续该对话，使用同一模型与选项。回复的全部文字原样作为一条 user 消息发送（`::system` 等标记行不解析）；以 `\` 开头时去掉这一个反斜杠（用来发送以 `/` 开头的字面量等）。
- 回复一条仍在生成中的消息：会排队，等它生成完再继续。
- 重试在同一组消息里进行，回复这一轮的任何一条消息都等同于回复最终成功的那次。
- 没有正常完成的回答（所有尝试都失败，或被拒答/内容过滤拦下）不能继续对话，回复它会得到提示；按它下面的 **🔁 Retry** 按钮或回复 `/retry` 重新生成（群里有权限用 bot 的人都可以；重试和原请求一样记在提问的人名下）。
- 带"引用"（quote）的回复会被忽略（与旧版一致）。
- 回复 + `/m spec$正文`：换成别的模型/选项继续这段对话（可以多个 spec）。

## 3. 回答的样子

```
🤖 anthropic/claude-opus-5.5                       ← 第一行：模型（小字，模型名加粗）
▸ ⚠️ Attempt 1 failed: …                           ← 之前失败的尝试（如有），按顺序折叠：当时的输出、完整错误、用量
▸ 💭 2 · 🔧 1                                       ← 连续的推理和工具调用合成一个折叠（小字写数量），里面依次是：
    ▸ 💭                                            ←   推理（标题只有一个小号的 💭）
    ▸ 🔧 web_search: rust latest version            ←   工具调用：标题写它做了什么（查询、网址、代码）和不寻常的结局，展开看内容
    ▸ 💭                                            ←   工具调用之后的又一段推理
……模型的回答（富文本 Markdown，图片在它出现的位置）……
▸ 📚 Sources · 5                                    ← 紧跟它所属的那段回答
▸ ⚠️ Error: …                                      ← 没有正常完成时：完整的原始错误对象
ℹ️ …                                               ← 小字：bot 的提示（参数映射、文件内联、截断原因…）
effort max · web search · in 1,234 (cached 1,000) · out 567 (reasoning 400) · ≈$0.012 · 12.3s    ← 小字
```

- **严格按时间顺序**：推理、工具调用、回答、图片、拒答都显示在它实际产生的位置，只合并相邻的同类片段，不会挪动顺序（Gemini 的搜索、地图、URL context 在流的最后才报告，显示在它所支撑的回答之前，也就是它实际发生的位置；出图模型思考时画的草图放在推理折叠里）。相邻的几次工具调用合成一个折叠，标题是每次调用的摘要（同一个工具多次时写成 `web_search ×2: 查询1; 查询2`）。中间没有被正文打断的一串推理和工具调用（不止一个折叠时）放进一个外层折叠，标题是"💭 推理段数 · 🔧 工具调用次数"；正文穿插时每串一个，只有一项时不加外层。推理默认一直折叠（`[render] reasoning_open = true` 时，模型还在思考、后面还没有别的内容的那段推理是展开的；但每次刷新都会把读者手动收起的它重新展开）。
- Markdown 按 CommonMark 解析，另加"CJK 友好"的强调规则：中文、日文、韩文里 `**注意：**内容`、`这是**“引用”**的` 这种标点贴着星号、两边没有空格的写法也会加粗。
- **工具调用**的标题写它做了什么，以及不寻常的结局：退出码（`exit 1`）、超时、错误码（`error: max_uses_exceeded`）、网址抓取失败的原因（`paywall`）。展开后是可读的内容，不是裸 JSON：搜索词，找到的来源（标题链接、网站、日期、摘录），打开或在里面查找的网页，抓取的网页和时间，代码和它的输出（stdout、stderr、退出码），生成的文件，出图用的尺寸、质量等。认不出的格式原样显示 JSON；认得的格式里没展示的字段也以 JSON 列在后面，所以不丢信息。只有加密内容之类人读不了的数据不显示，在 `/dump` 里。
- **📚 Sources** 列出回答引用的链接；提供方给了被引用的原文时，链接下面是这段原文（截短）。引用的是文档（如用户发的 PDF）时写文档名和页码。Gemini 的 Search Suggestions（Google 要求和搜索支撑的回答一起展示）是来源下面的一行小字链接，不折叠。
- 模型正在思考时，推理折叠的标题写它此刻在想什么：思考摘要里最新的一个小标题（如 `💭 Checking the dates`），没有小标题时是最新一段的开头；推理块外面还有一层折叠时，外层标题后面也带上这句。这一段思考一结束（后面出现了工具调用或回答，或回答生成完），标题就回到只有 💭。
- reasoning token 数在页脚（`out … (reasoning …)`），是整轮的总数：各家 API 都不给分段的数。API 没返回推理内容（只有加密的推理）时也照常显示这个数，不加额外标注。
- 费用前有 `≈` 的是 bot 按价目表估算的（API 没给费用），没有的是 API 报告的（OpenRouter、xAI）；价目表外的额外收费只在费用后简短标注（`+ code execution`），不解释计费细节。
- ℹ️ 提示只在有新情况时出现：关于请求本身的提示（如回放方式的变化），如果回复的那条回答已经显示过，就不再重复。
- 实际服务的情况和请求不同时，页脚写出来：`served gemini-3.8-flash`（回答的不是请求的模型；同一模型带日期的快照不算），`tier priority→default`（请求的等级没给到），`tier priority`（没指定等级、却按非默认等级服务），`geo us`（Claude 在美国推理，价格 1.1 倍），`effort medium`（请求没写推理强度时，API 实际用的强度）。方舟降级到别的模型、某项限流额度只剩不到 10%、xAI 给输出加了水印时，各有一条 ℹ️ 提示。
- 用量：Claude 一开始就显示输入 token 数；直连方舟、智谱、Gemini 的用量（和估算费用）实时更新；经 OpenRouter 的其他模型在结束时才有用量（OpenRouter 不转发中途用量）。
- **没有正常完成**（出错、被拒答或内容过滤、提供方以错误结束）时，错误块里是完整的原始错误对象（只去掉账户的 `user_id`；OpenRouter 回显的请求在 `/info`）。输出到长度上限不算出错，页脚会注明。
- **重试不发新消息**：自动重试（默认最多 4 次尝试（含首次），失败后依次等 2、10、30 秒，见 `[retry]`；限流至少等 20 秒，上游要求更久时以上游为准（`retry-after-ms`，或秒数、日期形式的 `retry-after`）；只重试网络、超时、限流、上游故障这类可能成功的错误，上游用 `x-should-retry` 明确说了能不能重试时听它的）和手动重试都在同一组消息里进行，失败的尝试折叠在上面。
- 模型生成的图片直接嵌在回答里它出现的位置，图下小字标注尺寸、文件大小和格式；生成的视频、音频可以直接播放，其它文件（例如代码工具写出的文件）作为附件；支持流式出图的模型在生成过程中先显示预览（"⏳ preview"），一张张替换成最终的图；一条回复尽量就是一条消息，只有超过 Telegram 的上限（32768 字、500 块、50 个图片/文件）才拆成多条。
- **结构无歧义（防伪）**：每条回复消息的第一行是模型名（小字，让正文占主要篇幅）；除此之外 bot 加的所有东西都在折叠块或小字页脚里，而模型输出的 Markdown 永远渲染不出这两种块（`<details>` 等 HTML 一律按字面显示）。所以任何人都能准确指出哪部分是模型的回答。
- 超过一条富消息的上限会自动拆成多条，后续消息第一行是 `🤖 模型 · part N`。
- Telegram 会把每段超过 70000 字节的文字静默截断（切在字符中间时整段变空），bot 会把长文字拆成多个文本节点发送来避免；回复发完后还会读回比对，若仍被改动，就在消息里附上原始 Markdown。
- 某个块（如超过 20 列的表格、嵌套过深的列表）无法用富消息表达时，该块显示为原始 Markdown；整条富消息被 Telegram 拒绝时，模型部分改为原始 Markdown 再发；仍被拒绝才退回纯文本。Telegram 不接受的空结构（例如结尾多出的一个 ```` ``` ````、流式中刚打开的代码块）会被自动处理。
- `render.markdown = "raw"` 时模型输出始终以原始 Markdown 展示。
- **刷新节奏**：bot 按实测的 Telegram 限速模型提前排好每次发送/编辑，不会触发 FLOOD_WAIT（[research/telegram-flood.md](research/telegram-flood.md)）。生成中的回答在开头 10 秒内约 1.5 秒刷新一次，1 分钟内约 3 秒，之后逐渐放慢到 10 秒；同一个 bot 在同一个聊天里的多个回答共用 Telegram 的额度（每分钟约 20 次操作；每个 bot 分开算），新开始的、生成完的回答优先。很长的消息（例如推理阶段超过约 8000 字）每次编辑都很"重"，这时刷新会明显变慢（每分钟最多约 5 次）。
- bot 重启打断的生成：Gemini 4 的 long decode 从最近的 checkpoint 在原消息里接着写，用量、费用、耗时和 `/continue` 的预算都从重启前接着算（重启前已经计费的部分不会丢）；视频和智谱 GLM-Image 这类异步任务接着查同一个任务；其他模型重新生成，被打断的那次折叠显示为"interrupted by a bot restart"。
- **没有"停止生成"**：群里大家都在看，一旦开始就生成到底。

## 4. 命令

| 命令 | 说明 |
|---|---|
| `/help` `/help options` | 帮助 / 选项列表（`/start` 同 `/help`） |
| `/models [关键词]` | 当前 bot 的全部前缀（只含它有的模型）；带关键词时在前缀名和 spec 里搜索，OpenRouter 允许直接写模型 ID 时（`allow_unlisted_models`）还搜索它的模型目录 |
| `/model <前缀>` | 模型能力（模态、推理档位、最大输出、价格）及各 endpoint（是否第一方、量化）；该 provider 允许直接写模型 ID 时也接受 ID；直连官方 API 的模型显示由哪个平台直连、是否在 bot 的模型表里 |
| 回复 + `/raw` | 一条消息：该回答的原始 Markdown（短的直接放代码块，推理在折叠块里；长的、或含 Telegram 会改写的字符如制表符时附 `.md` 文件），以及原图/原文件（附件） |
| 回复 + `/dump` | 一条消息：这段对话从开头到被回复的那条为止的 JSON 文件：每一轮的全部内容（文字、带签名的推理、工具调用、图片、文件、provider 的原样输出）、模型与选项、状态、每次尝试的结果/用量/错误；图片和文件以 base64 内嵌（同一个只嵌一次）。请求体见 `/info` |
| 回复 + `/info` | 一条消息：请求摘要（每次尝试的状态、到第一段输出的时间、上游 ID；每次上游调用的请求 ID、实际服务的模型和等级、响应头和首个事件的耗时、限流余量），附完整的请求详情 JSON：每次尝试的请求体、上游 ID、用量、错误、上游回显，以及每次调用的响应元数据（去掉内容的响应对象，和一部分响应头：请求 ID、限流、耗时、服务等级、弃用提示；不含组织、项目、用户 ID 和 cookie，因为聊天里的人都能看 `/info`） |
| 回复 + `/retry` | 重新生成一个失败的回答（同消息下的 🔁 Retry 按钮） |
| 回复 + `/continue` | 让停在 long decode 预算上的回答（Gemini 4）接着写：新内容在新消息里，并入同一轮对话 |
| 回复 + `/m spec$正文` | 换模型继续 |
| `/stats [today\|7d\|30d\|all]` | 本 bot 在这个聊天里的用量统计。周期按 `general.timezone` 的自然日算：today = 今天 0 点起，7d / 30d 含今天，all = 全部时间，默认 30d；回复里写明起始时间。群里：本群；`/stats <user_id>`，或不带周期回复某人的消息，只看这个成员在本群的用量（群里有权限用 bot 的人都可以）。私聊：只有这个私聊（管理员见下）。表里的 token 数按 K / M 显示（`1.2K`、`45K`、`1.2M`，不到 1000 时是原数）；每条回答页脚里的用量仍是精确数 |
| `/usage` | 本聊天的限额和你在本聊天的限额（见下文"限额"） |
| `/prefix` / `/prefix 名字` | 本聊天的前缀列表 / 查看一个（创建见上文"本聊天的前缀"） |
| `/unprefix 名字` | 删除本聊天的一个前缀（创建者或管理员） |
| `/ping` | 本聊天的 chat id、你的 user id（和 @用户名）、本聊天是否被允许、你在这里是否被封、你是不是管理员、bot 名和运行时间 |

回复类命令用错对象时：
- 回复一个**还在生成**的回答：`/raw` `/dump` `/retry` `/continue` `/prefix 名字` 都提示"还在生成，完成后再试"；`/info` 照常（显示到目前为止的尝试）；直接回复或 `/m` 会排队：先回一条"⏳ queued"，等那个回答完成后再开始。
- 回复一个**失败**的回答：直接回复或 `/m` 提示它失败了、请按 🔁 Retry；`/raw` 提示没有最终文本、去看 `/info`；`/continue` 提示改用 `/retry`；`/dump` `/info` 照常（含每次尝试的错误）。
- 回复 bot 的**非回答消息**（命令输出、提示）：这些命令提示"不是模型的回答"。直接回复它：群里不理，私聊里给用法提示。
- 回复**别人**的消息：群里这些命令不理（不是任何 bot 的消息）；`/stats` `/ban` `/unban` `/quota` 回复某人的消息则指向那个人。

bot 每次启动都会注册命令菜单：所有聊天、所有语言共用一份（先清掉旧版本或旧 bot 按语言、聊天类型留下的列表，免得它们把新列表盖住）；管理员在自己的私聊里还会看到管理命令（需要先和这个 bot 说过话）。

### 权限

每个 bot 各自独立；在一个 bot 下，群和私聊都是平等的"聊天"，在一个聊天里设置或统计的东西不会影响别的聊天。

- 没被允许的聊天里只回答 `/ping` 和管理员的 `/allow`，其它一律不理（查询类命令、其它管理命令也不例外）。
- 被允许的聊天里：
  - 只显示信息的命令（`/help` `/models` `/model` `/stats` `/usage` `/ping`，回复类的 `/raw` `/info` `/dump`）对所有人回答，被封的人也一样（能看自己的限额和统计）。
  - 花钱的请求（新对话、回复继续对话、`/m`、`/retry`、`/continue`、🔁 Retry 按钮）：发的人在这里被封时，bot 回复"你在这个聊天被封禁了"（按钮则弹出提示）；限额用完时，回复里写明哪个限额用完、多久后恢复。
- 允许一个聊天：管理员在群里发 `/allow`（允许本群，群还没被允许时也可以），或在自己的私聊里发 `/allow <chat_id>`（私聊的 id 就是对方的 user id）；`/deny` 撤销。允许之后聊天里的所有人都能用。
- 封禁只存在于某个群里，别的聊天不受影响；被封的人已经在生成的回答照常写完。没有整个 bot 范围的封禁。
- 管理员没有特权：他们的私聊在首次启动时被允许一次，之后可以 `/deny` 自己的私聊，也可以在群里封自己（被封的管理员在那个群里仍能用管理命令，比如解封自己）。私聊被移出白名单后，在那里再发 `/allow` 就能恢复。管理员和其他人一样受花费限额限制。
- 其他 bot 和以群/频道身份匿名发出的消息一律忽略。
- 配置里的 `[access].allowed_chats`（群 id 和私聊都可以写，私聊的 id 就是对方的 user id）：每个 id 加进配置后，在下一次启动时为每个 bot 允许一次；之后以命令为准，从配置里删掉不会撤销（用 `/deny`）。

### 管理员（`[access].admins`）

示例配置里 `[access]` 的三个名单都从环境变量读：`OMNI_ADMINS`（管理员）、`OMNI_ALLOWED_CHATS`（首次启动时允许的聊天）、`OMNI_PRIORITY_USERS`（可以用更贵等级的人），都是逗号分隔的 id，空着就是没有；在配置里直接写数组（`admins = [123456789]`）也可以。

管理命令只改收到它的那个 bot。群里的管理命令只作用于本群，也不显示其它聊天的数据；私聊里的作用于整个 bot，用 id 指定聊天（群为负数，私聊的 id 就是对方的 user id）。

| 命令 | 说明 |
|---|---|
| `/allow` / `/deny` | 群里：允许/撤销本群（`/allow` 在没被允许的群里也有效）。私聊：`/allow <chat_id>` / `/deny <chat_id>`，允许/撤销任意群或私聊 |
| `/ban` / `/unban` | 群里：`/ban <user_id>`，或回复某人的消息发 `/ban`，在本群封禁；`/unban` 同理。私聊：`/ban <user_id> <group_id>` / `/unban <user_id> <group_id>`。管理员也可以被封（包括封自己） |
| `/acl` | 群里：本群是否被允许、本群的封禁；私聊：所有聊天的 |
| `/prefix <chat_id>` / `/unprefix <chat_id> <名字>` | （私聊）看某个聊天的前缀 / 删除其中一个；群里管理员可以用 `/unprefix 名字` 删任何人的 |
| `/quota …` | 限额，见下文"限额" |
| `/stats [周期]`（私聊） | 整个 bot：总额，以及按模型、按聊天、按人、按"聊天 · 人"四个排名（按花费，各前 10）；聊天和人都带 id，方便再细看 |
| `/stats model\|chat\|user\|chat-user [周期]`（私聊） | 只看其中一个排名，前 50 |
| `/stats <chat_id> [user_id] [周期]`（私聊） | 某个聊天（按模型、按人）；再带 user_id：那个人在那个聊天里（按模型） |
| `/alias <前缀$> <spec>` / `/unalias <前缀$>` | （私聊）给本 bot 定义/删除运行时前缀，不用改配置、不用重启；末尾的 `$` 可以省略，空前缀写 `$`。`/unalias` 只删运行时定义的（同名的配置前缀随之恢复），配置文件里的前缀要改配置 |
| `/credits` | （私聊）OpenRouter key 的额度、已用、剩余，今天/本周/本月（UTC）的花费，免费模型的次数和过期时间；完整的原始响应折叠在下面；直连的平台里有余额接口的（DeepSeek、Moonshot）另列余额 |

### 限额

限额是"后付费令牌桶"，只存在于聊天里：一个聊天可以有整个聊天的限额、成员的默认限额（对每个成员分别计算）和某个成员单独的限额；每个聊天一个桶，每个"聊天 + 成员"一个桶。没有跨聊天的个人限额，也没有整个 bot 的默认值。请求开始前，所在聊天的桶和你在这个聊天的桶（设了限额的才检查）都要 > 0，结束后按实际费用扣减（可扣成负数，恢复后才能再用）。管理员从不被限额挡住，但费用照常记。`/usage` 谁都能用，显示本聊天的限额和你在本聊天的限额。

管理员设置限额（金额单位 USD：容量、每日恢复量）：

- 群里：`/quota` 显示本群的限额和余额；`/quota chat <容量> <每日恢复>` / `/quota chat off` 设置/删除整个群的限额；`/quota members <容量> <每日恢复>` / `/quota members off` 设置/删除每个成员的默认限额；`/quota <user_id> <容量> <每日恢复>` / `/quota <user_id> off` 单独设置某个成员；回复某人的消息发 `/quota <容量> <每日恢复>` / `/quota off`，就是设置这个人。
- 私聊：`/quota` 列出本 bot 的全部限额；`/quota <chat_id>` 显示那个聊天的；`/quota <chat_id> chat|members|<user_id> <容量> <每日恢复>`（或末尾写 `off`）设置/删除其中一条。

费用按每次上游调用记账、按 generation id 去重：流中断、bot 重启或崩溃导致没收到用量的调用，会稍后向 OpenRouter `/generation` 补查，不漏记也不重复扣。

## 5. 多 bot

`[[bots]]` 可以配置多个 bot，每个都像一个单独部署的实例：各自的授权名单、封禁、限额、运行时前缀（`/alias`）和 Telegram 限速额度。

- `[[bots]].models`：这个 bot 有哪些模型，写成 `openrouter:author/model`、`<provider>:<模型 ID>` 的通配符（默认 `*`）。不匹配的模型、以及没配置的 provider 的模型（例如没有 key 的平台），对它来说都不存在：不列出、搜不到，作为前缀不认识。配置文件里的前缀所有 bot 共用，各按自己的 `models` 过滤。
- 群里每个 bot 只回答它有的前缀，不认识的静默忽略；几个 bot 都有的前缀，每个都会回答一次。私聊里不认识的前缀会报错。
- 不加 @ 的命令每个 bot 都会回答，例如群里发 `/deny` 会让在场的每个 bot 都撤销本群，`/deny@某个bot` 只对那一个；回复类命令（`/m` `/raw` `/dump` `/info` `/retry` `/continue`）不加 @ 时只由发出被回复消息的 bot 处理。
- 回复某条消息继续对话，由发出那条消息的 bot 处理。
