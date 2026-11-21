# 架构

## 1. 总览

```
Telegram (MTProto, 每个 bot 一个 grammers 连接)
   │ updates (catch_up)
   ▼
intake（每 bot 一个顺序循环）──► inbound 表；pts/qts 在整批处理完后推进
   │ dispatch
   ▼
handler：解析触发/回复/命令 → 权限、下载媒体 → turns（对话树）
   │ spawn
   ▼
jobs：等待父回合 → attempts（重试）→ OpenRouter / 各平台官方 API 流式 → Accumulator
   │                                         │
   │                                         └─ rawlog（verbose：原始 SSE，blob 落盘）
   ▼
view：RenderState → compose（富消息块）→ paginate → outbox（幂等 send/edit，按 (bot, 聊天) 限速）
```

四个 crate：

| crate | 职责 | 依赖 |
|---|---|---|
| `omni-core` | 对话内容模型（`Turn`/`Part`/`Reasoning`/`Citation`/`NativeOutput`）、blob 引用、触发语法与 `::role` 解析、用量、JSON 工具（merge patch、blob 脱敏） | 纯逻辑 |
| `omni-markdown` | Markdown → 自有 AST（带源码范围）→ 富消息块 `RBlock`/`RText`（镜像 TL PageBlock）或经典 entities；限额统计与切分；`latex` 把公式改写成各 Telegram app 都画得出的写法（D82） | pulldown-cmark（`vendor/` 里打了 CJK 友好强调补丁的副本） |
| `omni-llm` | Provider 抽象（`CallInput`→`Prepared`→流事件）、SSE 解析、错误分类、OpenRouter（目录、Chat Completions / Responses / Messages 三种接口、选项/预设解析）、各平台官方 API 直连（`provider` 及各平台模块，见 §7） | reqwest |
| `omni-bot` | 二进制 `omni`：配置、日志、SQLite、grammers Session、Telegram 收发、多 bot（各自独立）、任务、渲染、命令、权限、限额 | grammers、rusqlite |

## 2. 对话数据模型

对话是一棵树（回复任意一条消息即分叉），存于 `turns` 表：

- `role`：system / user / assistant；`data`：`omni_core::Turn` 的 JSON；`parent_id`。
- `Turn.parts`：规范化内容，按顺序：`Text`（含 `citations`）、`Image`、`File`（PDF/音频/视频/文本…，按 MIME 在请求时决定发送方式）、`RemoteMedia`（如 YouTube URL）、`Location`（Telegram 的定位/地点/实时位置，供 Gemini 的 Google Maps 用）、`Reasoning`（kind：Text/Summary/Encrypted/Redacted，`opaque` 存签名/密文）、`ToolCall`（服务端内置工具的输入输出原文）、`Refusal`。
- `Turn.native`：**该轮在其 API 方言下的原样输出**（例如 Chat Completions 的 assistant message，含完整 `reasoning_details` 签名/密文；Responses 的全部输出项；Messages 的 content block）。下一轮请求若方言与模型都相同，原样回放 native（最大保真）；否则只用规范化 `parts`（例如换模型继续时只带文本）。
- `Turn.origin`：Live（真实消息/真实生成）或 Synthetic（用户用 `::role` 写的伪造历史）。
- assistant 回合还有 `spec`（provider、模型、选项、alias 标签）与 `status`（pending/running/done/failed）。

`attempts`：每次上游调用一行，存脱敏后的请求体（含每个补丁的来源、OpenRouter 回显的上游请求）、部分/最终输出、原始错误 JSON、上游 generation id、用量。回合的最终内容 = 成功那次尝试的输出。

`tg_messages`：Telegram 消息 ↔ 回合（及尝试、分段号）。一轮的所有尝试的所有消息都映射到同一回合，所以回复失败尝试的消息 = 回复成功的那次。

blob（图片、文件、生成图、长 base64）以 sha256 内容寻址存于 `data/blobs/ab/cd/<sha>`；数据库与日志只存引用。`tg_files` 以 Telegram photo/document id 去重下载。

## 3. 可靠性：重启不重不漏

1. **入站**：grammers 以 `catch_up` 模式运行，使用我们基于同一 SQLite 实现的 `Session`。需要处理的消息写入 `inbound`（唯一键 bot+chat+msg_id）。持久化的 pts/qts/channel pts 只在"携带该状态的所有 update 都已处理完"之后才推进——grammers 给同一批 getDifference 结果的所有消息打上的是整批的最终状态，所以我们在看到一个**不同**的状态时才保存上一个状态。崩溃最多导致已处理的 update 被重新投递（被唯一键吸收），永远不会跳过未处理的。关机时也不会把内存中的状态写回（它可能包含已缓冲未处理的 update）。
2. **处理**：`inbound.status = new` 的行在启动时重新分发；handler 先查该 inbound 是否已建过回合，已建则只重启任务（幂等）。
3. **生成**：启动时所有 pending/running 的 assistant 回合重新运行。running 的最后一次尝试标记为 `interrupted`，其消息被改写为"已被重启中断"（保留已生成的部分），再开新尝试（中断不计入重试次数）。OpenRouter 不支持断线续流（见 openrouter-gaps.md），所以除 Gemini 4 long decode（从落盘的 checkpoint 在原消息上接着写）和异步任务（接着查同一个任务）外只能重新生成；流断开后会用 generation id 查询实际费用并入账。从 checkpoint 接着写时，checkpoint 里记的是到它为止已计费的全部用量（包括被打断的那次调用），连同原来的开始时间和上游 ID 一起恢复，所以页脚、`/info` 和 `/continue` 的预算都不会少算。
4. **发送**：每条待发消息先在 `outbox` 写入随机 `random_id` 再调用 `messages.sendMessage`。Telegram 永不遗忘用过的 random_id：重发同一个 id 返回已发出的那条消息而不是重复发送；`RANDOM_ID_DUPLICATE` 表示前一次仍在进行，退避后重试。编辑天然幂等（`MESSAGE_NOT_MODIFIED` 视为成功）。

## 4. 多 bot

- 每个 bot 一个 grammers 连接、一个更新循环、一组按 chat 的限速门（`Gates`）。
- 层级是 bot → 聊天 → 人。每个 bot 是独立实例；一个 bot 下群和私聊是平等的"聊天"，授权（允许的聊天、群内封禁）、限额（每个聊天一个桶，每个"聊天 + 成员"一个桶）和统计都在聊天之内，一个聊天里的设置和统计不影响别的聊天。运行时前缀（`aliases`）按 bot 分开，配置里的 `[[aliases]]` 所有 bot 共用。`[[bots]].models`（通配符，默认 `*`）不匹配的模型、未配置的 provider 的模型，对该 bot 不存在。
- 成员关系 `bot_chats` 从入群事件和收到的消息学习（只记录，不参与路由）。
- 触发：每个 bot 独立解析前缀，只回答它能解析的 spec；群里不认识的 spec 静默跳过（可能属于别的 bot），私聊和 `/m` 里报错。几个 bot 都认识同一个前缀时各自回答。直接写的模型 ID 只在该 provider 的 `allow_unlisted_models = true` 时可用。直连平台的模型（直接写或经前缀）还必须在 bot 的模型表里（`Provider::knows`）：否则私聊和前缀报错，群里直接写的不理，`/models` 不列出。
- 回复：由发出被回复消息的 bot 处理（按 bot 维度查 `tg_messages`）。
- 命令：每个 bot 都回答发给它（`@`）或未加 @ 的命令；回复类命令（`/m` `/raw` `/full` `/dump` `/info` `/retry` `/continue`，以及作为回复的 `/prefix`）未加 @ 时只由发出被回复消息的 bot 处理。
- 启动时为每个 bot 注册命令菜单：先重置旧版本可能留下的按语言/聊天类型的列表（它们会盖住默认列表），再设所有聊天、所有语言共用的一份；管理员的私聊（和 bot 说过话之后）另设含管理命令的列表。

## 5. 渲染与限速

- 输入（`intake`、`handler`、`media`、`tg::richin`）：消息的文字（富消息转成 Markdown）、媒体（照片、文件，富消息里的图片和文件）、位置（定位/地点/实时位置，富消息的地图块）记入 `inbound`；相册每张一条，等 1.5 秒后只由第一条被记录的消息处理（`conv::album_leader`），附上整个相册和其它张的说明；被回复的非 bot 消息只附上它的媒体和位置。
- `prefixes`：群或私聊自己的前缀（自定义 chatbot，表 `chat_prefixes`，按 bot + 聊天）：创建时把模型和选项解析定死，内存里缓存；`App::resolve_spec` 先查 bot 的前缀，再查这个群或私聊的；新对话里每个这样的前缀单独一条链（它的 system prompt 和历史以 synthetic 回合写在用户消息前面），所以删除前缀不影响已有对话。
- `compose`：把回合状态组装为富消息块（头部、折叠的推理/工具/来源、正文、提示、错误、页脚），每条消息第一行是 bot 的头部（小字），bot 生成的其它部分只用模型 Markdown 永远不会产生的块（details、footer），保证模型输出无法冒充。工具调用的内容由 `toolview` 按常见格式（搜索、来源、网页、代码输出、文件、错误）显示成可读的块，认不出的、没展示的字段显示为 JSON（D60）。
- `help`：`/help` 的全部文字（D68），英文和中文（`/help_zh`）各一份：一条富消息，快速上手、命令表，每个主题一个折叠块；`/help <主题或命令>`、`/help_<主题>` 只看一个主题；管理员另有所在聊天类型的管理主题。文字是 Markdown（主题里 `::: 标题` … `:::` 是折叠块），`$` 换成配置的前缀标记，文件大小上限、`bold_cost_usd`、能否直接写模型 ID 按部署填入；不写具体的厂商和模型。测试保证每个命令都有主题、整个 `/help` 是一条消息。
- `paginate`：按 32640 字 / 499 块切成多条消息（富消息上限 32768 / 500，留出续页头部的位置），超大块按结构拆分（代码按行、列表按项、表格按行并重复表头、折叠块按子块，后续消息里的折叠标题加一次 ` (cont.)`，不论跨几条消息都只加一次）。多条时每条的头部都标 `part i/n`（生成中 `part i`）。生成中先用 `rich::trim_folds` 把推理和工具调用的折叠缩到最新部分，整条不超过 `render.show_more_chars`（Telegram 的 "Show More"，D81）；写完的回答只因这些折叠超过一条消息时缩到一条，提示 `/full`（`compose::full_process` 把完整的折叠分页发出）。
- `view`：每次尝试一个同步任务，只渲染最新状态（watch 通道合并中间状态）；与已发出内容哈希比较，只编辑变化的分段，新增分段回复前一段发送；最终状态多余的分段删除；回答的第一条消息发出后（或恢复时已有消息），删掉它回复的那条消息的 `⏳ queued` 提示（`reply::drop_queued`，D98）。
- 降级链：富消息被拒（400 类内容错误）→ 模型部分改为原始 Markdown 块 → 仍被拒则纯文本 entities。
- 限速（`tg::flood` + `tg::gate`，D38）：每个 (bot, chat) 一个门，一次只做一个操作。每个请求带类别（完毕内容 > 新回答第一条 > 生成中更新）和预估轻重；门只在 Telegram 预算（按实测模型、带余量的滑动窗口）放得下时才放行，同时满足条件的按类别、再按到期时间排。拿到许可后按最新版本渲染，若比申请时更重且此刻放不下就退回重排。生成中的更新按回答年龄定节奏（`view::pace`）。被接受的操作写进 `flood_log`，重启后据此恢复；每页消息当前显示的大小/结构存在 `outbox.shape_*`。

## 6. OpenRouter 请求构建

1. 接口（`options::dialect_for`）：OpenAI、xAI 走 Responses，Anthropic 走 Messages，其余走 Chat Completions；方舟用同一套 Chat Completions 转换（图片 `detail: xhigh`）。
2. 历史转换（`chat` / `responses` / `messages` 的 `convert_history`）：同方言同模型的 assistant 回合回放 native（CC：`content` + `reasoning_details`；Responses：全部输出项，含加密推理与 `phase`，OpenAI 的还记下 `made_with`——当时 `safety_identifier` 和工具的指纹，GPT-6.2 起对不上时 OpenAI 悄悄拿掉那段推理，bot 预先提示；Messages：原始 content block），否则只发文本（别的模型生成的图片、文件放不进 assistant 消息，报错，`provider::unsent_media`；只发文本的回答带推理或工具调用时提示，`provider::text_only_note`，用和回放同一个判断；Gemini API 上所有 Gemini 模型的回答都回放 native；`+c` 时之前回答有文件就提示代码环境是新的，`provider::code_files_note`，D97）；用户内容按模态转换（图片：OpenAI `detail` original 或 high、xAI high、CC 不发 detail；PDF 走各接口的原生文件输入；音频 `input_audio`、视频/YouTube `video_url` 只在 CC 上；文本文件内联为独立 text 段）；不支持的输入模态直接报错。
3. 选项（`request::parse`：封闭的小集合，没有任意 JSON）：配置预设 → 用户选项逐项覆盖（`options::overlay`）→ 先按官方能力、再按 OpenRouter 能力校验（`request::validate`）→ `options::plan`：没指定的取能力拉满的默认值（来自 `/models` 实时目录），每项记录来源（`/info`）。
4. 额外检查：原生搜索在所选 endpoint 不存在时拒绝；service tier 自动加入对应 tier endpoint；PDF 强制 native 引擎；最终请求体再按 provider 白名单和 priority 名单检查（`policy::check`）。
5. 流解析（`chat::ChunkParser`，Responses/Messages 各有 `EventParser`）：跳过 `:` 注释心跳；`reasoning_details` 按 `index` 合并；图片 data URL 解码；`url_citation` 转为引用；带内错误（HTTP 200 后的 `error` 块）按 `error_type` 分类；没有 finish/`[DONE]` 就断开视为可重试的网络错误；空闲超时（默认 3600 秒，方舟 1800 秒；保活注释也算数据，每个请求结束时日志记录最长间隔 `longest_gap_ms`）。

## 7. 官方直连 provider

- `omni_llm::provider`：`Provider` trait（`prepare` → `Prepared`，`stream` → 事件，`knows` 模型表，`balance` 余额）、`DirectConfig`、价目表工具（`TokenPrice`、`PricingSink` 给没有费用的用量估价）、`PROVIDERS`（id、名字、key 的环境变量、默认地址）。bot 启动时为每个有 key 的 `[providers.<id>]` 建一个，`App.direct` 按 id 查；`jobs` 按回合的 provider 分派。
- 每个平台一个模块：自己的模型表（上下文、最大输出、推理档位、价格）、`RouteCaps`（官方能力即通道能力）、历史转换（同方言同模型原样回放 native，否则只发文本）、请求体、错误分类。流解析复用三种方言的解析器（OpenRouter 的 Responses / Messages `EventParser`、Chat Completions `ChunkParser`，各自包一层补上平台特有的事件）和 `chatstream`；Gemini 是自己的 generateContent 解析器（含 long decode 的续写和 checkpoint：`Prepared::with_continuation`、`merge_natives` 按方言分派）。
- 一个平台有两种接口时（DeepSeek 的 Chat + Anthropic 兼容、Moonshot 的 Chat + Responses、百炼的 Chat + Responses），两种方言分开记 native，只在同方言同模型时回放。
- 出图/视频接口也是方言（OpenAI Images、xAI Imagine 图片和视频、方舟 Seedream、智谱图片和视频、百炼、Gemini Omni 的 Interactions API），由同一个 provider 按模型 ID 分派；异步任务（视频、GLM-Image）建好就发 `Checkpoint`，`with_continuation` 把它变成"查询同一个任务"的请求。
- `omni_llm::meta`：每次调用经过一个 `Collector`（包住 raw 日志的 `Observer`），按接口家族（Responses、Messages、generateContent、Chat Completions）收集去掉内容的响应信封、一部分响应头和耗时，不需要各 provider 解析；`jobs` 把它存进 `attempts.meta`（`/info` 显示），并据此算出页脚里"和请求不同"的服务事实（模型、等级、地域、推理强度）和提示（D59）。重试提示（`retry-after-ms`、`x-should-retry`）在 `chatstream::run_with` 里统一处理。
- 流事件 `Restart`：provider 要把这次调用的全部输出重新发一遍（OpenAI 断流后从保存的结果恢复，D61），bot 丢掉这次调用已收到的部分；`ContextSize`：API 报告的精确上下文大小（xAI），下一轮的上下文直接用它。

## 8. 日志

- `verbose`（默认）：`raw` target 记录每个上游请求体、响应头、每个 SSE 事件（微秒时间戳，便于统计 token 间隔）；大 base64/data URL 存为 blob，日志里写 `<blob sha256:… N bytes mime>`。
- `normal`：只记录生命周期；`quiet`：只记录警告和错误。也可以直接写任意 tracing 过滤串，或用 `RUST_LOG` 覆盖。
- 每行都带 span 前缀：生成相关为 `turn{turn=…}:conv{bot=… chat=… user=… model=…}:attempt{n=… id=…}`，入站处理为 `inbound{id=…}:msg{bot=… chat=… user=… msg=…}`，更新循环为 `bot{bot=…}`。
- 输出到 stdout，并按天（UTC 零点）滚动写入 `data/logs/omni.YYYY-MM-DD.utc.log`（`log.file = false` 关闭；`log.keep_days` 为保留天数，默认 0 = 永久保存）。
