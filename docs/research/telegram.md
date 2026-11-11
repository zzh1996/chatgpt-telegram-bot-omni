# Telegram platform research for an MTProto LLM-streaming bot (snapshot 2026-11-11)

Context: Rust bot on grammers (TL layer 233), logged in as a bot account, streaming LLM output into busy groups
(many concurrent users, threaded by reply chains). Everything below was checked against live docs/sources on
2026-11-11. **UNVERIFIED** marks anything not stated by an official source (inference, community lore, or untested).

## 0. Sources and documentation status

**Status of the docs (important):** core.telegram.org's MTProto docs stop at **layer 225**. The schema page,
the layers page and the constructor/method pages all show "Layer 225". The pages
`/constructor/inputRichMessage`, `/constructor/richMessage`, `/constructor/pageBlockThinking`,
`/constructor/inputMediaWidget`, `/constructor/sendMessageStopDraftAction`, `/method/ephemeral.sendMessage`,
`/method/messages.getRichMessage`, `/constructor/textMath` etc. all return "The page has not been saved". The RPC error
DB `errors.json` is at layer 227. So MTProto-level semantics here come from:
(a) the **Bot API docs**, which document the features (10.1 to 10.4);
(b) **TDLib** (Telegram's official client library, also the engine behind the Bot API server): its layer-233 schema
and its C++ source. These are marked "(TDLib)" and pinned to commit `c8828d5` (2026-11-09);
(c) inference, marked **UNVERIFIED**.

Legend (used inline):
- [BAPI#x] = https://core.telegram.org/bots/api#x (x = lower-cased type/method/section anchor)
- [CHG] https://core.telegram.org/bots/api-changelog · [FEAT#x] https://core.telegram.org/bots/features#x
- [WID] https://core.telegram.org/bots/widgets · [AI] https://core.telegram.org/api/bots/ai#live-response-streaming
- [FORUM] https://core.telegram.org/api/forum#bot-forums · [UPD] https://core.telegram.org/api/updates
- [CFG] https://core.telegram.org/api/config · [CFGC] https://core.telegram.org/constructor/config
- [ENT] https://core.telegram.org/api/entities · [ERRDOC] https://core.telegram.org/api/errors
- [ERR] https://core.telegram.org/api/errors.json (machine-readable; has `user_only`/`bot_only` method lists)
- [FILES] https://core.telegram.org/api/files · [FREF] https://core.telegram.org/api/file-references
- [FAQ] https://core.telegram.org/bots/faq · [L] https://core.telegram.org/api/layers
- [TL] https://github.com/tdlib/td/blob/c8828d50f5eac2fc7089ce1089d7ce9095c61b5e/td/generate/scheme/telegram_api.tl
- [TDAPI] https://github.com/tdlib/td/blob/c8828d50f5eac2fc7089ce1089d7ce9095c61b5e/td/generate/scheme/td_api.tl
- [TD:file#Ln] = https://github.com/tdlib/td/blob/c8828d50f5eac2fc7089ce1089d7ce9095c61b5e/td/telegram/file#Ln
- [TBA] https://github.com/tdlib/telegram-bot-api/blob/master/telegram-bot-api/Client.cpp (Bot API server source)
- [TDHIST] https://github.com/tdlib/td/commits/master/td/generate/scheme/telegram_api.tl
- [TOS] https://telegram.org/tos/bot-developers#6-2-6-enabling-topics-in-private-chats
- Blog: [B-THR] https://telegram.org/blog/comments-in-video-chats-threads-for-bots (2025-10-10) ·
  [B-GUEST] https://telegram.org/blog/ai-bot-revolution-11-new-features (2026-05-07) ·
  [B-RICH] https://telegram.org/blog/watch-apps-and-more (2026-06-11) ·
  [B-EPH] https://telegram.org/blog/communities-editor-invisible-messages (2026-07-14) ·
  [B-BTN] https://telegram.org/blog/welcome-messages-buttons-TG-13 (2026-08-25)
- [PRISMA] https://github.com/TelegramMessenger/libprisma#supported-languages

**Timeline.** Layers come from TDLib's schema commits [TDHIST]; Bot API versions from [CHG].

| Date | Layer (TDLib commit) | Bot API | What appeared |
|---|---|---|---|
| 2025-12-31 | ≤224 | 9.3 | `sendMessageDraft`, topics in private chats (`has_topics_enabled`); `sendMessageTextDraftAction#376d975c random_id text` |
| 2026-02-09 | | 9.4 | bots can `createForumTopic` in private chats; `allows_users_to_create_topics` |
| 2026-03-01 | | 9.5 | "Allowed all bots to use the method sendMessageDraft" |
| 2026-04-07 | 225 | | last layer documented on core.telegram.org; "Documented rate limits for live drafts" [L] |
| 2026-05-08 | | 10.0 | empty draft text allowed ("Thinking…" placeholder) |
| 2026-05-13 | 227 | | `richMessage`, `inputRichMessage*`, `pageBlockThinking`, `pageBlockMath`, `inputSendMessageRichMessageDraftAction` |
| 2026-06-11 | | 10.1 | Rich messages: `sendRichMessage`, `sendRichMessageDraft`, `editMessageText.rich_message` [B-RICH] |
| 2026-06-15 | 228 | | `ephemeral.*`, `updateNewEphemeralMessage` |
| 2026-07-14 | | 10.2 | Ephemeral messages/commands; `InputRichBlock*` (block input) [B-EPH] |
| 2026-07-17 | 229 | | `sendMessageTextDraftAction#3630b85a` (can_stop/keep_on_stop), `sendMessageStopDraftAction`, `pageBlockButtonRow`, `pageBlockDocument` |
| 2026-08-24 | 230 (08-25) | 10.3 | rich buttons, `is_compact` tables, expandable quotes, document blocks, `can_stop`, `MessageGenerationStopped` [B-BTN] |
| 2026-10-12 | 232 | | `quote_rich` in `inputReplyToMessage`/`messageReplyHeader` |
| 2026-10-23 / 11-09 | 233 | 10.4 | HTML widgets (`inputMediaWidget`...), rich-message quotes, ephemeral callback replies in any chat, private-topic links |

Constructor IDs that changed after the documented layer 225, as of 233 [TL]: `inputReplyToMessage#75c1ad70` (adds `quote_rich:flags.8`),
`messageReplyHeader#ddb5ab75` (adds `quote_rich:flags.14`), `message#7600b9d3` (adds `rich_message:flags2.13`),
`sendMessageTextDraftAction#3630b85a`, `messages.sendMessage#fef48f62`, `messages.editMessage#b106e66c`.

---

## 1. Rich messages

### 1.1 What they are, where they work
- "Rich messages support advanced structured formatting options like headings, lists, tables, media, block
  quotations, collapsible blocks, footnotes, and formulas" [BAPI#rich-message-formatting-options]. Positioned for
  "reports, AI-streamed answers, documentation snippets, technical papers" [FEAT#rich-messages]. Try them with @RichTextDemoBot.
- Released with Bot API 10.1 (2026-06-11) [CHG] and the blog post [B-RICH]: "up to 32,768 characters now, with a
  merciful Show More button after the first 8,000 or so". Since 2026-07-14, Premium **users** also have a rich text
  editor that sends up to 32,768 chars [B-EPH], so incoming user messages can be rich too.
- MTProto shape [TL]:
  `message.rich_message:flags2.13?RichMessage`;
  `richMessage#baf39d8b flags:# rtl:flags.0?true part:flags.1?true blocks:Vector<PageBlock> photos:Vector<Photo> documents:Vector<Document>`;
  `inputRichMessage#e4c449fc flags:# rtl:flags.0?true noautolink:flags.1?true blocks:Vector<PageBlock> photos:flags.2?Vector<InputPhoto> documents:flags.3?Vector<InputDocument> users:flags.4?Vector<InputUser>`.
- `part` flag: TDLib maps it to `richMessage.is_full=false`, where "getFullRichMessage must be used to get the full
  message" [TDAPI L132-136], [TD:RichMessage.cpp#L86]. That call is `messages.getRichMessage#501569cf peer id`,
  and errors.json lists it as **user_only** [ERR], so bots cannot fetch the full version. The Bot API server never
  re-fetches either [TBA]. So bots presumably always get full content: **UNVERIFIED**. Log it if you ever see `part`.
- **Sending (TDLib):** `messages.sendMessage` with `message=""`, no entities, and `rich_message=InputRichMessage`
  [TD:MessagesManager.cpp#L1553]. **Editing:** `messages.editMessage` with `rich_message`. TDLib also sets the
  `message` flag, with `""` [TD:MessagesManager.cpp#L1799-L1840]. TDLib allows **Text→Rich and Rich→Text** edits:
  the old content may be Text/RichText/Game and the new content text or rich [TD:MessagesManager.cpp#L23690-L23703].
  Bot API: `editMessageText` "edit text, rich and game messages"; `text` is "required if rich_message isn't specified" [BAPI#editmessagetext].
- **Chats:** `sendRichMessage.chat_id` accepts any chat. `message_thread_id` works "for forum supergroups and private
  chats of bots with forum topic mode enabled". "If the message contains a block with a media element, then the bot
  must have the right to send the media to the chat" [BAPI#sendrichmessage]. So rich messages work in private chats,
  basic groups, supergroups, channels, forum topics and bot-forum topics.
  - Rich content is also allowed in ephemeral messages (`ephemeral.sendMessage.rich_message`; TDLib
    `is_allowed_ephemeral_message_content`=true) and in inline results (`InputRichMessageContent`, "Only previously
    uploaded files").
  - Rich messages can't be album members [TD:MessageContentType.cpp#L330], and they are editable
    [TD:MessageContentType.cpp#L762].
- **Old clients:** there is no documented fallback, and `richMessage` has no plain-text alternative field. The
  2026-08-25 release added "a stylish placeholder that appears when outdated apps can't render a new feature — and
  gives you a button for instantly downloading the latest update" [B-BTN]. Expect old apps to show "unsupported
  message" with no readable text: **UNVERIFIED**.

### 1.2 Limits
Values are from [BAPI#rich-message-limits]. Client config keys are from [TD:ConfigManager.cpp#L1432-L1436] and the
defaults from [TD:OptionManager.cpp#L219-L223].

| Limit | Value | appConfig key |
|---|---|---|
| Total text | ≤ 32768 **Unicode code points** in all text of all blocks (paragraphs, code, table cells, details titles, formula sources, captions), **verified 2026-11-11** (32768 ASCII or CJK or 16385 emoji accepted, 32769 → `RICH_MESSAGE_TEXT_TOO_LONG`) | `rich_message_length_limit` |
| **Per text string** | **Silently truncated to 70000 UTF-8 bytes** — no error. If the cut falls inside a character the string is stored **empty** (23334 CJK = 70002 B → empty paragraph; an empty code block/cell is then rejected), otherwise the tail is lost (17501 emoji → 17500). Several adjacent text nodes are fine (4×8192 CJK in one paragraph = 98 KB intact; no per-message byte limit). Verified 2026-11-11 by reading messages back | – |
| Normalization | Stored text: tab, CR, NUL and other C0 controls → space (also in code blocks); U+2028 → line break; U+2029 → space; U+007F, U+0085, zero-width/BOM/RTL marks, combining marks, ZWJ sequences kept. Verified 2026-11-11 | – |
| Blocks | ≤ 500, counting nested blocks, list items, ordered list items, table rows, quotation blocks and details blocks; **table cells do not count**. Verified 2026-11-11 (501 → `RICH_MESSAGE_BLOCKS_TOO_MANY`) | `rich_message_max_blocks` |
| Nesting | ≤ 16 levels "of nested formatting and blocks" | `rich_message_max_depth` |
| Media | ≤ 50 photo/document references per message — the same photo counts each time it appears. Verified 2026-11-11 (51 → `RICH_MESSAGE_MEDIA_TOO_MANY`) | `rich_message_max_media` |
| Empty content | Rejected (`RICH_MESSAGE_CONTENT_REQUIRED`): empty preformatted, heading, footer, collapsed quote; list without items or item without blocks; quote/details without blocks; table without rows; empty math (`…MATH_SOURCE_REQUIRED`); a message of only a divider (`RICH_MESSAGE_EMPTY`). Accepted: empty paragraph, empty cell, empty inline spans, whitespace-only code. Verified 2026-11-11 | – |
| Table | ≤ 20 columns (rows count as blocks) | `rich_message_max_table_cols` |
| Button row | 1–8 buttons (`InputRichBlockButtons`) | – |
| Map | width+height ≤ 10000, ratio ≤ 20, zoom 0–24 (`InputRichBlockMap`) | – |
| Media id in MD/HTML | 1–64 chars `[A-Za-z0-9_-]` (`InputRichMessageMedia`) | – |
| Quote of a rich msg | ≤ 32 quoted texts, total ≤ 1024 chars (`ReplyParameters.quote_rich_texts`) | `quote_length_max` |
| Code block length / per-block size | no separate limit documented (bounded by 32768): **UNVERIFIED** | – |
| Edit count / edit window for own msgs | not documented: **UNVERIFIED** | – |
| UI | "Show More" collapse after ~8000 chars [B-RICH] | – |

No rich-message-specific RPC error strings exist in errors.json (layer 227) [ERR]. Expect `MESSAGE_TOO_LONG` or
new strings: **UNVERIFIED**, so log the raw errors.

### 1.3 Allowed PageBlock / RichText constructors (sending), with the Bot API mapping
The Bot API `InputRichBlock*` list [BAPI#inputrichblock] maps to telegram_api as TDLib does it in
[TD:WebPageBlock.cpp] (`get_input_page_block`):

| Bot API block | MTProto constructor (layer 233) |
|---|---|
| Paragraph | `pageBlockParagraph text` |
| SectionHeading(size 1–6) | `pageBlockHeading1`…`pageBlockHeading6 text` (L855-865) |
| Preformatted | `pageBlockPreformatted text language` |
| Footer / Divider / Anchor | `pageBlockFooter` / `pageBlockDivider` / `pageBlockAnchor name` |
| MathematicalExpression | `pageBlockMath source` |
| List (unordered / ordered) | `pageBlockList items:Vector<pageListItemBlocks(checkbox,checked,blocks)>` / `pageBlockOrderedList reversed start type items:Vector<pageListOrderedItemBlocks(num,blocks,value,type)>` |
| BlockQuotation(blocks, credit) | `pageBlockBlockquoteBlocks blocks caption` (L3590); text-only quote: `pageBlockBlockquote collapsed=false text caption` |
| ExpandableBlockQuotation (10.3) | `pageBlockBlockquote collapsed=true text caption` (L2070) |
| PullQuotation | `pageBlockPullquote text caption` |
| Collage / Slideshow | `pageBlockCollage` / `pageBlockSlideshow items caption:pageCaption(text,credit)` |
| Table | `pageBlockTable bordered striped compact title rows:Vector<pageTableRow(cells:Vector<pageTableCell>)>` |
| Details | `pageBlockDetails open blocks title` |
| Map | `inputPageBlockMap geo:InputGeoPoint zoom w h caption` |
| Buttons (10.3) | `pageBlockButtonRow align_left/align_center/align_right buttons:Vector<pageButton(text, type:InlineButtonType, style:richButtonStyle)>` |
| Photo | `pageBlockPhoto spoiler photo_id caption` (photo must also be in `inputRichMessage.photos`) |
| Video / Animation | `pageBlockVideo autoplay loop spoiler video_id caption` (animation = autoplay+loop) |
| Audio / VoiceNote | `pageBlockAudio audio_id caption` |
| Document (10.3) | `pageBlockDocument document_id caption` |
| Thinking (drafts only) | `pageBlockThinking text` |

- **RichText** (TDLib `get_input_rich_text`, [TD:RichText.cpp#L852]): `textEmpty`, `textPlain`, `textBold`,
  `textItalic`, `textUnderline`, `textStrike`, `textFixed` (inline code), `textUrl text url webpage_id=0`,
  `textEmail`, `textPhone`, `textConcat`, `textSubscript`, `textSuperscript`, `textMarked`, `textAnchor text name`,
  `textMath source`, `textCustomEmoji document_id alt`, `textSpoiler`, `textMentionName text user_id` (also pass the
  InputUser in `users`), `textDate` (flags relative/short_time/long_time/short_date/long_date/day_of_week, text, date),
  `textButton text type style` (a button inside text), and `textTonAddress`.
  - Footnotes and in-document links are `textAnchor(text,name)` targets plus `textUrl(text,"#"+urlencode(name),0)`
    links [TD:RichText.cpp#L480-L523].
  - `textMention`, `textHashtag`, `textCashtag`, `textBotCommand`, `textAutoUrl`, `textAutoEmail`, `textAutoPhone`
    and `textBankCard` are produced by **server-side auto-detection**. TDLib sends them as plain text. Set
    `noautolink` (= Bot API `skip_entity_detection`) to disable detection.
- **Not for messages:**
  - `textImage` (TDLib: "richTextIcon… instant view only" [TDAPI L4646-4650]).
  - Instant-View-only blocks: `pageBlockTitle/Subtitle/AuthorDate/Header/Subheader/Kicker/Cover/Embed/EmbedPost/Channel/RelatedArticles`.
    The Bot API doesn't expose them, so this is inference.
  - `textDiff`: the AI-editor diff, not exposed to bots; **UNVERIFIED**.
- **Media references (TDLib):**
  - `pageBlockPhoto.photo_id` must equal the `InputPhoto.id` in `photos`. Video, audio and document ids must be in
    `documents` [TD:WebPageBlock.cpp#L2352-L2362].
  - For new files, TDLib first calls `messages.uploadMedia` for each file and then references the returned
    Photo/Document [TD:MessagesManager.cpp#L22373-L22390].
  - In Markdown/HTML sources, `files:Vector<InputRichFile>` with `inputRichFilePhoto#9b00622b id:string photo:InputPhoto`
    / `inputRichFileDocument#83281dbd id:string document:InputDocument` binds `tg://photo?id=<id>`,
    `tg://video?id=`, `tg://audio?id=` and `tg://document?id=` links.
  - Drafts can't upload: "Direct upload of new files and explicit upload of files by a URL isn't supported"
    [BAPI#sendrichmessagedraft]. TDLib's error is "New files can't be uploaded using the method".

### 1.4 Behaviour of specific blocks
- **`pageBlockThinking`**: "A block with a 'Thinking…' placeholder… The block may be used only in
  sendRichMessageDraft, therefore it can't be received in messages" [BAPI#richblockthinking]. Recommended custom
  emoji: https://t.me/addemoji/AIActions. It is "a presentation element, not a transcript of the model's reasoning"
  [FEAT#streaming-replies]. What the server does if you put it in `sendMessage`/`editMessage` (reject or strip) is
  **UNVERIFIED**. Never use it in persisted messages.
- **`pageBlockDetails`** is a collapsible block (HTML `<details>`). The `title` (summary) is always shown, the
  `blocks` are hidden unless `open` is set ("visible by default") [BAPI#richblockdetails]. It can nest any blocks,
  and the nesting counts toward the 500-block and 16-level limits. This is the right container for collapsed reasoning.
- **`pageBlockBlockquote collapsed`**: an expandable quotation, text-only plus a credit. Use
  `pageBlockBlockquoteBlocks` to nest blocks, but that variant is not collapsible.
- **`pageBlockMath` / `textMath`**: "Formula source is treated as raw LaTeX" [BAPI#rich-markdown-style]; "Extensive
  LaTeX support, including both inline and as block formulas" [FEAT#rich-messages]. The supported LaTeX subset is
  undocumented (**UNVERIFIED**, so test macros like `\begin{aligned}` with @RichTextDemoBot). The formula source
  counts toward the 32768 chars.
- **`pageBlockPreformatted language`**: the Bot API points to libprisma for "programming and markup languages for which
  syntax highlighting is supported" [BAPI#formatting-options]. That is 288 grammars, including
  `rust, python/py, go, typescript/ts, javascript/js, bash/sh/shell, json, yaml/yml, toml, sql, diff, markdown/md,
  latex/tex, docker/dockerfile, kotlin, swift, c, cpp` [PRISMA]. Unknown names presumably render unhighlighted (**UNVERIFIED**).
- **Tables**:
  - Cell flags: `pageTableCell flags header align_center align_right valign_middle valign_bottom text colspan rowspan`
    (left/top is the default). "If omitted [text], then the cell is invisible" [BAPI#richblocktablecell].
  - "Table cells can contain only inline formatting" [BAPI#rich-markdown-style].
  - Table options: `bordered/striped/compact` and a caption (`title`). Limits: 20 columns, and every row is a block.
- **Images**:
  - `pageBlockPhoto` (with spoiler and a caption with credit), collage/slideshow, or media inside lists, quotes and
    details.
  - Markdown/HTML media URLs: "Media blocks support only HTTP and HTTPS URLs" [BAPI#rich-markdown-style]. The server
    fetches them.
  - When building blocks locally, upload first (`messages.uploadMedia`) and reference the photo by id (see 1.3).
- **Buttons**:
  - Inline `textButton` or `pageBlockButtonRow`.
  - Types: `inlineButtonTypeUrl`, `inlineButtonTypeCallback data` (1–64 bytes), `inlineButtonTypeWebView` ("private
    chats only"), `inputInlineButtonTypeUrlAuth` ("Not supported for ephemeral messages"), `inlineButtonTypeSwitchInline`,
    `inlineButtonTypeCopy copy_text` (1–256 chars), `inlineButtonTypeDisabled`, user-profile.
  - Styles: `richButtonStyle bg_primary/bg_danger/bg_success/link`; "link" is callback-only
    [BAPI#richmessagebutton]. Callback presses arrive as callback queries (Bot API). On MTProto that is presumably
    `updateBotCallbackQuery`: **UNVERIFIED** for in-text buttons.

### 1.5 Replies, quotes, previews, notifications, search, forward, copy
- **Replying** to a rich message is normal (`inputReplyToMessage`).
- **Partial quotes** of rich messages exist since Bot API 10.4 / layer 232:
  - `TextQuote.rich_texts`. Its `text` "may be empty if the quote was chosen from a rich message".
  - The position is measured "in the text formed by joining plain texts of all quotable texts of the message,
    separated by line feeds" [BAPI#textquote].
  - On MTProto, received quotes are `messageReplyHeader.quote_rich:flags.14?Vector<RichText>` (`quote_text` may be
    empty). Sent quotes use `inputReplyToMessage.quote_rich:flags.8?Vector<RichText>` [TL].
  - Quotable units: paragraph/heading/pre/footer/math text; block/pull quote text and credit; details summary; table
    caption and **each cell**; media/collage/slideshow/map captions; nested blocks.
  - "Mathematical expressions must be quoted entirely. Buttons…, anchors and dividers can't be quoted". Kept
    formatting: bold, italic, underline, strike, spoiler, sub, sup, date, custom emoji, math [BAPI#replyparameters].
- **Cross-chat quoting**: `ExternalReplyInfo` has a `widget` field but no `rich_message` field
  [BAPI#externalreplyinfo]. The features page says regular messages "support exclusive interactions like partial
  quotes and quoting messages to other chats" [FEAT#regular-messages]. The partial-quote half of that is outdated by
  10.4; cross-chat quotes of rich messages are likely unsupported (**UNVERIFIED**).
- **Forward** is allowed: only "service messages and messages with protected content can't be forwarded". **Copy**
  (`copyMessage`) excludes widgets but not rich messages [BAPI#forwardmessage], [BAPI#copymessage].
- **Search**: TDLib's local search indexes all rich texts joined with spaces
  [TD:MessageContent.cpp#L13496-L13515]. Server-side search is **UNVERIFIED**. Bots can't search anyway:
  `messages.search` is user_only [ERR].
- **Reply previews, notification text, chat-list snippet, and "copy text" UX** are undocumented: **UNVERIFIED**.
  The likely plain-text form is the line-feed-joined quotable texts, by analogy with the quote position rule.
- **Bot context**: store your own raw Markdown per `(peer, msg_id)`. Reconstructing it from blocks is lossy, and bots
  can't read history (see §7).

### 1.6 Server-side Markdown/HTML parsers (reference only; we build blocks locally)
`inputRichMessageMarkdown#4b572c` and `inputRichMessageHTML#dacb836a` (flags `rtl noautolink`, `files`) carry raw
source. TDLib passes it through unparsed [TD:RichMessage.cpp#L283-L300], so parsing is server-side. Documented syntax
[BAPI#rich-markdown-style], [BAPI#rich-html-style]:
- **Markdown**: "compatible with GitHub Flavored Markdown where possible and can contain arbitrary HTML".
  - Inline: `**b**`/`__b__`, `*i*`/`_i_`, `~~s~~`, `` `code` ``, `==mark==`, `||spoiler||`, `$x^2$`,
    `[t](url|mailto:|tel:|tg://user?id=)`, `![](tg://emoji?id=…)`, `![t](tg://time?unix=…&format=wDT)`.
  - Blocks: `#`…`######`; fenced code with a language; `---`; `-`, `*`, `+` and `1.` lists; `- [ ]`/`- [x]` task
    lists; `>` quotes; `![](https://…/photo.jpg "caption")`; pipe tables with `:---:` alignment; footnotes
    `[^id]` + `[^id]: …`; `$$…$$` and fenced `math`.
  - Supported HTML inside Markdown: `<details open><summary>`, `<tg-collage>`, `<tg-slideshow>`, `<u>`, `<sub>`/`<sup>`
    and the rest. "Markdown isn't parsed inside block HTML tags other than `<details>`, `<tg-collage>` and `<tg-slideshow>`".
- **HTML tags**: `b/strong i/em u/ins s/strike/del code mark sub sup tg-spoiler a(href|name) tg-reference tg-emoji
  tg-time tg-math h1-h6 p pre>code.language-x footer hr ul ol(start type reversed) li(value type) input[checkbox]
  blockquote[expandable]+cite aside img video audio tg-document figure/figcaption/cite tg-map tg-collage
  tg-slideshow table[bordered striped compact]/caption/tr/th/td(colspan rowspan align valign) details/summary
  tg-math-block tg-button(type=url|callback_data|web_app|login_url|switch_inline_query*|copy_text|disabled, style)
  tg-button-row(align)`. `<tg-thinking>` is for drafts only.
  - Named entities are limited to `&lt; &gt; &amp; &quot; &apos; &nbsp; &hellip; &mdash; &ndash; &lsquo; &rsquo;
    &ldquo; &rdquo;`; numerical entities are allowed.

### 1.7 Bot API equivalents (for reading docs and mapping concepts)
- **Types**: `RichMessage {blocks, is_rtl}`, `InputRichMessage {blocks | html | markdown, media, is_rtl,
  skip_entity_detection}` ("Exactly one of the fields html, markdown, or blocks must be used"),
  `InputRichMessageMedia`, `RichBlock*`/`InputRichBlock*`, `RichText*` (27 kinds), `RichMessageButton`.
- **Methods**: `sendRichMessage`, `sendRichMessageDraft`; `editMessageText.rich_message`; `editEphemeralMessageText.rich_message`.
- **Fields**: `Message.rich_message`, `TextQuote.rich_texts`, `ReplyParameters.quote_rich_texts`,
  `InputRichMessageContent` (inline/guest/Web App results). All in [BAPI#rich-messages].

---

## 2. Interactive HTML widgets (`inputMediaWidget` & co.)

- **Purpose**: "small interactive blocks drawn right inside a message… one self-contained HTML document with inline
  CSS and JavaScript… in a sandbox with no network access at all" (polls, tickers, two-player games, promo codes) [WID].
  - Introduced in Bot API 10.4 (2026-11-09) [CHG], layer 233 [TDHIST].
  - A widget is "fixed code plus changeable state". The HTML never changes after sending ("No method replaces it").
    Only the **bot** writes the state.
- **Who can send**: bots, via `sendWidget`/`sendSharedWidget`, or users through a bot's inline mode.
  - **Production: private chats with the bot only**, and sent widgets "automatically have protected content and
    therefore can't be forwarded" [BAPI#sendwidget].
  - Groups (basic + supergroups, topics), inline mode and forwards are **test-server only** [WID#availability].
  - In groups the bot needs `can_send_other_messages`. Channels refuse widgets.
  - Old apps show "unsupported media with its caption" [WID#where-widgets-live].
- **MTProto (layer 233) [TL]**:
  - Send: `messages.sendMedia(media=inputMediaWidget#44f6c04e flags:# allow_queries shared_widget width:int height:int
    html:string public_data:flags.2?string chat_data:flags.3?string, message=<caption>)`. Re-send a shared widget with
    `inputMediaWidgetId#7c855cb5 widget_id chat_data`.
  - Receive: `messageMediaWidget#2238f214 flags:# is_copy can_query background_query width height payload:bytes
    public_data public_data_hash chat_data widget_id`. `payload` is gzip-compressed HTML; TDLib calls `gzdecode`
    [TD:MessageContent.cpp#L1929].
  - State pushes: `updateMessageWidgetData peer msg_id public_data public_data_hash chat_data pts pts_count`.
  - Queries: `updateBotWidgetQuery#b373fe68 query_id query_type:WidgetQueryType user_id peer msg_id chat_instance
    chat_type data:bytes widget_id? wallet_card?`, plus `updateInlineBotWidgetQuery` and `updateBotWidgetCopyQuery`
    (from forwards).
  - Answers and edits: `messages.setBotWidgetQueryAnswer#f8cdec6a query_id result?:string`;
    `messages.editMessageWidgetData#f624bc1e peer id public_data? chat_data?` (returns Updates);
    `messages.editInlineBotMessageWidgetData`; `messages.editWidgetData#eac343c0 widget_id public_data cache_time?`
    (shared widgets).
  - `messages.getWidgetPublicData#6fc53181 peer msg_id hash` is what **user clients** poll (returns
    `messages.widgetPublicData hash data period` or `…NotModified period`). Bots don't need it.
- **Sandbox** [WID#what-widgets-support]:
  - Runtime: JS in **QuickJS** (ES2023), "never in a WebView". `fetch`, `XMLHttpRequest`, `WebSocket`, `EventSource`,
    WebRTC, `sendBeacon` and Workers **throw `NetworkDisabledError`**. No external scripts, styles, fonts or images
    (`data:` images only). No navigation, no `alert`/`prompt`. No keyboard or text input; form controls are removed.
  - **"nothing scrolls inside a widget, and there is no fullscreen"**: `overflow:auto/scroll` becomes hidden. Height
    is 50–600 px (auto or fixed), width is clamped to 200 px…the bubble's max (phones about 320–430 px).
    `<details>/<summary>` and `toggle` work.
  - Storage: `localStorage`/`sessionStorage`, 4 KB per area, per message, per device; never synced.
  - Bridge (`window.TelegramWidget`): `request(data)` (1–64 bytes → `widget_query` / `updateBotWidgetQuery`;
    promise result = the bot's answer; errors `NOT_ALLOWED, BUSY, RATE_LIMITED, TIMEOUT, FAILED, INVALID_ANSWER`),
    `chatData`/`publicData` + `chatDataChanged`/`publicDataChanged` events, `watchPublicData()`, `copyText` (≤256),
    `openLink` (https only, ≤256 chars, user confirms), `openTelegramLink`, haptics, `viewer` (only where requests
    are allowed), and a TON wallet.
  - Resource limits: CPU 300 ms/s and 500 ms per task (beyond that the widget is stopped), 8 MB memory, 2000 DOM
    nodes, 48 K UTF-16 text units per document, 400 CSS rules, 64 timers [WID#limits].
- **Limits and errors** [BAPI#sendwidget], [WID#limits]:
  - HTML is 1–32768 bytes **and ≤4096 bytes after zlib level 9** (`INPUT_MEDIA_WIDGET_TOO_LARGE`), with no NUL
    (`INPUT_MEDIA_WIDGET_HTML_INVALID`).
  - `public_data` and `chat_data` are each 0–1024 **bytes** (`INPUT_MEDIA_WIDGET_DATA_TOO_LARGE`,
    `INPUT_MEDIA_WIDGET_DATA_INVALID`/`WIDGET_DATA_INVALID`).
  - Answers: ≤1024 bytes (`WIDGET_RESULT_TOO_LARGE`), within **15 s** ("query is too old" after that).
  - Queries: "at most 5 queries per 5 seconds" per user per bot. Background queries (private chat only) happen once
    on appear, then ≤1 per 5 s.
  - Edits: an `editMessageWidgetData` call "counts as one sent message against your bot's limits for sending
    messages". A shared widget takes ≤1 edit/s. `cache_time` is 5–3600 s (default 30). An unchanged edit returns
    "message is not modified". Edits are silent: no "edited" mark, no notification.
- **Semantics**:
  - `chat_data` belongs to one message and is "shared by all members in a group" (use `viewer.id` to draw per-user
    views).
  - `public_data` is seen everywhere the widget appears. A static widget's forward gets a frozen copy; a shared
    widget has one state for all its messages. Treat both as public, and "Never put secrets in a widget".
- **Can a widget hold a long LLM answer with scrolling, collapsible reasoning and live content? No.**
  - The code is ≤4 KB compressed and frozen at send.
  - The live state is ≤1 KB + ≤1 KB.
  - There is no scrolling, and the height caps at 600 px.
  - There is no network, so polling only refreshes `public_data`, whose period is ≥5 s.
  - Every edit costs a sent message, and production is DMs only.
  - At most it can be a tiny status or control card in DMs. Not useful for busy groups today.

---

## 3. Draft streaming ("live drafts")

### 3.1 Mechanics (MTProto)
- **Send**: `messages.setTyping#58943ee2 flags:# peer:InputPeer top_msg_id:flags.0?int action:SendMessageAction`
  with one of these actions [AI], [TL]:
  - `sendMessageTextDraftAction#3630b85a flags:# can_stop:flags.0?true keep_on_stop:flags.1?true random_id:long text:TextWithEntities`
  - `inputSendMessageRichMessageDraftAction#a937c7be flags:# can_stop keep_on_stop random_id:long rich_message:InputRichMessage`.
    The receiving side gets `sendMessageRichMessageDraftAction#52564893 … rich_message:RichMessage`.
- **Bot API equivalents**:
  - `sendMessageDraft(chat_id, message_thread_id, draft_id, text 0-4096 chars, parse_mode/entities, can_stop,
    keep_on_stop)` and `sendRichMessageDraft(… rich_message …)`. Both return True [BAPI#sendmessagedraft],
    [BAPI#sendrichmessagedraft].
  - `draft_id` becomes MTProto `random_id` and must be non-zero [TBA], [TD:Requests.cpp#L5134-L5164].
  - Empty text shows a "Thinking…" placeholder (Bot API 10.0).
- **Identity**: "Every live draft is identified by" `random_id` + chat ID + topic ID (bot forums).
  - "To update a live draft, simply send a new sendMessageTextDraftAction with the same random_id… clients…
    fading in… the added characters (… max(0, strlen(new) - strlen(prev)))". That means **append-only text animates
    nicely; rewrites don't**.
  - A different random_id "will add a new live draft along with existing ones" [AI].
  - Caveats: the Bot API says that otherwise "the draft is replaced without animation". TDLib's client contract says
    a pending message is "deleted whenever any incoming message or a pending message with another draft_id is
    received in the message thread" [TDAPI L11629-11637]. And [AI] warns "some clients may treat all drafts sent to
    the same topic as having the same random_id". **Plan for one visible draft per chat/topic.**
- **Rendering and lifetime**: incoming drafts "are rendered as a normal messages, always pinned to the bottom of the
  chat" [AI].
  - Clients delete a draft `message_typing_draft_ttl` seconds after receiving it (**30**, [CFG]), "or when receiving a
    normal message within the same chat/bot forum topic, whichever comes first".
  - The Bot API calls the draft "ephemeral and acts as a temporary 30-second preview – once the output is finalized,
    you must call sendMessage with the complete message to persist it" [BAPI#sendmessagedraft].
  - So: **re-send at least every <30 s** during long pauses (e.g. during reasoning).
- **`random_id` vs the final message**:
  - **There is no linkage.** The final `messages.sendMessage` has its own random_id, which belongs to the
    message-dedup namespace (§7). The draft disappears because *any* normal bot message arrives in that chat/topic.
  - "random_ids in live drafts … can and often must be reused" [UPD#updatemessageid-updates].
  - Whether reusing the draft random_id as the message random_id does anything is **UNVERIFIED**, so generate them
    independently.
- **Ordering**: the final message is an ordinary message with the server ID/date at send time, so it lands after
  any user messages sent meanwhile. The draft itself always stays at the bottom. Thread it with `reply_to`.
- **Where**:
  - **Private 1-on-1 chats only.** "Currently, live drafts can only be sent to users" [AI]; Bot API `chat_id` is
    "Unique identifier for the target private chat".
  - Groups fail with `400 TEXTDRAFT_PEER_INVALID: sendMessageTextDraftAction can only be used in private 1-on-1
    chats` (listed for `messages.setTyping`) [ERR]. The rich draft variant presumably fails the same way: **UNVERIFIED**.
  - Bot-forum topics are supported (`top_msg_id`).
  - Since Bot API 9.5 all bots can stream, without threaded mode [CHG].
- **Stop button** (`can_stop`):
  - "Pass True to show the user a button to stop further drafts". The bot gets Bot API `stopped_message_generation` /
    `MessageGenerationStopped {chat, message_thread_id, draft_id}`.
  - `keep_on_stop`: "keep the draft in the chat when the button is pressed. The draft will still disappear after a
    short time or if the bot sends a message. To fully preserve the partial draft, the bot should send it as a new
    message" [BAPI#sendmessagedraft], [BAPI#messagegenerationstopped].
  - **On MTProto (TDLib)**: the user's client calls `messages.setTyping(peer=bot, top_msg_id, sendMessageStopDraftAction#fbf902b0 random_id)`
    [TD:Requests.cpp#L5166], [TD:DialogAction.cpp#L250-L258]. The bot receives
    **`updateUserTyping#2a17bf5c user_id top_msg_id? action=sendMessageStopDraftAction{random_id}`**. TDLib turns
    that into `updateStopMessageDraft(chat_id, forum_topic_id, draft_id)` and ignores it unless it comes from the
    private chat itself [TD:DialogActionManager.cpp#L124-L141].
  - `updateUserTyping` has no pts, so a stop event is **lost if you're disconnected** at that moment.
  - Whether a bot can send `sendMessageStopDraftAction` itself to clear a draft is **UNVERIFIED** (TDLib only allows
    users). To clear a draft, send the final message, or let the 30 s TTL expire.

### 3.2 Rate limits and sizes
- `messages.setTyping` limits, **per peer** [AI]:
  - "Max 20 calls in 5 seconds" and "Max 40 calls in 30 seconds"; exceeding them returns `FLOOD_WAIT_%d`
    ("making 20 calls at second 0 and then 1 more call at second 1 will return FLOOD_WAIT_3").
  - Plus "short cool-down FLOOD_WAITs (at most 3 seconds)… even if no ratelimit was hit".
  - "The same ratelimits apply to… sendMessageDraft, sendChatAction". So typing indicators and drafts to one user
    share one budget.
- Sustained ≤40/30 s means **≤1.33 updates/s on average**. Use tiered N-chars/M-seconds batching plus a local limiter, as [AI] advises.
- Size: a text draft is 0–4096 chars [BAPI#sendmessagedraft], and `MESSAGE_TOO_LONG` is listed for setTyping [ERR].
  Rich drafts fall under the rich limits (§1.2), and no uploads are allowed.
- Concurrency: the limits are per peer, so concurrent drafts to one user share them. Different users are independent.
  The number of simultaneous drafts per chat is not capped by the docs, but the client caveats above apply. Use
  bot-forum topics for parallel conversations with one user.

---

## 4. Threads/topics for bots ("threaded mode")

- "Bots now support threaded conversations to manage several different topics in parallel… especially useful for AI
  chatbots" [B-THR]. Bot API 9.3 / 9.4 / 10.4 [CHG]. Enabled by **Threaded mode in @BotFather** [FORUM].
- **MTProto flags**: `user.bot_forum_view` (flags2.16) means threaded mode is on. `user.bot_forum_can_manage_topics`
  (flags2.17) means users may create, modify and delete topics; @BotFather's "Disallow users to create new threads"
  controls it, and violations get `BOT_FORUM_CREATE_FORBIDDEN` [FORUM]. Bot API: `User.has_topics_enabled` and
  `User.allows_users_to_create_topics` (getMe only).
- **Topic operations**:
  - Create: `messages.createForumTopic(peer=user, title, icon_color?, icon_emoji_id?, random_id, title_missing?)`.
    The topic id = the id of the `messageActionTopicCreate` service message. General = id 1.
  - Send into a topic: `inputReplyToMessage.reply_to_msg_id=topic_id`. Replies inside a topic use
    `reply_to_msg_id=<msg>` plus `top_msg_id=<topic>`.
  - Delete: `messages.deleteTopicHistory`. "There is no dedicated update emitted when a topic is deleted"; you get
    `updateDeleteMessages` [FORUM].
  - Drafts and typing take `top_msg_id`.
- Links: `https://t.me/<bot_username>/<message_thread_id>` "opens the topic only for the user in the private chat
  with whom the topic exists" (10.4) [BAPI#createforumtopic]. That's useful for a group→DM hand-off.
- **Cost**: enabling topics in private chats makes "all subsequent Telegram Stars purchases… subject to a
  non-refundable fee of 15%" while enabled [TOS].
- In **groups** there is nothing AI-specific: use forum supergroups (creating topics needs the admin right
  `can_manage_topics`) or reply chains.
  - Guest mode lets bots that aren't members be @mentioned in any chat. The bot "is able to issue one reply" and gets
    only the summoning message plus the message it replies to [FEAT#guest-bots], [B-GUEST]. On MTProto that is
    `updateBotGuestChatQuery … qts` (Bot API `guest_message` answered with `answerGuestQuery`; the MTProto answer
    method is **UNVERIFIED**).

---

## 5. Ephemeral messages

- **What they are**: "Ephemeral interactions allow a bot and an individual member of a group or supergroup chat to
  communicate privately on the public timeline… They may disappear automatically after some time, or if the app is
  restarted" [BAPI#ephemeral-messages-and-commands], [B-EPH] (2026-07-14). Users can also send **ephemeral commands**
  (`botCommand#9852d6d2 flags:# ephemeral:flags.0?true command description`, Bot API `BotCommand.is_ephemeral`).
  Those are invisible to other members and other bots.
- **Who can receive what, and when** [BAPI#ephemeral-messages-and-commands]:
  - "Any bot can send an ephemeral message to a user **within 15 seconds** of the incoming eligible action". Eligible
    actions are a callback query (`callback_query_id`) or an incoming ephemeral message
    (`reply_parameters.ephemeral_message_id`). The message "will be sent to the exact client application that
    triggered the action".
  - "If the bot is a **chat administrator**, it can send an ephemeral message to any non-bot member of the chat **at
    any time**". Delivery then "may be… across multiple active client applications of the user, but is regardless not
    guaranteed to be delivered to any of them".
  - Since 10.4, an ephemeral **reply to a callback query works in any chat** (except channel DM chats), without
    `receiver_user_id`. `replace_callback_query_message` shows it "in place of the original message".
  - "It is not guaranteed that the user will receive the message, especially if they are offline".
  - A reply to an ephemeral message "must itself be an ephemeral message" and is allowed only "within 15 seconds of
    being sent" [BAPI#replyparameters].
- **MTProto (TDLib) [TL]**:
  - Send: `ephemeral.sendMessage#ba8d5f35 flags:# invert_media welcome anchor noforwards peer:flags.8?InputPeer
    receiver_id:InputUser query_id:flags.0?long message entities media reply_markup rich_message:flags.4?InputRichMessage
    random_id reply_to:flags.5?InputReplyTo` (returns Updates). TDLib sets `anchor` from `replace_callback_query_message`
    [TD:Requests.cpp], [TD:MessagesManager.cpp#L25622-L25679], [TD:MessagesManager.cpp#L1511-L1541].
  - Edit and delete: `ephemeral.editMessage` (text/media/markup/rich), `ephemeral.deleteMessage peer? receiver_id id`.
  - Updates: `updateNewEphemeralMessage`, `updateEditEphemeralMessage`, `updateDeleteEphemeralMessages`, and
    `updateEphemeralBotCallbackQuery` (button press in an ephemeral message).
  - Reply target: `inputReplyToEphemeralMessage#4119b95e id`. Message type: `ephemeralMessage#dd27bee9 … id from_id
    peer_id receiver_id top_msg_id date message entities media reply_markup reply_to rich_message chat_instance anchor_msg_id`.
  - None of these updates carry pts/qts, so **they are not recoverable after downtime** (§7).
- **Content**: text, rich, photo, video, animation, audio, voice, video note, document, sticker, location, venue,
  contact [TDAPI L13415-13430], [FEAT#ephemeral-messages]. Not allowed: `login_url` buttons.
- **Editing**: yes (`editEphemeralMessageText` with a `rich_message` parameter, `…Media`, `…Caption`,
  `…ReplyMarkup`). "It is not guaranteed that the user will receive the message edit event". Deleting
  (`deleteEphemeralMessage`) is also not guaranteed [BAPI#editephemeralmessagetext].
- **Lifetime and IDs**:
  - The lifetime isn't specified ("after some time").
  - TDLib stores incoming ephemeral messages with a local `ttl_period = 7 * 86400` [TD:MessagesManager.cpp#L11648]
    (client-side; **UNVERIFIED** as a server value).
  - `ephemeral_message_id` "may be reused for another ephemeral message after the message is deleted or expires" [BAPI#message].
- **Unknown (UNVERIFIED)**: flood limits for ephemeral sends and edits, whether they count toward the 20/min group
  budget, and random_id dedup semantics.

---

## 6. Classic limits and errors

| Item | Value / rule | Source |
|---|---|---|
| Text message | 1–4096 "characters after entities parsing"; config `message_length_max` = "Maximum length of messages (length in utf8 codepoints)" → **Unicode code points**, TDLib checks `utf8_length` (code points) | [BAPI#sendmessage], [CFGC], [TD:MessageContent.cpp#L5133] |
| Caption | 0–1024 chars for bots (`caption_length_limit_default` 1024; 4096 only for Premium users) | [BAPI#sendphoto], [CFG] |
| Entity offsets/lengths | **UTF-16 code units**; rtrim trailing whitespace from entity length | [ENT] |
| Max entities per message | **Undocumented**. `ENTITIES_TOO_LONG` = "You provided too many styled message entities". Community constant 100 ("found by experimentation… telegram will simply ignore further formatting") → **UNVERIFIED**, handle both outcomes | [ERR], https://docs.python-telegram-bot.org/en/v13.0/telegram.constants.html |
| Nesting (Bot API) | bold/italic/underline/strike/spoiler can contain/be part of anything **except pre and code**; "blockquote and expandable_blockquote entities can't be nested"; all other entities can't contain each other | [BAPI#formatting-options] |
| Nesting (TDLib, finer) | pre/code/date contain nothing; **pre/code may be inside a blockquote** ("Pre and Code can't be part of other entities, except blockquote"); blockquotes can't nest; a blockquote can't be inside link-like entities | [TD:MessageEntity.cpp#L1628-L1646] |
| Expandable quote | `messageEntityBlockquote#f1ccaaac flags:# collapsed:flags.0?true offset length` | [TL] |
| Reply quote | 0–1024 chars, must be an exact substring incl. bold/italic/underline/strike/spoiler/custom_emoji/date | [BAPI#replyparameters] |
| Date entity span | ≤31 UTF-16 units with format flags, ≤127 without (`ENTITY_DATE_TOO_LONG`) | [ENT] |
| Send rate | "In a single chat, avoid sending more than one message per second… eventually… 429"; "In a group, bots are not be able to send more than **20 messages per minute**"; bulk ~30 msg/s (paid broadcast up to 1000/s, MTProto `allow_paid_floodskip` at 0.1 Star/msg) | [FAQ], [BAPI#paid-broadcasts] |
| Edit rate | **Undocumented** for `editMessage`. **Measured 2026-11-11** (groups, rich messages): sends and edits share one per-chat budget (20 operations or 5 heavy ones per fixed minute; 5 operations, at most 1 heavy, per 5 s; deletes free), where an operation is heavy by the size and block structure of the text; see [telegram-flood.md](telegram-flood.md) | [WID#changing-the-state], measured |
| Typing/draft rate | 20/5 s, 40/30 s per peer (setTyping) | [AI] |
| Callback alert | `answerCallbackQuery` text 0–200 chars | [BAPI#answercallbackquery] |
| Copy button | 1–256 chars | [BAPI#copytextbutton] |

**Error strings** (code, name, meaning; from [ERR] unless noted):
- `420 FLOOD_WAIT_%d`: "Please wait %d seconds before repeating the action" (any method). The Bot API renders it
  as "Too Many Requests: retry after X" [TBA].
- `420 FLOOD_PREMIUM_WAIT_%d`: upload/download speed limit for non-Premium accounts. "the query must be
  automatically repeated by the client after X seconds" [FILES]. Bots can't be Premium, so treat it like FLOOD_WAIT.
- `420 SLOWMODE_WAIT_%d` on send methods.
- `400 MESSAGE_NOT_MODIFIED`: "The provided message data is identical to the previous message data". Treat it as
  success. The Bot API text is "message is not modified: specified new message content and reply markup are exactly
  the same as a current content and reply markup of the message" [TBA].
- `400 MESSAGE_TOO_LONG` (sendMessage, editMessage, sendMedia, setTyping, …), `400 ENTITY_BOUNDS_INVALID` (offset or
  length wrong; see UTF-16 rules), `400 ENTITIES_TOO_LONG`, `400 MEDIA_CAPTION_TOO_LONG`, `400 MESSAGE_EMPTY`,
  `400 MESSAGE_ID_INVALID`, `400 MESSAGE_EDIT_TIME_EXPIRED`, `403 MESSAGE_AUTHOR_REQUIRED`, `403 CHAT_WRITE_FORBIDDEN`,
  `400 REPLY_MARKUP_TOO_LONG`, `400 TOPIC_CLOSED`/`TOPIC_DELETED`, `400/403 USER_IS_BLOCKED`, `400 TEXTDRAFT_PEER_INVALID`,
  `400 RANDOM_ID_EMPTY`, `500 RANDOM_ID_DUPLICATE`.

---

## 7. Reliability for MTProto bots

- **Idempotent sends with `random_id`** ([UPD#updatemessageid-updates], quoted):
  - "if the client attempts invoking a method passing a random_id which was already used by a previous method call of
    the same type (or a different type) towards the same peer, from any session of the current account at any time in
    the past (**used random_ids stored by the server do not expire**), the method call will simply **return the
    messages generated by the previous method call**, instead of sending a new message, and if the previous method
    call is currently inflight… a **RANDOM_ID_DUPLICATE** error will be emitted."
  - `updateMessageID#4e90bfd6 id random_id` "is emitted by all methods that take a random_id and generate a Message…
    even if the method response… gets lost, it will eventually be re-delivered… (for example by updates.getDifference)".
  - `sendMultiMedia` "generates that many updateMessageID" (one random_id per `inputSingleMedia`).
  - Recipe:
    1. Persist `(logical_msg_id → random_id, peer)` **before** the first attempt.
    2. On a timeout or a lost session, retry with the same random_id. You get the existing message back, not a duplicate.
    3. On `500 RANDOM_ID_DUPLICATE`, back off and retry (the original call is still in flight).
    4. Also map `updateMessageID` → message id.
  - Never reuse a random_id for a different message to the same peer: you'd silently get the old message back.
    Edits need no random_id; they're idempotent and return `MESSAGE_NOT_MODIFIED` on a repeat.
- **Catch-up after downtime** [UPD]:
  - `updates.getState`, `updates.getDifference`, `updates.getChannelDifference`, `messages.getMessages` and
    `channels.getMessages` all say "Both users and bots can use this method" (method pages).
  - Persist `pts/qts/date/seq` and the per-channel `pts`.
  - On startup, call only `updates.getDifference`. Channel catch-up is "automatically triggered (only for channels
    that need catching up) by… updateChannelTooLong".
  - Paginate `updates.differenceSlice` (save `intermediate_state`) and `updates.channelDifference` until `final`.
    Recommended limits: "10-100 for channels and 1000-10000 otherwise".
  - "Message boxes… usually… 100000 for channels and 5000000 for the common message box". Beyond that you get
    `updates.differenceTooLong` / `updates.channelDifferenceTooLong`: reset the state and fill gaps manually.
  - Call getDifference after "no updates for 15 minutes or longer", after a new-session notification, and after
    unfilled gaps (wait ≤0.5 s first).
- **Retention for bots**: MTProto docs specify box sizes, not a time limit, for bots: **UNVERIFIED**. The "24 hours"
  rule is the Bot API server's own queue ("will not be kept longer than 24 hours" [BAPI#getting-updates]), and "earliest
  100 unconfirmed updates" is Bot API `getUpdates` paging [FAQ]. Neither applies to MTProto directly.
  - **Not recoverable at all** (no pts/qts in [TL]): `updateBotCallbackQuery`, `updateInlineBotCallbackQuery`,
    `updateBotInlineQuery`, `updateUserTyping` (incl. **draft stop**), `updateBotWidgetQuery*`, `updateNewEphemeralMessage`,
    `updateEditEphemeralMessage`, `updateEphemeralBotCallbackQuery`.
  - qts-sequenced, so recoverable: `updateBotStopped`, `updateChatParticipant`, `updateChannelParticipant`,
    `updateBotMessageReaction(s)`, `updateBotChatInviteRequester`, `updateBotGuestChatQuery`, business updates, and others.
  - Messages (`updateNewMessage`/`updateNewChannelMessage`) are pts-based and recoverable.
- **Parallel sessions**: "each update will be delivered via only one of the currently active sessions, chosen
  randomly" [UPD]. Don't run two consumers on one bot authorization unless they coordinate.
- **Fetching messages as a bot**:
  - Use `messages.getMessages(id:Vector<InputMessage>)` for private chats and basic groups (the common ID sequence).
    Use `channels.getMessages(channel, id)` for supergroups and channels (per-channel IDs, the same for everyone)
    [UPD#message-id-sequences].
  - `InputMessage` variants: `inputMessageID`, **`inputMessageReplyTo id`** (fetch the parent of a message, which is
    handy for walking reply chains), `inputMessagePinned`, `inputMessageCallbackQuery id query_id` ("Used by bots…").
  - Bots **cannot** use `messages.getHistory`, `messages.search`, `messages.getReplies`, `messages.getDiscussionMessage`,
    `messages.getRichMessage`, `messages.getForumTopics` or `messages.readHistory` (`user_only` in [ERR]). So keep your
    own store of conversation trees.
- **File references** [FREF], [FILES]:
  - Errors: `FILE_REFERENCE_EXPIRED` / `FILE_REFERENCE_INVALID` (treat both the same) on `upload.getFile` and
    `messages.sendMedia`; `FILE_REFERENCE_%d_EXPIRED`/`_INVALID` (index into `multi_media`) on `sendMultiMedia`;
    `FILE_REFERENCE_EMPTY`.
  - Fix: "refreshed by refetching the message, story, etc where the media last appeared". Store `(peer, msg_id)` per
    file id and re-fetch with get(Channel)Messages.
  - The expiry period is undocumented: **UNVERIFIED**.
  - Media inside rich messages: TDLib uses `fileSourceRichMessage` via `messages.getRichMessage`, which is user-only.
    Bots must re-fetch the message instead (**UNVERIFIED** that this refreshes rich-media refs).

---

## 8. Media handling

- **Deduplication**:
  - The file-reference DB keys files by `fileIdPhoto id:long` / `fileIdDocument id:long`, which "uniquely identify a
    photo or a document" [FREF].
  - TDLib's `file_unique_id` = type class + document id (photos: + size type). **`access_hash` is not part of
    identity** [TD:files/FileLocation.hpp#L106-L175]. Bot API: `file_unique_id` "is supposed to be the same over time
    and for different bots".
  - Forwards and resends reference the same object ("resent… without reuploading", [FILES#resending-existing-files]),
    so the **id is stable across forwards/resends**.
  - A *new upload* of identical bytes probably gets a new id: **UNVERIFIED**. `messages.getDocumentByHash`
    (sha256+size+mime) only finds "some types of documents like GIFs".
  - Key your cache by `photo.id` (+ chosen size type) / `document.id`; keep `access_hash` + `file_reference` only for
    download.
- **Albums**: `message.grouped_id:flags.17?long` (Bot API `media_group_id`), ≤10 items per group. "For photo albums,
  clients should display an album caption only if exactly one photo in the group has a caption" [FILES#albums-grouped-media].
  There is no official "collect all parts" guidance. Heuristic (**UNVERIFIED**):
  1. Buffer by `(peer, grouped_id)` and finish after ~1–2 s with no new part, or at 10 parts. Parts are usually sent
     by one `sendMultiMedia` and typically arrive together.
  2. Treat the caption-bearing item's mention/reply as the trigger for the whole group.
  3. If parts are missing, fetch neighbouring IDs (±9) via `channels.getMessages`/`messages.getMessages` and filter
     by `grouped_id`.
- **Downloads**:
  - The MTProto bot has **no 20 MB cap**. That cap is the cloud Bot API's ("bots can download files of up to 20MB"
    [BAPI#getfile]); a Local Bot API server, itself an MTProto client, can "Download files without a size limit"
    [BAPI#using-a-local-bot-api-server].
  - Files can be up to 2000 MB (4000 parts × 512 KB, `upload_max_fileparts_default`) or 4000 MB when uploaded by
    Premium users (8000 parts) [CFG], [FILES].
  - `upload.getFile`: `limit` ≤1 MB, divisible by 4 KB, and 1 MB divisible by `limit` (or the `precise` rules); the
    requested part must stay within one 1 MB window.
  - Handle `FILE_MIGRATE_X`, `upload.fileCdnRedirect` (when `cdn_supported` is set) and
    `FLOOD_WAIT`/`FLOOD_PREMIUM_WAIT`. Limit parallelism per `small/large_queue_max_active_operations_count` [FILES#downloading-files].
- **Uploads** (generated images, documents):
  - `upload.saveFilePart` for files ≤10 MB, `upload.saveBigFilePart` above that.
  - Part size: a multiple of 1 KB that divides 512 KB; 512 KB is recommended.
  - Parts persist "between several minutes and several hours". Errors include `FILE_PARTS_INVALID`,
    `FILE_PART_SIZE_CHANGED` and others [FILES#uploading-files].
  - Then `messages.sendMedia(inputMediaUploadedPhoto | inputMediaUploadedDocument(mime_type, attributes:[documentAttributeFilename], force_file))`.
  - Photos: Bot API limit "at most 10 MB… width and height must not exceed 10000 in total. Width and height ratio
    must be at most 20" [BAPI#sendphoto]. MTProto errors: `PHOTO_INVALID_DIMENSIONS`, `PHOTO_SAVE_FILE_INVALID`,
    `IMAGE_PROCESS_FAILED`.
  - Send the original PNG as a document if fidelity matters.
- **`messages.sendMultiMedia`**:
  - `inputSingleMedia#1cc6e91f flags:# media random_id message entities`, up to 10.
  - The docs say each item can be "uploaded or pre-existing". In practice TDLib **first converts uploaded media via
    `messages.uploadMedia`** into `inputMediaPhoto`/`inputMediaDocument` [TD:MessagesManager.cpp#L22373-L22390],
    [TD:MessageContent.cpp#L6420-L6441]. Do the same.
  - "Documents and audio files can be only grouped in an album with messages of the same type" [BAPI#sendmediagroup].
  - Errors: `MULTI_MEDIA_TOO_LONG`, `MEDIA_GROUPED_INVALID`, `FILE_REFERENCE_%d_*`.
- **Images inside rich messages**: `messages.uploadMedia(peer, inputMediaUploadedPhoto)` returns a `messageMediaPhoto`.
  Use it as `inputPhoto(id, access_hash, file_reference)` in `inputRichMessage.photos`, plus
  `pageBlockPhoto(photo_id=id)`. It counts toward the 50-media limit.

---

## 9. Recommendations for an LLM-streaming bot in busy groups

### (a) Streaming partial output
- **Groups, supergroups and topics: drafts are impossible** (`TEXTDRAFT_PEER_INVALID`). Use the classic method:
  `messages.sendMessage` (placeholder, `reply_to` = the user's message, optionally `quote_text`) followed by
  `messages.editMessage`.
  - The budget is the binding constraint. Officially "20 messages per minute" per group [FAQ], and edits very likely
    count too (**UNVERIFIED**).
  - So run **one token bucket per chat, shared by all concurrent streams in that chat**:
    1. Size it at about 15–18 operations/min (keep headroom for final messages).
    2. Divide fairly between active streams, prioritising "final" and "first" operations.
    3. Always send the *latest* snapshot (coalesce) and drop intermediate ones.
    4. On `FLOOD_WAIT_X`, freeze that chat's queue for X s and resume with the latest content.
  - With 4 concurrent streams that is ≈1 edit per 15 s per stream. Make the interval adaptive: fast in quiet
    groups, final-only in very busy ones.
  - Between edits, show `sendMessageTypingAction` via `setTyping`. It has a separate 20/5 s and 40/30 s per-peer
    budget and lasts ≤5 s [BAPI#sendchataction]. Whether typing counts toward message limits is **UNVERIFIED**.
  - **Format while streaming**: stream as plain text with a few safe entities (or as simple rich paragraphs). At the
    end, do one `editMessage` that converts the message to the full rich layout (Text→Rich edits are allowed by
    TDLib). That avoids re-rendering half-parsed Markdown tables and fences.
  - If streaming as rich, keep each snapshot within 500 blocks and 16 levels. Remember the server re-validates the
    entire `InputRichMessage` on every edit.
  - Optional for crowded groups (**UNVERIFIED** rate behaviour): stream into an **ephemeral message** visible only to
    the requester, then post one public final message. This needs the bot to be an admin (any-time ephemeral) or a
    request that arrived as an ephemeral command or button tap (15 s window). Delivery of ephemeral edits "is not
    guaranteed". Test before relying on it.
- **Private chats: use live drafts.**
  1. Pick one non-zero `random_id` per answer and set `can_stop=1`.
  2. Show `pageBlockThinking` (rich draft) or empty text while the model reasons.
  3. Append-only updates, ~1 per second (≤20/5 s and ≤40/30 s per user), refreshed at least every 25 s.
  4. Finish with `messages.sendMessage(rich_message)` using its own persisted message `random_id`.
  5. On `updateUserTyping{sendMessageStopDraftAction}`, cancel the LLM request and, if wanted, send the partial
     text as a real message.
  - Drafts aren't persisted. The 4096 cap applies to text drafts, so use rich drafts for long answers.
  - Parallel answers to the same user collide (one visible draft, and any final message clears the others). Serialize
    per chat, or use bot-forum topics (15% Stars fee caveat).
  - Optional group→DM hand-off: post a short group reply with a `https://t.me/<bot>/<thread_id>` topic link (10.4).

### (b) Final long answers with collapsed reasoning
- Send a **rich message** (blocks built locally):
  `[pageBlockDetails(open=false, title="Reasoning · 12 s", blocks=[paragraphs/lists/pre…]), <answer blocks…>]`.
  - Code goes into `pageBlockPreformatted(language)` (libprisma names), formulas into `pageBlockMath`/`textMath`, and
    tables into `pageBlockTable`: at most 20 columns, with inline-only cells.
  - Set `noautolink`. Otherwise LLM text like "@name", "#tag" or "/cmd" is auto-detected (and mentions might ping
    people; **UNVERIFIED**). Add explicit `textUrl`s for links you want.
- **Budget**: 32768 chars including formula source, 500 blocks including list items and table rows, depth 16, 50
  media. Reasoning eats this budget, so truncate it or move it into a `.md` document (`pageBlockDocument` or a
  separate `sendMedia`) when it's huge.
- Above the limits, split the answer into several rich messages chained by `reply_to`. Clients collapse anything
  beyond ~8000 chars behind "Show More" anyway [B-RICH].
- Never put `pageBlockThinking` in final messages.
- Old-client fallback is unknown (**UNVERIFIED**). Keep a config switch for the classic layout: plain text ≤4096
  code points, reasoning in an expandable blockquote (`messageEntityBlockquote collapsed`; `pre` inside it is
  allowed), and ~≤100 entities, with a plain-text retry on `ENTITIES_TOO_LONG`/`ENTITY_BOUNDS_INVALID`.

### (c) Per-user notices (errors, quotas, stats) without spamming the group
- **Ephemeral messages** (`ephemeral.sendMessage`, `receiver_id=user`):
  - **Bot is an admin**: send at any time to any non-bot member.
  - **Not an admin**: only within 15 s of a callback query (button) or as a reply to the user's ephemeral command
    (declare commands such as `/usage`, `/settings`, `/raw` with `botCommand.ephemeral`).
  - Delivery is best-effort ("not guaranteed", especially offline), so never send anything critical only this way.
    Also the update isn't recoverable after downtime.
- **Button-triggered notices**: `messages.setBotCallbackAnswer` with an alert (≤200 chars), or an ephemeral reply.
  For the latter you must still answer the callback query.
- **Fallback** for non-admin bots with notices not triggered by the user: a DM (only if the user has started the bot),
  or a tiny public reply deleted after N seconds (this costs group budget).

### (d) Raw Markdown / original files on demand
1. Store the raw Markdown, prompts and original attachments server-side, keyed by `(peer, msg_id)`. Bots can't read
   history, and file refs expire, so keep `(peer, msg_id)` sources for refetching.
2. Attach a callback button (inline keyboard or a `pageBlockButtonRow`) such as "Raw" or "Files" to the final message.
3. On `updateBotCallbackQuery`, **within 15 s**, call `ephemeral.sendMessage(query_id=…, receiver_id=user)` with:
   - a `.md` document (`inputMediaUploadedDocument`, `force_file`) for anything large, or
   - a rich message with `pageBlockPreformatted(language="markdown")` (≤32768 chars).
   Then call `messages.setBotCallbackAnswer`. This works for non-admin bots too. Since 10.4 callback ephemeral replies
   work in any chat. Optionally set `anchor` (replace the message only for that user).
4. `inlineButtonTypeCopy` holds only 256 chars, so it's useful for short snippets, not whole answers.
- For a persistent copy, use a deep link (`t.me/<bot>?start=raw_<id>`) that sends the file in the DM.
- Inline `pageBlockDocument` inside the public answer is visible to everyone and counts toward the 50-media limit.
  Prefer on-demand.

### Cross-cutting
- Treat MTProto details of features newer than layer 225 as "TDLib-derived". Log raw RPC errors for rich, ephemeral
  and widget calls; new error strings are not in errors.json yet.
- Persist the update state and a random_id ledger. Assume callbacks, stop-draft and ephemeral events are lossy
  across restarts.
- Skip widgets for now: DMs only in production, 4 KB code, 1 KB state, no scroll.

### Open questions to test on the test server
1. The edit flood budget per group (do edits count toward the 20/min, and do rich edits cost more?).
2. Ephemeral send/edit rate limits.
3. What the server does with `pageBlockThinking` in `sendMessage`.
4. Reply-preview and notification text of rich messages.
5. How old clients render rich messages.
6. Whether bots ever receive `richMessage.part`.
7. The supported LaTeX subset and unknown code languages.
8. Whether `noautolink` suppresses mention notifications.
9. File-reference lifetime.
10. Whether MTProto bot updates are retained by time or only by message-box size.
