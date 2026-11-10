# 用户手册

## 1. 开始对话

消息第一行里第一个 `$` 之前是**前缀**，之后是发给模型的正文，**原样发送**：通常写成 `c$你好`；`c$ 你好` 发出的正文开头就带一个空格。

```
<spec>[,<spec>...]$<正文>
spec   := <模型>[+<选项>...]
模型   := alias | OpenRouter 模型 ID | provider:模型 ID
选项   := flag | key=value | {JSON}
```

- `$正文`：当前 bot 的默认模型（配置里的 `default_model`）。
- `alias$`：配置文件或管理员 `/alias` 定义的快捷名，`/models` 列出当前 bot 负责的全部 alias。
- `author/model$`：任意 OpenRouter 模型 ID，新模型无需改配置。群里只接受目录里存在的 ID（避免误触发）；本地目录里没有时会先立即刷新一次目录再判断，所以刚上线的模型也能马上用。私聊会直接把错误告诉你。
- `ark:<模型 ID>$`：直连火山方舟（豆包），见下文"火山方舟"。
- 多个 spec 用逗号分隔，会并行生成多条回答（上限见 `max_parallel_specs`）。多 bot 部署时，每个 spec 由负责该模型的 bot 回答。
- 第一行没有 `$`：不触发。前缀看起来像模型名（只含字母、数字和 `._:/-`）但不认识：直接报错（群里只由一个 bot 报）；像 `it costs 5$` 这样带空格的文字不算前缀。

### 选项

| 选项 | 作用（OpenRouter 请求体） |
|---|---|
| `s` / `search` | 厂商**原生**网页搜索（`openrouter:web_search`, `engine: native`）。所选 endpoint 没有原生搜索时直接报错，绝不静默换成 Exa |
| `s=exa` / `s=auto` / `s=parallel` … | 显式选择 OpenRouter 的搜索引擎（第三方，非厂商原生） |
| `e=<effort>` / `effort=` | `reasoning.effort`：none/minimal/low/medium/high/xhigh/max（不在模型支持列表里会给出提示） |
| `l` | 同 `e=low`（旧版写法） |
| `nothink` | `reasoning.effort = none` |
| `budget=<n>` | `reasoning.max_tokens`（思考 token 预算） |
| `summary=<auto\|concise\|detailed>` | 推理摘要粒度（OpenAI） |
| `max=<n>` | `max_tokens` |
| `t=` `top_p=` `top_k=` `seed=` | 采样参数 |
| `v=<…>` | `verbosity` |
| `tier=<flex\|priority\|…>` | `service_tier`（自动把对应的 tier endpoint 加进固定 provider 列表）。默认大家都用平台默认等级；`flex`（更便宜）谁都能用；`priority`/`fast`/`ultrafast` 等更贵的等级只有 `[access].priority_users` 里的人能指定 |
| `p=<slug>[\|<slug>…]` | 换成该模型的另一个**官方**提供方，如 Gemini `p=google-vertex`、GPT `p=azure`、Claude `p=amazon-bedrock`（白名单见配置 `allowed_providers`；区域如 `google-vertex/us` 也行）。不允许任意第三方，也没有 `p=any` |
| `img` / `text` | 输出模态 image+text / 仅 text |
| `1k` `2k` `4k` / `size=` | 图片尺寸 `image_config.image_size` |
| `ar=16:9` 及旧写法 `S W H w h` | 图片宽高比 1:1 16:9 9:16 4:3 3:4 |
| `json` | `response_format: json_object` |
| `pure` | 不应用自动"能力拉满"预设（仍保留固定第一方 provider 等保真措施） |
| `debug` | 要求 OpenRouter 回显真实上游请求（配置 `echo_upstream_body` 默认已对所有请求开启），用回复 + `/info` 查看 |
| `{...}` | JSON merge patch，最后应用，可改任何字段；值为 `null` 表示删除该字段 |

旧版的连写单字母仍可用：`g+s4W$` = 搜索 + 4K + 16:9。

### 火山方舟（豆包，`ark:`）

`db$`、`dbe$`、`dbl$`、`dbt$`、`db2$`、`db2l$`、`db2m$`、`db21$` 等 alias 直连方舟 Chat API（需要 `ARK_API_KEY`）。可用选项（其它选项直接报错，不会被静默忽略）：

| 选项 | 作用（方舟请求体） |
|---|---|
| `e=<none\|minimal\|low\|medium\|high>` | `reasoning_effort`（默认 high；`minimal` = 不思考） |
| `nothink` | `thinking.type = disabled` + `reasoning_effort = minimal` |
| `max=<n>` | `max_tokens`（方舟的 max_tokens **只计回答**，不含思考） |
| `t=` `top_p=` | 采样参数 |
| `detail=<low\|high\|xhigh>` | 图片精度（默认 xhigh） |
| `s` | 豆包搜索 `web_search_options`（需先在方舟控制台开通） |
| `json` / `tier=` / `pure` / `{...}` | 同上 |

方舟默认 `max_tokens` 只有 4096 且会静默截断，所以 bot 总是显式发送：模型最大回答长度（受上下文限制），上限 65536（`[providers.ark].max_tokens_cap`；方舟按"输入 + max_tokens"预扣 TPM，设得过大会更快触发 429）。方舟响应不带费用，统计里的费用按价目表（人民币，最低档位）估算，汇率见 `cny_per_usd`。

### 默认参数（能力拉满）

除非 `+pure`，每个请求会根据 OpenRouter 实时模型目录自动加上：

- `reasoning.effort` = 模型支持的最高档（没有档位列表的推理模型发 `reasoning.enabled = true`）；
- `max_tokens` = 该模型第一方 endpoint 的最大输出，但不超过上下文窗口剩余空间（用上一轮同模型回答报告的精确 token 数 + 之后新消息的保守估算），且不超过 `max_tokens_cap`（默认 393216）；万一仍超出上下文，会自动缩小后立即重发；
- Gemini 4（long decode）：`max_tokens` 是跨多次续写的**总预算**（默认 393216），模型超过单次输出上限时 bot 自动续写；达到预算时回答末尾会提示，回复 `/continue` 继续写；
- OpenAI 模型 `reasoning.summary = detailed`；
- 图片输出模型 `modalities = ["image","text"]`；配置中的预设（例如 Gemini 图片 4K）。

以及始终生效的**保真措施**（`+p=any` 关闭前两项）：

- `provider.only = [模型厂商自己的 endpoint]`、`allow_fallbacks = false`；
- `require_parameters = true`（不支持某参数的 endpoint 直接报错，而不是静默丢参数）；
- 关闭 `context-compression`；PDF 强制 `file-parser` 的 `native` 引擎，若 OpenRouter 仍然自行解析会在回答中提示。

每个请求实际发送了什么，用 `/info` 查看（包含每个补丁的来源）。

### 自定义 system prompt 与前文历史

正文中单独成行的 `::system`、`::user`、`::assistant`（也可 `::sys` `::u` `::a` `::model`）把正文切成多轮：

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
- 想发送一行字面量 `::user`，写成 `\::user`。

### 图片、文件、相册

- 发送带前缀说明（caption）的图片/文件/相册；或用带前缀的消息**回复**一条图片/文件/相册消息，就会附上它们。
- 下载按 Telegram 文件 ID 去重缓存，同一张图只下载一次。
- PDF、音频、视频：仅当模型在 OpenRouter 上原生支持该输入模态时才发送，否则报错（绝不本地转换冒充）。
- 纯文本类文件（.txt/.py/.md…）：Chat Completions 没有原生文本文件输入，因此作为**独立的 text 段**原样内联，并在回答中注明。
- `@https://www.youtube.com/watch?v=…` / `@https://youtu.be/…`：把 YouTube 视频作为 URL 附件交给模型（仅 Gemini 经 AI Studio 支持），正文不变。

## 2. 继续对话

- **回复** bot 的任意一条消息（包括长回答的任意分段、生成的图片）即继续该对话，使用同一模型与选项。回复的全部文字原样发送；以 `\` 开头时去掉这一个反斜杠（用来发送以 `/` 开头的字面量等）。
- 回复一条仍在生成中的消息：会排队，等它生成完再继续。
- 某一轮重试后成功：回复这一轮的任何一条消息（包括失败尝试的那几条）都等同于回复成功的那一条。
- 某一轮所有尝试都失败：回复它会得到错误提示；对它回复 `/retry` 重新生成。
- 带"引用"（quote）的回复会被忽略（与旧版一致）。
- 回复 + `/m spec$正文`：换成别的模型/选项继续这段对话（可以多个 spec）。

## 3. 回答的样子

```
🤖 anthropic/claude-opus-5.5                       ← 第一行：模型
▸ 💭 Reasoning · 1,234 tokens                      ← 可折叠（生成中、正文出来前展开）
▸ 🔧 Tool calls · 2
……模型的回答（富文本 Markdown）……
▸ 📚 Sources · 5
▸ ⚠️ Error: …                                      ← 展开是原始错误 JSON
ℹ️ …                                               ← 小字：bot 的提示（参数映射、文件内联、截断原因…）
+s · in 1,234 (cached 1,000) · out 567 · $0.0123 · 12.3s · via Anthropic   ← 小字：选项、用量、费用
```

- 模型生成的图片直接嵌在回答里它出现的位置，图下小字标注尺寸、文件大小和格式；一条回复尽量就是一条消息，只有超过 Telegram 的上限（32768 字、500 块、50 个图片/文件）才拆成多条。
- **结构无歧义（防伪）**：每条回复消息的第一行是模型名；除此之外 bot 加的所有东西都在折叠块或小字页脚里，而模型输出的 Markdown 永远渲染不出这两种块（`<details>` 等 HTML 一律按字面显示）。所以任何人都能准确指出哪部分是模型的回答。
- 超过一条富消息的上限会自动拆成多条，后续消息第一行是 `🤖 模型 · part N`。
- Telegram 会把每段超过 70000 字节的文字静默截断（切在字符中间时整段变空），bot 会把长文字拆成多个文本节点发送来避免；回复发完后还会读回比对，若仍被改动，就在消息里附上原始 Markdown。
- 某个块（如超过 20 列的表格、嵌套过深的列表）无法用富消息表达时，该块显示为原始 Markdown；整条富消息被 Telegram 拒绝时，模型部分改为原始 Markdown 再发；仍被拒绝才退回纯文本。Telegram 不接受的空结构（例如结尾多出的一个 ```` ``` ````、流式中刚打开的代码块）会被自动处理。
- `render.markdown = "raw"` 时模型输出始终以原始 Markdown 展示。
- 失败的尝试会保留在原消息中（附错误），重试在新消息里进行；bot 重启打断的生成也一样（原消息标注"interrupted"）。Gemini 4 的 long decode 例外：从最近的 checkpoint 在原消息里接着写。
- **没有"停止生成"**：群里大家都在看，一旦开始就生成到底。

## 4. 命令

| 命令 | 说明 |
|---|---|
| `/help` `/help options` | 帮助 / 选项列表 |
| `/models [关键词]` | 当前 bot 负责的 alias；带关键词则搜索 OpenRouter 目录 |
| `/model <alias 或 ID>` | 模型能力（模态、推理档位、最大输出、价格）及各 endpoint（是否第一方、量化） |
| 回复 + `/raw` | 一条消息：该回答的原始 Markdown（短的直接放代码块，推理在折叠块里；长的、或含 Telegram 会改写的字符如制表符时附 `.md` 文件），以及原图/原文件（附件） |
| 回复 + `/info` | 一条消息：请求摘要，附完整的请求详情 JSON（每次尝试的请求体、补丁来源、上游 ID、用量、错误、上游回显） |
| 回复 + `/retry` | 重新生成一个失败的回答 |
| 回复 + `/continue` | 让停在 long decode 预算上的回答（Gemini 4）接着写：新内容在新消息里，并入同一轮对话 |
| 回复 + `/m spec$正文` | 换模型继续 |
| `/stats [today\|7d\|30d\|all]` | 群里：本群统计；私聊：你自己在所有群的用量 |
| `/usage` | 你（和本群）的限额桶状态 |
| `/whoami` `/ping` | 显示 user id / chat id、是否被允许 |

### 管理员（`[access].admins`）

| 命令 | 说明 |
|---|---|
| `/allow [chat_id]` / `/deny [chat_id]` | 允许/移除群（不带参数 = 当前群）；正数 id 视为用户 |
| `/allow_user <id>` / `/deny_user <id>` | 允许/移除私聊用户 |
| `/ban <id>` / `/unban <id>` | 拉黑用户（任何地方都忽略） |
| `/acl` | 查看名单 |
| `/quota user\|chat <id\|default> <容量USD> <每日恢复USD>` / `… off` | 按人/按群的花费令牌桶；`default` 为默认规则 |
| `/alias <名字> <spec>` / `/unalias <名字>` | 运行时定义/删除 alias（不用改配置、不用重启） |
| `/credits` | OpenRouter key 的额度与用量 |
| `/stats all [周期]`（私聊） | 全局统计 |

限额是"后付费令牌桶"：请求开始前检查桶是否 > 0，结束后按实际费用扣减（可扣成负数，恢复后才能再用）。费用按每次上游调用记账、按 generation id 去重：流中断、bot 重启或崩溃导致没收到用量的调用，会稍后向 OpenRouter `/generation` 补查，不漏记也不重复扣。

## 5. 多 bot

`[[bots]]` 可以配置多个 bot。对每个请求，按配置顺序找第一个**在该群里**且 `models` 模式匹配的 bot 来回答；未加 @ 的命令只由该群中排第一的 bot 回答；回复某条消息由发出那条消息的 bot 处理。私聊里每个 bot 只回答自己负责的模型。
