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
jobs：等待父回合 → attempts（重试）→ OpenRouter / 方舟流式 → Accumulator
   │                                         │
   │                                         └─ rawlog（verbose：原始 SSE，blob 落盘）
   ▼
view：RenderState → compose（富消息块）→ paginate → outbox（幂等 send/edit，按 (bot, 聊天) 限速）
```

四个 crate：

| crate | 职责 | 依赖 |
|---|---|---|
| `omni-core` | 对话内容模型（`Turn`/`Part`/`Reasoning`/`Citation`/`NativeOutput`）、blob 引用、触发语法与 `::role` 解析、用量、JSON 工具（merge patch、blob 脱敏） | 纯逻辑 |
| `omni-markdown` | Markdown → 自有 AST（带源码范围）→ 富消息块 `RBlock`/`RText`（镜像 TL PageBlock）或经典 entities；限额统计与切分 | pulldown-cmark |
| `omni-llm` | Provider 抽象（`CallInput`→`Prepared`→流事件）、SSE 解析、错误分类、OpenRouter（目录、Chat Completions / Responses / Messages 三种接口、选项/预设解析）、火山方舟直连 | reqwest |
| `omni-bot` | 二进制 `omni`：配置、日志、SQLite、grammers Session、Telegram 收发、多 bot（各自独立）、任务、渲染、命令、权限、限额 | grammers、rusqlite |

## 2. 对话数据模型

对话是一棵树（回复任意一条消息即分叉），存于 `turns` 表：

- `role`：system / user / assistant；`data`：`omni_core::Turn` 的 JSON；`parent_id`。
- `Turn.parts`：规范化内容，按顺序：`Text`（含 `citations`）、`Image`、`File`（PDF/音频/视频/文本…，按 MIME 在请求时决定发送方式）、`RemoteMedia`（如 YouTube URL）、`Reasoning`（kind：Text/Summary/Encrypted/Redacted，`opaque` 存签名/密文）、`ToolCall`（服务端内置工具的输入输出原文）、`Refusal`。
- `Turn.native`：**该轮在其 API 方言下的原样输出**（例如 Chat Completions 的 assistant message，含完整 `reasoning_details` 签名/密文；Responses 的全部输出项；Messages 的 content block）。下一轮请求若方言与模型都相同，原样回放 native（最大保真）；否则只用规范化 `parts`（例如换模型继续时只带文本）。
- `Turn.origin`：Live（真实消息/真实生成）或 Synthetic（用户用 `::role` 写的伪造历史）。
- assistant 回合还有 `spec`（provider、模型、选项、alias 标签）与 `status`（pending/running/done/failed）。

`attempts`：每次上游调用一行，存脱敏后的请求体（含每个补丁的来源、OpenRouter 回显的上游请求）、部分/最终输出、原始错误 JSON、上游 generation id、用量。回合的最终内容 = 成功那次尝试的输出。

`tg_messages`：Telegram 消息 ↔ 回合（及尝试、分段号）。一轮的所有尝试的所有消息都映射到同一回合，所以回复失败尝试的消息 = 回复成功的那次。

blob（图片、文件、生成图、长 base64）以 sha256 内容寻址存于 `data/blobs/ab/cd/<sha>`；数据库与日志只存引用。`tg_files` 以 Telegram photo/document id 去重下载。

## 3. 可靠性：重启不重不漏

1. **入站**：grammers 以 `catch_up` 模式运行，使用我们基于同一 SQLite 实现的 `Session`。需要处理的消息写入 `inbound`（唯一键 bot+chat+msg_id）。持久化的 pts/qts/channel pts 只在"携带该状态的所有 update 都已处理完"之后才推进——grammers 给同一批 getDifference 结果的所有消息打上的是整批的最终状态，所以我们在看到一个**不同**的状态时才保存上一个状态。崩溃最多导致已处理的 update 被重新投递（被唯一键吸收），永远不会跳过未处理的。关机时也不会把内存中的状态写回（它可能包含已缓冲未处理的 update）。
2. **处理**：`inbound.status = new` 的行在启动时重新分发；handler 先查该 inbound 是否已建过回合，已建则只重启任务（幂等）。
3. **生成**：启动时所有 pending/running 的 assistant 回合重新运行。running 的最后一次尝试标记为 `interrupted`，其消息被改写为"已被重启中断"（保留已生成的部分），再开新尝试（中断不计入重试次数）。OpenRouter 不支持断线续流（见 openrouter-gaps.md），所以除 Gemini 4 long decode（从落盘的 checkpoint 在原消息上接着写）外只能重新生成；流断开后会用 generation id 查询实际费用并入账。
4. **发送**：每条待发消息先在 `outbox` 写入随机 `random_id` 再调用 `messages.sendMessage`。Telegram 永不遗忘用过的 random_id：重发同一个 id 返回已发出的那条消息而不是重复发送；`RANDOM_ID_DUPLICATE` 表示前一次仍在进行，退避后重试。编辑天然幂等（`MESSAGE_NOT_MODIFIED` 视为成功）。

## 4. 多 bot

- 每个 bot 一个 grammers 连接、一个更新循环、一组按 chat 的限速门（`Gates`）。
- 层级是 bot → 聊天 → 人。每个 bot 是独立实例；一个 bot 下群和私聊是平等的"聊天"，授权（允许的聊天、群内封禁）、限额（每个聊天一个桶，每个"聊天 + 成员"一个桶）和统计都在聊天之内，一个聊天里的设置和统计不影响别的聊天。运行时前缀（`aliases`）按 bot 分开，配置里的 `[[aliases]]` 所有 bot 共用。`[[bots]].models`（通配符，默认 `*`）不匹配的模型、未配置的 provider 的模型，对该 bot 不存在。
- 成员关系 `bot_chats` 从入群事件和收到的消息学习（只记录，不参与路由）。
- 触发：每个 bot 独立解析前缀，只回答它能解析的 spec；群里不认识的 spec 静默跳过（可能属于别的 bot），私聊和 `/m` 里报错。几个 bot 都认识同一个前缀时各自回答。直接写的模型 ID 只在该 provider 的 `allow_unlisted_models = true` 时可用。
- 回复：由发出被回复消息的 bot 处理（按 bot 维度查 `tg_messages`）。
- 命令：每个 bot 都回答发给它（`@`）或未加 @ 的命令；回复类命令（`/m` `/raw` `/dump` `/info` `/retry` `/continue`）未加 @ 时只由发出被回复消息的 bot 处理。
- 启动时为每个 bot 注册命令菜单：先重置旧版本可能留下的按语言/聊天类型的列表（它们会盖住默认列表），再设所有聊天、所有语言共用的一份；管理员的私聊（和 bot 说过话之后）另设含管理命令的列表。

## 5. 渲染与限速

- `compose`：把回合状态组装为富消息块（头部、折叠的推理/工具/来源、正文、提示、错误、页脚），每条消息第一行是 bot 的头部，bot 生成的其它部分只用模型 Markdown 永远不会产生的块（details、footer），保证模型输出无法冒充。
- `paginate`：按 32640 字 / 499 块切成多条消息（富消息上限 32768 / 500，留出续页头部的位置），超大块按结构拆分（代码按行、列表按项、表格按行并重复表头、折叠块按子块）。
- `view`：每次尝试一个同步任务，只渲染最新状态（watch 通道合并中间状态）；与已发出内容哈希比较，只编辑变化的分段，新增分段回复前一段发送；最终状态多余的分段删除。
- 降级链：富消息被拒（400 类内容错误）→ 模型部分改为原始 Markdown 块 → 仍被拒则纯文本 entities。
- 限速（`tg::flood` + `tg::gate`，D38）：每个 (bot, chat) 一个门，一次只做一个操作。每个请求带类别（完毕内容 > 新回答第一条 > 生成中更新）和预估轻重；门只在 Telegram 预算（按实测模型、带余量的滑动窗口）放得下时才放行，同时满足条件的按类别、再按到期时间排。拿到许可后按最新版本渲染，若比申请时更重且此刻放不下就退回重排。生成中的更新按回答年龄定节奏（`view::pace`）。被接受的操作写进 `flood_log`，重启后据此恢复；每页消息当前显示的大小/结构存在 `outbox.shape_*`。

## 6. OpenRouter 请求构建

1. 接口（`options::dialect_for`）：OpenAI、xAI 走 Responses，Anthropic 走 Messages，其余走 Chat Completions；方舟用同一套 Chat Completions 转换（图片 `detail: xhigh`）。
2. 历史转换（`chat` / `responses` / `messages` 的 `convert_history`）：同方言同模型的 assistant 回合回放 native（CC：`content` + `reasoning_details`；Responses：全部输出项，含加密推理与 `phase`；Messages：原始 content block），否则只发文本；用户内容按模态转换（图片：OpenAI `detail` original 或 high、xAI high、CC 不发 detail；PDF 走各接口的原生文件输入；音频 `input_audio`、视频/YouTube `video_url` 只在 CC 上；文本文件内联为独立 text 段并提示）；不支持的输入模态直接报错。
3. 选项（`request::parse`：封闭的小集合，没有任意 JSON）：配置预设 → 用户选项逐项覆盖（`options::overlay`）→ 先按官方能力、再按 OpenRouter 能力校验（`request::validate`）→ `options::plan`：没指定的取能力拉满的默认值（来自 `/models` 实时目录），每项记录来源（`/info`）。
4. 额外检查：原生搜索在所选 endpoint 不存在时拒绝；service tier 自动加入对应 tier endpoint；PDF 强制 native 引擎；最终请求体再按 provider 白名单和 priority 名单检查（`policy::check`）。
5. 流解析（`chat::ChunkParser`，Responses/Messages 各有 `EventParser`）：跳过 `:` 注释心跳；`reasoning_details` 按 `index` 合并；图片 data URL 解码；`url_citation` 转为引用；带内错误（HTTP 200 后的 `error` 块）按 `error_type` 分类；没有 finish/`[DONE]` 就断开视为可重试的网络错误；空闲超时（默认 3600 秒，方舟 1800 秒；保活注释也算数据，每个请求结束时日志记录最长间隔 `longest_gap_ms`）。

## 7. 日志

- `verbose`（默认）：`raw` target 记录每个上游请求体、响应头、每个 SSE 事件（微秒时间戳，便于统计 token 间隔）；大 base64/data URL 存为 blob，日志里写 `<blob sha256:… N bytes mime>`。
- `normal`：只记录生命周期；`quiet`：只记录警告和错误。也可以直接写任意 tracing 过滤串，或用 `RUST_LOG` 覆盖。
- 每行都带 span 前缀：生成相关为 `turn{turn=…}:conv{bot=… chat=… user=… model=…}:attempt{n=… id=…}`，入站处理为 `inbound{id=…}:msg{bot=… chat=… user=… msg=…}`，更新循环为 `bot{bot=…}`。
- 输出到 stdout，并按天（UTC 零点）滚动写入 `data/logs/omni.YYYY-MM-DD.utc.log`（`log.file = false` 关闭；`log.keep_days` 为保留天数，默认 0 = 永久保存）。
