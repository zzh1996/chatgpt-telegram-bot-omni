# omni — 多 bot、多模型的 Telegram 大模型测试 bot

[chatgpt-telegram-bot](https://github.com/zzh1996/chatgpt-telegram-bot) 的 Rust 重写版。面向自部署者和开发者：在 Telegram（主要是群聊）里，像直接调用官方 API 一样体验和测试大模型。

- **MTProto**（grammers），不是 HTTP Bot API；发送用持久化的 `random_id` 实现幂等。
- **单进程多 bot**：每个 bot 有独立的速率限制，可按模型/群组分工。
- **默认走 OpenRouter**，接口按"方言"抽象，便于以后直连各家官方 API。
- **请求干净透明**：不注入任何 system prompt；所有参数（包括默认的"能力拉满"预设）都可查看（/info）、可覆盖、可关闭（`+pure`）；上游错误原样展示。
- **重启不重不漏**：入站消息先落盘、处理完一批后才推进 Telegram 的 pts（重复投递被唯一键去重）；生成中断后自动在新消息里重试；半截消息会被标注而不会悬空。
- **Telegram 富消息**：思考过程放在可折叠块里，表格/公式/代码高亮；超出能力的部分按块回退为原始 Markdown；bot 自己的信息都带下划线标签，模型输出无法伪造。

## 快速开始（Docker）

```sh
cp .env.example .env                       # 填入 TELEGRAM_API_ID / TELEGRAM_API_HASH / TELEGRAM_BOT_TOKEN / OPENROUTER_API_KEY
cp config/omni.example.toml config/omni.toml
# 编辑 config/omni.toml：至少把你的 Telegram user id 填进 [access].admins
docker compose up -d --build
docker compose logs -f
```

- `api_id`/`api_hash` 来自 <https://my.telegram.org/apps>；bot token 来自 @BotFather。
- 群里使用需要在 @BotFather 关闭 bot 的 privacy mode，然后管理员在群里发 `/allow`。
- 数据（SQLite、图片等 blob、日志）都在 `./data`；配置在 `./config`。
- 不用 Docker：`cargo run --release -p omni-bot -- --config config/omni.toml`，校验配置：`omni check-config`。

## 用法速览

| 写法 | 含义 |
|---|---|
| `$ 你好` | bot 的默认模型 |
| `c$ 你好` | alias（`/models` 查看） |
| `anthropic/claude-opus-5.5$ 你好` | 任意 OpenRouter 模型 ID，新模型无需改配置 |
| `c,g,gpt$ 比较一下` | 多个模型并行 |
| `c+s$` / `c+e=low$` / `g+4k+ar=16:9$` | 选项：原生搜索、推理强度、图片尺寸/比例… |
| `c+{"temperature":0.2}$` | 任意 JSON merge patch 直接改请求体 |
| `c$ ::system` + 换行 + 角色段落 | 自定义 system prompt 和前文历史 |
| 回复 bot 消息 | 原样作为下一轮用户消息（开头的 `\` 会被去掉，用于转义） |
| 回复 + `/m g$ 文字` | 换模型继续这段对话 |
| 回复 + `/raw` `/info` `/retry` | 原始 Markdown/文件、请求详情、重试失败的回答 |

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
cargo test -p omni-llm --test live -- --ignored --nocapture    # 在线测试（需要网络；设置 OPENROUTER_API_KEY 时测流式补全）
```

代码结构：

```
crates/omni-core      领域模型：对话树、内容块、触发语法解析、用量
crates/omni-markdown  Markdown → Telegram（富消息块 / 经典 entities），限额切分
crates/omni-llm       Provider 抽象、OpenRouter（目录、Chat Completions 方言、选项、SSE、错误分类）
crates/omni-bot       可执行文件：MTProto、多 bot 路由、SQLite、任务、渲染、命令
```
