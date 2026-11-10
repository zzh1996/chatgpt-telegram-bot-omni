# 设计决策记录

日期：2026-11-11（重写开始）。"已确认"= 与项目作者确认过；"假设"= 我自行决定、可随时推翻。

## 已确认

| # | 决策 |
|---|---|
| D1 | Rust 重写；MTProto（grammers），不用 HTTP Bot API；SQLite；Docker Compose 一键运行；单进程多 bot。 |
| D2 | 默认走 OpenRouter，接口按方言抽象，保留直连官方 API 的能力。 |
| D3 | **默认参数"能力拉满"**：最高推理强度、最大输出、最详细的推理摘要、最高图像精细度、能关的安全过滤都关。用户可覆盖（选项/JSON patch），`+pure` 关闭自动预设。"干净请求"指不注入 prompt、不篡改内容、不本地伪造功能，且实际发送的参数全部可见（/info）。 |
| D4 | Markdown 显示可配置：默认干净渲染，可选原始 Markdown（`render.markdown = "raw"`）。 |
| D5 | 不迁移旧 shelve 数据库，全新开始。 |
| D6 | 文本类文件：API 原生支持就按文件发，否则作为独立 text 段原样内联，并在回答中注明"由 bot 内联"。 |
| D7 | 优先 Telegram 富消息。"回退"不是退回经典 4096+entities，而是富消息中 bot 控制的结构保持富文本、AI 生成部分显示原始 Markdown（按块粒度）；只有富消息被整体拒绝时才退到纯文本。 |
| D8 | **不要按钮；没有"停止生成"功能**（群里旁观者需要看到完整输出）。功能用"回复 + 命令"实现。 |
| D9 | 回复 bot 消息时全文原样作为正文，不解析前缀；换模型用命令（`/m spec$ text`）。任何 API 合法的字符串都必须能发出：回复以 `\` 开头时去掉这一个反斜杠；字面量 `::role` 行写成 `\::role`。 |
| D10 | 回复一个"所有尝试都失败"的回合：直接报错提示（并提示可用 /retry）。重试后成功的回合：回复其任何一条消息都视为回复成功的那条。 |
| D11 | 豆包加**直连火山方舟** provider（`ark:`，Chat API），alias 沿用旧前缀（`db$` 等）。 |
| D12 | "verbosity 开到最大"指**日志级别 verbose**（记录原始上游流量，blob 落盘），默认开启且不能有性能问题；不是 API 的 `verbosity` 参数（A4 不变）。 |
| D13 | Gemini long decode：默认总预算约 39 万 token（393216），`+max=` 覆盖；达到预算后回复 `/continue` 续写。 |
| D14 | 重启恢复：API 能取回结果的就在原消息上更新、不重试；做不到的（OpenRouter 上除 Gemini 4 long decode 外都做不到）允许重试。 |
| D15 | 回复排版（看过实际消息后定的）：紧凑；不用下划线和斜体；第一行只写模型（不写用户输入的前缀/alias）；用量、费用用末尾小字；推理块显示 token 数而不是字符数；不要 "Answer" 标签。"防伪"只要求结构可无歧义地解析回去——专业的人能准确指出哪部分是模型最终输出——不需要强烈的视觉标记。 |
| D16 | `$` 后面的正文原样发送：通常不加空格（`c$你好`），加了空格就表示正文开头真的要有空格。 |
| D17 | 缺口文档（docs/openrouter-gaps.md）只写**真缺口**：官方能做到、OpenRouter 无论怎么配置都做不到的；格式、参数、默认值差异另放 docs/openrouter-fidelity.md。 |

## 假设（可推翻）

| # | 假设 | 理由 |
|---|---|---|
| A1 | 默认固定模型厂商自己的 endpoint（`provider.only`、无 fallback、`require_parameters`）；`+p=any` 才交给 OpenRouter 路由。DeepSeek 等开源模型因此需要用官方节点服务的带日期 slug（配置里已这样写）。 | 测试"原汁原味"的模型；OpenRouter 默认常把开源模型路由到第三方量化节点。 |
| A2 | 自动 `max_tokens` = min(第一方 endpoint 的最大输出, 393216, 上下文 − 估算 prompt − 余量)。prompt 用上一轮同模型回答报告的精确 token 数（`context_tokens`）+ 之后新消息的保守估算；endpoint 限额缓存、启动预取，不增加延迟；仍超上下文时解析报错里的数字（或减半）立即重发，不计失败次数。方舟另有 65536 上限（按 max_tokens 预扣 TPM）。 | 尽量长、不截断、不超限、不加延迟。注意 OpenRouter 按 max_tokens 冻结在途额度：额度很小的 key 上并发多个贵模型请求可能 402（可重试）。 |
| A3 | `+s` 只用厂商原生搜索；endpoint 没有原生搜索时报错，`+s=exa` 等才用 OpenRouter 的第三方搜索。 | 不接入额外工具、不静默替换（原则）。 |
| A4 | 不默认设置 `verbosity`。 | 它改变回答风格（Claude 上还会覆盖 effort），不属于"能力"。 |
| A5 | 带引用（quote）的回复忽略（同旧版）；用户消息中的 Telegram 格式不转为 Markdown（同旧版，发送纯文本）。 | 保持旧行为。 |
| A6 | 群聊限速默认 3 秒/操作/bot（同旧版），私聊 1 秒。 | Telegram 群聊约 20 条/分钟。 |
| A7 | 重试：最多 4 次（含首次），只重试可重试错误（429、5xx、超时、断流），退避 10s 起翻倍、上限 120s，尊重 Retry-After；400/401/402/403/404 等不重试。重启中断不计入次数。 | 用户反馈过"不可能成功也在重试"。 |
| A8 | 生成的图片作为单独的照片消息发送（回复回答的第一条消息），原图用 /raw 以文件形式获取。 | 富消息内嵌图片需先 uploadMedia，留待后续。 |
| A9 | 发送给 OpenRouter 的 `user` 字段：仅当配置了 `user_id_salt` 时发送 sha256(salt+user_id)。 | 旧版对 OpenAI 发 safety_identifier；OpenRouter 本身也会再哈希。 |
| A10 | 群里只接受模型目录中存在的原始模型 ID；本地目录未命中时先立即刷新目录（20 秒内最多一次）再判断，仍不存在才忽略；私聊不限制。 | 避免群聊里普通文字中的 `a/b$` 误触发，同时新模型无需等待。 |
| A11 | 用量账本每次上游调用一行，按 generation id 幂等；拿不到用量的调用（断流、重启、崩溃）登记为待查，后台向 `/generation` 补查。 | 统计不漏不重。 |
| A12 | 回复里的选项（如 `+s`）放在末尾小字里，不放第一行。 | 第一行只写模型；但同一模型不同选项并行比较时仍需能区分。 |

## 已知限制 / 待办（按优先级）

1. **重启时真正的"续流"**：OpenRouter 无状态，除 Gemini 4 long decode（已实现：checkpoint 落盘，重启后在原消息上接着写）外，重启只能重试；`/generation/content` 只能补回断开前已送达的文本（已用于填充被打断的那条消息）。可选方案：一个很少重启的独立 relay 进程持有上游连接并缓冲 SSE，bot 重启后从 relay 接着读（待作者决定）。
2. **方舟 provider 未经真实请求验证**（等 `ARK_API_KEY`）：`max_tokens` 的真实上限、`encrypted_content` 回传、`detail:"xhigh"`、错误码。
3. **OpenAI 走 Responses 方言**（`phase`、加密推理项）——目前都走 Chat Completions；OpenRouter 文档称 CC 缺 `phase` 会导致早停。
4. **Anthropic 走 Messages 方言**（原生 thinking 签名块、`thinking.display`、引用块、工具变更块）。
5. 私聊使用 Telegram live draft 流式（仅私聊可用，群里只能 send+edit）。
6. 群内 per-user 提示用 ephemeral message（bot 需是管理员或 15 秒窗口）。
7. 富消息内嵌生成的图片（`messages.uploadMedia` + `pageBlockPhoto`）。
8. 测试工具：`[dev] inject_dir` 可注入模拟用户消息、导出 bot 发出的消息结构、探测富消息规则（bot 收不到其它 bot 的消息，测试不需要用户账号）；媒体消息的端到端测试仍需真人发送。
