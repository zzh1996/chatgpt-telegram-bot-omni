//! `/help`: how to use the bot, for someone who knows nothing about it, as one
//! rich message.
//!
//! `/help` is a quick start and a table of the commands, then every topic as a
//! fold (nothing to navigate: tap what you need); `/help <topic>` or
//! `/help <command>` (also `/help_<topic>`) shows one topic open. Examples are
//! code spans, so a tap copies them. Administrators also get the admin topic
//! of the chat they are in: a group's commands act on that group only, the
//! private chat's on every chat.
//!
//! The help is about the bot, not about platforms or models: which models
//! exist is configuration (/models lists them), so the texts name none, apart
//! from examples.
//!
//! The texts are Markdown, plus folds inside a topic: a line `::: Title`
//! starts one and a line `:::` ends it. `$` always stands for the configured
//! trigger marker (amounts are written in USD, never with `$`), and `{…}`
//! names a value of this deployment ([`Ctx`]).

use omni_markdown::rich::{RBlock, RText};

/// What the texts depend on: the deployment and who asks where.
pub struct Ctx {
    /// The trigger marker (`general.trigger_marker`).
    pub marker: String,
    /// OpenRouter model ids can be typed instead of prefixes (`author/model$`).
    pub ids_openrouter: bool,
    /// Some platform's own API takes typed model ids (`provider:model$`).
    pub ids_platforms: bool,
    /// `general.max_file_mb`.
    pub max_file_mb: u64,
    /// `render.bold_cost_usd` (0: never).
    pub bold_cost_usd: f64,
    /// `render.answer_first`.
    pub answer_first: bool,
    pub admin: bool,
    pub group: bool,
    pub lang: Lang,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Who {
    Everyone,
    /// Administrators, in a group: the commands that act on that group.
    AdminInGroup,
    /// Administrators, in their private chat: the commands for every chat.
    AdminInPrivate,
}

struct Topic {
    key: &'static str,
    title: &'static str,
    /// Commands this topic explains: `/help <command>` shows it.
    commands: &'static [&'static str],
    who: Who,
    body: &'static str,
    /// The same in Chinese (`/help_zh`).
    title_zh: &'static str,
    body_zh: &'static str,
}

/// The language of the help: `/help` English, `/help_zh` Chinese (Chinese
/// grammar, technical terms as they are).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Zh,
}

const INTRO: &str = r#"I answer with AI models. Each model has a prefix: /models lists the prefixes of this bot. The examples here use `c` and `g` as prefixes; use the ones /models shows.

**Quick start**
1. Send a prefix, then `$`, then your message: `c$Hello!`
2. Reply to my answer to continue the conversation.
3. /models lists every prefix and its model.

| Command | What it does |
|---|---|
| `c$text` | start a conversation with the model of prefix `c` |
| reply to an answer | continue the conversation |
| /models | every prefix; `/models word` searches |
| `/model c` | the model of a prefix, and its details |
| reply `/m g$text` | continue with another model |
| reply /raw | the answer's original Markdown and files |
| reply /full | the answer's complete reasoning and tool calls |
| reply /dump | the conversation as a JSON file |
| reply /info | the request's details |
| reply /retry | try a failed answer again |
| reply /continue | keep writing a long answer that stopped at its length budget |
| /prefix | this chat's own prefixes (custom chatbots) |
| /stats | usage and costs in this chat |
| /usage | spend limits in this chat |
| /ping | ids, and whether I work here |
| /help | this help; `/help options` one topic |

Tap a topic for everything about it:"#;

const OUTRO: &str = r#"`/help options` (or `/help_options`) shows one topic by itself, `/help stats` the topic of a command. 中文帮助：/help_zh"#;

const INTRO_ZH: &str = r#"我用各家的 AI 模型回答问题。每个模型对应一个前缀：/models 列出这个 bot 的所有前缀。下面的例子用 `c` 和 `g` 作前缀，实际请用 /models 里的。

**快速上手**
1. 发送前缀、`$` 和你的消息：`c$你好！`
2. 回复我的回答，就能接着聊。
3. /models 列出所有前缀和它们对应的模型。

| 命令 | 作用 |
|---|---|
| `c$文字` | 用前缀 `c` 的模型开始对话 |
| 回复一条回答 | 接着这段对话聊 |
| /models | 所有前缀；`/models 关键词` 搜索 |
| `/model c` | 前缀对应的模型及其详情 |
| 回复 `/m g$文字` | 换一个模型接着聊 |
| 回复 /raw | 回答的原始 Markdown 和文件 |
| 回复 /full | 回答完整的推理和工具调用 |
| 回复 /dump | 整段对话的 JSON 文件 |
| 回复 /info | 请求详情 |
| 回复 /retry | 重新生成失败的回答 |
| 回复 /continue | 让写到长度预算停下的长回答接着写 |
| /prefix | 本聊天自己的前缀（自定义 chatbot） |
| /stats | 本聊天的用量和费用 |
| /usage | 本聊天的限额 |
| /ping | 各种 id，以及我在这里能不能用 |
| /help_zh | 这份帮助；`/help_zh options` 单看一个主题 |

点开下面的主题查看详细说明："#;

const OUTRO_ZH: &str = r#"`/help_zh options`（或 `/help_zh_options`）单独显示一个主题，`/help_zh stats` 显示某个命令所在的主题。English: /help"#;

const TOPICS: &[Topic] = &[
    Topic {
        key: "basics",
        title: "💬 Start and continue a conversation",
        commands: &["m"],
        who: Who::Everyone,
        body: r#"**Start**: a prefix, then `$`, then your message, all in one message:
- `c$What is the capital of France?`
- `$Hello` uses the empty prefix, if this bot has one.
- Only the first line needs the prefix; the message can be as long as you like.
- Everything after `$` is sent exactly as you wrote it, a space right after `$` included.

**Several models at once**: separate the prefixes with commas, and each model answers on its own: `c,g$Compare tabs and spaces`.

**Continue**: reply to any of my messages (also a part of a long answer, or a generated image). Your whole reply is the next message, to the same model with the same options.
- If I am still answering, your reply waits until I am done.
- Start with `\` to send text that begins with `/`: `\/raw` sends the text `/raw`.
- Replies that quote part of my message are ignored.

**Continue with another model**: reply `/m prefix$text` to an answer, e.g. `/m g$What do you think?`. Several prefixes work too: `/m c,g$Which is right?`.

**When an answer fails** it cannot be continued: tap 🔁 Retry under it, or reply /retry to it.

::: When a message starts a conversation
- The prefix ends at the first `$` of the first line.
- In a group I ignore prefixes I do not know (they may belong to another bot, or be plain text), so normal chatting never starts me.
- In a private chat a first line with `$` always counts, and an unknown prefix is an error.
- A reply starts a conversation only to bring the photo, file, album or location it replies to; replies to other messages are ignored, prefix or not (replies to my answers continue them).
- With several prefixes, an empty one between two commas asks the empty prefix too: `c,,g$`.
- In a group with several of these bots, each answers the prefixes it has.
:::"#,
        title_zh: "💬 开始和继续对话",
        body_zh: r#"**开始**：前缀、`$`、你的消息，写在同一条消息里：
- `c$法国的首都是哪里？`
- `$你好` 用的是名字为空的前缀（如果这个 bot 有的话）。
- 只有第一行需要前缀，消息本身多长都可以。
- `$` 之后的内容原样发送，紧跟在 `$` 后面的空格也会发出去。

**同时问几个模型**：用逗号隔开几个前缀，每个模型各自回答：`c,g$比较一下 tab 和空格`。

**继续对话**：回复我的任何一条消息（长回答的任意一段、生成的图片也可以）。你回复的全部内容就是下一条消息，发给同一个模型，用同样的选项。
- 如果我还在回答，你的回复会等我答完再处理。
- 想发送以 `/` 开头的文字，就在最前面加 `\`：`\/raw` 发出的是文字 `/raw`。
- 引用（quote）了我某段话的回复会被忽略。

**换个模型接着聊**：对一条回答回复 `/m 前缀$文字`，例如 `/m g$你怎么看？`。也可以写几个前缀：`/m c,g$哪个对？`。

**回答失败时**不能接着聊：点它下面的 🔁 Retry，或者对它回复 /retry。

::: 什么样的消息会开始对话
- 前缀到第一行的第一个 `$` 为止。
- 群里我会忽略不认识的前缀（可能是别的 bot 的，也可能只是普通文字），所以正常聊天不会触发我。
- 私聊里第一行只要有 `$` 就算前缀，不认识的前缀会报错。
- 回复别的消息时，只有为了带上它的图片、文件、相册或位置才会开始对话；回复其他消息会被忽略，带前缀也一样（回复我的回答是接着聊）。
- 写几个前缀时，两个逗号之间留空表示也问名字为空的前缀：`c,,g$`。
- 群里有好几个这样的 bot 时，每个 bot 只回答它有的前缀。
:::"#,
    },
    Topic {
        key: "models",
        title: "🧩 Models and prefixes",
        commands: &["models", "model"],
        who: Who::Everyone,
        body: r#"- /models — every prefix of this bot and the model it calls.
- `/models word` — search prefixes and models.
- `/model c` — the model a prefix calls and which API serves it; for OpenRouter models also its inputs, reasoning levels, longest output, prices and endpoints.

**Prefixes**: a prefix is a short name for a model with its options.{DIRECT_IDS}

**Which model answers**: the first line of every answer names the platform and the model: `provider:model` for a platform's own API, `openrouter:author/model` through OpenRouter.

- Platforms this bot has no key for are off: their models are not listed, and naming them is an error.
- Only models whose price is known can be used."#,
        title_zh: "🧩 模型和前缀",
        body_zh: r#"- /models — 这个 bot 的所有前缀，以及它们调用的模型。
- `/models 关键词` — 搜索前缀和模型。
- `/model c` — 前缀调用的是哪个模型、由哪个 API 提供；OpenRouter 的模型还会列出输入类型、推理档位、最长输出、价格和 endpoint。

**前缀**：前缀是"模型 + 选项"的简称。{DIRECT_IDS}

**是哪个模型在回答**：每条回答的第一行写着平台和模型：平台自己的 API 是 `provider:model`，经 OpenRouter 的是 `openrouter:author/model`。

- 这个 bot 没有配置 key 的平台不可用：它们的模型不会列出来，指定它们会报错。
- 只能用价格已知的模型。"#,
    },
    Topic {
        key: "options",
        title: "⚙️ Options: search, reasoning, sizes…",
        commands: &[],
        who: Who::Everyone,
        body: r#"Options go between the model and `$`, each after a `+`: `c+s$What is new in Rust?`. Letters can be written together, `c+sh$` is web search at the highest reasoning level; options with `=` each need their own `+`: `g+size=4K+ar=16:9$`.

| Option | What it does |
|---|---|
| `s` | web search (the platform's own tool) |
| `c` | code execution (the platform's own tool) |
| `u` | read the web pages your message links to |
| `m` | maps; your latest Telegram location is your position |
| `i` | draw images in a chat |
| `h` | the model's highest reasoning level |
| `l` | its lowest: reasoning off where it can be turned off |
| `e=high` | reasoning effort, one of the model's levels; `e=none` turns reasoning off |
| `budget=8000` | reasoning token budget (models that use one) |
| `v=low` | answer length: `low`, `medium` or `high` |
| `size=4K` | image size `1K`, `2K` or `4K` (videos: 720p, 1080p, 4K) |
| `ar=16:9` | aspect ratio, any the model supports |
| `q=high` | image or video quality, one of the model's levels |
| `n=4` | how many images |
| `bg=transparent` | image background: `auto`, `opaque` or `transparent` |
| `sec=8` | video length in seconds |
| `audio=off` | video sound `on` or `off` (models with an audio switch) |
| `rewrite=on` | prompt rewriting by the platform `on` or `off` (models where it is a paid extra) |
| `p=…` | another official provider of the model (through OpenRouter only) |
| `tier=flex` | service tier: `flex` is cheaper; faster tiers only for priority users |

- Which options a model has depends on it and its platform. Every option is checked before anything is sent: one the model does not have, or a value it does not have, is an error that says what it has. Anything not in this table is an error.
- Without options a request reasons at the level the platform recommends for the model; when that is not the highest, the answer says so, and `h` gives the highest. Generated images and videos come at the platform's default quality and size where more costs more, and the answer names the options for the most (`size=4K`, `q=max`). Everything else is the most the model allows: the longest output, the sharpest view of your images, videos and documents. Earlier answers' reasoning stays in the conversation where the platform allows it. I never add a system prompt.

::: More about p= and tier=
- `p=` names another official provider of the model on OpenRouter, by OpenRouter's name for it; several, separated by a vertical bar, mean any of them. It is an error on a platform's own API.
- `tier=flex` is cheaper and slower where the platform has it. `tier=priority` and fast modes cost more and are only for priority users. `tier=default` changes nothing.
:::"#,
        title_zh: "⚙️ 选项：搜索、推理、尺寸……",
        body_zh: r#"选项写在模型和 `$` 之间，每个前面加 `+`：`c+s$Rust 最近有什么新消息？`。字母可以连写，`c+sh$` 就是网页搜索加最高推理强度；带 `=` 的选项各自需要一个 `+`：`g+size=4K+ar=16:9$`。

| 选项 | 作用 |
|---|---|
| `s` | 网页搜索（平台自己的工具） |
| `c` | 代码执行（平台自己的工具） |
| `u` | 读取你的消息里链接的网页 |
| `m` | 地图；你最近发的 Telegram 位置就是你所在的位置 |
| `i` | 在对话里画图 |
| `h` | 该模型最高的推理强度 |
| `l` | 最低的推理强度：能关闭推理的就关闭 |
| `e=high` | 推理强度，取该模型的档位之一；`e=none` 关闭推理 |
| `budget=8000` | 推理 token 预算（使用预算的模型） |
| `v=low` | 回答的详细程度：`low`、`medium` 或 `high` |
| `size=4K` | 图片尺寸 `1K`、`2K` 或 `4K`（视频：720p、1080p、4K） |
| `ar=16:9` | 宽高比，模型支持的任意一种 |
| `q=high` | 图片或视频的质量，取该模型的档位之一 |
| `n=4` | 生成几张图 |
| `bg=transparent` | 图片背景：`auto`、`opaque` 或 `transparent` |
| `sec=8` | 视频长度（秒） |
| `audio=off` | 视频声音 `on` 或 `off`（有声音开关的模型） |
| `rewrite=on` | 平台改写提示词 `on` 或 `off`（改写要另收费的模型） |
| `p=…` | 换成该模型的另一个官方提供方（仅限经 OpenRouter） |
| `tier=flex` | 服务等级：`flex` 更便宜；更快的等级只给优先用户 |

- 一个模型有哪些选项，取决于模型本身和它的平台。所有选项在发出前都会检查：模型没有的选项或取值会报错，并告诉你它有哪些。表里没有的选项都会报错。
- 不加选项时，推理强度用平台给这个模型推荐的档位；不是最高档时回答里会注明，加 `h` 就是最高档。生成的图片和视频在更高质量要多花钱时用平台默认的质量和尺寸，回答里会写出拉到最高的选项（`size=4K`、`q=max`）。其他都用模型允许的最大值：最长的输出、看你的图片、视频和文档时的最高精度。平台允许的话，之前回答的推理会留在对话里。我从不额外加 system prompt。

::: 关于 p= 和 tier=
- `p=` 指定该模型在 OpenRouter 上的另一个官方提供方，用 OpenRouter 对它的叫法；用竖线隔开几个，表示其中任意一个都可以。在平台自己的 API 上会报错。
- `tier=flex` 在平台提供时更便宜、也更慢。`tier=priority` 和 fast 模式更贵，只给优先用户。`tier=default` 不改变任何东西。
:::"#,
    },
    Topic {
        key: "files",
        title: "📎 Photos, files and other attachments",
        commands: &[],
        who: Who::Everyone,
        body: r#"**Send something with your message**:
- Write the prefix in the caption of a photo, a file or an album: caption `c$What is in this picture?`
- Or reply with `c$…` to a photo, file, album or location (only that message's media is sent, not its text). A reply to any other message (someone's text, another bot's answer) is ignored, even with a prefix.
- To add it to a conversation, reply to my answer with the photo, file or album.

**What works**: photos and albums (one message, all images in order), files, voice messages, videos and round videos, GIFs, static and video stickers, rich messages (sent as Markdown), and polls, contacts, dice and checklists (sent as text).
- A location is your position for a model's maps tool (`+m`).
- `@` and a YouTube link in your message (`@https://youtu.be/…`) give the video to models that take YouTube links.
- Files can be up to {MAX_FILE_MB} MB.
- What a model cannot read (a PDF for some, audio, an animated sticker) is an error: nothing is converted or left out quietly."#,
        title_zh: "📎 图片、文件和其他附件",
        body_zh: r#"**随消息发送附件**：
- 把前缀写在图片、文件或相册的说明（caption）里：说明写 `c$这张图里有什么？`
- 或者用 `c$…` 回复一条图片、文件、相册或位置消息（只发送那条消息里的媒体，不发送它的文字）。回复其他消息（别人的文字、其他 bot 的回答）会被忽略，带前缀也一样。
- 想在对话中加入附件，就带着图片、文件或相册回复我的回答。

**支持的内容**：图片和相册（算作一条消息，所有图片按顺序发送）、文件、语音、视频和圆形视频消息、GIF、静态和视频贴纸、富消息（按 Markdown 发送），以及投票、联系人、骰子和待办清单（按文字发送）。
- 位置就是模型地图工具（`+m`）里你所在的位置。
- 在消息里写 `@` 加 YouTube 链接（`@https://youtu.be/…`），能看 YouTube 链接的模型会收到这个视频。
- 文件最大 {MAX_FILE_MB} MB。
- 模型读不了的内容（有的模型读不了 PDF、音频，所有模型都读不了动画贴纸）会报错：不会偷偷转换，也不会悄悄丢掉。"#,
    },
    Topic {
        key: "images",
        title: "🎨 Images and videos",
        commands: &[],
        who: Who::Everyone,
        body: r#"**Draw in a chat**: some chat models draw while they answer, others draw with `+i`. Reply to keep changing the picture: the model sees the whole conversation.

**Image and video models** make images or a video from one prompt.{IMAGE_MODELS}
- To edit, attach images, or reply to an image or video they made, saying what to change.
- They take one prompt, not a conversation: replying to an answer they cannot edit is an error.

**Options**: size `size=`, aspect ratio `ar=`, quality `q=`, how many `n=`, background `bg=`, video length `sec=`, sound `audio=`, prompt rewriting `rewrite=`. By default: the platform's default quality, size and resolution where more costs more (the answer names the options for the most), the highest where it costs the same, and no watermark. Values a model does not have are errors that list the valid ones.

- A preview shows while an image is drawn. Videos are made in the background and keep going if the bot restarts.
- Reply /raw to an answer for the original files."#,
        title_zh: "🎨 生成图片和视频",
        body_zh: r#"**在对话里画图**：有的聊天模型回答时直接画图，有的要加 `+i` 才画。回复它就能接着改图：模型能看到整段对话。

**图片和视频模型**根据一条提示词生成图片或一段视频。{IMAGE_MODELS}
- 想编辑，就附上图片，或者回复它生成的图片或视频，写明要改什么。
- 它们只接收一条提示词，不接收对话：回复一条它们没法编辑的回答会报错。

**选项**：尺寸 `size=`，宽高比 `ar=`，质量 `q=`，张数 `n=`，背景 `bg=`，视频长度 `sec=`，声音 `audio=`，提示词改写 `rewrite=`。默认：更高质量要多花钱时用平台默认的质量、尺寸和分辨率（回答里会写出拉到最高的选项），价格一样时用最高的，不加水印。模型没有的取值会报错，并列出可选值。

- 画图过程中会先显示预览。视频在后台生成，bot 重启后也会接着生成。
- 对回答回复 /raw，可以拿到原始文件。"#,
    },
    Topic {
        key: "prompts",
        title: "📜 System prompt and history",
        commands: &[],
        who: Who::Everyone,
        body: r#"When you start a conversation, lines that are only `::system`, `::user` or `::assistant` split your message into turns:
```
c$::system
You are a pirate.
::user
Hello
::assistant
Arr!
::user
Who are you?
```
- Short forms: `::sys`, `::u`, `::a` (or `::model`).
- Text before the first such line is a user turn.
- Ending with an `::assistant` turn asks the model to continue that text (if it allows that).
- Photos and files go with the last user turn.
- To send a line that is just `::user`, write `\::user`.
- These lines only count when you start a conversation; in replies they are plain text."#,
        title_zh: "📜 System prompt 和历史",
        body_zh: r#"开始对话时，只写着 `::system`、`::user` 或 `::assistant` 的行会把你的消息分成多轮：
```
c$::system
你是一个海盗。
::user
你好
::assistant
Arr!
::user
你是谁？
```
- 简写：`::sys`、`::u`、`::a`（或 `::model`）。
- 第一个这样的行之前的文字算作一轮 user 消息。
- 以一轮 `::assistant` 结尾，就是让模型接着这段文字往下写（前提是模型允许）。
- 图片和文件附在最后一轮 user 消息上。
- 想发送内容恰好是 `::user` 的一行，写成 `\::user`。
- 这些行只在开始对话时起作用；在回复里它们就是普通文字。"#,
    },
    Topic {
        key: "prefixes",
        title: "🏷️ This chat's own prefixes",
        commands: &["prefix", "unprefix"],
        who: Who::Everyone,
        body: r#"Anyone here can save a model, its options, a system prompt and a short history as a prefix of this chat, a custom chatbot of sorts:
```
/prefix pirate c+s$::system
You are a pirate.
::user
Hello
::assistant
Arr!
```
Then `pirate$Who are you?` sends that system prompt and history, then your message, to `c+s`.
- Or reply `/prefix pirate` to one of my answers: that conversation, with its model and options, becomes the prefix.
- Names: 3 to 16 letters or digits, not taken yet. Only this chat has the prefix.
- The history ends with an `::assistant` turn, or there is only a system prompt.
- /prefix lists this chat's prefixes; `/prefix pirate` shows one.
- `/unprefix pirate` deletes one (its creator or an admin). Answers made with it can still be continued.
- When you use it you can add options (`pirate+e=low$…`) and `::user` / `::assistant` lines, but not `::system`.
- A prefix starts a new conversation, so it does not work with /m."#,
        title_zh: "🏷️ 本聊天自己的前缀",
        body_zh: r#"这里的任何人都可以把"模型、选项、system prompt 和一小段历史"存成本聊天的前缀，相当于一个自定义 chatbot：
```
/prefix pirate c+s$::system
你是一个海盗。
::user
你好
::assistant
Arr!
```
之后发 `pirate$你是谁？`，就会把这段 system prompt 和历史，再加上你的消息，发给 `c+s`。
- 也可以对我的某条回答回复 `/prefix pirate`：那段对话连同它的模型和选项就成了前缀。
- 名字：3 到 16 个字母或数字，不能和已有的重名。前缀只属于这个聊天。
- 历史要以一轮 `::assistant` 结尾，或者只有 system prompt。
- /prefix 列出本聊天的前缀；`/prefix pirate` 查看其中一个。
- `/unprefix pirate` 删除一个（创建者或管理员）。用它生成的回答仍然可以接着聊。
- 使用时可以加选项（`pirate+e=low$…`），也可以加 `::user` / `::assistant` 行，但不能加 `::system`。
- 前缀会开始一段新对话，所以不能用在 /m 里。"#,
    },
    Topic {
        key: "answers",
        title: "📖 Reading an answer, and commands for answers",
        commands: &["raw", "full", "dump", "info", "retry", "continue"],
        who: Who::Everyone,
        body: r#"- **First line**: the platform and the model. While the answer is being written its icon moves on with every update; 🤖 when it is done. An answer in several messages says which part each one is: `part 1/3` once it is done (`part 1` while it is written).
- **💭 Reasoning**: the model's reasoning. While it thinks, the title also says what it is thinking about.
- **🔧** tool calls (searches, code, pages read) with readable results. Several in a row are grouped: `💭 2 · 🔧 1`.
- **📚 Sources**: the links the answer cites.
- **ℹ️** a note from me, e.g. that a search was asked for but none ran.
- **Small text at the end**: settings, tokens, cost and time, then how the platform actually served the request.{BOLD_COST}
- **⚠️** errors, with the full original error. I retry on my own when that can help; earlier attempts are folded above.

Everything I add is a fold or small text, so you can always tell which part is the model's answer.{ORDER} There is no stop button: once started, an answer is written to the end.

Long reasoning and tool calls are shortened to their latest part, with a note at the start of the fold: while the answer is being written (so that Telegram's "Show More" hides nothing), and in a finished answer that only they would make longer than one message. /full shows all of them.

**Commands for an answer** (reply with them to it):
- /raw — its original Markdown, and the original files
- /full — its complete reasoning and tool calls, folded as in the answer, in as many messages as they need
- /dump — the whole conversation up to it, as a JSON file
- /info — request details: attempts, timings, rate limits, and the full requests as JSON
- /retry — answer a failed message again (like the 🔁 Retry button)
- /continue — keep writing an answer that stopped at its output budget (models that write long answers in parts)
- `/m prefix$text` — continue with another model

::: The small text at the end, item by item
- What was asked for: `effort max · web search` and the like.
- `in 1,234 (cached 1,000)` — input tokens, of which 1,000 were read from the cache (billed at the cache price); `cache write` — written to it.
- `out 567 (reasoning 400)` — output tokens, and how many were reasoning.
- The cost in USD. `≈` means my estimate from the platform's prices (the platform reported none); `+ code execution` and the like name a charge the estimate leaves out. `+ 24 searches ≈{USD}0.336` is a charge a monthly free allowance may cover, so it is not in the cost or the spend limits; the amount is what it costs once the allowance is used up.
- How long it took.
- `attempt 2/4` — I retried; `continued` — written by /continue.
- `via …` — which provider served it on OpenRouter.
- `served …` — another model answered than the one asked for.
- `tier priority→default` — the tier asked for was not given; `tier flex` — the tier it ran on.
- `geo …` — where it ran, when the platform reports it.
- `effort …` — the effort the platform used when none was asked for.
- `long decode: 3 hops` — a long answer written in several parts.
- `finish: …` — why it stopped, when that was not the normal end.
:::"#,
        title_zh: "📖 怎么看回答，以及针对回答的命令",
        body_zh: r#"- **第一行**：平台和模型。回答还在写时，这里的图标随每次更新变化；写完是 🤖。分成几条消息的回答，每条都写它是第几部分：写完后是 `part 1/3`（还在写时是 `part 1`）。
- **💭 Reasoning**：模型的推理。模型思考时，标题后面还会写它此刻在想什么。
- **🔧** 工具调用（搜索、代码、读取的网页），结果是可读的形式。连续的几段会合成一组：`💭 2 · 🔧 1`。
- **📚 Sources**：回答引用的链接。
- **ℹ️** 我加的提示，例如要求了搜索、但这次没有搜索。
- **末尾的小字**：设置、token、费用和耗时，然后是平台实际怎么处理这个请求的。{BOLD_COST}
- **⚠️** 错误，附带完整的原始错误。可能有用的时候我会自动重试；之前的尝试折叠在上面。

我加的内容都在折叠块或小字里，所以总能分清哪部分是模型的回答。{ORDER}没有停止按钮：回答一旦开始，就会写完。

很长的推理和工具调用会只显示最新的一部分，折叠块开头有说明：回答还在写的时候（这样 Telegram 的"Show More"不会遮住任何东西），以及写完的回答仅仅因为它们而超过一条消息的时候。/full 显示全部。

**针对回答的命令**（回复这条回答时使用）：
- /raw — 它的原始 Markdown 和原始文件
- /full — 它完整的推理和工具调用，像回答里一样折叠，需要几条消息就发几条
- /dump — 到它为止的整段对话，JSON 文件
- /info — 请求详情：每次尝试、耗时、限流余量，以及完整请求的 JSON
- /retry — 重新生成失败的回答（和 🔁 Retry 按钮一样）
- /continue — 让达到输出预算而停下的回答接着写（分段写长回答的模型）
- `/m 前缀$文字` — 换一个模型接着聊

::: 末尾小字的各项含义
- 请求了什么：`effort max · web search` 之类。
- `in 1,234 (cached 1,000)` — 输入 token，其中 1,000 是从缓存读的（按缓存价计费）；`cache write` — 写入缓存的。
- `out 567 (reasoning 400)` — 输出 token，其中多少是推理。
- 费用（USD）。`≈` 表示是我按平台价格估算的（平台没有给出费用）；`+ code execution` 之类表示估算里没算进去的收费。`+ 24 searches ≈{USD}0.336` 是可能被每月免费额度覆盖的收费，所以不算进费用和限额；金额是免费额度用完后的价钱。
- 用了多长时间。
- `attempt 2/4` — 我重试过；`continued` — 是 /continue 写的。
- `via …` — 在 OpenRouter 上由哪个提供方处理。
- `served …` — 实际回答的模型和请求的不同。
- `tier priority→default` — 没有给到请求的等级；`tier flex` — 实际运行的等级。
- `geo …` — 在哪里运行（平台报告时）。
- `effort …` — 没有指定推理强度时，平台实际用的强度。
- `long decode: 3 hops` — 长回答分几段写成。
- `finish: …` — 不是正常结束时，停下的原因。
:::"#,
    },
    Topic {
        key: "usage",
        title: "📊 Costs, statistics and spend limits",
        commands: &["stats", "usage"],
        who: Who::Everyone,
        body: r#"- /stats — this bot's usage in this chat over the last 30 days: API calls (each retry counts), tokens and cost by model (in a group also by person).
- `/stats today`, `/stats 7d`, `/stats 30d`, `/stats all` — other periods, in calendar days of the bot's time zone.
- In a group, `/stats user_id`, or /stats as a reply to someone's message: one member.
- /usage — this chat's spend limits, and yours here.

**Spend limits**: an admin can give this chat, each member or one person a limit in USD that refills every day. When one runs out, new requests are refused until it refills; the message says which limit and when.

**Costs**: every answer shows its cost at the end; `≈` means I estimated it from the platform's prices.

**Faster tiers** (`tier=priority`, fast modes) cost more and are only for priority users."#,
        title_zh: "📊 费用、统计和限额",
        body_zh: r#"- /stats — 最近 30 天这个 bot 在本聊天的用量：按模型统计的 API 调用次数（每次重试都算）、token 和费用（群里还按人统计）。
- `/stats today`、`/stats 7d`、`/stats 30d`、`/stats all` — 其他时间段，按 bot 所在时区的自然日计算。
- 群里：`/stats user_id`，或者回复某人的消息发 /stats，只看这个成员。
- /usage — 本聊天的限额，以及你在这里的限额。

**限额**：管理员可以给这个聊天、每个成员或者某个人设一个每天恢复的额度（USD）。用完后，新请求会被拒绝，直到额度恢复；提示里会写明是哪个限额、多久后恢复。

**费用**：每条回答的末尾都写着费用；`≈` 表示是我按平台价格估算的。

**更快的等级**（`tier=priority`、fast 模式）更贵，只给优先用户。"#,
    },
    Topic {
        key: "access",
        title: "🔐 Where I work",
        commands: &["ping", "help", "start"],
        who: Who::Everyone,
        body: r#"- I only answer in chats an admin has allowed. Anywhere else I only answer /ping (and an admin's /allow).
- /ping — this chat's id, your user id, whether this chat is allowed and whether you are banned here. Send it to an admin to get access.
- Someone banned in a chat can still use /help, /stats and /usage there, but cannot start requests.
- I ignore messages from other bots and from anonymous group admins.
- In a group with several of these bots, a command without `@` goes to each of them; `/stats@name` to one."#,
        title_zh: "🔐 我在哪里能用",
        body_zh: r#"- 我只在管理员允许的聊天里回答。在其他地方只回答 /ping（以及管理员的 /allow）。
- /ping — 本聊天的 id、你的 user id、本聊天是否被允许、你在这里是否被封禁。把它发给管理员，就能申请使用。
- 在某个聊天被封禁的人，在那里仍然可以用 /help、/stats 和 /usage，但不能发起请求。
- 我会忽略其他 bot 的消息，以及群里匿名管理员的消息。
- 群里有好几个这样的 bot 时，不带 `@` 的命令每个 bot 都会处理；`/stats@name` 只发给其中一个。"#,
    },
    Topic {
        key: "admin",
        title: "🛠️ Admin commands in this group",
        commands: &["allow", "deny", "ban", "unban", "acl", "quota"],
        who: Who::AdminInGroup,
        body: r#"In a group, admin commands act on this group only:
- /allow — allow this group (also before it is allowed); /deny — remove it.
- `/ban user_id`, or /ban as a reply to their message — ban someone here; /unban the same way.
- /acl — whether this group is allowed, and who is banned here.
- /quota — this group's spend limits and what is left of them.
- `/quota chat 10 2` — the whole group: 10 USD, refilling 2 USD a day; `/quota chat off` removes it.
- `/quota members 1 0.5` — a limit for each member; `/quota members off`.
- `/quota user_id 2 1`, or `/quota 2 1` as a reply to their message — one person; `off` removes it.
- `/unprefix name` — delete anyone's prefix here.

Limits apply to admins too. In a group with several of these bots, `/deny` without `@` removes the group for each of them; `/deny@name` for one. Everything else (other chats, the whole bot's statistics, its prefixes, balances) is in your private chat with me, where /help lists it."#,
        title_zh: "🛠️ 本群的管理命令",
        body_zh: r#"群里的管理命令只作用于本群：
- /allow — 允许本群（本群还没被允许时也能用）；/deny — 撤销。
- `/ban user_id`，或者回复某人的消息发 /ban — 在本群封禁他；/unban 的用法相同。
- /acl — 本群是否被允许，以及谁在本群被封禁。
- /quota — 本群的限额和剩余额度。
- `/quota chat 10 2` — 整个群：10 USD，每天恢复 2 USD；`/quota chat off` 删除。
- `/quota members 1 0.5` — 每个成员各自的限额；`/quota members off` 删除。
- `/quota user_id 2 1`，或者回复某人的消息发 `/quota 2 1` — 单独设置一个人；`off` 删除。
- `/unprefix name` — 删除本群任何人的前缀。

管理员也受限额限制。群里有好几个这样的 bot 时，不带 `@` 的 /deny 会让每个 bot 都撤销本群；`/deny@name` 只对其中一个。其他命令（别的聊天、整个 bot 的统计、bot 的前缀、余额）在你和我的私聊里，在那里发 /help_zh 可以看到。"#,
    },
    Topic {
        key: "admin",
        title: "🛠️ Admin commands",
        commands: &["allow", "deny", "ban", "unban", "acl", "quota", "alias", "unalias", "credits"],
        who: Who::AdminInPrivate,
        body: r#"In private chat, admin commands act on every chat of this bot. A group's id is negative; a private chat's id is the person's user id (/ping shows both).

**Access**
- `/allow chat_id` — allow a chat; `/deny chat_id` — remove it.
- `/ban user_id group_id` — ban someone in a group; `/unban user_id group_id` — lift it.
- /acl — every allowed chat and every ban.

**Spend limits** (capacity and daily refill in USD; they apply to admins too)
- /quota — every limit of this bot; `/quota chat_id` — one chat's.
- `/quota chat_id chat 10 2` — the whole chat.
- `/quota chat_id members 1 0.5` — each member of the chat.
- `/quota chat_id user_id 2 1` — one person in the chat.
- `off` instead of the two numbers removes a limit: `/quota chat_id members off`.

**Statistics**
- /stats — the whole bot: totals, and the top 10 by model, chat, person, and person in a chat. Periods as usual: `/stats 7d`.
- `/stats model` (or `chat`, `user`, `chat-user`) — one of these rankings, top 50.
- `/stats chat_id` — one chat; `/stats chat_id user_id` — one person in it.

**Prefixes**
- `/alias name$ spec` — add a prefix to this bot, no restart needed. The spec is the id of a model this bot has, with options, as in a message (`provider:model+e=low`); a model it does not have is refused. `/alias $ spec` sets the empty prefix.
- `/unalias name$` — remove a prefix added with /alias (a configured prefix of that name comes back).
- `/prefix chat_id` — a chat's own prefixes; `/unprefix chat_id name` — delete one.

**Balances**
- /credits — the OpenRouter key's limit and spending, and the balances of platforms that report one."#,
        title_zh: "🛠️ 管理命令",
        body_zh: r#"私聊里的管理命令作用于这个 bot 的所有聊天。群的 id 是负数；私聊的 id 就是对方的 user id（/ping 都会显示）。

**权限**
- `/allow chat_id` — 允许一个聊天；`/deny chat_id` — 撤销。
- `/ban user_id group_id` — 在某个群封禁某人；`/unban user_id group_id` — 解封。
- /acl — 所有被允许的聊天和所有封禁。

**限额**（容量和每日恢复量，单位 USD；管理员也受限）
- /quota — 这个 bot 的所有限额；`/quota chat_id` — 某个聊天的。
- `/quota chat_id chat 10 2` — 整个聊天。
- `/quota chat_id members 1 0.5` — 聊天里的每个成员。
- `/quota chat_id user_id 2 1` — 聊天里的某个人。
- 把两个数字换成 `off` 就是删除：`/quota chat_id members off`。

**统计**
- /stats — 整个 bot：总额，以及按模型、聊天、人、"聊天里的人"的前 10 名。时间段的写法和平时一样：`/stats 7d`。
- `/stats model`（或 `chat`、`user`、`chat-user`）— 其中一个排名的前 50 名。
- `/stats chat_id` — 某个聊天；`/stats chat_id user_id` — 那个聊天里的某个人。

**前缀**
- `/alias name$ spec` — 给这个 bot 加一个前缀，不用重启。spec 是这个 bot 有的某个模型的 ID 加选项，写法和消息里一样（`provider:model+e=low`）；bot 没有的模型会被拒绝。`/alias $ spec` 设置名字为空的前缀。
- `/unalias name$` — 删除用 /alias 加的前缀（配置里同名的前缀会恢复）。
- `/prefix chat_id` — 某个聊天自己的前缀；`/unprefix chat_id name` — 删除其中一个。

**余额**
- /credits — OpenRouter key 的额度和花费，以及能报告余额的平台的余额。"#,
    },
];

fn visible(ctx: &Ctx) -> impl Iterator<Item = &'static Topic> + '_ {
    TOPICS.iter().filter(move |t| match t.who {
        Who::Everyone => true,
        Who::AdminInGroup => ctx.admin && ctx.group,
        Who::AdminInPrivate => ctx.admin && !ctx.group,
    })
}

/// A topic's text for this deployment: the marker, then the values.
fn fill(text: &str, ctx: &Ctx) -> String {
    let m = &ctx.marker;
    if ctx.lang == Lang::Zh {
        return fill_zh(text, ctx);
    }
    let direct = match (ctx.ids_platforms, ctx.ids_openrouter) {
        (true, true) => format!(
            " You can also type a model id instead of a prefix: `provider:model{m}` for a platform's own API, \
             `author/model{m}` through OpenRouter."
        ),
        (true, false) => format!(" You can also type a model id instead of a prefix: `provider:model{m}`."),
        (false, true) => format!(" You can also type an OpenRouter model id instead of a prefix: `author/model{m}`."),
        (false, false) => String::new(),
    };
    let media_how = if ctx.ids_platforms {
        format!(" Use them through a prefix from /models, or type the model's id: `provider:model{m}a watercolor fox`.")
    } else {
        " Use them through a prefix from /models.".to_string()
    };
    let bold = if ctx.bold_cost_usd > 0.0 {
        format!(" `≈` means the cost is my estimate; a cost above {} USD is bold.", ctx.bold_cost_usd)
    } else {
        " `≈` means the cost is my estimate.".to_string()
    };
    let order = if ctx.answer_first {
        " When the answer is one piece, it comes first, then the small text, then its reasoning and tool calls; when \
         answer and reasoning alternate, everything is in the order it happened."
    } else {
        " Everything is in the order it happened."
    };
    text.replace('$', m)
        .replace("{DIRECT_IDS}", &direct)
        .replace("{IMAGE_MODELS}", &media_how)
        .replace("{MAX_FILE_MB}", &ctx.max_file_mb.to_string())
        .replace("{BOLD_COST}", &bold)
        .replace("{ORDER}", order)
        // A dollar sign that is not the trigger marker (amounts in examples).
        .replace("{USD}", "$")
}

/// [`fill`] for the Chinese texts.
fn fill_zh(text: &str, ctx: &Ctx) -> String {
    let m = &ctx.marker;
    let direct = match (ctx.ids_platforms, ctx.ids_openrouter) {
        (true, true) => format!(
            "也可以直接写模型 ID 代替前缀：平台自己的 API 写 `provider:model{m}`，经 OpenRouter 写 `author/model{m}`。"
        ),
        (true, false) => format!("也可以直接写模型 ID 代替前缀：`provider:model{m}`。"),
        (false, true) => format!("也可以直接写 OpenRouter 的模型 ID 代替前缀：`author/model{m}`。"),
        (false, false) => String::new(),
    };
    let media_how = if ctx.ids_platforms {
        format!("用 /models 里指向它们的前缀，或者直接写模型 ID：`provider:model{m}一只水彩风格的狐狸`。")
    } else {
        "用 /models 里指向它们的前缀。".to_string()
    };
    let bold = if ctx.bold_cost_usd > 0.0 {
        format!("`≈` 表示费用是我估算的；超过 {} USD 的费用会加粗。", ctx.bold_cost_usd)
    } else {
        "`≈` 表示费用是我估算的。".to_string()
    };
    let order = if ctx.answer_first {
        "回答只有一段正文时，正文在最上面，然后是小字，推理和工具调用在最下面；正文和推理穿插时，一切按发生的顺序显示。"
    } else {
        "一切按发生的顺序显示。"
    };
    text.replace('$', m)
        .replace("{DIRECT_IDS}", &direct)
        .replace("{IMAGE_MODELS}", &media_how)
        .replace("{MAX_FILE_MB}", &ctx.max_file_mb.to_string())
        .replace("{BOLD_COST}", &bold)
        .replace("{ORDER}", order)
        // A dollar sign that is not the trigger marker (amounts in examples).
        .replace("{USD}", "$")
}

fn md(s: &str, depth: usize) -> Vec<RBlock> {
    omni_markdown::rich::markdown_to_blocks(s, depth, omni_markdown::rich::RichLimits::TELEGRAM)
}

/// A topic's text as blocks: Markdown, with `::: Title` … `:::` as folds
/// (outside code blocks).
fn blocks(text: &str, depth: usize) -> Vec<RBlock> {
    let mut out = Vec::new();
    let (mut plain, mut fold): (String, Option<(String, String)>) = (String::new(), None);
    let mut in_code = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        if !in_code {
            if let Some(title) = line.strip_prefix("::: ").filter(|_| fold.is_none()) {
                out.extend(md(&std::mem::take(&mut plain), depth));
                fold = Some((title.trim().to_string(), String::new()));
                continue;
            }
            if line.trim() == ":::"
                && let Some((title, body)) = fold.take()
            {
                out.push(RBlock::Details { open: false, title: RText::plain(title), blocks: md(&body, depth + 1) });
                continue;
            }
        }
        let buf = match &mut fold {
            Some((_, body)) => body,
            None => &mut plain,
        };
        buf.push_str(line);
        buf.push('\n');
    }
    if let Some((title, body)) = fold {
        out.push(RBlock::Details { open: false, title: RText::plain(title), blocks: md(&body, depth + 1) });
    }
    out.extend(md(&plain, depth));
    out
}

/// `/help`: the quick start and the commands, then every topic this person
/// can use as a fold.
pub fn overview(ctx: &Ctx) -> Vec<RBlock> {
    let zh = ctx.lang == Lang::Zh;
    let mut b = md(&fill(if zh { INTRO_ZH } else { INTRO }, ctx), 0);
    for t in visible(ctx) {
        let (title, body) = if zh { (t.title_zh, t.body_zh) } else { (t.title, t.body) };
        b.push(RBlock::Details {
            open: false,
            title: RText::bold(RText::plain(title)),
            blocks: blocks(&fill(body, ctx), 1),
        });
    }
    b.extend(md(&fill(if zh { OUTRO_ZH } else { OUTRO }, ctx), 0));
    b
}

/// `/help <topic or command>`: that topic's title and text, or `Err` with the
/// topics there are.
pub fn topic(ctx: &Ctx, name: &str) -> Result<(String, Vec<RBlock>), String> {
    let name = name.trim().trim_start_matches('/').to_ascii_lowercase();
    let zh = ctx.lang == Lang::Zh;
    match visible(ctx).find(|t| t.key == name || t.commands.contains(&name.as_str())) {
        Some(t) if zh => Ok((t.title_zh.to_string(), blocks(&fill(t.body_zh, ctx), 0))),
        Some(t) => Ok((t.title.to_string(), blocks(&fill(t.body, ctx), 0))),
        None => {
            let keys: Vec<String> = visible(ctx).map(|t| format!("`{}`", t.key)).collect();
            Err(if zh {
                format!("没有帮助主题 `{name}`。主题有：{}；也可以写命令名，例如 `/help_zh stats`。", keys.join("、"))
            } else {
                format!("No help topic `{name}`. Topics: {}; or a command's name, e.g. `/help stats`.", keys.join(", "))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(admin: bool, group: bool) -> Ctx {
        Ctx {
            marker: "%".into(),
            ids_openrouter: true,
            ids_platforms: true,
            max_file_mb: 50,
            bold_cost_usd: 0.2,
            answer_first: true,
            admin,
            group,
            lang: Lang::En,
        }
    }

    fn zh(admin: bool, group: bool) -> Ctx {
        Ctx { lang: Lang::Zh, ..ctx(admin, group) }
    }

    fn text(blocks: &[RBlock]) -> String {
        format!("{blocks:?}")
    }

    /// Every command of the bot is explained by a topic its users can see.
    #[test]
    fn every_command_has_a_topic() {
        for c in crate::intake::COMMANDS {
            let (admin, group) = match *c {
                "alias" | "unalias" | "credits" => (true, false),
                "allow" | "deny" | "ban" | "unban" | "acl" | "quota" => (true, true),
                _ => (false, true),
            };
            assert!(topic(&ctx(admin, group), c).is_ok(), "/help {c}");
        }
        assert!(topic(&ctx(true, false), "allow").is_ok(), "private admin topic");
        // Admin topics only for admins.
        assert!(topic(&ctx(false, true), "quota").is_err());
        assert!(topic(&ctx(false, false), "admin").is_err());
    }

    /// The marker is the configured one everywhere; nothing else uses `$` but
    /// amounts (`≈$0.336`, written `{USD}`); the deployment's values are filled in.
    #[test]
    fn texts_use_the_configured_marker_and_values() {
        let all = text(&overview(&ctx(true, false)));
        assert!(all.contains("c%Hello!") && all.contains("/alias name% spec"), "{all}");
        assert!(!all.replace("≈$", "≈").contains('$'), "a `$` that is not the marker or an amount");
        assert!(all.contains("up to 50 MB") && all.contains("above 0.2 USD") && all.contains("author/model%"));
        for placeholder in ["{MAX_FILE_MB}", "{IMAGE_MODELS}", "{BOLD_COST}", "{DIRECT_IDS}", "{ORDER}", "{USD}"] {
            assert!(!all.contains(placeholder), "{placeholder}");
        }
        assert!(!all.contains(":::"), "every fold is closed and converted");
        let mut c = ctx(false, true);
        (c.ids_openrouter, c.ids_platforms, c.bold_cost_usd) = (false, false, 0.0);
        let plain = text(&overview(&c));
        assert!(!plain.contains("type a model id") && !plain.contains("is bold"));
    }

    /// One message: the overview fits Telegram's limits with every topic.
    #[test]
    fn the_overview_is_one_message() {
        let blocks = overview(&ctx(true, false));
        let chars: usize = blocks.iter().map(|b| b.metrics().chars).sum();
        assert!(chars < 25_000, "{chars} characters");
        let folds = |b: &[RBlock]| b.iter().filter(|b| matches!(b, RBlock::Details { .. })).count();
        assert_eq!(folds(&blocks), TOPICS.len() - 1, "every topic but the other chat type's admin topic");
        assert_eq!(folds(&overview(&ctx(false, true))), TOPICS.len() - 2, "no admin topic for others");
        assert!(text(&overview(&ctx(true, true))).contains("act on this group only"));
    }

    #[test]
    fn a_topic_by_name_or_command() {
        let (title, b) = topic(&ctx(false, true), "/STATS").unwrap();
        assert!(title.contains("Costs") && !b.is_empty());
        assert!(topic(&ctx(false, true), "options").unwrap().0.contains("Options"));
        let e = topic(&ctx(false, true), "nope").unwrap_err();
        assert!(e.contains("`options`") && !e.contains("`admin`"), "{e}");
    }

    /// The Chinese help has every topic, the configured marker and the values,
    /// and is as complete as the English one.
    #[test]
    fn the_chinese_help() {
        for t in TOPICS {
            assert!(!t.title_zh.is_empty() && !t.body_zh.is_empty(), "{}", t.key);
            // The same examples and commands: every code span of the English text is in the Chinese one,
            // apart from example sentences written in the language.
            let spans = |s: &str| s.split('`').skip(1).step_by(2).map(str::to_string).collect::<Vec<_>>();
            let zh_spans = spans(t.body_zh);
            for span in spans(t.body).iter().filter(|x| x.is_ascii() && !x.contains(' ') || x.starts_with('/')) {
                if span.chars().any(|c| c.is_ascii_alphabetic()) && (span.contains('$') || span.starts_with('/')) {
                    continue; // examples like `c$Hello` are translated
                }
                assert!(zh_spans.contains(span), "{}: `{span}` is missing in Chinese", t.key);
            }
            assert_eq!(t.body.matches("::: ").count(), t.body_zh.matches("::: ").count(), "{}", t.key);
            assert_eq!(t.body.matches("```").count(), t.body_zh.matches("```").count(), "{}", t.key);
        }
        let all = text(&overview(&zh(true, false)));
        assert!(all.contains("c%你好！") && all.contains("/alias name% spec"), "{all}");
        assert!(!all.replace("≈$", "≈").contains('$'), "a `$` that is not the marker or an amount");
        assert!(all.contains("文件最大 50 MB") && all.contains("超过 0.2 USD") && all.contains("author/model%"));
        assert!(!all.contains(":::"), "unconverted fold");
        for placeholder in ["{MAX_FILE_MB}", "{IMAGE_MODELS}", "{BOLD_COST}", "{DIRECT_IDS}", "{USD}"] {
            assert!(!all.contains(placeholder), "{placeholder}");
        }
        let (title, _) = topic(&zh(false, true), "stats").unwrap();
        assert!(title.contains("费用"), "{title}");
        assert!(topic(&zh(false, true), "nope").unwrap_err().contains("没有帮助主题"));
        let chars: usize = overview(&zh(true, false)).iter().map(|b| b.metrics().chars).sum();
        assert!(chars < 25_000, "{chars}");
    }

    /// Every `$` (the marker) is in code, so any marker the configuration
    /// allows reads literally and cannot change the Markdown around it.
    #[test]
    fn the_marker_is_always_in_code() {
        let mut texts = vec![INTRO, OUTRO, INTRO_ZH, OUTRO_ZH];
        for t in TOPICS {
            texts.push(t.body);
            texts.push(t.body_zh);
        }
        for text in texts {
            let mut fenced = false;
            for line in text.lines() {
                if line.trim_start().starts_with("```") {
                    fenced = !fenced;
                    continue;
                }
                if fenced {
                    continue;
                }
                let mut in_code = false;
                for c in line.chars() {
                    match c {
                        '`' => in_code = !in_code,
                        '$' => assert!(in_code, "`$` outside code in: {line}"),
                        _ => {}
                    }
                }
            }
        }
        // Other markers, one and two characters, in both languages and both chat types.
        for marker in ["¥", ">>", "%"] {
            for lang in [Lang::En, Lang::Zh] {
                for (admin, group) in [(true, true), (true, false), (false, true)] {
                    let c = Ctx { marker: marker.into(), lang, ..ctx(admin, group) };
                    let all = text(&overview(&c));
                    // Amounts in examples (`≈$0.336`) keep their dollar sign.
                    assert!(all.contains("≈$0.336"), "{marker} {lang:?}");
                    assert!(!all.replace("≈$", "≈").contains('$'), "{marker} {lang:?}");
                    assert!(all.contains(&format!("c{marker}")), "{marker} {lang:?}");
                }
            }
        }
    }

    /// Folds inside a topic; `:::` in a code block is code.
    #[test]
    fn folds_inside_topics() {
        let b = blocks("Intro\n\n::: More\nInside\n:::\n```\n::: not a fold\n```\nEnd", 0);
        assert!(matches!(&b[1], RBlock::Details { title, .. } if title.to_plain() == "More"), "{b:?}");
        assert!(text(&b).contains("::: not a fold"));
    }
}
