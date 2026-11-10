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
| D8 | **不要按钮；没有"停止生成"功能**（群里旁观者需要完整输出；多数厂商断连也不停止计费）。功能用"回复 + 命令"实现。 |
| D9 | 回复 bot 消息时全文原样作为正文，不解析前缀；换模型用命令（`/m spec$ text`）。任何 API 合法的字符串都必须能发出：回复以 `\` 开头时去掉这一个反斜杠；字面量 `::role` 行写成 `\::role`。 |
| D10 | 回复一个"所有尝试都失败"的回合：直接报错提示（并提示可用 /retry）。重试后成功的回合：回复其任何一条消息都视为回复成功的那条。 |

## 假设（可推翻）

| # | 假设 | 理由 |
|---|---|---|
| A1 | 默认固定模型厂商自己的 endpoint（`provider.only`、无 fallback、`require_parameters`）；`+p=any` 才交给 OpenRouter 路由。DeepSeek 等开源模型因此需要用官方节点服务的带日期 slug（配置里已这样写）。 | 测试"原汁原味"的模型；OpenRouter 默认常把开源模型路由到第三方量化节点。 |
| A2 | 自动 `max_tokens` 上限 393216（`max_tokens_cap`）。 | 目录里部分模型报 ~94 万；OpenRouter 按 max_tokens 预占额度，过大易在并发时 402。 |
| A3 | `+s` 只用厂商原生搜索；endpoint 没有原生搜索时报错，`+s=exa` 等才用 OpenRouter 的第三方搜索。 | 不接入额外工具、不静默替换（原则）。 |
| A4 | 不默认设置 `verbosity`。 | 它改变回答风格（Claude 上还会覆盖 effort），不属于"能力"。 |
| A5 | 带引用（quote）的回复忽略（同旧版）；用户消息中的 Telegram 格式不转为 Markdown（同旧版，发送纯文本）。 | 保持旧行为。 |
| A6 | 群聊限速默认 3 秒/操作/bot（同旧版），私聊 1 秒。 | Telegram 群聊约 20 条/分钟。 |
| A7 | 重试：最多 4 次（含首次），只重试可重试错误（429、5xx、超时、断流），退避 10s 起翻倍、上限 120s，尊重 Retry-After；400/401/402/403/404 等不重试。重启中断不计入次数。 | 用户反馈过"不可能成功也在重试"。 |
| A8 | 生成的图片作为单独的照片消息发送（回复回答的第一条消息），原图用 /raw 以文件形式获取。 | 富消息内嵌图片需先 uploadMedia，留待后续。 |
| A9 | 发送给 OpenRouter 的 `user` 字段：仅当配置了 `user_id_salt` 时发送 sha256(salt+user_id)。 | 旧版对 OpenAI 发 safety_identifier；OpenRouter 本身也会再哈希。 |
| A10 | 群里只接受模型目录中存在的原始模型 ID（新模型最多等 10 分钟目录刷新，未命中会触发后台刷新）；私聊不限制。 | 避免群聊里普通文字中的 `a/b$` 误触发。 |

## 已知限制 / 待办（按优先级）

1. **尚未用真实 Telegram / OpenRouter key 联调**（等 key）。重点验证：富消息发送/编辑的真实报错名、群里编辑速率、`reasoning_details` 流式合并方式、图片 `image_config` 键名、`require_parameters` 与 `modalities` 的关系等（docs/openrouter-gaps.md 末尾清单）。
2. **OpenAI 走 Responses 方言**（GPT-5.4+/6 的 `phase`、加密推理项）——目前所有模型都走 Chat Completions；OpenRouter 文档称缺 `phase` 会导致早停。
3. **Anthropic 走 Messages 方言**（原生 thinking 签名块、`thinking.display`、引用块、工具变更块）。
4. 私聊使用 Telegram live draft 流式（仅私聊可用，群里只能 send+edit）。
5. 群内 per-user 提示用 ephemeral message（bot 需是管理员或 15 秒窗口）。
6. 富消息内嵌生成的图片（`messages.uploadMedia` + `pageBlockPhoto`）。
7. Gemini 4 long-decode continuation（OpenRouter 唯一的"续写"机制）。
8. 直连官方 API 的 provider（OpenAI background 模式 + `starting_after` 才能真正断线续流）。
9. 消息格式化：把用户消息里的 Telegram entities 还原为 Markdown（可选）。
