# omni — 多 bot、多模型的 Telegram 大模型测试 bot

[chatgpt-telegram-bot](https://github.com/zzh1996/chatgpt-telegram-bot) 的 Rust 重写版。面向自部署者和开发者：在 Telegram（主要是群聊）里，像直接调用官方 API 一样体验和测试大模型。

- **MTProto**（grammers），不是 HTTP Bot API；发送用持久化的 `random_id` 实现幂等。
- **单进程多 bot**：每个 bot 都像独立实例（各自的模型范围、授权名单、限额、前缀和速率限制），可按模型/群组分工。
- **OpenRouter 与各家官方 API**：默认走 OpenRouter；配了 key 的平台也可以直连它自己的 API（OpenAI、Anthropic、Gemini、xAI、DeepSeek、智谱、Moonshot、阿里云百炼、火山方舟；国内厂商接国内官网，和旧版一样），模型写成 `openai:gpt-6.1-sol` 这样，回复第一行也这样显示。
- **请求干净透明**：不注入任何 system prompt；所有参数（包括默认的"能力拉满"预设）都可查看（/info），并可用选项逐项覆盖（如 `+e=low`）；上游错误原样展示。
- **重启不重不漏**：入站消息先落盘、处理完一批后才推进 Telegram 的 pts（重复投递被唯一键去重）；被重启打断的生成在原消息里重新生成（Gemini 4 long decode 从 checkpoint 接着写）；半截消息会被标注而不会悬空。
- **多模态进出**：Telegram 能发的都能当输入（照片、相册、文件、语音、视频、贴纸、富消息、位置…）；模型生成的图片、视频、音频、文件按产生的位置显示，`/raw` 取原文件。出图和视频按各平台官方推荐的方式：聊天模型出图（Nano Banana，OpenAI/Grok 的 `+i`），或直接写出图/视频模型的 ID 调它的接口（gpt-image、Grok Imagine、Seedream、CogView/GLM-Image、千问图像/Wan）。
- **Telegram 富消息**：思考过程放在可折叠块里，表格/公式/代码高亮；工具调用（搜索、代码执行…）显示成可读的内容而不是裸 JSON；超出能力的部分按块回退为原始 Markdown；每条回复第一行是模型名，bot 加的其它内容都在折叠块或小字页脚里，模型输出无法伪造。

## 快速开始（Docker）

```sh
cp .env.example .env                       # 填入 TELEGRAM_API_ID / TELEGRAM_API_HASH / TELEGRAM_BOT_TOKEN / OPENROUTER_API_KEY
                                           # （可选：各平台官方 API 的 key，如 OPENAI_API_KEY、DEEPSEEK_API_KEY）
cp config/omni.example.toml config/omni.toml
# 编辑 config/omni.toml：至少把你的 Telegram user id 填进 [access].admins
docker compose up -d --build
docker compose logs -f
```

- `api_id`/`api_hash` 来自 <https://my.telegram.org/apps>；bot token 来自 @BotFather。
- 群里使用需要在 @BotFather 关闭 bot 的 privacy mode，然后管理员在群里发 `/allow`（每个 bot 分别授权）。
- 数据（SQLite、图片等 blob、日志）都在 `./data`；配置在 `./config`。日志默认 verbose 且永久保存（`[log] keep_days = 0`），磁盘会一直增长。
- 同一个 bot token 不要同时跑在旧 bot 和新 bot 上，否则两边都会回答。
- 如果请求报 "No endpoints found matching your data policy"，是 OpenRouter 账号的隐私设置把模型厂商自己的 endpoint 过滤掉了，到 OpenRouter 的 Privacy 设置里放开。
- 不用 Docker：`cargo run --release -p omni-bot -- --config config/omni.toml`，校验配置：`omni check-config`。

## 用法速览

| 写法 | 含义 |
|---|---|
| `$你好` | 名字为空的前缀（配置里 `[[aliases]] name = ""`） |
| `c$你好` | 前缀（`/models` 查看）；`$` 后的正文原样发送 |
| `anthropic/claude-opus-5.5$你好` | 直接写 bot 有的任意 OpenRouter 模型 ID（需 `allow_unlisted_models = true`），新模型无需改配置 |
| `openai:gpt-6.1-sol$你好` | 直连该平台的官方 API（需配置它的 key；直接写 ID 同样需 `allow_unlisted_models = true`） |
| `c,g,61s$比较一下` | 多个模型并行 |
| `c+s$` / `c+e=low$` / `gi+4W$` | 选项：官方搜索、推理强度、图片尺寸/比例…（本地校验，`/help options`） |
| `openai:gpt-image-2.6-sunburst$一只猫` | 直接调出图/视频模型（回复生成的图 = 编辑它；`+n=` `+q=` `+bg=` `+sec=`） |
| `c$::system` + 换行 + 角色段落 | 自定义 system prompt 和前文历史 |
| 图片/文件/相册的说明里写前缀，或带前缀回复图片/文件/相册/位置 | 附件交给模型；相册当一条消息；位置给 Gemini 的 Google Maps（`+m`）；富消息按 Markdown |
| 回复 bot 消息 | 原样作为下一轮用户消息（开头的 `\` 会被去掉，用于转义） |
| 回复 + `/m g$文字` | 换模型继续这段对话 |
| 回复 + `/raw` `/dump` `/info` `/retry` `/continue` | 原始 Markdown/文件、整段对话的 JSON、请求详情、重试失败的回答、继续写撞到预算的回答 |

完整说明见 [docs/usage.md](docs/usage.md)。

## 文档

- [docs/usage.md](docs/usage.md) — 用户手册（语法、选项、命令、管理）
- [docs/architecture.md](docs/architecture.md) — 架构、数据模型、可靠性设计
- [docs/decisions.md](docs/decisions.md) — 设计决策记录、已知限制与待办
- [docs/openrouter-gaps.md](docs/openrouter-gaps.md) — **官方 API 能做、经 OpenRouter 无论怎么配置都做不到的功能（按厂商，含待实测的疑似项）**
- [docs/openrouter-fidelity.md](docs/openrouter-fidelity.md) — 经 OpenRouter 时与直连官方的差异：路由、注入的默认值、参数映射、错误归一化、断连计费，以及 bot 的规避措施
- [docs/research/](docs/research/) — 各家官方 API、OpenRouter、Telegram 的调研笔记（2026-11-11）

## 开发

```sh
cargo test --workspace                                         # 单元 + 集成测试
cargo test -p omni-llm --test live -- --ignored --nocapture    # 在线测试（需要网络；设置 OPENROUTER_API_KEY 时测流式补全，LIVE_MODEL 指定模型）
```

代码结构：

```
crates/omni-core      领域模型：对话树、内容块、触发语法解析、用量
crates/omni-markdown  Markdown → Telegram（富消息块 / 经典 entities），限额切分
crates/omni-llm       Provider 抽象、OpenRouter（目录、Chat Completions / Responses / Messages 三种接口、选项、SSE、错误分类）、各平台官方 API 直连（OpenAI、Anthropic、Gemini、xAI、DeepSeek、智谱、Moonshot、百炼、方舟）
crates/omni-bot       可执行文件：MTProto、多 bot（各自独立）、SQLite、任务、渲染、命令、权限与限额
```
